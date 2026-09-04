//! Built-in admin UI plus a replaceable public theme.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::random_token;
use crate::{App, Shared};

#[derive(RustEmbed)]
#[folder = "web-admin/dist"]
struct AdminAssets;

#[derive(RustEmbed)]
#[folder = "target/theme/dist"]
struct DefaultThemeAssets;

#[derive(Clone, Deserialize, Serialize)]
pub struct Theme {
    pub name: String,
    pub short: String,
    pub description: String,
    pub version: String,
    pub author: String,
    pub url: String,
    #[serde(default)]
    pub selected: bool,
}

pub async fn serve(State(app): State<Shared>, headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let known = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok());
    if is_api_path(path) {
        return (StatusCode::NOT_FOUND, format!("no such endpoint: /{path}")).into_response();
    }

    if path == "admin" || path.starts_with("admin/") {
        let path = path.strip_prefix("admin").unwrap_or(path).trim_start_matches('/');
        return embedded::<AdminAssets>(
            path,
            "the panel is not built; run `npm run build` in web-admin/",
            known,
        );
    }

    let theme = app.db.get("theme").unwrap_or_default();
    if let Some(root) = external_theme(&app.themes, &theme) {
        if let Some(response) = disk(&root, path, known) {
            return response;
        }
    }
    embedded::<DefaultThemeAssets>(path, "the default theme is missing; run scripts/theme.sh", known)
}

fn is_api_path(path: &str) -> bool {
    path == "api" || path.starts_with("api/")
}

/// Everything a build writes under `assets/` carries a content hash, so a miss
/// there is a request for a file that no longer exists, never a route. Falling
/// back to index.html answers a script tag with HTML, which the browser refuses
/// on MIME type. Hashed names are also why `asset` marks these immutable for a
/// year, so both answers read the prefix from here.
fn is_asset(path: &str) -> bool {
    path.starts_with("assets/")
}

fn embedded<T: RustEmbed>(requested: &str, remedy: &str, known: Option<&str>) -> Response {
    let path = if requested.is_empty() { "index.html" } else { requested };
    if let Some(file) = T::get(path) {
        return asset(path, file.data.into_owned(), known);
    }
    if is_asset(path) {
        return (StatusCode::NOT_FOUND, format!("no such asset: /{path}")).into_response();
    }
    match T::get("index.html") {
        Some(index) => asset("index.html", index.data.into_owned(), known),
        None => (StatusCode::NOT_FOUND, remedy.to_owned()).into_response(),
    }
}

fn disk(root: &Path, requested: &str, known: Option<&str>) -> Option<Response> {
    let path = if requested.is_empty() { "index.html" } else { requested };
    if let Some(data) = read_inside(root, path) {
        return Some(asset(path, data, known));
    }
    // None, not a 404: an external theme without the file leaves the answer to
    // the built-in one, which refuses it there.
    if is_asset(path) {
        return None;
    }
    read_inside(root, "index.html").map(|data| asset("index.html", data, known))
}

/// One file, with the caching the two kinds of path earn.
///
/// Hashed names under `assets/` are immutable for a year: the browser never
/// comes back to ask, so an ETag there would be a hash computed for nobody.
///
/// Everything else is the SPA shell, and it is `no-cache` -- revalidate every
/// time -- because its bytes change under the same URL: a new hub build, or
/// the public page being switched to another theme. Without a validator that
/// means re-sending the whole shell on every request, which is what a CDN in
/// front is then tempted to paper over with a timed `s-maxage`, and a timed
/// window is exactly what makes a theme switch take a minute to show up. With
/// one, the revalidation costs a 304 and the switch lands on the next request.
fn asset(path: &str, data: Vec<u8>, known: Option<&str>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    if is_asset(path) {
        let cache = "public, max-age=31536000, immutable";
        return ([(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache)], data)
            .into_response();
    }
    // Half a SHA-256 of the body. Strong, because it is the body: two shells
    // that hash the same are the same shell.
    let etag = format!("\"{}\"", &hex::encode(Sha256::digest(&data))[..32]);
    let headers = [
        (header::CONTENT_TYPE, mime.as_ref()),
        (header::CACHE_CONTROL, "no-cache"),
        (header::ETAG, etag.as_str()),
    ];
    if known == Some(etag.as_str()) {
        return (StatusCode::NOT_MODIFIED, headers).into_response();
    }
    (headers, data).into_response()
}

/// Reads only regular files whose canonical path remains below `root`.
/// Canonicalizing both sides also rejects symlinks that point outside it.
fn read_inside(root: &Path, relative: &str) -> Option<Vec<u8>> {
    let root = root.canonicalize().ok()?;
    let file = root.join(relative).canonicalize().ok()?;
    if !file.starts_with(&root) || !file.is_file() {
        return None;
    }
    fs::read(file).ok()
}

fn valid_short(short: &str) -> bool {
    !short.is_empty()
        && short != "default"
        && short.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

fn external_theme(themes: &Path, short: &str) -> Option<PathBuf> {
    if !valid_short(short) {
        return None;
    }
    let themes = themes.canonicalize().ok()?;
    let root = themes.join(short).canonicalize().ok()?;
    if !root.starts_with(&themes) || !root.is_dir() {
        return None;
    }
    let dist = root.join("dist").canonicalize().ok()?;
    // Only that the entry point exists, not what is in it: this runs on every
    // request the theme serves. A symlinked one is still read through
    // `read_inside`, which is where escaping the directory is refused.
    if !dist.starts_with(&root) || !dist.is_dir() || !dist.join("index.html").is_file() {
        return None;
    }
    Some(dist)
}

fn manifest(root: &Path, short: &str) -> Option<Theme> {
    let data = read_inside(root, "theme.json")?;
    if data.len() > 64 * 1024 {
        return None;
    }
    let theme: Theme = serde_json::from_slice(&data)
        .inspect_err(|e| tracing::warn!("{short}/theme.json 不是有效的 manifest，主题不会出现在列表里：{e}"))
        .ok()?;
    (theme.short == short).then_some(theme)
}

pub fn themes(app: &App) -> std::io::Result<Vec<Theme>> {
    let mut list = vec![serde_json::from_str(include_str!("../target/theme/theme.json"))
        .expect("the built-in theme manifest must be valid")];

    if let Ok(base) = app.themes.canonicalize() {
        for entry in fs::read_dir(&base)? {
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else { continue };
            let Some(short) = entry.file_name().to_str().map(str::to_owned) else { continue };
            if !kind.is_dir() || !valid_short(&short) || external_theme(&base, &short).is_none() {
                continue;
            }
            let Ok(root) = entry.path().canonicalize() else { continue };
            if let Some(theme) = manifest(&root, &short) {
                list.push(theme);
            }
        }
    }

    let configured = app.db.get("theme").unwrap_or_default();
    let selected =
        if list.iter().any(|theme| theme.short == configured) { configured } else { "default".into() };
    for theme in &mut list {
        theme.selected = theme.short == selected;
    }
    list[1..].sort_by(|a, b| a.name.cmp(&b.name));
    Ok(list)
}

pub fn selectable(app: &App, short: &str) -> bool {
    short.is_empty()
        || short == "default"
        || themes(app).is_ok_and(|list| list.iter().any(|t| t.short == short))
}

// ---- installing a theme from an uploaded archive ----

/// What one archive may expand to. A gz stream hides any ratio behind the
/// 32 MiB upload ceiling, so the expanded total is its own gate rather than a
/// multiple of what arrived; the entry count and the per-file size bound the
/// two other shapes a decompression bomb takes.
const MAX_ENTRIES: usize = 2_000;
const MAX_FILE: u64 = 8 << 20;
const MAX_EXPANDED: u64 = 64 << 20;

/// Installs a theme from its published `theme.tar.gz`, under the name its own
/// manifest carries.
///
/// Nothing reaches `<themes>/<short>` until the whole archive has been
/// unpacked and checked. The work lands in a sibling directory whose name
/// `valid_short` rejects, so a half-written theme is invisible to both
/// `themes()` and `serve`, and publishing it is a single rename -- there is no
/// moment where the public page is served out of an incomplete directory.
/// A theme being replaced is renamed aside rather than deleted, and renamed
/// back if the publish cannot complete.
///
/// `expect` is the short name the caller is replacing, when it is replacing a
/// particular one -- an upload installs whatever it carries, an update may not.
pub fn install<R: Read>(themes: &Path, archive: R, expect: Option<&str>) -> Result<Theme> {
    let staging = themes.join(format!(".staging-{}", &random_token()[..16]));
    let installed = unpack(archive, &staging).and_then(|()| publish(themes, &staging, expect));
    if installed.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    installed
}

fn unpack<R: Read>(archive: R, into: &Path) -> Result<()> {
    fs::create_dir(into)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut expanded = 0u64;
    for (seen, entry) in archive.entries()?.enumerate() {
        let mut entry = entry.context("主题包不是有效的 tar.gz")?;
        if seen >= MAX_ENTRIES {
            bail!("主题包里的条目超过 {MAX_ENTRIES} 个");
        }
        // Only what a theme is made of. A symlink, hard link or device node
        // has no business in one, and each is a way to write somewhere the
        // path check below never sees.
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            bail!("主题包里有不支持的条目：{}", entry.path()?.display());
        }
        let size = entry.size();
        if size > MAX_FILE {
            bail!("{} 超过单个文件 {} MiB 的上限", entry.path()?.display(), MAX_FILE >> 20);
        }
        // Subtraction, because adding two entry sizes can wrap; `size` is
        // already known to be the smaller of the two.
        if expanded > MAX_EXPANDED - size {
            bail!("主题包解压后超过 {} MiB", MAX_EXPANDED >> 20);
        }
        expanded += size;
        // Refuses an entry whose path leaves `into` -- absolute, `..`, or
        // through a symlinked parent -- and reports it as `false` rather than
        // an error.
        if !entry.unpack_in(into)? {
            bail!("主题包里的路径越出了主题目录");
        }
    }
    Ok(())
}

/// Checks the unpacked directory is a theme this hub can actually serve, then
/// moves it into place under the name its manifest asks for.
fn publish(themes: &Path, staging: &Path, expect: Option<&str>) -> Result<Theme> {
    let manifest = read_inside(staging, "theme.json").context("主题包里没有 theme.json")?;
    if manifest.len() > 64 * 1024 {
        bail!("theme.json 过大");
    }
    let theme: Theme = serde_json::from_slice(&manifest).context("theme.json 格式不对")?;
    if !valid_short(&theme.short) {
        bail!("theme.json 里的 short 不能作为目录名：{:?}", theme.short);
    }
    // Updating a theme replaces the one named on the button. A package whose
    // manifest carries some other `short` would install a second theme instead
    // -- or overwrite an unrelated one -- and report it as that update.
    if let Some(expected) = expect.filter(|&expected| expected != theme.short) {
        bail!("这个包里是主题 {:?}，不是 {expected:?}", theme.short);
    }
    // The one file `serve` needs. Without it every request falls through to
    // the built-in theme, which looks exactly like the upload having done
    // nothing at all.
    if !staging.join("dist").join("index.html").is_file() {
        bail!("主题包里没有 dist/index.html");
    }

    let destination = themes.join(&theme.short);
    let replaced = themes.join(format!(".replaced-{}", &random_token()[..16]));
    let replacing = destination.exists();
    if replacing {
        fs::rename(&destination, &replaced)?;
    }
    match fs::rename(staging, &destination) {
        Ok(()) => {
            if replacing {
                let _ = fs::remove_dir_all(&replaced);
            }
            Ok(theme)
        }
        Err(e) => {
            // Put back whatever was serving a moment ago.
            if replacing {
                let _ = fs::rename(&replaced, &destination);
            }
            Err(e.into())
        }
    }
}

/// The panel's thumbnail for one theme: an optional `preview.png` sitting
/// beside `theme.json`, outside `dist/` because it is metadata, not something
/// the theme serves.
///
/// The name is a constant rather than a manifest field, which is the whole
/// point: a path that never comes from the archive is a path with nothing to
/// escape through. The size gate is here and not only in `unpack` because a
/// theme copied straight into the directory never went through it.
pub fn preview(themes: &Path, short: &str) -> Option<Vec<u8>> {
    if !valid_short(short) {
        return None;
    }
    let root = themes.join(short);
    if fs::metadata(root.join(PREVIEW)).ok()?.len() > MAX_FILE {
        return None;
    }
    read_inside(&root, PREVIEW)
}

const PREVIEW: &str = "preview.png";

/// Deletes an installed theme. The built-in one has no directory and its name
/// is not a valid one, so it cannot be named here. Deleting the theme that is
/// currently selected is allowed: `serve` falls back to the built-in one from
/// the next request on, which is the same path a broken theme already takes.
pub fn remove(themes: &Path, short: &str) -> Result<()> {
    if !valid_short(short) {
        bail!("没有这个主题");
    }
    let base = themes.canonicalize()?;
    let root = base.join(short).canonicalize()?;
    // Canonical on both sides: a symlink pointing out of the themes directory
    // is not something to delete through.
    if !root.starts_with(&base) || !root.is_dir() {
        bail!("没有这个主题");
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_files_cannot_escape_their_dist_directory() {
        let base = std::env::temp_dir().join(format!(
            "monitor-theme-path-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let dist = base.join("theme/dist");
        fs::create_dir_all(dist.join("assets")).unwrap();
        fs::write(dist.join("index.html"), "index").unwrap();
        fs::write(dist.join("assets/app.js"), "safe").unwrap();
        fs::write(base.join("secret"), "secret").unwrap();

        assert_eq!(read_inside(&dist, "assets/app.js").unwrap(), b"safe");
        assert!(read_inside(&dist, "../../secret").is_none());
        assert!(read_inside(&dist, "/etc/passwd").is_none());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(base.join("secret"), dist.join("assets/link")).unwrap();
            assert!(read_inside(&dist, "assets/link").is_none());
        }

        fs::remove_dir_all(base).unwrap();
    }

    /// Everything an uploaded archive has to clear before it can take the
    /// place of a theme that is currently being served.
    #[test]
    fn an_uploaded_theme_is_checked_before_it_replaces_the_one_in_place() {
        let base = std::env::temp_dir().join(format!(
            "monitor-install-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        let archive = base.join("upload.tar.gz");

        let manifest: &[u8] =
            r#"{"name":"极光","short":"aurora","description":"","version":"1","author":"a","url":""}"#
                .as_bytes();
        let pack = |files: &[(&str, &[u8])], link: Option<(&str, &str)>| {
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
                fs::File::create(&archive).unwrap(),
                flate2::Compression::fast(),
            ));
            for (name, data) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                builder.append_data(&mut header, name, *data).unwrap();
            }
            if let Some((name, target)) = link {
                let mut header = tar::Header::new_gnu();
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_size(0);
                header.set_mode(0o777);
                builder.append_link(&mut header, name, target).unwrap();
            }
            builder.into_inner().unwrap().finish().unwrap();
            fs::File::open(&archive).unwrap()
        };

        // The directory is named by the manifest, never by the file that
        // arrived.
        let theme = install(&base, pack(&[("theme.json", manifest), ("dist/index.html", b"v1")], None), None)
            .unwrap();
        assert_eq!(theme.short, "aurora");
        assert_eq!(fs::read(base.join("aurora/dist/index.html")).unwrap(), b"v1");

        // Same name again: replaced whole, not merged into.
        install(
            &base,
            pack(&[("theme.json", manifest), ("dist/index.html", b"v2"), ("dist/old.js", b"x")], None),
            None,
        )
        .unwrap();
        install(&base, pack(&[("theme.json", manifest), ("dist/index.html", b"v3")], None), None).unwrap();
        assert_eq!(fs::read(base.join("aurora/dist/index.html")).unwrap(), b"v3");
        assert!(!base.join("aurora/dist/old.js").exists(), "the replaced theme must not leave files behind");

        // A theme the hub cannot serve, a name that cannot be a directory, and
        // a symlink -- the one entry type that writes where the path check
        // never looks.
        for bad in [
            pack(&[("theme.json", manifest)], None),
            pack(&[("theme.json", r#"{"name":"x","short":"../evil","description":"","version":"1","author":"a","url":""}"#.as_bytes()), ("dist/index.html", b"x")], None),
            pack(&[("theme.json", manifest), ("dist/index.html", b"x")], Some(("dist/link", "/etc/passwd"))),
            pack(&[("dist/index.html", b"x")], None),
        ] {
            assert!(install(&base, bad, None).is_err());
        }

        // None of that touched the theme being served, and none of it left a
        // staging directory behind.
        assert_eq!(fs::read(base.join("aurora/dist/index.html")).unwrap(), b"v3");
        let left: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|name| name != "upload.tar.gz")
            .collect();
        assert_eq!(left, ["aurora"]);

        // An optional preview.png rides along; a theme without one, and a name
        // that is not a theme, both answer nothing rather than erroring.
        install(
            &base,
            pack(&[("theme.json", manifest), ("dist/index.html", b"v3"), ("preview.png", b"PNG")], None),
            None,
        )
        .unwrap();
        assert_eq!(preview(&base, "aurora").unwrap(), b"PNG");
        install(&base, pack(&[("theme.json", manifest), ("dist/index.html", b"v3")], None), None).unwrap();
        assert!(preview(&base, "aurora").is_none() && preview(&base, "default").is_none());

        // An update replaces the theme whose button was pressed, so a package
        // that renamed itself is refused rather than installed alongside.
        assert!(install(
            &base,
            pack(&[("theme.json", manifest), ("dist/index.html", b"v4")], None),
            Some("nebula"),
        )
        .is_err());
        assert!(!base.join("nebula").exists());
        assert_eq!(fs::read(base.join("aurora/dist/index.html")).unwrap(), b"v3");
        install(&base, pack(&[("theme.json", manifest), ("dist/index.html", b"v4")], None), Some("aurora"))
            .unwrap();
        assert_eq!(fs::read(base.join("aurora/dist/index.html")).unwrap(), b"v4");

        // And deleting it is the way back out.
        remove(&base, "aurora").unwrap();
        assert!(!base.join("aurora").exists());
        assert!(remove(&base, "default").is_err() && remove(&base, "../etc").is_err());

        fs::remove_dir_all(base).unwrap();
    }

    /// The shell changes under a fixed URL -- a new build, or the public page
    /// switched to another theme -- so it revalidates. The validator is what
    /// keeps that from costing the whole file, and what makes a switch land on
    /// the next request instead of when a proxy's timer runs out.
    #[test]
    fn the_spa_shell_revalidates_by_etag_and_a_hashed_asset_carries_none() {
        let etag = |r: &Response| r.headers().get(header::ETAG).map(|v| v.to_str().unwrap().to_owned());

        let first = asset("index.html", b"<html>default</html>".to_vec(), None);
        assert_eq!(first.status(), StatusCode::OK);
        let tag = etag(&first).expect("the shell must carry a validator");

        // Same shell, and the browser says which one it holds: nothing to send.
        let again = asset("index.html", b"<html>default</html>".to_vec(), Some(&tag));
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);

        // Switched theme: same URL, same request header, different bytes -- so
        // the tag moved with them and the answer is the new shell.
        let switched = asset("index.html", b"<html>demo</html>".to_vec(), Some(&tag));
        assert_eq!(switched.status(), StatusCode::OK);
        assert_ne!(etag(&switched), Some(tag));

        // A hashed name is immutable for a year: the browser never comes back
        // to ask, so hashing it would be work done for nobody.
        let hashed = asset("assets/index-CSjcYfL9.js", b"console.log(1)".to_vec(), None);
        assert_eq!(hashed.status(), StatusCode::OK);
        assert_eq!(etag(&hashed), None);
    }

    /// The two guards on a theme name, which the settings page runs together:
    /// which strings may name a directory on disk, and which names the panel
    /// may switch to.
    #[test]
    fn a_theme_name_is_checked_before_it_reaches_the_disk_or_the_settings_row() {
        assert!(valid_short("aurora") && valid_short("my-theme_2"));
        // Anything that could steer the join elsewhere.
        for bad in ["", "..", "a/b", "a\\b", "./x", "~", "a b", "th\u{e9}me"] {
            assert!(!valid_short(bad), "{bad:?} must not name a theme directory");
        }
        // The built-in theme is served from the binary; a directory answering
        // to its name would take its place.
        assert!(!valid_short("default"));

        // Still a theme the panel can switch to, along with the empty string
        // it stores as: switching back is the way out of a broken external
        // theme, so it can never be refused. `selectable` names it twice for
        // that reason -- in its own right, and through the list it leads.
        let app = App::for_test(crate::db::Db::open(":memory:").unwrap());
        assert!(selectable(&app, "") && selectable(&app, "default"));
        assert!(!selectable(&app, "aurora"), "a theme that is not installed is not selectable");
    }
}

//! Built-in admin UI plus a replaceable public theme.

use std::fs;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

use crate::{App, Shared};

#[derive(RustEmbed)]
#[folder = "web-admin/dist"]
struct AdminAssets;

#[derive(RustEmbed)]
#[folder = "web-theme/dist"]
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

pub async fn serve(State(app): State<Shared>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if is_api_path(path) {
        return (StatusCode::NOT_FOUND, format!("no such endpoint: /{path}")).into_response();
    }

    if path == "admin" || path.starts_with("admin/") {
        let path = path.strip_prefix("admin").unwrap_or(path).trim_start_matches('/');
        return embedded::<AdminAssets>(path, "web-admin");
    }

    let theme = app.db.get("theme").unwrap_or_default();
    if let Some(root) = external_theme(&app.themes, &theme) {
        if let Some(response) = disk(&root, path) {
            return response;
        }
    }
    embedded::<DefaultThemeAssets>(path, "web-theme")
}

fn is_api_path(path: &str) -> bool {
    path == "api" || path.starts_with("api/")
}

fn embedded<T: RustEmbed>(requested: &str, source: &str) -> Response {
    let path = if requested.is_empty() { "index.html" } else { requested };
    if let Some(file) = T::get(path) {
        return asset(path, file.data.into_owned());
    }
    match T::get("index.html") {
        Some(index) => asset("index.html", index.data.into_owned()),
        None => (StatusCode::NOT_FOUND, format!("frontend not built; run `npm run build` in {source}/"))
            .into_response(),
    }
}

fn disk(root: &Path, requested: &str) -> Option<Response> {
    let path = if requested.is_empty() { "index.html" } else { requested };
    read_inside(root, path)
        .map(|data| asset(path, data))
        .or_else(|| read_inside(root, "index.html").map(|data| asset("index.html", data)))
}

fn asset(path: &str, data: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache)], data).into_response()
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
    if !dist.starts_with(&root) || !dist.is_dir() || read_inside(&dist, "index.html").is_none() {
        return None;
    }
    Some(dist)
}

fn manifest(root: &Path, short: &str) -> Option<Theme> {
    let data = read_inside(root, "theme.json")?;
    if data.len() > 64 * 1024 {
        return None;
    }
    let theme: Theme = serde_json::from_slice(&data).ok()?;
    (theme.short == short).then_some(theme)
}

pub fn themes(app: &App) -> std::io::Result<Vec<Theme>> {
    let mut list = vec![serde_json::from_str(include_str!("../web-theme/theme.json"))
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
}

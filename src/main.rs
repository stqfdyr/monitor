//! monitor-hub: collects from monitor agents and serves the panel.
//!
//! Zero configuration to start. Everything beyond the listen address and the
//! database path is set in the panel and stored in SQLite, so there is no
//! config file to lose track of and no secrets sitting in a plaintext TOML.

mod agent_ws;
mod api;
mod auth;
mod db;
mod frontend;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{header, Extensions, HeaderMap, StatusCode, Version};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::{Local, Months, NaiveDate};
use tokio::signal::unix::{signal, SignalKind};
use tower_http::compression::Predicate;
use tracing::{info, warn};

use agent_ws::Agent;
use db::Db;

pub type Shared = Arc<App>;

pub struct App {
    pub db: Db,
    /// Every connected agent: its outbound channel, the session that opened it
    /// and its latest report. One map, because being connected and having
    /// current figures are one fact about a node, not two. See `agent_ws`.
    pub agents: RwLock<HashMap<i64, Agent>>,
    /// Last rendered node list per audience, `[public, admin]`, with the
    /// millisecond it was built. Shared by every browser stream so viewers do
    /// not multiply the query load. See `api::live_snapshot`.
    pub snapshot: Mutex<[(i64, axum::extract::ws::Utf8Bytes); 2]>,
    pub throttle: auth::Throttle,
    pub http: reqwest::Client,
    /// Public base URL when `--site` was given, empty otherwise -- the
    /// default, where the hub is reached at whatever ip:port the browser used
    /// and the panel falls back to its own origin. Behind a reverse proxy it
    /// has to be set: a loopback listener would put 127.0.0.1 in the install
    /// commands the panel builds.
    pub site: String,
    /// Parent directory containing one folder per installed public theme.
    pub themes: PathBuf,
}

impl App {
    fn new(db: Db, site: String, themes: PathBuf) -> Self {
        Self {
            db,
            agents: RwLock::default(),
            snapshot: Mutex::new([(0, Default::default()), (0, Default::default())]),
            throttle: auth::Throttle::default(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            site,
            themes,
        }
    }

    #[cfg(test)]
    pub fn for_test(db: Db) -> Self {
        Self::new(db, String::new(), PathBuf::from("themes"))
    }

    pub fn public_page(&self) -> bool {
        self.db.get("public_page").as_deref() != Some("off")
    }

    /// Whether a session cookie may be marked Secure. With `--site` that is
    /// its scheme; without one the hub does not know the address it was
    /// reached on, so the request has to say: a TLS-terminating proxy sets
    /// `X-Forwarded-Proto`, and a hub answering plain HTTP directly has no
    /// such header. Marking it Secure over plain HTTP would make the browser
    /// drop the session rather than keep it.
    ///
    /// The header is client-settable and nothing beyond this flag trusts it.
    /// A forged `https` costs the sender its own session; a forged `http`
    /// drops Secure from a cookie the sender already holds. Neither reaches
    /// another browser's session.
    pub fn secure_cookies(&self, headers: &HeaderMap) -> bool {
        if !self.site.is_empty() {
            return !self.site.starts_with("http://");
        }
        forwarded_proto(headers) == Some("https")
    }
}

/// The scheme the browser used, as reported by a reverse proxy. Chained
/// proxies append to the header, so the browser's own hop is the first value.
fn forwarded_proto(headers: &HeaderMap) -> Option<&str> {
    let chain = headers.get("x-forwarded-proto")?.to_str().ok()?;
    Some(chain.split(',').next()?.trim())
}

/// Where the agent binaries are published. Not a setting: anyone pointing this
/// elsewhere is forking the project and already rebuilding this line.
const AGENT_REPO: &str = "stqfdyr/agent";

/// The one-liner pasted onto a new VPS.
async fn install_script() -> Response {
    ([(header::CONTENT_TYPE, "text/x-shellscript")], include_str!("../install.sh")).into_response()
}

/// Where the hub fetches an agent release, with the panel's GitHub proxy in
/// front of it when there is one. The proxy belongs to the hub rather than to
/// each install command: a hub that cannot reach github.com cannot relay to
/// *any* node, so the answer is the same for all of them.
///
/// This URL is fetched on an anonymous request, so the operator setting it is
/// pointing that path somewhere new. It stays inside the bounds `agent_binary`
/// already holds: four at a time, a 120-second timeout, and a streamed body.
fn release_url(app: &App, arch: &str) -> String {
    let direct = format!(
        "https://github.com/{AGENT_REPO}/releases/latest/download/monitor-agent-{arch}-unknown-linux-musl"
    );
    match app.db.get("github_proxy").filter(|v| !v.trim().is_empty()) {
        Some(proxy) => format!("{}/{direct}", proxy.trim().trim_end_matches('/')),
        None => direct,
    }
}

/// How many release downloads the hub relays at once.
///
/// This route takes no credentials, and one request costs an outbound fetch of
/// GitHub plus 1.8 MB of egress -- the most expensive thing an anonymous caller
/// can ask this process to do. Streaming bounds the memory each one holds;
/// nothing bounded how many there could be, the same gap the password gate in
/// `auth` closes.
///
/// Four, because a node installs once: a handful of machines set up together,
/// not a workload. Refused rather than queued, for the same reason as there.
const RELAY_SLOTS: usize = 4;
static RELAY_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(RELAY_SLOTS);

/// Holds a relay permit until the last byte has gone out. The handler returns
/// once the response head is built, so a permit dropped there would gate the
/// fetch and leave the transfer -- the part that costs -- unbounded.
struct Metered<S> {
    inner: S,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl<S: futures_core::Stream + Unpin> futures_core::Stream for Metered<S> {
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Hands out the agent binary from the hub itself, so a node that can reach
/// the hub can install without reaching GitHub: IPv6-only machines never
/// resolve github.com, and neither do blocked networks.
async fn agent_binary(State(app): State<Shared>, Path(arch): Path<String>) -> Response {
    if !matches!(arch.as_str(), "x86_64" | "aarch64") {
        return (StatusCode::NOT_FOUND, "unknown architecture").into_response();
    }
    let Ok(permit) = RELAY_GATE.try_acquire() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many downloads in flight, try again").into_response();
    };
    let url = release_url(&app, &arch);
    // The default client timeout is tuned for API calls, not a 1.8 MB download.
    let fetched = app.http.get(&url).timeout(std::time::Duration::from_secs(120)).send().await;
    match fetched {
        // Streamed rather than collected: holding each release whole put a few
        // hundred parallel requests within reach of the unit file's memory
        // ceiling. Passing the bytes through costs one buffer per request.
        Ok(res) if res.status().is_success() => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            axum::body::Body::from_stream(Metered { inner: Box::pin(res.bytes_stream()), _permit: permit }),
        )
            .into_response(),
        Ok(res) => {
            (StatusCode::BAD_GATEWAY, format!("release download failed: {}", res.status())).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("release download failed: {e}")).into_response(),
    }
}

// ---- startup ----

struct Args {
    listen: SocketAddr,
    /// True when `--listen` was left out, which is the only case where a
    /// refused v6 wildcard may quietly fall back to v4: an operator who names
    /// an address means it.
    listen_defaulted: bool,
    database: String,
    site: String,
    themes: PathBuf,
}

/// The address to listen on when nobody said. A v6 wildcard also accepts IPv4
/// through v4-mapped addresses, so one socket serves both -- but only where
/// the kernel allows it: `bindv6only=1` would make it v6-only and drop every
/// IPv4 node, and a kernel booted with `ipv6.disable=1` has no
/// `/proc/sys/net/ipv6` at all and cannot bind the address in the first place.
///
/// Worth the proc read because getting it wrong is silent at both ends: a
/// v6-only node has no route to an IPv4 address, so it just never connects,
/// and nothing on either side says why.
fn default_listen() -> &'static str {
    match std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only") {
        Ok(flag) if flag.trim() == "0" => "[::]:28080",
        _ => "0.0.0.0:28080",
    }
}

fn parse_args() -> Result<Args> {
    let mut listen = None;
    let mut database = "monitor.db".to_owned();
    let mut site = String::new();
    let mut themes = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match arg.as_str() {
            "--listen" => listen = Some(value()),
            "--db" => database = value(),
            "--site" => site = value(),
            "--themes" => themes = Some(PathBuf::from(value())),
            "-h" | "--help" => {
                println!(
                    "monitor-hub {}\n\n\
                     Usage: monitor-hub [--listen [::]:28080] [--db monitor.db] [--themes themes] [--site https://hub.example.com]\n\n\
                     --listen defaults to [::]:28080, one socket serving IPv6 and IPv4\n\
                     both; where the kernel has no dual-stack sockets it is 0.0.0.0:28080.\n\
                     --themes defaults to a themes/ directory beside the database.\n\
                     --site is only needed behind a reverse proxy, where the address the\n\
                     panel is reached on is not the one agents should use. Left out, the\n\
                     hub answers on whatever ip:port it is asked, and the panel builds\n\
                     install commands from the address in the browser's bar.",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let listen_defaulted = listen.is_none();
    let listen: SocketAddr = listen.unwrap_or_else(|| default_listen().to_owned()).parse()?;
    let themes = themes.unwrap_or_else(|| {
        std::path::Path::new(&database).parent().unwrap_or_else(|| std::path::Path::new(".")).join("themes")
    });
    Ok(Args { listen, listen_defaulted, database, site: site.trim_end_matches('/').to_owned(), themes })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MONITOR_LOG")
                .unwrap_or_else(|_| "monitor_hub=info,tower_http=warn".into()),
        )
        .init();

    let args = parse_args()?;
    std::fs::create_dir_all(&args.themes)?;
    let app = Arc::new(App::new(Db::open(&args.database)?, args.site.clone(), args.themes));
    let url = advertised_url(&args.site, args.listen);
    first_run(&app, &url)?;
    if exposed_over_plain_http(&url) {
        warn!(
            "this hub answers plain HTTP at {url}; sessions and agent tokens travel in the clear. \
             Put it behind a TLS reverse proxy -- the panel builds install commands from the \
             browser's own address, so nothing here has to change -- then --listen 127.0.0.1:PORT \
             so this port is no longer reachable in the clear"
        );
    }

    tokio::spawn(housekeeping(app.clone()));

    let router = Router::new()
        // Agents.
        .route("/api/agent/ws", get(agent_ws::handler))
        .route("/install.sh", get(install_script))
        .route("/agent/{arch}", get(agent_binary))
        // Read paths; the public page reaches these unauthenticated.
        .route("/api/me", get(api::me))
        .route("/api/nodes", get(api::nodes))
        .route("/api/nodes/{id}/metrics", get(api::metrics))
        .route("/api/ws", get(api::live_ws))
        // Sign-in.
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/logout", post(auth::logout))
        .route("/api/auth/github", get(auth::github_start))
        .route("/api/auth/github/callback", get(auth::github_callback))
        // Panel.
        .route("/api/nodes", post(api::create_node))
        .route("/api/nodes/order", put(api::reorder_nodes))
        .route("/api/nodes/{id}", put(api::update_node).delete(api::delete_node))
        .route("/api/nodes/{id}/token", post(api::reset_token))
        .route("/api/nodes/{id}/traffic", put(api::patch_traffic))
        .route("/api/ping-tasks", get(api::ping_tasks).post(api::save_ping_task))
        .route("/api/ping-tasks/{id}", delete(api::delete_ping_task))
        .route("/api/settings", get(api::settings).put(api::save_settings))
        .route("/api/themes", get(api::themes))
        .fallback(frontend::serve)
        // A report is a few hundred bytes; anything larger is not one.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        // A day of one node's chart is 236 kB of JSON against 30 kB gzipped,
        // and the theme's bundle is much the same shape. Not on a 101: both
        // sockets upgrade through one, and a body encoder has no business
        // wrapping a connection about to stop being HTTP. The default predicate
        // skips images, SSE and anything under 32 bytes.
        .layer(tower_http::compression::CompressionLayer::new().compress_when(
            tower_http::compression::predicate::DefaultPredicate::new().and(
                |status: StatusCode, _: Version, _: &HeaderMap, _: &Extensions| {
                    status != StatusCode::SWITCHING_PROTOCOLS
                },
            ),
        ))
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(listener) => listener,
        // A box that refuses the dual-stack wildcard still has to come up, and
        // on such a box IPv4 is all there is to serve.
        Err(e) if args.listen_defaulted && args.listen.is_ipv6() => {
            let v4 = SocketAddr::from(([0, 0, 0, 0], args.listen.port()));
            warn!("could not bind {} ({e}); falling back to {v4}", args.listen);
            tokio::net::TcpListener::bind(v4).await?
        }
        Err(e) => return Err(e.into()),
    };
    info!("listening on {} ({url})", listener.local_addr()?);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Waits for whichever stop signal arrives first. SIGTERM is the one that
/// matters: it is how systemd stops a service, and without it a deploy kills
/// the hub outright rather than letting it finish the requests it holds.
async fn shutdown() {
    // SIGTERM is always registerable; a failure here is a broken runtime, and
    // falling back to Ctrl-C alone would reinstate the bug above.
    let mut term = signal(SignalKind::terminate()).expect("listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutting down");
}

/// The address to print at startup: `--site` when it was given, otherwise the
/// listen address with a wildcard resolved to a real one, because
/// `http://0.0.0.0:28080` is not something a browser can open.
fn advertised_url(site: &str, listen: SocketAddr) -> String {
    if !site.is_empty() {
        return site.to_owned();
    }
    let ip =
        if listen.ip().is_unspecified() { outbound_ip().unwrap_or_else(|| listen.ip()) } else { listen.ip() };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// This box's own address on the route off it. Asking the kernel to route a
/// datagram it never sends is the cheapest way to pick one interface out of
/// several, and it answers with no network at all. Behind NAT it gives the
/// private address: the hub cannot know its public one, which is why
/// install-hub.sh prints the address it looked up instead.
fn outbound_ip() -> Option<IpAddr> {
    [("0.0.0.0:0", "1.1.1.1:80"), ("[::]:0", "[2606:4700:4700::1111]:80")].into_iter().find_map(
        |(bind, route_to)| {
            let socket = std::net::UdpSocket::bind(bind).ok()?;
            socket.connect(route_to).ok()?;
            socket.local_addr().ok().map(|addr| addr.ip())
        },
    )
}

/// True when the hub's own address sends cookies and tokens in the clear.
/// Plain HTTP to loopback is local development; to anything else it means the
/// session cookie is readable by every hop in between.
///
/// A hub behind a TLS-terminating proxy or tunnel is not this case, by either
/// route: `--site` is then the https:// address even though the listener speaks
/// plain HTTP, and without one the listener is on loopback where nobody else
/// can reach it.
fn exposed_over_plain_http(site: &str) -> bool {
    let Some(rest) = site.strip_prefix("http://") else {
        return false;
    };
    !host_is_loopback(rest)
}

/// Loopback test over an `authority` like `example.com:8080` or `[::1]:8080`.
/// IPv6 literals are bracketed, so the port cannot be split off at the first
/// colon.
fn host_is_loopback(authority: &str) -> bool {
    let authority = authority.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.is_empty() || host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// Prints a one-time admin password when the database is first created.
/// Without it a fresh hub has no way in until GitHub is configured.
fn first_run(app: &App, url: &str) -> Result<()> {
    if app.db.get("admin_password_hash").is_some() {
        return Ok(());
    }
    let password = auth::random_token()[..24].to_owned();
    app.db.set("admin_password_hash", &auth::hash_password(&password)?)?;
    println!(
        "\n  Monitor hub is ready.\n\n  \
         Sign in at {url}/admin\n  \
         Emergency password: {password}\n\n  \
         This is shown once. Change it, and set up GitHub sign-in, under Settings.\n"
    );
    Ok(())
}

/// Billing cycles as whole months. `once` has none, so it never rolls over.
fn cycle_months(cycle: &str) -> Option<u32> {
    Some(match cycle {
        "monthly" => 1,
        "quarterly" => 3,
        "semiannual" => 6,
        "yearly" => 12,
        "biennial" => 24,
        "triennial" => 36,
        _ => return None,
    })
}

/// A node still reporting past its expiry date was renewed, so roll the date
/// forward by whole cycles until it is in the future.
fn renewed(expires: NaiveDate, cycle: &str, today: NaiveDate) -> Option<NaiveDate> {
    let months = Months::new(cycle_months(cycle)?);
    let mut next = expires;
    while next < today {
        next = next.checked_add_months(months)?;
    }
    (next != expires).then_some(next)
}

fn renew_online_nodes(app: &App) -> Result<()> {
    // The hub's timezone, as with the traffic boundaries: an expiry date is a
    // date a person wrote down, and on a CST hub `Utc` answers "yesterday"
    // until 08:00, while the panel beside it already says it has expired.
    let today = Local::now().date_naive();
    let online: Vec<i64> = app.agents.read().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    for node in app.db.nodes()? {
        if !online.contains(&node.id) {
            continue;
        }
        let Some(expires) = node.expires_at.as_deref().and_then(|d| d.parse::<NaiveDate>().ok()) else {
            continue;
        };
        let Some(next) = renewed(expires, &node.billing_cycle, today) else { continue };
        app.db.set_expiry(node.id, &next.to_string())?;
        info!("node {} is still up past {expires}, expiry rolled to {next}", node.name);
    }
    Ok(())
}

/// Expires sessions, trims history and rolls over expiry dates once an hour.
async fn housekeeping(app: Shared) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3_600));
    loop {
        ticker.tick().await;
        let keep =
            app.db.get("retention_days").and_then(|v| v.parse::<i64>().ok()).unwrap_or(30).clamp(1, 3_650);
        if let Err(e) = app.db.prune(keep) {
            warn!("pruning history failed: {e:#}");
        }
        if let Err(e) = app.db.expire_sessions() {
            warn!("expiring sessions failed: {e:#}");
        }
        if let Err(e) = renew_online_nodes(&app) {
            warn!("rolling expiry dates failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, Uri};

    fn app(site: &str) -> App {
        App::new(Db::open(":memory:").unwrap(), site.into(), PathBuf::from("themes"))
    }

    /// A request as a reverse proxy would forward it, or as it arrives with
    /// none in front.
    fn proto(forwarded: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(scheme) = forwarded {
            headers.insert("x-forwarded-proto", scheme.parse().unwrap());
        }
        headers
    }

    /// Whichever wildcard this kernel supports, it has to parse and carry the
    /// default port: a typo here would surface only as a refused bind at
    /// startup, on someone else's machine.
    #[test]
    fn the_default_listener_is_a_wildcard_on_the_default_port() {
        let addr: SocketAddr = default_listen().parse().expect("the default must parse");
        assert!(addr.ip().is_unspecified(), "{addr}");
        assert_eq!(addr.port(), 28_080);
    }

    #[test]
    fn an_expired_node_that_is_still_up_rolls_forward_whole_cycles() {
        let d = |s: &str| s.parse::<NaiveDate>().unwrap();
        // One day past a monthly expiry: the next month, clamped to its end.
        assert_eq!(renewed(d("2026-01-31"), "monthly", d("2026-02-01")), Some(d("2026-02-28")));
        // Years overdue: add cycles until the date is ahead of today.
        assert_eq!(renewed(d("2024-03-10"), "yearly", d("2026-08-28")), Some(d("2027-03-10")));
        // Not due yet, and one-off billing: left alone.
        assert_eq!(renewed(d("2026-09-01"), "monthly", d("2026-08-28")), None);
        assert_eq!(renewed(d("2020-01-01"), "once", d("2026-08-28")), None);
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), p.parse::<Uri>().unwrap());

        // The shape that hides a misconfigured OAuth callback.
        assert_eq!(spa("/api/oauth_callback?code=x").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api/nope").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api").await.status(), StatusCode::NOT_FOUND);

        // Client-side routes still fall through to the app.
        assert_eq!(spa("/admin").await.status(), StatusCode::OK);
        assert_eq!(spa("/").await.status(), StatusCode::OK);
        // A path merely starting with the letters "api" is not an API path.
        assert_eq!(spa("/apiary").await.status(), StatusCode::OK);
    }

    /// A build writes hashed filenames under `assets/`, so a miss there is a
    /// tab left open across a deploy. Answering with index.html hands a script
    /// tag HTML, which fails on MIME type long after the request that caused
    /// it. Both bundles go through the same fallback, so both have to refuse.
    #[tokio::test]
    async fn a_missing_hashed_asset_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), p.parse::<Uri>().unwrap());

        assert_eq!(spa("/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/admin/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);

        // A route that merely begins with those letters is still a route.
        assert_eq!(spa("/assetsomething").await.status(), StatusCode::OK);
        // And a deep client route still reloads into the app.
        assert_eq!(spa("/node/7").await.status(), StatusCode::OK);
    }

    /// What decides the Secure flag: `--site` when it is set, and the proxy
    /// in front when it is not -- the default ip:port deployment, where the
    /// hub does not know its own address.
    #[test]
    fn the_cookie_flag_follows_site_when_it_is_set_and_the_proxy_when_it_is_not() {
        // Local development: no Secure flag, or the browser refuses to keep
        // the cookie at all.
        for local in ["http://127.0.0.1:28080", "http://localhost:28080", "http://[::1]:28080"] {
            assert!(!app(local).secure_cookies(&proto(None)), "{local}");
            assert!(!exposed_over_plain_http(local), "{local} is not exposed");
        }
        // A given --site outranks anything the request claims, in both
        // directions: it is the operator's word against a client-settable
        // header.
        assert!(app("https://hub.example.com").secure_cookies(&proto(Some("http"))));
        assert!(!app("http://hub.example.com").secure_cookies(&proto(Some("https"))));
        assert!(!exposed_over_plain_http("https://m.example.com"));

        // No --site: the proxy's header is the only word on the scheme.
        let bare = app("");
        assert!(!bare.secure_cookies(&proto(None)), "plain HTTP, answered directly");
        assert!(bare.secure_cookies(&proto(Some("https"))));
        // Chained proxies append, so the browser's own hop is the first value.
        assert!(bare.secure_cookies(&proto(Some("https, http"))));
        assert!(!bare.secure_cookies(&proto(Some("http, https"))));
    }

    /// A hub in the clear has to say so, and a wildcard listener is not an
    /// address anyone can open -- both are about the URL the hub advertises,
    /// which is `--site` only when there is one.
    #[test]
    fn the_advertised_url_resolves_a_wildcard_listener_and_defers_to_site() {
        let listen = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            advertised_url("https://hub.example.com", listen("127.0.0.1:28080")),
            "https://hub.example.com"
        );
        assert_eq!(advertised_url("", listen("127.0.0.1:9911")), "http://127.0.0.1:9911");
        assert_eq!(advertised_url("", listen("[::1]:9911")), "http://[::1]:9911");
        // Genuinely in the clear: warn, and still no Secure, which is why the
        // warning is worth printing.
        for remote in ["http://203.0.113.10:28080", "http://hub.example.com"] {
            assert!(!app(remote).secure_cookies(&proto(None)), "{remote}");
            assert!(exposed_over_plain_http(remote), "{remote} is exposed");
        }

        let resolved = advertised_url("", listen("0.0.0.0:28080"));
        assert!(resolved.starts_with("http://") && resolved.ends_with(":28080"), "{resolved}");
        // A box with no route off it keeps the wildcard; there is nothing else
        // to print. Anywhere else it must not be what gets printed.
        if outbound_ip().is_some() {
            assert!(!resolved.contains("0.0.0.0"), "{resolved}");
            assert!(exposed_over_plain_http(&resolved), "{resolved} is exposed");
        }
    }

    #[test]
    fn the_public_page_is_on_unless_it_is_switched_off() {
        let app = app("http://x");
        assert!(app.public_page());
        app.db.set("public_page", "off").unwrap();
        assert!(!app.public_page());
        app.db.set("public_page", "on").unwrap();
        assert!(app.public_page());
    }

    /// A stream that is over before it starts, standing in for a release.
    struct Nothing;

    impl futures_core::Stream for Nothing {
        type Item = ();

        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<()>> {
            std::task::Poll::Ready(None)
        }
    }

    /// The gate is worth nothing if the permit is released when the handler
    /// returns: the head is built in microseconds and the 1.8 MB behind it is
    /// the cost. The permit has to live on the body.
    #[test]
    fn a_relay_permit_is_held_for_as_long_as_the_body_is() {
        let queued: Vec<_> =
            (1..RELAY_SLOTS).map(|_| RELAY_GATE.try_acquire().expect("up to the limit")).collect();
        let body = Metered { inner: Nothing, _permit: RELAY_GATE.try_acquire().expect("the last slot") };

        assert!(RELAY_GATE.try_acquire().is_err(), "the request past the limit must be refused");
        drop(body);
        assert!(RELAY_GATE.try_acquire().is_ok(), "a finished download gives its slot back");
        drop(queued);
    }

    /// The proxy is a hub setting rather than an install-command argument, so
    /// this is the one place that builds the URL. A trailing slash on the
    /// setting must not turn into a double slash the proxy will not match.
    #[test]
    fn a_github_proxy_prefixes_the_release_url_and_an_empty_one_does_not() {
        let app = app("");
        let direct = release_url(&app, "x86_64");
        assert!(direct.starts_with("https://github.com/stqfdyr/agent/releases/"), "{direct}");

        for set in ["https://ghfast.top", "https://ghfast.top/", "  https://ghfast.top/  "] {
            app.db.set("github_proxy", set).unwrap();
            assert_eq!(release_url(&app, "x86_64"), format!("https://ghfast.top/{direct}"), "{set:?}");
        }
        // Cleared in the panel, which stores an empty string rather than
        // dropping the row.
        app.db.set("github_proxy", "").unwrap();
        assert_eq!(release_url(&app, "x86_64"), direct);
    }

    #[test]
    fn first_run_sets_a_password_once_and_leaves_it_alone_after() {
        let app = app("http://x");
        first_run(&app, "http://x").unwrap();
        let hash = app.db.get("admin_password_hash").unwrap();
        assert!(hash.starts_with("$argon2"));
        first_run(&app, "http://x").unwrap();
        assert_eq!(app.db.get("admin_password_hash").unwrap(), hash, "must not rotate on restart");
    }
}

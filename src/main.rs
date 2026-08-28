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
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::{Months, NaiveDate, Utc};
use tokio::sync::mpsc;
use tracing::{info, warn};

use agent_ws::Live;
use db::Db;

pub type Shared = Arc<App>;

pub struct App {
    pub db: Db,
    /// Current state per node, refreshed on every agent report.
    pub live: RwLock<HashMap<i64, Live>>,
    /// Outbound channel per connected agent, used to push probe assignments,
    /// tagged with the session that opened it. See `agent_ws::serve`.
    pub agents: Mutex<HashMap<i64, (u64, mpsc::Sender<String>)>>,
    /// Last rendered node list per audience, `[public, admin]`, with the
    /// millisecond it was built. Shared by every browser stream so viewers do
    /// not multiply the query load. See `api::live_snapshot`.
    pub snapshot: Mutex<[(i64, axum::extract::ws::Utf8Bytes); 2]>,
    pub throttle: auth::Throttle,
    pub http: reqwest::Client,
    /// Public base URL, used for install commands and to decide cookie flags.
    pub site: String,
    /// Parent directory containing one folder per installed public theme.
    pub themes: PathBuf,
}

impl App {
    fn new(db: Db, site: String, themes: PathBuf) -> Self {
        Self {
            db,
            live: RwLock::default(),
            agents: Mutex::default(),
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
        Self::new(db, "http://localhost:8080".into(), PathBuf::from("themes"))
    }

    pub fn public_page(&self) -> bool {
        self.db.get("public_page").as_deref() != Some("off")
    }

    /// Secure cookies everywhere except a plain-HTTP local hub, where the
    /// browser would otherwise refuse to store the session at all.
    pub fn secure_cookies(&self) -> bool {
        !self.site.starts_with("http://")
    }

    pub fn install_command(&self, token: &str) -> String {
        format!("curl -fsSL {}/install.sh | sh -s -- --server {} --token {}", self.site, self.site, token)
    }
}

/// The one-liner pasted onto a new VPS.
async fn install_script(State(app): State<Shared>) -> Response {
    let script = include_str!("../install.sh").replace("@@REPO@@", &app.repo());
    ([(header::CONTENT_TYPE, "text/x-shellscript")], script).into_response()
}

/// Hands out the agent binary from the hub itself. A node that can reach the
/// hub can now install, even when it cannot reach GitHub at all: IPv6-only
/// machines never resolve github.com, and neither do blocked networks.
async fn agent_binary(State(app): State<Shared>, Path(arch): Path<String>) -> Response {
    if !matches!(arch.as_str(), "x86_64" | "aarch64") {
        return (StatusCode::NOT_FOUND, "unknown architecture").into_response();
    }
    let url = format!(
        "https://github.com/{}/releases/latest/download/monitor-agent-{arch}-unknown-linux-musl",
        app.repo()
    );
    // The default client timeout is tuned for API calls, not a 1.6 MB download.
    let fetched = app.http.get(&url).timeout(std::time::Duration::from_secs(120)).send().await;
    match fetched {
        Ok(res) if res.status().is_success() => match res.bytes().await {
            Ok(body) => ([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response(),
            Err(e) => (StatusCode::BAD_GATEWAY, format!("release download failed: {e}")).into_response(),
        },
        Ok(res) => {
            (StatusCode::BAD_GATEWAY, format!("release download failed: {}", res.status())).into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("release download failed: {e}")).into_response(),
    }
}

impl App {
    fn repo(&self) -> String {
        self.db.get("release_repo").unwrap_or_else(|| "stqfdyr/agent".into())
    }
}

// ---- startup ----

struct Args {
    listen: SocketAddr,
    database: String,
    site: String,
    themes: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut listen = "0.0.0.0:8080".to_owned();
    let mut database = "monitor.db".to_owned();
    let mut site = String::new();
    let mut themes = None;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match arg.as_str() {
            "--listen" => listen = value(),
            "--db" => database = value(),
            "--site" => site = value(),
            "--themes" => themes = Some(PathBuf::from(value())),
            "-h" | "--help" => {
                println!(
                    "monitor-hub {}\n\n\
                     Usage: monitor-hub [--listen 0.0.0.0:8080] [--db monitor.db] [--themes themes] [--site https://hub.example.com]\n\n\
                     --themes defaults to a themes/ directory beside the database.\n\
                     --site is the public URL agents and browsers reach this hub on. It is\n\
                     baked into install commands and decides whether session cookies are\n\
                     marked Secure, so set it once you are behind TLS.",
                    env!("CARGO_PKG_VERSION")
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    let listen: SocketAddr = listen.parse()?;
    if site.is_empty() {
        site = format!("http://{listen}");
    }
    let themes = themes.unwrap_or_else(|| {
        std::path::Path::new(&database).parent().unwrap_or_else(|| std::path::Path::new(".")).join("themes")
    });
    Ok(Args { listen, database, site: site.trim_end_matches('/').to_owned(), themes })
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
    first_run(&app)?;
    if exposed_over_plain_http(&args.site) {
        warn!("--site is plain HTTP on a remote host; sessions and agent tokens will travel in the clear");
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
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!("listening on {} (public URL {})", args.listen, args.site);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// True when `--site` would send cookies and tokens over the wire in the clear.
/// Plain HTTP to a loopback address is just local development; plain HTTP to
/// anything else means the session cookie is readable by every hop in between.
///
/// A hub behind a TLS-terminating proxy or a Cloudflare tunnel is *not* this
/// case: there `--site` is the https:// address, even though the listener
/// itself speaks plain HTTP on loopback.
fn exposed_over_plain_http(site: &str) -> bool {
    let Some(rest) = site.strip_prefix("http://") else {
        return false;
    };
    !host_is_loopback(rest)
}

/// Loopback test over an `authority` like `example.com:8080` or `[::1]:8080`.
/// IPv6 literals are bracketed, so the port cannot simply be split off at the
/// first colon.
fn host_is_loopback(authority: &str) -> bool {
    let authority = authority.split('/').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.is_empty() || host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// Prints a one-time admin password the first time the database is created.
/// Without it a fresh hub would have no way in until GitHub is configured.
fn first_run(app: &App) -> Result<()> {
    if app.db.get("admin_password_hash").is_some() {
        return Ok(());
    }
    let password = auth::random_token()[..24].to_owned();
    app.db.set("admin_password_hash", &auth::hash_password(&password)?)?;
    println!(
        "\n  Monitor hub is ready.\n\n  \
         Sign in at {}/admin\n  \
         Emergency password: {password}\n\n  \
         This is shown once. Change it, and set up GitHub sign-in, under Settings.\n",
        app.site
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

/// A node still reporting after its expiry date was plainly renewed, so roll
/// the date forward by whole cycles until it is in the future again.
fn renewed(expires: NaiveDate, cycle: &str, today: NaiveDate) -> Option<NaiveDate> {
    let months = Months::new(cycle_months(cycle)?);
    let mut next = expires;
    while next < today {
        next = next.checked_add_months(months)?;
    }
    (next != expires).then_some(next)
}

fn renew_online_nodes(app: &App) -> Result<()> {
    let today = Utc::now().date_naive();
    let online: Vec<i64> = app.live.read().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
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

    #[test]
    fn an_expired_node_that_is_still_up_rolls_forward_whole_cycles() {
        let d = |s: &str| s.parse::<NaiveDate>().unwrap();
        // One day past a monthly expiry: next natural month, clamped to its last day.
        assert_eq!(renewed(d("2026-01-31"), "monthly", d("2026-02-01")), Some(d("2026-02-28")));
        // Years overdue: keep adding cycles until the date is ahead of today.
        assert_eq!(renewed(d("2024-03-10"), "yearly", d("2026-08-28")), Some(d("2027-03-10")));
        // Not due yet, and one-off billing: left alone.
        assert_eq!(renewed(d("2026-09-01"), "monthly", d("2026-08-28")), None);
        assert_eq!(renewed(d("2020-01-01"), "once", d("2026-08-28")), None);
    }

    #[test]
    fn secure_cookies_track_the_public_scheme() {
        assert!(!app("http://127.0.0.1:8080").secure_cookies());
        assert!(app("https://hub.example.com").secure_cookies());
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), p.parse::<Uri>().unwrap());

        // The shape that hid a misconfigured OAuth callback for an entire
        // debugging session.
        assert_eq!(spa("/api/oauth_callback?code=x").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api/nope").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api").await.status(), StatusCode::NOT_FOUND);

        // Client-side routes still fall through to the app.
        assert_eq!(spa("/admin").await.status(), StatusCode::OK);
        assert_eq!(spa("/").await.status(), StatusCode::OK);
        // A path merely starting with the letters "api" is not an API path.
        assert_eq!(spa("/apiary").await.status(), StatusCode::OK);
    }

    #[test]
    fn plain_http_warning_fires_for_remote_hosts_only() {
        // Local development: no warning.
        assert!(!exposed_over_plain_http("http://127.0.0.1:8080"));
        assert!(!exposed_over_plain_http("http://localhost:8080"));
        assert!(!exposed_over_plain_http("http://[::1]:8080"));
        // Behind TLS, including a tunnel that forwards to a loopback listener.
        assert!(!exposed_over_plain_http("https://m.example.com"));
        // Genuinely in the clear over the network.
        assert!(exposed_over_plain_http("http://203.0.113.10:8080"));
        assert!(exposed_over_plain_http("http://hub.example.com"));
    }

    #[test]
    fn the_install_command_carries_the_public_url_and_token() {
        let app = app("https://hub.example.com");
        let command = app.install_command("tok123");
        assert!(command.contains("https://hub.example.com/install.sh"));
        assert!(command.contains("--server https://hub.example.com"));
        assert!(command.contains("--token tok123"));
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

    #[test]
    fn first_run_sets_a_password_once_and_leaves_it_alone_after() {
        let app = app("http://x");
        first_run(&app).unwrap();
        let hash = app.db.get("admin_password_hash").unwrap();
        assert!(hash.starts_with("$argon2"));
        first_run(&app).unwrap();
        assert_eq!(app.db.get("admin_password_hash").unwrap(), hash, "must not rotate on restart");
    }
}

//! monitor-hub: collects from monitor agents and serves the panel.
//!
//! Zero configuration to start. Everything beyond the listen address and the
//! database path is set in the panel and stored in SQLite, so there is no
//! config file to lose track of and no secrets sitting in a plaintext TOML.

mod agent_ws;
mod api;
mod auth;
mod db;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Result;
use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use tokio::sync::mpsc;
use tracing::{info, warn};

use agent_ws::Live;
use db::Db;

pub type Shared = Arc<App>;

pub struct App {
    pub db: Db,
    /// Current state per node, refreshed on every agent report.
    pub live: RwLock<HashMap<i64, Live>>,
    /// Outbound channel per connected agent, used to push probe assignments.
    pub agents: Mutex<HashMap<i64, mpsc::Sender<String>>>,
    pub throttle: auth::Throttle,
    pub http: reqwest::Client,
    /// Public base URL, used for install commands and to decide cookie flags.
    pub site: String,
}

impl App {
    fn new(db: Db, site: String) -> Self {
        Self {
            db,
            live: RwLock::default(),
            agents: Mutex::default(),
            throttle: auth::Throttle::default(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("http client"),
            site,
        }
    }

    #[cfg(test)]
    pub fn for_test(db: Db) -> Self {
        Self::new(db, "http://localhost:8080".into())
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
        format!(
            "curl -fsSL {}/install.sh | sh -s -- --server {} --token {}",
            self.site, self.site, token
        )
    }
}

// ---- embedded frontend ----

#[derive(rust_embed::Embed)]
#[folder = "../web/dist"]
struct Assets;

async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        // Hashed build assets are immutable; the entry HTML must not be cached.
        let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
        return (
            [(header::CONTENT_TYPE, mime.as_ref()), (header::CACHE_CONTROL, cache)],
            file.data.into_owned(),
        )
            .into_response();
    }
    // Unknown paths fall through to the SPA so client-side routes work on reload.
    match Assets::get("index.html") {
        Some(index) => Html(index.data.into_owned()).into_response(),
        None => (StatusCode::NOT_FOUND, "frontend not built; run `npm run build` in web/").into_response(),
    }
}

/// The one-liner pasted onto a new VPS.
async fn install_script(State(app): State<Shared>) -> Response {
    let script = include_str!("../../install.sh").replace("@@REPO@@", &app.repo());
    ([(header::CONTENT_TYPE, "text/x-shellscript")], script).into_response()
}

impl App {
    fn repo(&self) -> String {
        self.db.get("release_repo").unwrap_or_else(|| "stqfdyr/monitor".into())
    }
}

// ---- startup ----

struct Args {
    listen: SocketAddr,
    database: String,
    site: String,
}

fn parse_args() -> Result<Args> {
    let mut listen = "0.0.0.0:8080".to_owned();
    let mut database = "monitor.db".to_owned();
    let mut site = String::new();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_default();
        match arg.as_str() {
            "--listen" => listen = value(),
            "--db" => database = value(),
            "--site" => site = value(),
            "-h" | "--help" => {
                println!(
                    "monitor-hub {}\n\n\
                     Usage: monitor-hub [--listen 0.0.0.0:8080] [--db monitor.db] [--site https://hub.example.com]\n\n\
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
    Ok(Args { listen, database, site: site.trim_end_matches('/').to_owned() })
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
    let app = Arc::new(App::new(Db::open(&args.database)?, args.site.clone()));
    first_run(&app)?;
    if app.secure_cookies() && args.site.starts_with("http://") {
        warn!("--site is plain HTTP; sessions and agent tokens will travel in the clear");
    }

    tokio::spawn(housekeeping(app.clone()));

    let router = Router::new()
        // Agents.
        .route("/api/agent/ws", get(agent_ws::handler))
        .route("/install.sh", get(install_script))
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
        .route("/api/nodes/{id}", put(api::update_node).delete(api::delete_node))
        .route("/api/nodes/{id}/token", post(api::reset_token))
        .route("/api/nodes/{id}/traffic", put(api::patch_traffic))
        .route("/api/ping-tasks", get(api::ping_tasks).post(api::save_ping_task))
        .route("/api/ping-tasks/{id}", delete(api::delete_ping_task))
        .route("/api/settings", get(api::settings).put(api::save_settings))
        .fallback(serve_asset)
        // A report is a few hundred bytes; anything larger is not one.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    info!("listening on {} (public URL {})", args.listen, args.site);
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
        .await?;
    Ok(())
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

/// Expires sessions and trims history once an hour.
async fn housekeeping(app: Shared) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3_600));
    loop {
        ticker.tick().await;
        let keep = app
            .db
            .get("retention_days")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(30)
            .clamp(1, 3_650);
        if let Err(e) = app.db.prune(keep) {
            warn!("pruning history failed: {e:#}");
        }
        if let Err(e) = app.db.expire_sessions() {
            warn!("expiring sessions failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secure_cookies_track_the_public_scheme() {
        let db = || Db::open(":memory:").unwrap();
        assert!(!App::new(db(), "http://127.0.0.1:8080".into()).secure_cookies());
        assert!(App::new(db(), "https://hub.example.com".into()).secure_cookies());
    }

    #[test]
    fn the_install_command_carries_the_public_url_and_token() {
        let app = App::new(Db::open(":memory:").unwrap(), "https://hub.example.com".into());
        let command = app.install_command("tok123");
        assert!(command.contains("https://hub.example.com/install.sh"));
        assert!(command.contains("--server https://hub.example.com"));
        assert!(command.contains("--token tok123"));
    }

    #[test]
    fn the_public_page_is_on_unless_it_is_switched_off() {
        let app = App::new(Db::open(":memory:").unwrap(), "http://x".into());
        assert!(app.public_page());
        app.db.set("public_page", "off").unwrap();
        assert!(!app.public_page());
        app.db.set("public_page", "on").unwrap();
        assert!(app.public_page());
    }

    #[test]
    fn first_run_sets_a_password_once_and_leaves_it_alone_after() {
        let app = App::new(Db::open(":memory:").unwrap(), "http://x".into());
        first_run(&app).unwrap();
        let hash = app.db.get("admin_password_hash").unwrap();
        assert!(hash.starts_with("$argon2"));
        first_run(&app).unwrap();
        assert_eq!(app.db.get("admin_password_hash").unwrap(), hash, "must not rotate on restart");
    }
}

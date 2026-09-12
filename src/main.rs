//! monitor-hub: collects from monitor agents and serves the panel.
//!
//! No configuration is required to start. Everything beyond the listen address
//! and the database path is configured in the panel and stored in SQLite,
//! leaving no config file to track and no secrets in plaintext TOML.

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
    /// Every connected agent: its outbound channel, the session that opened it,
    /// and its latest report. A single map, since connectivity and current
    /// figures are one fact about a node rather than two. See `agent_ws`.
    pub agents: RwLock<HashMap<i64, Agent>>,
    /// Last rendered node list per audience, `[public, admin]`, with the
    /// millisecond it was built. Shared by every browser stream so viewers do
    /// not multiply the query load. See `api::live_snapshot`.
    pub snapshot: Mutex<[(i64, axum::extract::ws::Utf8Bytes); 2]>,
    pub throttle: auth::Throttle,
    /// Failed agent registrations, counted separately from failed sign-ins: the
    /// two have different threat models, and a batch install run with a stale
    /// key must not lock the operator out of the panel.
    pub registrations: auth::Throttle,
    pub http: reqwest::Client,
    /// Public base URL when `--site` was given, empty otherwise. In the default
    /// case the hub is reached at whatever ip:port the browser used and the
    /// panel falls back to its own origin. Behind a reverse proxy it must be
    /// set, or a loopback listener would place 127.0.0.1 in the install commands
    /// the panel builds.
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
            registrations: auth::Throttle::default(),
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

    /// Whether a session cookie may be marked Secure. With `--site` this follows
    /// its scheme; without one the hub does not know the address it was reached
    /// on and must rely on the request: a TLS-terminating proxy sets
    /// `X-Forwarded-Proto`, while a hub answering plain HTTP directly has no such
    /// header. Marking the cookie Secure over plain HTTP would cause the browser
    /// to discard the session.
    ///
    /// The header is supplied by the trusted reverse proxy. Provisioning also
    /// checks it along with the request's Host/Origin; the listener must remain
    /// publicly unreachable so callers cannot bypass that proxy.
    pub fn secure_cookies(&self, headers: &HeaderMap) -> bool {
        if !self.site.is_empty() {
            return !self.site.starts_with("http://");
        }
        forwarded_proto(headers) == Some("https")
    }
}

/// The scheme the browser used, as reported by a reverse proxy. Chained proxies
/// append to the header, so the browser's own hop is the first value.
fn forwarded_proto(headers: &HeaderMap) -> Option<&str> {
    let chain = headers.get("x-forwarded-proto")?.to_str().ok()?;
    Some(chain.split(',').next()?.trim())
}

/// Where the agent binaries are published. Not a setting: redirecting it
/// implies a fork, which rebuilds this line anyway.
const AGENT_REPO: &str = "monitor-probe/agent";

/// The one-line installer pasted onto a new VPS.
async fn install_script() -> Response {
    ([(header::CONTENT_TYPE, "text/x-shellscript")], include_str!("../install.sh")).into_response()
}

/// Where the hub fetches an agent release, behind the panel's GitHub proxy when
/// one is configured. The proxy belongs to the hub rather than to each install
/// command: a hub that cannot reach github.com cannot relay to any node, so the
/// answer is the same for all of them.
///
/// This URL is fetched on an anonymous request, so setting it redirects that
/// path. It remains within the bounds `agent_binary` already enforces: four
/// concurrent transfers, a 120-second timeout, and a streamed body.
fn release_url(app: &App, arch: &str) -> String {
    proxied(
        app,
        format!(
            "https://github.com/{AGENT_REPO}/releases/latest/download/monitor-agent-{arch}-unknown-linux-musl"
        ),
    )
}

/// Places the panel's GitHub proxy in front of a github.com URL when one is
/// set. Shared by the agent relay and the theme updater: a hub that cannot reach
/// github.com for one cannot reach it for the other.
pub fn proxied(app: &App, url: String) -> String {
    match app.db.get("github_proxy").filter(|v| !v.trim().is_empty()) {
        Some(proxy) => format!("{}/{url}", proxy.trim().trim_end_matches('/')),
        None => url,
    }
}

/// How many release downloads the hub relays concurrently.
///
/// This route takes no credentials, and one request costs an outbound fetch from
/// GitHub plus 1.8 MB of egress -- the most expensive operation an anonymous
/// caller can request. Streaming bounds the memory each transfer holds; this
/// bounds how many may run, closing the same gap as the password gate in `auth`.
///
/// Four, because a node installs once: a handful of machines set up together is
/// the expected load, not a sustained workload. Refused rather than queued, for
/// the same reason.
const RELAY_SLOTS: usize = 4;
static RELAY_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(RELAY_SLOTS);

/// Longest a relay may hold its permit.
///
/// Deliberately generous, since a node on a slow link must still transfer
/// 1.8 MB; what it rules out is a transfer that never completes.
const RELAY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);

/// Holds a relay permit until the last byte has been sent. The handler returns
/// once the response head is built, so a permit dropped there would gate only
/// the fetch and leave the transfer -- the expensive part -- unbounded.
///
/// The permit is not held here, because "until the last byte" has no upper bound
/// of its own: a client that stops reading leaves hyper unable to flush, hyper
/// then stops polling this stream, and a deadline checked in `poll_next` would
/// never run -- nor would the upstream timeout on the reqwest body, which is
/// equally poll-driven. Four connections that accept the response and never read
/// it would hold all four slots for as long as they remained open, and
/// `/agent/{arch}` is the path every node installs through. The permit therefore
/// belongs to a task with its own timer, and this end of the channel -- dropped
/// with the body, whether it completed or the connection died -- releases it
/// early.
struct Metered<S> {
    inner: S,
    _done: tokio::sync::oneshot::Sender<()>,
}

/// Wraps `inner` and parks `permit` on a task that releases it when the body is
/// dropped or [`RELAY_DEADLINE`] elapses, whichever comes first.
fn metered<S>(inner: S, permit: tokio::sync::SemaphorePermit<'static>) -> Metered<S> {
    let (_done, body_gone) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Either arm ends the task, which drops the permit. `body_gone` resolves
        // as an error the moment the sender is dropped, which is the signal.
        let _permit = permit;
        let _ = tokio::time::timeout(RELAY_DEADLINE, body_gone).await;
    });
    Metered { inner, _done }
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

/// Serves the agent binary from the hub itself, so a node that can reach the hub
/// can install without reaching GitHub: IPv6-only machines cannot resolve
/// github.com, and neither can blocked networks.
///
/// ponytail: these bytes are relayed unverified, and `install.sh` executes them
/// as root on every node. Fetched directly from github.com that is TLS's
/// concern; through the panel's `github_proxy` it rests on the mirror alone.
/// Currently held by the setting accepting https:// only, and by stating so
/// where it is entered. The upgrade path is a pinned digest -- `agent.pin`
/// beside `web-theme.pin`, a fixed release tag, hashed after the fetch and
/// before the relay (4 permits x 1.73 MiB against MemoryMax=256M, so buffering
/// is free). Not a fetched checksum: whoever can replace the binary can replace
/// that too. Deliberately deferred, as it couples agent releases to hub
/// releases.
async fn agent_binary(State(app): State<Shared>, Path(arch): Path<String>) -> Response {
    if !matches!(arch.as_str(), "x86_64" | "aarch64") {
        return (StatusCode::NOT_FOUND, "unknown architecture").into_response();
    }
    let Ok(permit) = RELAY_GATE.try_acquire() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many downloads in flight, try again").into_response();
    };
    let url = release_url(&app, &arch);
    // The default client timeout is sized for API calls, not a 1.8 MB download.
    let fetched = app.http.get(&url).timeout(std::time::Duration::from_secs(120)).send().await;
    match fetched {
        // Streamed rather than collected: holding each release in full would put
        // a few hundred parallel requests within reach of the unit file's memory
        // ceiling. Passing the bytes through costs one buffer per request.
        Ok(res) if res.status().is_success() => (
            [(header::CONTENT_TYPE, "application/octet-stream")],
            axum::body::Body::from_stream(metered(Box::pin(res.bytes_stream()), permit)),
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
    /// True when `--listen` was omitted, the only case where a refused v6
    /// wildcard may fall back to v4 silently: an explicitly named address is
    /// taken literally.
    listen_defaulted: bool,
    database: String,
    site: String,
    themes: PathBuf,
}

/// The default listen address. A v6 wildcard also accepts IPv4 through
/// v4-mapped addresses, so one socket serves both -- but only where the kernel
/// permits it: `bindv6only=1` makes it v6-only and drops every IPv4 node, and a
/// kernel booted with `ipv6.disable=1` has no `/proc/sys/net/ipv6` and cannot
/// bind the address at all.
///
/// The proc read is justified because the failure is silent at both ends: a
/// v6-only node has no route to an IPv4 address, so it simply never connects.
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
    // The warning above derives from --site, the address the operator
    // advertises. This one derives from the socket actually open, and the two
    // diverge in the deployment that needs it most: `--site https://...` with
    // --listen left at its wildcard default prints nothing while the port answers
    // plain HTTP to anyone who finds it. The provisioning gate in `api` and the
    // X-Forwarded-Proto cookie flag both assume the proxy cannot be bypassed.
    else if !args.listen.ip().is_loopback() {
        warn!(
            "listening on {} in the clear. If a TLS proxy fronts this hub, callers can still reach \
             this port directly and set their own X-Forwarded-Proto -- --listen 127.0.0.1:{} so the \
             proxy is the only way in",
            args.listen,
            args.listen.port()
        );
    }
    // Checked once here, because the answer is static: `provisioning_allowed`
    // measures every request against --site, so a value that is not an https
    // domain permanently refuses adding and installing nodes however the panel is
    // reached. That refusal names the browser's address and the reverse proxy,
    // both of which are correct here, while the debug line naming --site is off
    // at the default log level. A warning rather than a fatal error: the hub
    // still serves everything else, and an operator upgrading into this check
    // should not lose a running hub. `install-hub.sh` refuses the same values
    // where they are entered.
    if !args.site.is_empty() && api::https_domain(&args.site).is_none() {
        warn!(
            "--site {} is not an https domain entry, so adding and installing nodes will be refused \
             however the panel is reached: it has to be https://, a domain rather than an address, \
             and nothing after the host",
            args.site
        );
    }

    tokio::spawn(housekeeping(app.clone()));

    let router = Router::new()
        // Agents.
        .route("/api/agent/ws", get(agent_ws::handler))
        .route("/api/agent/register", post(api::agent_register))
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
        .route("/api/register-window", post(api::open_register).delete(api::close_register))
        .route("/api/nodes/order", put(api::reorder_nodes))
        .route("/api/nodes/{id}", put(api::update_node).delete(api::delete_node))
        .route("/api/nodes/{id}/token", post(api::reset_token))
        .route("/api/nodes/{id}/traffic", put(api::patch_traffic))
        .route("/api/ping-tasks", get(api::ping_tasks).post(api::save_ping_task))
        .route("/api/ping-tasks/{id}", delete(api::delete_ping_task))
        .route("/api/sessions", get(api::sessions))
        .route("/api/sessions/{id}", delete(api::delete_session))
        .route("/api/settings", get(api::settings).put(api::save_settings))
        .route("/api/themes", get(api::themes))
        .route("/api/themes/{short}", delete(api::delete_theme))
        .route("/api/themes/{short}/preview", get(api::theme_preview))
        .route("/api/themes/{short}/update", post(api::update_theme))
        .route("/api/db", get(api::db_stats))
        .route("/api/db/backup", get(api::db_backup))
        .route("/api/db/vacuum", post(api::db_vacuum))
        .fallback(frontend::serve)
        // A report is a few hundred bytes; anything larger is not a report.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        // The two chunked uploads, merged after that layer rather than beneath
        // it. They raise the ceiling on a single request -- one 4 MiB piece --
        // not on the file behind it: a 256 MiB backup arrives as 64 such
        // requests, so no reverse proxy needs to know the database size. The
        // whole-file ceilings live on `total` and are checked before the first
        // byte is sent.
        .merge(
            Router::new()
                .route("/api/db/restore", post(api::db_restore))
                .route("/api/themes", post(api::upload_theme))
                .layer(tower_http::limit::RequestBodyLimitLayer::new(api::MAX_CHUNK))
                .with_state(app.clone()),
        )
        // Excludes the agent binary and database backups: both are already
        // compressed and both are megabytes, so deflating them would consume the
        // cores argon2 and the SQLite writer share for no gain.
        .layer(
            tower_http::compression::CompressionLayer::new().compress_when(
                tower_http::compression::predicate::DefaultPredicate::new()
                    .and(tower_http::compression::predicate::NotForContentType::const_new(
                        "application/octet-stream",
                    ))
                    .and(|status: StatusCode, _: Version, _: &HeaderMap, _: &Extensions| {
                        status != StatusCode::SWITCHING_PROTOCOLS
                    }),
            ),
        )
        .with_state(app);

    let listener = match tokio::net::TcpListener::bind(args.listen).await {
        Ok(listener) => listener,
        // A host that refuses the dual-stack wildcard must still start, and on
        // such a host IPv4 is all there is to serve.
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

/// Waits for whichever stop signal arrives first. SIGTERM is the significant
/// one: it is how systemd stops a service, and without handling it a deploy
/// terminates the hub outright rather than letting it finish in-flight
/// requests.
async fn shutdown() {
    // SIGTERM can always be registered; a failure here indicates a broken
    // runtime, and falling back to Ctrl-C alone would reinstate the problem
    // described above.
    let mut term = signal(SignalKind::terminate()).expect("listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutting down");
}

/// The address printed at startup: `--site` when given, otherwise the listen
/// address with any wildcard resolved to a concrete one, since
/// `http://0.0.0.0:28080` cannot be opened in a browser.
fn advertised_url(site: &str, listen: SocketAddr) -> String {
    if !site.is_empty() {
        return site.to_owned();
    }
    let ip =
        if listen.ip().is_unspecified() { outbound_ip().unwrap_or_else(|| listen.ip()) } else { listen.ip() };
    format!("http://{}", SocketAddr::new(ip, listen.port()))
}

/// This host's own address on its outbound route. Asking the kernel to route a
/// datagram it never sends is the cheapest way to select one interface among
/// several, and it answers without any network traffic. Behind NAT it yields the
/// private address, since the hub cannot know its public one, which is why
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

/// True when the hub's own address transmits cookies and tokens in the clear.
/// Plain HTTP to loopback is local development; to anything else it means the
/// session cookie is readable by every intermediate hop.
///
/// A hub behind a TLS-terminating proxy or tunnel is excluded by either route:
/// `--site` is then the https:// address even though the listener speaks plain
/// HTTP, and without one the listener is on loopback, unreachable by others.
fn exposed_over_plain_http(site: &str) -> bool {
    let Some(rest) = site.strip_prefix("http://") else {
        return false;
    };
    !host_is_loopback(rest)
}

/// Loopback test over an `authority` such as `example.com:8080` or `[::1]:8080`.
/// IPv6 literals are bracketed, so the port is not split off at the first
/// colon.
fn host_is_loopback(authority: &str) -> bool {
    let authority = authority.split('/').next().unwrap_or("");
    // RFC 3986 places userinfo before the host, so `127.0.0.1:28080@example.com`
    // reads as loopback to any check splitting at the first colon while the
    // browser resolves the name that follows -- and this decides whether the
    // plaintext warning is printed at all. `provisioning_allowed` parses --site
    // with reqwest::Url and strips it there; both must agree.
    let authority = authority.rsplit('@').next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    // Parsed rather than prefix-matched: `127.example.com` is a registered name
    // resolving wherever its owner points it, and reading it as loopback would
    // suppress the only warning that the cookie travels in the clear.
    host.is_empty() || host == "localhost" || host.parse::<IpAddr>().is_ok_and(|a| a.is_loopback())
}

/// Prints a one-time admin password when the database is first created, since a
/// fresh hub is otherwise inaccessible until GitHub is configured.
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
         This is shown once. Change it, and set up GitHub sign-in, under Security.\n"
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

/// A node still reporting past its expiry date has been renewed, so the date is
/// rolled forward by whole cycles until it lies in the future.
fn renewed(expires: NaiveDate, cycle: &str, today: NaiveDate) -> Option<NaiveDate> {
    let months = Months::new(cycle_months(cycle)?);
    let mut next = expires;
    while next < today {
        next = next.checked_add_months(months)?;
    }
    (next != expires).then_some(next)
}

fn renew_online_nodes(app: &App) -> Result<()> {
    // The hub's local timezone, as with the traffic boundaries: an expiry date
    // is one a person entered, and on a UTC+8 hub `Utc` reports the previous day
    // until 08:00 while the panel already shows it expired.
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
        let keep = app.db.retention_days();
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

    /// A request as a reverse proxy would forward it, or as it arrives with none
    /// in front.
    fn proto(forwarded: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Some(scheme) = forwarded {
            headers.insert("x-forwarded-proto", scheme.parse().unwrap());
        }
        headers
    }

    /// Whichever wildcard this kernel supports must parse and carry the default
    /// port; a typo here would surface only as a refused bind at startup.
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
        // Years overdue: cycles are added until the date is in the future.
        assert_eq!(renewed(d("2024-03-10"), "yearly", d("2026-08-28")), Some(d("2027-03-10")));
        // Not yet due, and one-off billing: both left unchanged.
        assert_eq!(renewed(d("2026-09-01"), "monthly", d("2026-08-28")), None);
        assert_eq!(renewed(d("2020-01-01"), "once", d("2026-08-28")), None);
    }

    #[tokio::test]
    async fn an_unknown_api_path_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), HeaderMap::new(), p.parse::<Uri>().unwrap());

        // The case that would conceal a misconfigured OAuth callback.
        assert_eq!(spa("/api/oauth_callback?code=x").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api/nope").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/api").await.status(), StatusCode::NOT_FOUND);

        // Client-side routes still fall through to the app.
        assert_eq!(spa("/admin").await.status(), StatusCode::OK);
        assert_eq!(spa("/").await.status(), StatusCode::OK);
        // A path merely beginning with "api" is not an API path.
        assert_eq!(spa("/apiary").await.status(), StatusCode::OK);
    }

    /// A build writes hashed filenames under `assets/`, so a miss there means a
    /// tab left open across a deploy. Answering with index.html would hand a
    /// script tag HTML, failing on MIME type long after the request that caused
    /// it. Both bundles share the same fallback, so both must refuse.
    #[tokio::test]
    async fn a_missing_hashed_asset_is_a_404_not_the_single_page_app() {
        let app = Arc::new(app("http://localhost:8080"));
        let spa = |p: &str| frontend::serve(State(app.clone()), HeaderMap::new(), p.parse::<Uri>().unwrap());

        assert_eq!(spa("/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(spa("/admin/assets/index-STALE.js").await.status(), StatusCode::NOT_FOUND);

        // A route merely beginning with those letters is still a route.
        assert_eq!(spa("/assetsomething").await.status(), StatusCode::OK);
        // A deep client route still reloads into the app.
        assert_eq!(spa("/node/7").await.status(), StatusCode::OK);
    }

    /// What determines the Secure flag: `--site` when set, otherwise the proxy in
    /// front -- the default ip:port deployment, where the hub does not know its
    /// own address.
    #[test]
    fn the_cookie_flag_follows_site_when_it_is_set_and_the_proxy_when_it_is_not() {
        // Local development: no Secure flag, or the browser discards the cookie
        // entirely.
        for local in ["http://127.0.0.1:28080", "http://localhost:28080", "http://[::1]:28080"] {
            assert!(!app(local).secure_cookies(&proto(None)), "{local}");
            assert!(!exposed_over_plain_http(local), "{local} is not exposed");
        }
        // A configured --site takes precedence over the request in both
        // directions: operator configuration outranks a client-settable header.
        assert!(app("https://hub.example.com").secure_cookies(&proto(Some("http"))));
        assert!(!app("http://hub.example.com").secure_cookies(&proto(Some("https"))));
        assert!(!exposed_over_plain_http("https://m.example.com"));
        // A registered name is not an address however it begins: reading one as
        // loopback would suppress the plaintext-cookie warning.
        assert!(exposed_over_plain_http("http://127.example.com"));
        assert!(exposed_over_plain_http("http://127.0.0.1.nip.io"));
        // Nor is userinfo an address: the host follows the '@', and reading the
        // part before it as loopback suppresses the same warning.
        assert!(exposed_over_plain_http("http://127.0.0.1:28080@hub.example.com"));
        assert!(!app("http://127.0.0.1:28080@hub.example.com").secure_cookies(&proto(None)));

        // Without --site the proxy's header is the only indication of scheme.
        let bare = app("");
        assert!(!bare.secure_cookies(&proto(None)), "plain HTTP, answered directly");
        assert!(bare.secure_cookies(&proto(Some("https"))));
        // Chained proxies append, so the browser's own hop is the first value.
        assert!(bare.secure_cookies(&proto(Some("https, http"))));
        assert!(!bare.secure_cookies(&proto(Some("http, https"))));
    }

    /// A hub serving in the clear must report it, and a wildcard listener is not
    /// an address anyone can open. Both concern the URL the hub advertises, which
    /// is `--site` only when one is set.
    #[test]
    fn the_advertised_url_resolves_a_wildcard_listener_and_defers_to_site() {
        let listen = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert_eq!(
            advertised_url("https://hub.example.com", listen("127.0.0.1:28080")),
            "https://hub.example.com"
        );
        assert_eq!(advertised_url("", listen("127.0.0.1:9911")), "http://127.0.0.1:9911");
        assert_eq!(advertised_url("", listen("[::1]:9911")), "http://[::1]:9911");
        // Genuinely in the clear: warn, and still no Secure flag, which is what
        // makes the warning worth printing.
        for remote in ["http://203.0.113.10:28080", "http://hub.example.com"] {
            assert!(!app(remote).secure_cookies(&proto(None)), "{remote}");
            assert!(exposed_over_plain_http(remote), "{remote} is exposed");
        }

        let resolved = advertised_url("", listen("0.0.0.0:28080"));
        assert!(resolved.starts_with("http://") && resolved.ends_with(":28080"), "{resolved}");
        // A host with no outbound route keeps the wildcard, there being nothing
        // else to print; anywhere else the wildcard must not appear.
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

    /// A stream that ends immediately, standing in for a release.
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

    /// The gate is worthless if the permit is released when the handler returns:
    /// the head is built in microseconds while the 1.8 MB behind it is the cost.
    /// The permit therefore outlives the handler, and -- since "until the last
    /// byte" is the client's decision -- no longer than RELAY_DEADLINE.
    #[tokio::test(start_paused = true)]
    async fn a_relay_permit_follows_the_body_but_not_past_the_deadline() {
        let queued: Vec<_> =
            (1..RELAY_SLOTS).map(|_| RELAY_GATE.try_acquire().expect("up to the limit")).collect();
        let body = metered(Nothing, RELAY_GATE.try_acquire().expect("the last slot"));
        tokio::task::yield_now().await;
        assert!(RELAY_GATE.try_acquire().is_err(), "the request past the limit must be refused");

        // A body that ends, or a connection that dies, returns the slot
        // immediately rather than waiting out the deadline.
        drop(body);
        tokio::task::yield_now().await;
        let finished = RELAY_GATE.try_acquire().expect("a finished download gives its slot back");
        drop(finished);

        // A client that accepts the response and then reads nothing never polls
        // the body, so the body cannot time itself out. Only an independent timer
        // can, which is why the permit does not travel with it.
        let stalled = metered(Nothing, RELAY_GATE.try_acquire().expect("the last slot"));
        tokio::task::yield_now().await;
        assert!(RELAY_GATE.try_acquire().is_err());
        tokio::time::advance(RELAY_DEADLINE + std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(RELAY_GATE.try_acquire().is_ok(), "a transfer that never finishes still gives its slot back");
        drop((stalled, queued));
    }

    /// The proxy is a hub setting rather than an install-command argument, so
    /// this is the only place the URL is built. A trailing slash in the setting
    /// must not become a double slash the proxy will not match.
    #[test]
    fn a_github_proxy_prefixes_the_release_url_and_an_empty_one_does_not() {
        let app = app("");
        let direct = release_url(&app, "x86_64");
        assert!(direct.starts_with("https://github.com/monitor-probe/agent/releases/"), "{direct}");

        for set in ["https://ghfast.top", "https://ghfast.top/", "  https://ghfast.top/  "] {
            app.db.set("github_proxy", set).unwrap();
            assert_eq!(release_url(&app, "x86_64"), format!("https://ghfast.top/{direct}"), "{set:?}");
        }
        // Cleared in the panel, which stores an empty string rather than removing
        // the row.
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

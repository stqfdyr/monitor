//! The panel and public-status HTTP surface.

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Local, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_ws::Agent;
use crate::auth::{
    authed, client_ip, current_session, hash_password, issue_session, issued_at, random_token, with_cookies,
};
use crate::db::{Node, PingTask, Traffic};
use crate::{agent_ws, App, Shared};

/// Present only on requests carrying a valid session. Handlers that take it
/// cannot be reached unauthenticated, so the check cannot be forgotten.
pub struct Admin;

impl FromRequestParts<Shared> for Admin {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, Self::Rejection> {
        if authed(app, &parts.headers) {
            Ok(Admin)
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

fn fail(e: impl std::fmt::Display) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
}

fn bad(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, message.to_owned()).into_response()
}

// ---- read paths, shared between the panel and the public page ----

/// One node as the UI consumes it: stored config, live metrics and the hub's
/// accumulated traffic in a single object.
fn node_view(node: &Node, current: Option<&Agent>, traffic: &Traffic, full: bool) -> Value {
    let mut view = json!({
        "id": node.id,
        "name": node.name,
        // A country, not an address: it says which market a node sits in,
        // which is what a status page is for, and it locates nothing. The
        // address it was derived from stays behind the panel with the rest.
        "country": node.country,
        "sort": node.sort,
        "public": node.public,
        "online": current.is_some(),
        // The live entry while connected, the stored one after. Zero means
        // connected but not yet reporting, which is not a time, so it falls
        // back to the stored one and "offline since" survives the gap.
        "last_seen": current.map(|a| a.last_seen).filter(|t| *t > 0).unwrap_or(node.last_seen),
        "metrics": current.map(|a| a.metrics.clone()).unwrap_or(Value::Null),
        "os": node.os,
        "kernel": node.kernel,
        "arch": node.arch,
        "virt": node.virt,
        "cpu_name": node.cpu_name,
        "cpu_cores": node.cpu_cores,
        "mem_total": node.mem_total,
        "swap_total": node.swap_total,
        "disk_total": node.disk_total,
        "agent_version": node.agent_version,
        "price": node.price,
        "currency": node.currency,
        "billing_cycle": node.billing_cycle,
        "expires_at": node.expires_at,
        "traffic_limit": node.traffic_limit,
        "traffic_mode": node.traffic_mode,
        "traffic_reset_day": node.traffic_reset_day,
        "total_rx": traffic.total_rx,
        "total_tx": traffic.total_tx,
        "month_rx": traffic.month_rx,
        "month_tx": traffic.month_tx,
        "month_start": traffic.month_start,
        // Same nature as the month and lifetime figures next to it, which the
        // public page already shows, so this one is public too.
        "day_rx": traffic.day_rx,
        "day_tx": traffic.day_tx,
    });
    // Raw kernel counters are a wire-protocol detail, and they expose a
    // machine's whole lifetime traffic to anyone loading the public page.
    if !full {
        if let Some(m) = view["metrics"].as_object_mut() {
            m.remove("boot_id");
            m.remove("net_rx_total");
            m.remove("net_tx_total");
        }
    }
    // Address, private notes and the token never leave the panel. The token is
    // here so the install command can be shown without reissuing it.
    if full {
        view["hostname"] = json!(node.hostname);
        view["ip"] = json!(node.ip);
        view["ipv4"] = json!(node.ipv4);
        view["ipv6"] = json!(node.ipv6);
        view["remark"] = json!(node.remark);
        view["token"] = json!(node.token);
    }
    view
}

fn visible_nodes(app: &App, full: bool) -> Result<Vec<Value>, anyhow::Error> {
    // One traffic query and one lock for the whole list: this is what every
    // visitor to the public page loads.
    let nodes = app.db.nodes()?;
    let traffic = app.db.all_traffic();
    let agents = app.agents.read().unwrap_or_else(|e| e.into_inner());
    let none = Traffic::default();
    Ok(nodes
        .iter()
        .filter(|n| full || n.public)
        .map(|n| node_view(n, agents.get(&n.id), traffic.get(&n.id).unwrap_or(&none), full))
        .collect())
}

pub async fn nodes(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    // The same rendered frame the browser streams get, and for the same
    // reason: otherwise every visitor rebuilds every node's row against the
    // connection the agents write through.
    ([(axum::http::header::CONTENT_TYPE, "application/json")], live_snapshot(&app, full).as_str().to_owned())
        .into_response()
}

#[derive(Deserialize)]
pub struct Window {
    #[serde(default = "default_hours")]
    hours: i64,
    /// How many points the caller can draw. Absent means the full budget.
    points: Option<i64>,
    /// Which half the caller will draw, `metrics` or `ping`. Each tab draws
    /// one, and the other was a third to two thirds of every response. Absent
    /// means both.
    series: Option<String>,
}

fn default_hours() -> i64 {
    6
}

pub async fn metrics(
    State(app): State<Shared>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(w): Query<Window>,
) -> Response {
    let full = authed(&app, &headers);
    if !readable(&app, full, id) {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    let hours = w.hours.clamp(1, if full { ADMIN_HOURS } else { PUBLIC_HOURS });
    let since = Utc::now().timestamp() - hours * 3_600;
    let step = sample_step(hours, w.points);
    let wants = |name: &str| w.series.as_deref().is_none_or(|s| s == name);
    // Probe names ride along with the samples they label, so the page needs no
    // second request. Names only: targets and assignments stay behind `Admin`.
    // Skipped when the probes were not asked for -- the resources tab has
    // nothing to label, and this is a turn at the write connection.
    let probes =
        if wants("ping") { app.db.ping_task_names().unwrap_or_else(|_| json!({})) } else { json!({}) };
    let metrics = if wants("metrics") { app.db.metrics(id, since, step) } else { Ok(vec![]) };
    let ping = if wants("ping") { app.db.ping_records(id, since, step) } else { Ok(vec![]) };
    match (metrics, ping) {
        (Ok(m), Ok(p)) => Json(json!({"metrics": m, "ping": p, "probes": probes})).into_response(),
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

/// Widest history window each audience may ask for.
///
/// The thinning below bounds the response, not the scan behind it: `hours=2160`
/// answers with 320 rows after reading every probe result the node has kept.
/// Measured at a month of retention that is 224 ms holding the single write
/// connection the agents report through, and it grows with `retention_days`.
///
/// The public ceiling is a week because that is the widest chart the themes
/// draw, so it costs nothing anyone was asking for. The panel keeps the quarter
/// year: it is one signed-in operator, not an anonymous caller.
const PUBLIC_HOURS: i64 = 24 * 7;
const ADMIN_HOURS: i64 = 24 * 90;

/// Seconds between the samples a window is drawn from.
///
/// Thinning is for what the screen cannot draw, not a house style: where the
/// samples fit, every one is sent. A chart of a hundred points reads as a
/// hundred samples taken, and on a probe that is a claim about the network.
/// Whole minutes, because that is the grid the metric rows sit on.
///
/// `points` is what the caller says it can draw. It only lowers the budget:
/// `SAMPLES` is the hub's ceiling, not the caller's, and it sits at a day of
/// minutes so the widest charted probe window comes back whole.
// ponytail: the budget is per series, so a response is SAMPLES × (1 + probes) --
// bounded by how many probes the admin created, not by the caller. Four probes
// at a day is ~90 kB gzipped; if that list ever grows long, scale SAMPLES by
// the probe count.
fn sample_step(hours: i64, points: Option<i64>) -> i64 {
    const SAMPLES: i64 = 1_440;
    let budget = points.unwrap_or(SAMPLES).clamp(60, SAMPLES);
    // Rounded up, or the budget is not one: a window that does not divide
    // evenly keeps the finer step and overruns it. `i64::div_ceil` is still
    // unstable, and both sides are positive here.
    60 * ((hours * 60 + budget - 1) / budget).max(1)
}

/// Guards a per-node read: the panel sees everything, the public page only
/// sees nodes that were explicitly published. `full` is the caller's own
/// `authed`, passed in because the handler needs it for the window ceiling too.
fn readable(app: &App, full: bool, id: i64) -> bool {
    full || (app.public_page() && app.db.node(id).ok().flatten().is_some_and(|n| n.public))
}

/// Per-connection read buffer for both WebSocket surfaces. The 128 KiB default
/// is tens of megabytes at a few hundred agents, for frames a few hundred bytes
/// long.
pub const SOCKET_BUFFER: usize = 4 * 1024;

/// Largest frame either socket accepts, matching the 64 KiB cap on the HTTP
/// body. The body limit is a tower layer and never applied here, where the
/// default ceiling is 64 MiB -- which a node's own token would buy, for content
/// that is stored and then handed to every viewer of the public page.
pub const MAX_FRAME: usize = 64 * 1024;

/// How long one rendered snapshot is reused. Just under the push interval, so
/// every tick still rebuilds once and no viewer is served a stale frame twice.
const SNAPSHOT_TTL_MS: i64 = 1_900;

/// The payload every browser stream sends, built at most once per tick however
/// many tabs are watching: the public page is anonymous, so a per-connection
/// build makes viewer count a multiplier on database work. Two slots, because
/// the admin view carries fields the public one must never see.
fn live_snapshot(app: &App, full: bool) -> Utf8Bytes {
    let now = Utc::now().timestamp_millis();
    let slot = usize::from(full);
    let mut cache = app.snapshot.lock().unwrap_or_else(|e| e.into_inner());
    // A cached frame's age has to be non-negative. A wall clock can step back
    // -- NTP correcting a fresh boot -- and against a bare upper bound the
    // negative that produces reads as young, pinning the panel to a stale
    // frame until real time catches up.
    if (0..SNAPSHOT_TTL_MS).contains(&now.saturating_sub(cache[slot].0)) {
        return cache[slot].1.clone();
    }
    let nodes = visible_nodes(app, full).unwrap_or_default();
    // `admin` rides along so the panel's first fetch and its stream share one
    // cached frame.
    let payload = Utf8Bytes::from(json!({"nodes": nodes, "admin": full}).to_string());
    cache[slot] = (now, payload.clone());
    payload
}

/// Drops the cached frames so the next push rebuilds. Without it a node the
/// panel just added blinks back out of the list until the frame ages out.
fn invalidate_snapshot(app: &App) {
    for slot in app.snapshot.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
        slot.0 = 0;
    }
}

/// Live stream for the browser. Each connection runs its own timer -- cheaper
/// to reason about than a fan-out channel -- over a shared snapshot, so a timer
/// costs nothing but a send.
pub async fn live_ws(State(app): State<Shared>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    upgrade
        .read_buffer_size(SOCKET_BUFFER)
        .max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| stream_live(app, socket, full))
}

async fn stream_live(app: Shared, mut socket: WebSocket, full: bool) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticker.tick().await;
        if socket.send(Message::Text(live_snapshot(&app, full))).await.is_err() {
            break;
        }
    }
}

// ---- panel write paths ----

pub async fn me(State(app): State<Shared>, headers: HeaderMap) -> Json<Value> {
    Json(json!({
        "authed": authed(&app, &headers),
        "github": app.db.get("github_client_id").is_some_and(|v| !v.is_empty()),
        "site_name": app.db.get("site_name").unwrap_or_else(|| "Monitor".into()),
        "public_page": app.public_page(),
        // The hub's own public URL when it was given one, which is what
        // belongs in an install command and in the OAuth callback — not
        // whichever address this browser used, which behind a proxy may be a
        // loopback port. Empty by default, and then the browser's address is
        // the only one anyone knows: the panel falls back to its own origin.
        "site": app.site,
    }))
}

pub async fn create_node(
    _: Admin,
    State(app): State<Shared>,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    let Ok(Json(mut node)) = body else { return bad("invalid node") };
    if node.name.trim().is_empty() {
        return bad("name is required");
    }
    node.name = node.name.trim().to_owned();
    let token = random_token();
    match app.db.create_node(&node, &token) {
        // Usable straight away: the install command is readable from the node
        // list, so adding and deploying need no reissue between them.
        Ok(id) => {
            invalidate_snapshot(&app);
            Json(json!({"id": id})).into_response()
        }
        Err(e) => fail(e),
    }
}

// ---- automatic registration ----

/// How long a registration window stays open.
///
/// Setting a batch of machines up takes minutes, and forgetting to close the
/// door afterwards is what people actually do. The window expires on its own
/// rather than waiting for someone to come back and switch it off.
const REGISTER_WINDOW: i64 = 3600;

/// How many nodes one window may register.
///
/// Without it, whoever holds the key for the hour can fill the node table.
/// A hundred is far past a plausible batch and far short of a problem.
const REGISTER_LIMIT: i64 = 100;

/// Trades a registration key for a node token, so a batch of machines can be
/// installed with one command instead of one panel visit each.
///
/// No session stands behind this one: the caller is `install.sh` on a machine
/// that has never talked to the hub. What stands in for a session is a key the
/// panel issues, good only inside [`REGISTER_WINDOW`].
///
/// One request costs two setting reads, a `COUNT`, and an `INSERT`. It sends
/// nothing outbound, and the router's 64 KiB body limit bounds the name.
pub async fn agent_register(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    // Plain text in, plain text out. The caller is a shell script, and so are
    // this route's neighbours -- `/install.sh` and `/agent/{arch}` answer with
    // a script and a binary. A bare token is one `$(curl ...)` away, with no
    // JSON parser to depend on in a POSIX `sh`.
    name: String,
) -> Response {
    let ip = client_ip(&headers, peer.ip());
    if app.throttle.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later").into_response();
    }
    // One answer for both "no window is open" and "that key is wrong": the
    // difference is only useful to someone who has neither.
    let closed = || (StatusCode::FORBIDDEN, "registration is closed").into_response();
    let until = app.db.get("register_until").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let Some(key) = app.db.get("register_key").filter(|k| !k.is_empty() && Utc::now().timestamp() < until)
    else {
        return closed();
    };
    if agent_ws::bearer(&headers) != Some(key.as_str()) {
        // Only a wrong key counts against the address. With the window shut
        // there is no secret to guess, and counting then would let anyone lock
        // an address they name in `X-Forwarded-For` out of the sign-in page.
        app.throttle.record_failure(ip);
        return closed();
    }
    match app.db.nodes_created_since(until - REGISTER_WINDOW) {
        Ok(n) if n >= REGISTER_LIMIT => {
            return (StatusCode::FORBIDDEN, "this window has registered enough nodes").into_response()
        }
        Err(e) => return fail(e),
        Ok(_) => {}
    }

    // The name comes off a machine nobody has vouched for yet: control
    // characters would break the panel's rows, and a length has to stop
    // somewhere. `chars()` rather than bytes, so the cut lands between them.
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let name = if name.is_empty() { "unnamed".to_owned() } else { name };
    // Field defaults live in `Node`'s serde attributes and nowhere else.
    // `Node::default()` is a different set of values -- private, reset day 0 --
    // and a node registered here has to land exactly where a panel-added one does.
    let node = match serde_json::from_value::<Node>(json!({ "name": name })) {
        Ok(node) => node,
        Err(e) => return fail(e),
    };
    let token = random_token();
    match app.db.create_node(&node, &token) {
        Ok(_) => {
            invalidate_snapshot(&app);
            token.into_response()
        }
        Err(e) => fail(e),
    }
}

/// Opens a registration window with a fresh key. Whatever key was there stops
/// working the moment this returns.
pub async fn open_register(_: Admin, State(app): State<Shared>) -> Response {
    let key = random_token();
    let until = (Utc::now().timestamp() + REGISTER_WINDOW).to_string();
    match app.db.set("register_key", &key).and_then(|()| app.db.set("register_until", &until)) {
        Ok(()) => Json(json!({"register_key": key, "register_until": until})).into_response(),
        Err(e) => fail(e),
    }
}

/// Closes it early -- the operator saying they are done before the hour is up.
pub async fn close_register(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.set("register_key", "").and_then(|()| app.db.set("register_until", "0")) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn update_node(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    let Ok(Json(mut node)) = body else { return bad("invalid node") };
    if node.name.trim().is_empty() {
        return bad("name is required");
    }
    node.name = node.name.trim().to_owned();
    match app.db.update_node(id, &node) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct NodeOrder {
    ids: Vec<i64>,
}

/// The list must name every node exactly once, checked inside the transaction
/// that renumbers rather than here: re-reading the node list first only makes
/// the check race the write it guards.
pub async fn reorder_nodes(_: Admin, State(app): State<Shared>, Json(order): Json<NodeOrder>) -> Response {
    match app.db.reorder_nodes(&order.ids) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        // Every way this fails is a list the caller got wrong.
        Err(e) => bad(&e.to_string()),
    }
}

pub async fn delete_node(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_node(id) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

/// Issues a fresh token, invalidating the old one at once.
///
/// Always an explicit act: rotate a token you think has leaked, then reinstall
/// the agent. Reading the install command does not go through here.
pub async fn reset_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    let token = random_token();
    let updated = app.db.node(id).map(|n| n.is_some()).unwrap_or(false);
    if !updated {
        return (StatusCode::NOT_FOUND, "no such node").into_response();
    }
    // The token is checked only at the handshake, so a session opened with the
    // old one would keep reporting. Dropping the sender ends that loop; it
    // reconnects and is refused. Its own teardown leaves the entry alone,
    // because the session tag no longer matches.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    // The token is part of the admin frame, which would otherwise go on
    // showing an install command for the credential just retired.
    invalidate_snapshot(&app);
    match app.db.reset_token(id, &token) {
        // Just the token: the panel builds the command, and one place knowing
        // its shape is enough.
        Ok(()) => Json(json!({"token": token})).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct TrafficPatch {
    total_rx: i64,
    total_tx: i64,
    month_rx: i64,
    month_tx: i64,
}

pub async fn patch_traffic(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    Json(p): Json<TrafficPatch>,
) -> Response {
    match app.db.set_traffic(id, p.total_rx, p.total_tx, p.month_rx, p.month_tx) {
        Ok(()) => {
            invalidate_snapshot(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

pub async fn ping_tasks(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.ping_tasks() {
        Ok(tasks) => Json(json!({"tasks": tasks})).into_response(),
        Err(e) => fail(e),
    }
}

pub async fn save_ping_task(_: Admin, State(app): State<Shared>, Json(mut task): Json<PingTask>) -> Response {
    if task.name.trim().is_empty() || task.target.trim().is_empty() {
        return bad("name and target are required");
    }
    // tcp ping needs an explicit port; a bare host would silently never connect.
    if !task.target.contains(':') {
        return bad("target must be host:port, for example 1.1.1.1:443");
    }
    task.interval = task.interval.clamp(5, 3_600);
    match app.db.save_ping_task(&task) {
        Ok(id) => {
            agent_ws::push_ping_tasks(&app);
            Json(json!({"id": id})).into_response()
        }
        Err(e) => fail(e),
    }
}

pub async fn delete_ping_task(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_ping_task(id) {
        Ok(()) => {
            agent_ws::push_ping_tasks(&app);
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => fail(e),
    }
}

/// Settings the panel may read. Secrets are deliberately absent: the client
/// can set the GitHub secret but never read it back.
const READABLE_SETTINGS: &[&str] = &[
    "site_name",
    "public_page",
    "github_client_id",
    "github_allowed_users",
    "retention_days",
    "theme",
    "github_proxy",
];

// ---- the database itself ----

/// The largest single request the two upload routes accept, and the reason
/// they sit outside the router's 64 KiB body limit. It is twice the 4 MiB the
/// panel sends, so the chunk size stays the panel's business alone and needs
/// no handshake to agree on.
///
/// **This, not the two ceilings below, is what a reverse proxy has to pass.**
/// A backup of any size arrives 4 MiB at a time, so `client_max_body_size` no
/// longer tracks the size of the database.
pub const MAX_CHUNK: usize = 8 * 1024 * 1024;

/// Whole-file ceilings, one per route, checked against the declared `total` on
/// the first request rather than by counting bytes as they arrive -- an
/// oversized upload is refused before a byte of it is sent.
///
/// The backup ceiling is where it is because restoring holds the connection
/// the agents write through: at the measured ~40 MB/s that is about 6.5
/// seconds of blocked reporting, and the reachable database sizes for a few
/// hundred nodes sit two orders of magnitude below it.
pub const MAX_RESTORE: u64 = 256 * 1024 * 1024;
pub const MAX_THEME: u64 = 32 * 1024 * 1024;

/// One request of an upload: `total` is the whole file, `offset` where this
/// piece belongs in it.
///
/// There is no upload id, no session and no server-side bookkeeping: the state
/// of an upload *is* the length of the file on disk. A piece continues one only
/// if it starts exactly where the last left off, `offset = 0` truncates
/// whatever an interrupted attempt left behind, and so nothing is ever left to
/// collect.
#[derive(Deserialize)]
pub struct Chunk {
    offset: u64,
    total: u64,
}

/// Appends one piece to `path`, answering with the file's length afterwards;
/// the caller compares that against `total` to know whether it is done.
///
/// A piece lands whole or not at all -- a failure truncates back to where it
/// started -- so retrying one always lines up on the same offset.
///
/// ponytail: strictly sequential, one round trip per chunk. Concurrent pieces
/// would need pwrite, a commit step and a hash to prove there are no holes,
/// and would buy about a second on a 6.7 MB backup.
async fn receive(path: &str, chunk: &Chunk, max: u64, body: axum::body::Body) -> Result<u64, anyhow::Error> {
    if chunk.total == 0 || chunk.total > max {
        anyhow::bail!("文件必须在 1 字节到 {} MiB 之间", max / 1024 / 1024);
    }
    if chunk.offset > chunk.total {
        anyhow::bail!("分片位置越过了文件末尾");
    }

    let mut options = std::fs::OpenOptions::new();
    // Only the first piece may create the file, and it truncates: whatever an
    // interrupted upload left behind is overwritten rather than collected.
    if chunk.offset == 0 {
        options.write(true).create(true).truncate(true);
    } else {
        options.append(true);
    }
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("这次上传已经不在了，请从头开始")
        }
        Err(e) => return Err(e.into()),
    };

    let already = file.metadata()?.len();
    if already != chunk.offset {
        anyhow::bail!("分片接不上：已经收到 {already} 字节，这一片却从 {} 开始", chunk.offset);
    }

    match append(&mut file, chunk, body).await {
        Ok(received) => Ok(chunk.offset + received),
        Err(e) => {
            // Undo a half-written piece so retrying it lines up again.
            let _ = file.set_len(chunk.offset);
            Err(e)
        }
    }
}

/// Streams one request body onto the end of `file`. The byte count is checked
/// here as well as by the route's body limit: these are the only paths on the
/// hub that write a caller's bytes to disk, so they do not lean on a layer
/// someone could reorder away.
async fn append(
    file: &mut std::fs::File,
    chunk: &Chunk,
    body: axum::body::Body,
) -> Result<u64, anyhow::Error> {
    use std::io::Write;
    use std::pin::Pin;

    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(piece) =
        std::future::poll_fn(|cx| futures_core::Stream::poll_next(Pin::new(&mut stream), cx)).await
    {
        let piece = piece?;
        received += piece.len() as u64;
        if chunk.offset + received > chunk.total {
            anyhow::bail!("这一片超出了声明的文件大小");
        }
        file.write_all(&piece)?;
    }
    Ok(received)
}

/// A scratch file beside the database, so the copy lands on the same
/// filesystem the database itself has room on. The random half keeps two
/// concurrent calls apart -- `VACUUM INTO` refuses a file that already exists.
fn scratch_path(app: &App, kind: &str) -> String {
    format!("{}.{kind}-{}.tmp", app.db.file(), &random_token()[..16])
}

pub async fn db_stats(_: Admin, State(app): State<Shared>) -> Response {
    match app.db.stats() {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => fail(e),
    }
}

/// Hands back a compact copy of the whole database.
///
/// The copy is written next to the live file and then unlinked while it is
/// still open, so it exists only as long as this response does -- a client
/// that disconnects half way through leaves nothing behind, and nothing on
/// disk outlives the download.
pub async fn db_backup(_: Admin, State(app): State<Shared>) -> Response {
    let path = scratch_path(&app, "backup");
    // Off the runtime: this one reads the entire database while holding the
    // connection the agents write through.
    let copied = {
        let (app, path) = (app.clone(), path.clone());
        tokio::task::spawn_blocking(move || app.db.backup_into(&path)).await
    };
    if let Err(e) = copied.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        let _ = std::fs::remove_file(&path);
        return fail(e);
    }
    let opened = tokio::fs::File::open(&path).await;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    match opened {
        Ok(file) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_LENGTH, size.to_string()),
                // The whole credential store: no shared cache keeps a copy.
                (header::CACHE_CONTROL, "no-store".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"monitor-{}.db\"", Local::now().format("%Y%m%d-%H%M%S")),
                ),
            ],
            axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response(),
        Err(e) => fail(e),
    }
}

/// Replaces the live database with an uploaded backup, one chunk per request.
///
/// The upload streams to a file beside the database and is checked as a whole
/// before a single page of it is copied: see `Db::check_backup`. Afterwards
/// every session in the restored file is dropped and the caller is handed a
/// new one -- a backup carries the session rows it had when it was taken, and
/// restoring one is no reason to bring logged-out sessions back to life.
pub async fn db_restore(
    _: Admin,
    State(app): State<Shared>,
    Query(chunk): Query<Chunk>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    // One fixed path, which is what lets the file's own length be the entire
    // protocol. Two admins uploading at once collide on the offset check
    // instead of interleaving into one file.
    // ponytail: one upload in flight per hub. A second slot would need ids and
    // a way to expire them, for a button one person presses once a year.
    let path = format!("{}.upload", app.db.file());
    let received = match receive(&path, &chunk, MAX_RESTORE, body).await {
        Ok(received) => received,
        Err(e) => return bad(&format!("{e:#}")),
    };
    if received < chunk.total {
        return Json(json!({"received": received})).into_response();
    }

    let outcome = restore(&app, &path).await;
    // SQLite writes a -wal and a -shm beside any file it opens in WAL mode,
    // and a plain copy of a running hub's database is exactly that. They go
    // when the connection closes cleanly; these three lines are what covers
    // the time it does not.
    for leftover in [path.clone(), format!("{path}-wal"), format!("{path}-shm")] {
        let _ = std::fs::remove_file(leftover);
    }
    match outcome {
        Ok(()) => {
            // Agents authenticate at the handshake, and the tokens they hold
            // may belong to different nodes now -- or to none. Dropping the
            // senders ends those loops; each reconnects against the database
            // that is actually here.
            app.agents.write().unwrap_or_else(|e| e.into_inner()).clear();
            invalidate_snapshot(&app);
            let cookie = match app.db.drop_all_sessions().and_then(|()| issue_session(&app, &headers)) {
                Ok(cookie) => cookie,
                Err(e) => return fail(e),
            };
            with_cookies(Json(json!({"ok": true})), [cookie])
        }
        Err(e) => bad(&format!("{e:#}")),
    }
}

async fn restore(app: &Shared, path: &str) -> Result<(), anyhow::Error> {
    // Both halves read the whole file, off the runtime: `PRAGMA
    // integrity_check` on a 256 MiB upload is not runtime work either, and the
    // copy that follows holds the connection the agents write through.
    let (app, source) = (app.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        app.db.check_backup(&source)?;
        app.db.restore_from(&source)
    })
    .await?
}

/// Drops history past the retention window and rebuilds the file around what
/// is left, which is the only way SQLite gives the space back to the
/// filesystem.
pub async fn db_vacuum(_: Admin, State(app): State<Shared>) -> Response {
    let keep = app.db.retention_days();
    let app = app.clone();
    // A rebuild of the whole file, holding the connection the agents write
    // through: it belongs on a blocking thread.
    let done = tokio::task::spawn_blocking(move || {
        let pruned = app.db.prune(keep)?;
        app.db.vacuum().map(|freed| json!({"pruned": pruned, "freed": freed}))
    })
    .await;
    match done.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(result) => Json(result).into_response(),
        Err(e) => fail(e),
    }
}

/// Installs an uploaded theme archive, one chunk per request.
///
/// The archive lands in the themes directory under a name `valid_short`
/// rejects, so a partial upload is invisible to both the theme list and the
/// public page. Installing it is `frontend::install`, which unpacks to a
/// staging directory and publishes with a rename -- the switch to a new theme
/// is atomic, and there is no moment where the page is served out of a
/// half-written directory.
pub async fn upload_theme(
    _: Admin,
    State(app): State<Shared>,
    Query(chunk): Query<Chunk>,
    body: axum::body::Body,
) -> Response {
    let path = app.themes.join(".upload.tar.gz");
    let name = path.to_string_lossy().into_owned();
    let received = match receive(&name, &chunk, MAX_THEME, body).await {
        Ok(received) => received,
        Err(e) => return bad(&format!("{e:#}")),
    };
    if received < chunk.total {
        return Json(json!({"received": received})).into_response();
    }

    // Off the runtime: gunzip plus a few thousand small writes.
    let installed = {
        let (app, path) = (app.clone(), path.clone());
        tokio::task::spawn_blocking(move || {
            crate::frontend::install(&app.themes, std::fs::File::open(&path)?, None)
        })
        .await
    };
    let _ = std::fs::remove_file(&path);
    match installed.map_err(|e| anyhow::anyhow!(e)).and_then(|r| r) {
        Ok(theme) => Json(json!({"theme": theme})).into_response(),
        Err(e) => bad(&format!("{e:#}")),
    }
}

/// The asset a theme repository publishes, and the only name the hub fetches:
/// the same `theme.tar.gz` the upload button takes.
const ARCHIVE: &str = "theme.tar.gz";

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
}

/// The `<owner>/<repo>` a theme's `url` names, when it names a GitHub
/// repository at all.
///
/// A whitelist, not a filter. Every address the update path fetches is built
/// out of these two strings, so nothing a manifest says can send the hub at a
/// host it did not choose -- which is why there is no private-address check to
/// get wrong here. The one host that is not github.com is the GitHub proxy in
/// the panel's own settings, which the operator set and the agent relay
/// already fetches through.
fn github_repo(url: &str) -> Option<(&str, &str)> {
    let (owner, rest) = url.strip_prefix("https://github.com/")?.split_once('/')?;
    // A link to a branch or a file is still a link to the repository.
    let repo = rest.split('/').next()?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (path_segment(owner) && path_segment(repo)).then_some((owner, repo))
}

/// One URL path segment the hub is willing to build a github.com address out
/// of: nothing that opens a new segment, and nothing that climbs out of this
/// one.
fn path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// Reinstalls one theme from the latest GitHub release of the repository its
/// own manifest points at.
///
/// The manifest supplies `<owner>/<repo>` and nothing else: the release is read
/// from api.github.com and the archive from github.com, both at addresses the
/// hub builds itself, so there is never a URL out of the theme to follow. The
/// installed version is compared against the release tag first, which is what
/// most presses do -- and only that -- so this is also the "check for updates"
/// button.
pub async fn update_theme(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match update(&app, &short).await {
        Ok((updated, version)) => Json(json!({"updated": updated, "version": version})).into_response(),
        Err(e) => bad(&format!("{e:#}")),
    }
}

async fn update(app: &App, short: &str) -> Result<(bool, String), anyhow::Error> {
    use anyhow::{bail, Context};

    let installed = crate::frontend::themes(app)?
        .into_iter()
        .find(|theme| theme.short == short)
        .context("没有这个主题")?;
    if short == "default" {
        bail!("内置主题跟着 hub 一起升级，不单独更新");
    }
    let (owner, repo) = github_repo(&installed.url)
        .context("这个主题的 url 不是 https://github.com/<owner>/<repo>，只能手动上传新包")?;

    // Unauthenticated: 60 requests an hour from this address, for a button
    // nobody holds down. GitHub answers 403 without a User-Agent.
    let release: Release = app
        .http
        .get(format!("https://api.github.com/repos/{owner}/{repo}/releases/latest"))
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("读不到 {owner}/{repo} 的最新 release"))?
        .json()
        .await?;

    // Tags read `v1.2.3`, manifests carry `1.2.3`. Equal means up to date;
    // anything else is installed, a deliberate downgrade included -- the author
    // moved the release, and this button follows the author.
    let tag = &release.tag_name;
    if tag.strip_prefix('v').unwrap_or(tag) == installed.version {
        return Ok((false, installed.version));
    }
    if !path_segment(tag) {
        bail!("release 的 tag {tag:?} 不能出现在下载地址里");
    }
    // Checked here rather than by downloading and reading a 404: the asset name
    // is the contract, and saying so is the whole error message.
    if !release.assets.iter().any(|asset| asset.name == ARCHIVE) {
        bail!("release {tag} 里没有 {ARCHIVE}");
    }

    // Through the panel's GitHub proxy when it has one -- the archive is the
    // part a blocked network cannot reach. The API call above is not proxied:
    // most proxies only front releases, and a hub that cannot read the tag
    // still has the upload button.
    let url =
        crate::proxied(app, format!("https://github.com/{owner}/{repo}/releases/download/{tag}/{ARCHIVE}"));
    let response =
        app.http.get(url).timeout(std::time::Duration::from_secs(120)).send().await?.error_for_status()?;
    // The transfer stops at Content-Length, so checking it is checking the
    // body: a header that understates the archive cannot make it arrive
    // larger. GitHub always sends one; a proxy that drops it is refused rather
    // than read unbounded.
    match response.content_length() {
        Some(size) if size <= MAX_THEME => {}
        Some(size) => bail!("主题包 {} MiB，超过 {} MiB 的上限", size / 1024 / 1024, MAX_THEME / 1024 / 1024),
        None => bail!("下载没有给出大小，无法确认它在 {} MiB 以内", MAX_THEME / 1024 / 1024),
    }
    let archive = response.bytes().await?;

    // Same unpacking, checks and atomic replace an upload goes through, told
    // which theme it is allowed to land on.
    let (themes, short) = (app.themes.clone(), short.to_owned());
    let theme = tokio::task::spawn_blocking(move || {
        crate::frontend::install(&themes, std::io::Cursor::new(archive), Some(&short))
    })
    .await??;
    Ok((true, theme.version))
}

/// The thumbnail the theme list shows, when the theme ships one. A theme
/// without it answers 404, which is what the panel hides the image on -- so
/// nothing has to report whether a preview exists.
pub async fn theme_preview(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match crate::frontend::preview(&app.themes, &short) {
        // Not cached: reinstalling a theme under the same name replaces the
        // image too, and this is a panel-only request on a local file.
        Some(png) => {
            ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "no-cache")], png).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Deletes an installed theme. Deleting the one in use is allowed: the public
/// page falls back to the built-in theme from the next request on, which is
/// the same path a broken theme already takes, and leaving the setting alone
/// means reinstalling the theme picks it up again.
pub async fn delete_theme(_: Admin, State(app): State<Shared>, Path(short): Path<String>) -> Response {
    match crate::frontend::remove(&app.themes, &short) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => bad(&format!("{e:#}")),
    }
}

pub async fn themes(_: Admin, State(app): State<Shared>) -> Response {
    match crate::frontend::themes(&app) {
        Ok(themes) => Json(json!({"themes": themes})).into_response(),
        Err(e) => fail(e),
    }
}

/// Every live session, the caller's own marked.
///
/// `id` is the stored SHA-256 of the session token, not the token: it names a
/// row without being something a browser could present as a cookie.
pub async fn sessions(_: Admin, State(app): State<Shared>, headers: HeaderMap) -> Response {
    let mine = current_session(&headers);
    match app.db.sessions() {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(hash, expires_at)| {
                    json!({
                        "current": mine.as_deref() == Some(hash.as_str()),
                        "created_at": issued_at(expires_at),
                        "id": hash,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => fail(e),
    }
}

/// Deleting a row that is not there is not an error: two panels open on the
/// same list both get the device signed out, which is what was asked for.
pub async fn delete_session(_: Admin, State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.db.drop_session(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => fail(e),
    }
}

pub async fn settings(_: Admin, State(app): State<Shared>) -> Json<Value> {
    let mut out = serde_json::Map::new();
    for key in READABLE_SETTINGS {
        out.insert((*key).to_owned(), json!(app.db.get(key).unwrap_or_default()));
    }
    out.insert(
        "github_secret_set".into(),
        json!(app.db.get("github_client_secret").is_some_and(|v| !v.is_empty())),
    );
    // Read-only here. A window is opened and closed through its own route, so
    // the key is always one the hub generated, and `save_settings` keeps
    // refusing both names.
    for key in ["register_key", "register_until"] {
        out.insert(key.into(), json!(app.db.get(key).unwrap_or_default()));
    }
    Json(Value::Object(out))
}

pub async fn save_settings(
    _: Admin,
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some(map) = body.as_object() else { return bad("expected an object") };
    // Set when the password changed, so the caller can be handed a fresh
    // session instead of being logged out by their own password change.
    let mut reissued = String::new();
    for (key, value) in map {
        let Some(value) = value.as_str() else { continue };
        let stored = match key.as_str() {
            "theme" if !crate::frontend::selectable(&app, value) => return bad("theme is not installed"),
            // Housekeeping clamps whatever it reads, so an unparseable value
            // would be stored, shown back, and quietly mean 30 days forever.
            "retention_days" if !value.parse::<i64>().is_ok_and(|d| (1..=3_650).contains(&d)) => {
                return bad("retention days must be a number from 1 to 3650")
            }
            // The hub fetches this URL itself, so it has to be one: a scheme
            // it cannot speak turns every agent download into a 502 that says
            // nothing about the setting that caused it.
            "github_proxy"
                if !(value.is_empty() || value.starts_with("http://") || value.starts_with("https://")) =>
            {
                return bad("GitHub proxy must start with http:// or https://")
            }
            k if READABLE_SETTINGS.contains(&k) || k == "github_client_secret" => value,
            // Changing the password logs every existing session out.
            "admin_password" => {
                if value.len() < 12 {
                    return bad("password must be at least 12 characters");
                }
                // Every existing session dies with the old password; the
                // caller gets a replacement below.
                match hash_password(value).and_then(|h| {
                    app.db.set("admin_password_hash", &h)?;
                    app.db.drop_all_sessions()?;
                    issue_session(&app, &headers)
                }) {
                    Ok(cookie) => {
                        reissued = cookie;
                        continue;
                    }
                    Err(e) => return fail(e),
                }
            }
            _ => return bad(&format!("unknown setting: {key}")),
        };
        if let Err(e) = app.db.set(key, stored) {
            return fail(e);
        }
    }
    with_cookies(Json(json!({"ok": true})), [reissued])
}

#[cfg(test)]
mod tests {
    use super::*;
    // Sessions are still hashed; only node tokens stopped being.
    use crate::auth::sha256;
    use crate::db::Db;

    fn app() -> App {
        App::for_test(Db::open(":memory:").unwrap())
    }

    /// The update button follows a manifest's `url` to build a download
    /// address, so what counts as a GitHub repository is the whole of the
    /// trust boundary: whatever this accepts, the hub will fetch.
    #[test]
    fn only_a_github_repository_url_can_name_a_release_to_download() {
        assert_eq!(github_repo("https://github.com/stqfdyr/monitor"), Some(("stqfdyr", "monitor")));
        // A link to the repository, however the author wrote it down.
        assert_eq!(github_repo("https://github.com/a/b.git"), Some(("a", "b")));
        assert_eq!(github_repo("https://github.com/a/b/tree/main"), Some(("a", "b")));
        assert_eq!(github_repo("https://github.com/a/b/"), Some(("a", "b")));

        for hostile in [
            "",
            // Not github.com, however much of it is in the string.
            "http://github.com/a/b",
            "https://github.com.evil.test/a/b",
            "https://github.com@evil.test/a/b",
            "https://evil.test/https://github.com/a/b",
            // On github.com, but not naming a repository to fetch from.
            "https://github.com/a",
            "https://github.com//b",
            "https://github.com/../../etc/passwd",
            "https://github.com/a/..",
            // Anything that could open a segment of its own in the URL built
            // out of it -- encoded, queried or fragmented.
            "https://github.com/a/b%2f..%2fc",
            "https://github.com/a/b?x=1",
            "https://github.com/a b",
        ] {
            assert_eq!(github_repo(hostile), None, "{hostile} must not name a download");
        }

        // The release tag lands in that URL too, and arrives from the API
        // rather than from the manifest.
        assert!(path_segment("v0.1.15") && path_segment("2024.1"));
        assert!(!path_segment("release/1.0") && !path_segment("..") && !path_segment(""));
    }

    /// The whole chunked-upload protocol: an upload is only ever as long as
    /// what has landed, so a piece continues it, restarts it, or is refused.
    #[tokio::test]
    async fn a_chunk_continues_an_upload_only_where_the_last_one_ended() {
        let path = std::env::temp_dir().join(format!("monitor-chunk-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let piece = |offset, total| Chunk { offset, total };
        let body = |bytes: &'static [u8]| axum::body::Body::from(bytes);

        // Two pieces in order, and the length answers where the next one goes.
        assert_eq!(receive(path, &piece(0, 6), 1024, body(b"abc")).await.unwrap(), 3);
        assert_eq!(receive(path, &piece(3, 6), 1024, body(b"def")).await.unwrap(), 6);
        assert_eq!(std::fs::read(path).unwrap(), b"abcdef");

        // A gap, a rewind and an overshoot are all the same refusal.
        assert!(receive(path, &piece(9, 12), 1024, body(b"xyz")).await.is_err());
        assert!(receive(path, &piece(3, 12), 1024, body(b"xyz")).await.is_err());
        assert!(receive(path, &piece(6, 7), 1024, body(b"toolong")).await.is_err());
        // ...and none of them moved the file, so the upload can carry on.
        assert_eq!(std::fs::metadata(path).unwrap().len(), 6);

        // The ceiling is checked against the declared total, before any bytes.
        assert!(receive(path, &piece(0, 4096), 1024, body(b"a")).await.is_err());
        assert!(receive(path, &piece(0, 0), 1024, body(b"")).await.is_err());

        // Starting over truncates whatever an interrupted attempt left.
        assert_eq!(receive(path, &piece(0, 2), 1024, body(b"hi")).await.unwrap(), 2);
        assert_eq!(std::fs::read(path).unwrap(), b"hi");
        std::fs::remove_file(path).unwrap();
    }

    /// A connected agent holding one report. The receiver comes back because
    /// dropping it closes the channel, which is the signal `reset_token` is
    /// tested for.
    fn connect(app: &App, id: i64, metrics: Value) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut agent = crate::agent_ws::Agent::new(7, tx);
        agent.metrics = metrics;
        agent.last_seen = Utc::now().timestamp();
        app.agents.write().unwrap().insert(id, agent);
        rx
    }

    /// A probe assigned to `nodes`. The window query draws a node's current
    /// assignments only, so a fixture holding ping records needs one behind
    /// them.
    fn task(app: &App, nodes: Vec<i64>) -> i64 {
        app.db
            .save_ping_task(&PingTask {
                id: 0,
                name: "probe".into(),
                target: "1.1.1.1:443".into(),
                interval: 60,
                nodes,
            })
            .unwrap()
    }

    fn node(app: &App, name: &str, public: bool) -> i64 {
        app.db
            .create_node(
                &Node { name: name.into(), public, remark: "secret note".into(), ..Default::default() },
                &format!("token-of-{name}"),
            )
            .unwrap()
    }

    /// A chart request costs about the same whatever it spans. This path asks
    /// for no session, so an unbounded window is megabytes of JSON that anyone
    /// can ask the hub to build on the connection the agents report through.
    #[test]
    fn a_history_window_costs_the_same_however_wide_it_is() {
        let app = app();
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        // A month of history at the rate the hub writes it. Two probes,
        // because the budget is per series and a single-probe fixture would
        // hide that.
        const PROBES: i64 = 2;
        for _ in 0..PROBES {
            task(&app, vec![id]);
        }
        for i in 0..30 * 1440 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
            for task in 1..=PROBES {
                app.db.insert_ping(id, task, now - i * 20, 42).unwrap();
            }
        }

        // Windows that do not divide evenly included: those are the ones a
        // step rounded the wrong way overruns on.
        for hours in [1, 6, 13, 23, 24, 168, 2_160] {
            let step = sample_step(hours, None);
            let since = now - hours * 3_600;
            let metrics = app.db.metrics(id, since, step).unwrap();
            let ping = app.db.ping_records(id, since, step).unwrap();
            // Against the budget itself, not against whatever the step worked
            // out to: derived from the step, this would only say that division
            // works. One bucket of slack, as the window rarely divides evenly.
            let cap = 1_441;
            assert!(metrics.len() <= cap, "{hours}h returned {} metric rows", metrics.len());
            assert!(
                ping.len() <= cap * PROBES as usize,
                "{hours}h returned {} ping rows for {PROBES} probes",
                ping.len()
            );
            // Thin, but not empty, and not reaching outside the window.
            assert!(!metrics.is_empty() && !ping.is_empty(), "{hours}h returned nothing");
            // A bucket the window opens halfway through starts before it.
            assert!(
                metrics.iter().all(|m| m["ts"].as_i64().unwrap() >= since - step),
                "{hours}h reached back too far"
            );
        }
        // The widest window is no dearer than a narrow one: unthinned, a month
        // of history is 43 200 rows.
        assert!(app.db.metrics(id, now - 2_160 * 3_600, sample_step(2_160, None)).unwrap().len() <= 1_441);

        // A day comes back as every minute it holds: thinning is for what the
        // screen cannot draw, nothing else.
        assert_eq!(sample_step(24, Some(2_000)), 60, "a day of minutes fits under the ceiling");
        assert_eq!(sample_step(6, Some(2_000)), 60, "and so does six hours");

        // A caller may ask for less than the budget, never more: the ceiling
        // belongs to the hub, since this path takes no credentials.
        assert!(sample_step(24, Some(390)) > sample_step(24, None));
        assert_eq!(sample_step(24, Some(100_000)), sample_step(24, None));
        assert_eq!(sample_step(24, Some(0)), sample_step(24, Some(60)));

        // Asking for one half leaves the other empty rather than shipping it:
        // on the day window that half was two thirds of the response.
        let series = |q: &str| serde_urlencoded::from_str::<Window>(q).unwrap().series;
        assert_eq!(series("hours=24&series=ping").as_deref(), Some("ping"));
        assert!(series("hours=24").is_none(), "no series means both, which is what curl gets");
    }

    /// What a thinned bucket may answer with. Keeping one row and dropping the
    /// rest made the seven-day chart integrate to twice the traffic the minutes
    /// hold, and drew a probe losing half its packets as an unbroken line.
    #[test]
    fn a_thinned_bucket_answers_with_its_mean_and_says_what_it_lost() {
        let app = app();
        let id = node(&app, "n", true);
        // Anchored on a bucket boundary, a whole bucket into the past. Hung
        // off `now`, the rows straddle the boundary depending on what second
        // the suite runs at.
        let base = Utc::now().timestamp() / 120 * 120 - 120;
        // One bucket: a quiet minute and a busy one, then a probe that
        // answered once and timed out three times.
        app.db.insert_metric(id, base + 10, &json!({"cpu": 0.0, "net_rx": 0})).unwrap();
        app.db.insert_metric(id, base + 70, &json!({"cpu": 40.0, "net_rx": 1_000})).unwrap();
        for _ in 0..3 {
            task(&app, vec![id]);
        }
        for (i, latency) in [30, -1, -1, -1].into_iter().enumerate() {
            app.db.insert_ping(id, 1, base + 10 + i as i64 * 20, latency).unwrap();
        }
        // A second probe that never answered, and a third that answered
        // cleanly.
        app.db.insert_ping(id, 2, base + 10, -1).unwrap();
        app.db.insert_ping(id, 3, base + 10, 12).unwrap();

        let m = &app.db.metrics(id, base, 120).unwrap()[0];
        assert_eq!(m["cpu"], 20.0, "the bucket is its mean, not one row of it");
        assert_eq!(m["net_rx"], 500);
        assert_eq!(m["ts"], base, "stamped with the bucket, so every series shares a grid");

        // By task, not by index: the rows share a timestamp, so `ORDER BY ts`
        // leaves their order to SQLite.
        let rows = app.db.ping_records(id, base, 120).unwrap();
        let probe = |task: i64| {
            rows.iter().find(|r| r["task_id"] == task).unwrap_or_else(|| panic!("no probe {task}"))
        };
        assert_eq!(probe(1)["latency"], 30, "the median of what answered, not of the timeouts");
        assert_eq!(probe(1)["loss"], 75);
        assert_eq!(probe(2)["latency"], json!(null), "a bucket that was all timeout has no latency");
        assert_eq!(probe(2)["loss"], 100);
        // One answer, so there is nothing for a band to span.
        assert!(probe(1).get("band").is_none(), "{:?}", probe(1));
        // A clean bucket carries no loss key at all, which is why the
        // percentage rounds up: the key's absence reads as "nothing was lost",
        // so nothing lost has to be the only way to produce it.
        assert!(probe(3).get("loss").is_none(), "{:?}", probe(3));

        // One timeout in a bucket too full for it to be a whole percent:
        // truncating answers the same as a clean bucket.
        let wide = node(&app, "wide", true);
        let wide_probe = task(&app, vec![wide]);
        let wide_base = base / 180 * 180;
        for i in 0..180 {
            app.db.insert_ping(wide, wide_probe, wide_base + i, if i == 0 { -1 } else { 20 }).unwrap();
        }
        let rows = app.db.ping_records(wide, wide_base, 180).unwrap();
        assert_eq!(rows.len(), 1, "the fixture has to be one bucket for this to mean anything");
        let row = &rows[0];
        assert_eq!(row["loss"], 1, "a bucket that lost one of 180 has not lost none");

        // What the band is for: the middle reading and the two ends the bucket
        // reached. Drawing 20 alone renders a 40 ms swing as a flat point.
        let jitter = node(&app, "jitter", true);
        let jitter_probe = task(&app, vec![jitter]);
        for (i, latency) in [10, 20, 50, 20, 20].into_iter().enumerate() {
            app.db.insert_ping(jitter, jitter_probe, wide_base + i as i64, latency).unwrap();
        }
        let row = &app.db.ping_records(jitter, wide_base, 180).unwrap()[0];
        assert_eq!(row["latency"], 20, "the middle answer, not the mean of 24");
        assert_eq!(row["band"], json!([10, 50]));

        // An even count has no middle answer, so it is the mean of the two
        // that straddle it. Every neighbouring pair is a different number, so
        // reaching one rank either way answers 20 or 30 rather than 25.
        let even = node(&app, "even", true);
        let even_probe = task(&app, vec![even]);
        for (i, latency) in [40, 10, 30, 20].into_iter().enumerate() {
            app.db.insert_ping(even, even_probe, wide_base + i as i64, latency).unwrap();
        }
        assert_eq!(app.db.ping_records(even, wide_base, 180).unwrap()[0]["latency"], 25);
    }

    #[test]
    fn the_public_view_hides_private_nodes_and_sensitive_fields() {
        let app = app();
        let open = node(&app, "open", true);
        node(&app, "hidden", false);
        app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9").unwrap();

        // A live report, so the public view has metrics to strip.
        let _held =
            connect(&app, open, json!({"boot_id": "abc", "net_rx_total": 134_000_000_000i64, "cpu": 1.0}));

        let public = visible_nodes(&app, false).unwrap();
        assert_eq!(public.len(), 1, "a node marked private must not be listed");
        assert_eq!(public[0]["name"], "open");
        // Handing the token out would let any visitor impersonate the node.
        for hidden in ["ip", "remark", "hostname", "token"] {
            assert!(public[0].get(hidden).is_none(), "{hidden} must not be public");
        }
        assert!(
            !serde_json::to_string(&public).unwrap().contains("token-of-open"),
            "no node's token may appear anywhere in a public payload"
        );
        // Raw kernel counters would hand out the machine's lifetime traffic.
        for hidden in ["boot_id", "net_rx_total", "net_tx_total"] {
            assert!(public[0]["metrics"].get(hidden).is_none(), "{hidden} must not be public");
        }
        assert_eq!(public[0]["metrics"]["cpu"], 1.0, "the rest of the report still goes out");

        let admin = visible_nodes(&app, true).unwrap();
        assert_eq!(admin.len(), 2);
        assert_eq!(admin[0]["ip"], "198.51.100.9");
        assert_eq!(admin[0]["remark"], "secret note");
    }

    #[tokio::test]
    async fn rotating_a_token_closes_the_session_the_old_one_opened() {
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let mut rx = connect(&app, id, Value::Null);

        let response = reset_token(Admin, axum::extract::State(app.clone()), Path(id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        // The agent loop selects on this receiver, so a closed channel is how
        // it learns to go. `try_recv`, because `recv().await` on a channel
        // wrongly left open would hang the suite rather than fail it.
        assert!(
            matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)),
            "the old agent's channel must be closed"
        );
        assert!(app.agents.read().unwrap().is_empty(), "the node must read as offline at once");
    }

    #[test]
    fn the_shared_snapshot_keeps_the_two_audiences_apart() {
        let app = app();
        let open = node(&app, "open", true);
        node(&app, "hidden", false);
        app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9").unwrap();

        let public = live_snapshot(&app, false);
        let admin = live_snapshot(&app, true);
        // Caching must never let one audience's payload reach the other.
        assert!(!public.as_str().contains("198.51.100.9"), "the public frame must carry no address");
        assert!(!public.as_str().contains("hidden"), "the public frame must carry no private node");
        assert!(admin.as_str().contains("198.51.100.9") && admin.as_str().contains("hidden"));

        // Two reads over unchanged data prove nothing, since a rebuild returns
        // the same bytes, so the data moves underneath first.
        node(&app, "late", true);
        assert_eq!(live_snapshot(&app, false), public, "the frame is reused, not rebuilt per viewer");
    }

    #[test]
    fn a_clock_stepping_backwards_does_not_pin_a_stale_frame() {
        let app = app();
        node(&app, "first", true);
        live_snapshot(&app, false);

        // NTP correcting a fresh boot leaves the cached stamp in the future.
        // That is not a young frame.
        app.snapshot.lock().unwrap()[0].0 = Utc::now().timestamp_millis() + 60_000;
        node(&app, "added-after", true);
        assert!(live_snapshot(&app, false).as_str().contains("added-after"));
    }

    /// The panel sends nothing but a name, and expects the node it just added
    /// to be in the frame it is already streaming.
    #[tokio::test]
    async fn a_node_added_from_the_panel_needs_only_a_name_and_shows_up_at_once() {
        let app = std::sync::Arc::new(app());
        node(&app, "existing", true);
        assert!(!live_snapshot(&app, true).as_str().contains("added"));

        let added: Node = serde_json::from_value(json!({"name": "added"})).unwrap();
        // The defaults the panel leans on by not sending them. `public` above
        // all: the other way publishes a node nobody published.
        assert!(added.public);
        assert_eq!(added.billing_cycle, "monthly");
        assert_eq!(added.traffic_reset_day, 1);

        let created = create_node(Admin, State(app.clone()), Ok(Json(added))).await;
        assert_eq!(created.status(), StatusCode::OK);
        // Frames are cached for nearly two seconds, so without dropping the
        // cache the node just added blinks back out of the list.
        assert!(live_snapshot(&app, true).as_str().contains("added"));

        // A name of nothing but spaces is refused, and leaves no node behind.
        let blank = Json(serde_json::from_value::<Node>(json!({"name": "   "})).unwrap());
        let refused = create_node(Admin, State(app.clone()), Ok(blank)).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.nodes().unwrap().len(), 2);
    }

    /// Every gate on the anonymous route, in the order a batch install meets
    /// them: shut, wrong key, open, expired, closed by hand.
    #[tokio::test]
    async fn registration_only_works_inside_a_window_the_panel_opened() {
        let app = std::sync::Arc::new(app());
        let register = |key: Option<&str>, name: &str| {
            let mut headers = HeaderMap::new();
            if let Some(key) = key {
                headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
            }
            agent_register(
                State(app.clone()),
                ConnectInfo("198.51.100.7:40000".parse().unwrap()),
                headers,
                name.to_owned(),
            )
        };

        // Nothing was opened, so no key is the right one.
        assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
        assert!(app.db.nodes().unwrap().is_empty());

        assert_eq!(open_register(Admin, State(app.clone())).await.status(), StatusCode::OK);
        let key = app.db.get("register_key").unwrap();
        assert_eq!(register(Some("guess"), "a").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(register(None, "a").await.status(), StatusCode::FORBIDDEN);
        assert!(app.db.nodes().unwrap().is_empty());

        let issued = register(Some(&key), "  web-01\n").await;
        assert_eq!(issued.status(), StatusCode::OK);
        let token = axum::body::to_bytes(issued.into_body(), usize::MAX).await.unwrap().to_vec();
        let token = String::from_utf8(token).unwrap();
        // The point of the whole route: what came back is a token an agent can
        // connect with, not merely a 200.
        let id = app.db.node_by_token(&token).unwrap().expect("token opens a node");
        let node = app.db.nodes().unwrap().into_iter().find(|n| n.id == id).unwrap();
        assert_eq!(node.name, "web-01");
        // Registered nodes land on the panel's defaults, not on `Node::default()`.
        assert!(node.public);
        assert_eq!(node.traffic_reset_day, 1);

        // An hour later the same key buys nothing, which is what makes leaving
        // the window open harmless.
        app.db.set("register_until", &(Utc::now().timestamp() - 1).to_string()).unwrap();
        assert_eq!(register(Some(&key), "b").await.status(), StatusCode::FORBIDDEN);

        // Reopened, then shut by hand: the key from the open window stops working.
        open_register(Admin, State(app.clone())).await;
        let key = app.db.get("register_key").unwrap();
        assert_eq!(close_register(Admin, State(app.clone())).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(register(Some(&key), "c").await.status(), StatusCode::FORBIDDEN);
        assert_eq!(app.db.nodes().unwrap().len(), 1);
    }

    /// The ceiling on the anonymous route: a leaked key cannot fill the table.
    #[tokio::test]
    async fn one_window_stops_registering_at_the_limit() {
        let app = std::sync::Arc::new(app());
        open_register(Admin, State(app.clone())).await;
        let key = app.db.get("register_key").unwrap();
        for i in 0..REGISTER_LIMIT {
            node(&app, &format!("n{i}"), true);
        }
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {key}").parse().unwrap());
        let refused = agent_register(
            State(app.clone()),
            ConnectInfo("198.51.100.7:40000".parse().unwrap()),
            headers,
            "one-too-many".to_owned(),
        )
        .await;
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        assert_eq!(app.db.nodes().unwrap().len() as i64, REGISTER_LIMIT);
    }

    #[test]
    fn a_node_view_carries_traffic_even_while_offline() {
        let app = app();
        let id = node(&app, "n", true);
        app.db.accumulate(id, "b", 100, 100).unwrap();
        app.db.accumulate(id, "b", 900, 500).unwrap();
        app.db.touch_seen(id, 1_700_000_000).unwrap();

        let view = &visible_nodes(&app, true).unwrap()[0];
        assert_eq!(view["online"], false);
        assert_eq!(view["metrics"], Value::Null);
        assert_eq!(view["total_rx"], 800, "traffic is stored, not derived from the live state");
        assert_eq!(view["total_tx"], 400);
        // The live entry went with the connection, so "offline since" has to
        // come off the node row.
        assert_eq!(view["last_seen"], 1_700_000_000);
    }

    #[test]
    fn per_node_reads_follow_the_public_flag_and_the_public_page_switch() {
        let app = app();
        let open = node(&app, "open", true);
        let hidden = node(&app, "hidden", false);

        assert!(readable(&app, false, open), "a published node is readable by anyone");
        assert!(!readable(&app, false, hidden), "a private node is not");
        assert!(!readable(&app, false, 9999), "an unknown id is not");
        assert!(readable(&app, true, hidden), "the panel sees a private node");

        // Switching the public page off closes even the published node.
        app.db.set("public_page", "off").unwrap();
        assert!(!readable(&app, false, open));
        assert!(readable(&app, true, open), "and never closes it for the panel");
    }

    /// The window ceiling, which is a scan bound rather than a response bound:
    /// the thinning already holds the row count, and a quarter-year still reads
    /// every row behind it while holding the write connection.
    #[tokio::test]
    async fn an_anonymous_history_window_stops_at_a_week() {
        let app = std::sync::Arc::new(app());
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        // One sample a day for a month, so a row's presence names its window.
        // The minute of slack keeps day seven off the 168-hour cutoff exactly:
        // on the boundary, a second passing between these inserts and the query
        // below drops it and the count comes out one short.
        for day in 0..30 {
            app.db.insert_metric(id, now - day * 86_400 + 60, &json!({"cpu": 1.0})).unwrap();
        }
        let ask = |hours| {
            let query = format!("hours={hours}&series=metrics");
            metrics(
                State(app.clone()),
                HeaderMap::new(),
                Path(id),
                Query(serde_urlencoded::from_str::<Window>(&query).unwrap()),
            )
        };
        let rows =
            |body: &str| serde_json::from_str::<Value>(body).unwrap()["metrics"].as_array().unwrap().len();

        let week = axum::body::to_bytes(ask(168).await.into_body(), usize::MAX).await.unwrap();
        assert_eq!(rows(std::str::from_utf8(&week).unwrap()), 8, "a week reaches back seven days");

        // Asking for the quarter year an anonymous caller used to get answers
        // with the week: the extra rows exist, and reading them is the cost.
        let quarter = axum::body::to_bytes(ask(2_160).await.into_body(), usize::MAX).await.unwrap();
        assert_eq!(quarter, week, "an anonymous window past a week is clamped to one");
    }

    #[tokio::test]
    async fn changing_the_password_kills_other_sessions_but_not_the_caller() {
        let app = std::sync::Arc::new(app());
        let stale = random_token();
        app.db.create_session(&sha256(&stale), Utc::now().timestamp() + 3_600).unwrap();

        let body = Json(json!({"admin_password": "a-long-enough-password"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), HeaderMap::new(), body).await;

        assert!(!app.db.session_valid(&sha256(&stale)), "sessions must not outlive the old password");

        // The caller is handed a replacement rather than logged out by its own
        // password change.
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("a replacement session")
            .to_str()
            .unwrap();
        let token = cookie.split(';').next().unwrap().split('=').nth(1).unwrap();
        assert!(app.db.session_valid(&sha256(token)), "the replacement session must work");
    }

    /// The panel hides the delete button on the caller's own row, so the mark
    /// is the only thing standing between an admin and signing themselves out.
    #[tokio::test]
    async fn the_session_list_marks_the_caller_and_hides_expired_rows() {
        let app = std::sync::Arc::new(app());
        let (mine, theirs, stale) = (random_token(), random_token(), random_token());
        let now = Utc::now().timestamp();
        app.db.create_session(&sha256(&mine), now + 3_600).unwrap();
        app.db.create_session(&sha256(&theirs), now + 7_200).unwrap();
        app.db.create_session(&sha256(&stale), now - 1).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("monitor_session={mine}").parse().unwrap());
        let body = axum::body::to_bytes(
            sessions(Admin, axum::extract::State(app.clone()), headers).await.into_body(),
            usize::MAX,
        )
        .await
        .unwrap();
        let rows: Vec<Value> = serde_json::from_slice(&body).unwrap();

        assert_eq!(rows.len(), 2, "an expired session is not a session");
        assert_eq!(rows[0]["id"], sha256(&theirs), "newest first");
        assert_eq!(rows[0]["current"], false);
        assert_eq!(rows[1]["id"], sha256(&mine));
        assert_eq!(rows[1]["current"], true, "the caller's own row must be marked");
        assert_eq!(rows[1]["created_at"].as_i64().unwrap(), now + 3_600 - 14 * 86_400);

        delete_session(Admin, axum::extract::State(app.clone()), Path(sha256(&theirs))).await;
        assert!(!app.db.session_valid(&sha256(&theirs)), "the deleted device is signed out");
        assert!(app.db.session_valid(&sha256(&mine)), "and nobody else is");
    }

    #[tokio::test]
    async fn a_short_password_is_refused_and_changes_nothing() {
        let app = std::sync::Arc::new(app());
        let live = random_token();
        app.db.create_session(&sha256(&live), Utc::now().timestamp() + 3_600).unwrap();

        let body = Json(json!({"admin_password": "short"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), HeaderMap::new(), body).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(app.db.get("admin_password_hash").is_none(), "the password must not have changed");
        assert!(app.db.session_valid(&sha256(&live)), "a rejected change must not log anyone out");
    }

    /// Housekeeping clamps whatever it finds, so an unparseable value is not an
    /// error downstream -- it silently means 30 days, in a box still displaying
    /// what was typed.
    #[tokio::test]
    async fn a_retention_window_that_would_never_apply_is_refused() {
        let app = std::sync::Arc::new(app());
        let put = |v: &str| {
            save_settings(
                Admin,
                State(app.clone()),
                HeaderMap::new(),
                Json(json!({"retention_days": v.to_owned()})),
            )
        };
        for junk in ["", "abc", "0", "-1", "9999"] {
            assert_eq!(put(junk).await.status(), StatusCode::BAD_REQUEST, "{junk:?}");
        }
        assert!(app.db.get("retention_days").is_none(), "a refused window must not be stored");
        assert_eq!(put("7").await.status(), StatusCode::OK);
        assert_eq!(app.db.get("retention_days").as_deref(), Some("7"));
    }

    #[tokio::test]
    async fn settings_never_hand_back_the_github_secret() {
        let app = app();
        app.db.set("github_client_secret", "super-secret").unwrap();
        app.db.set("github_client_id", "public-id").unwrap();

        let Json(body) = settings(Admin, axum::extract::State(std::sync::Arc::new(app))).await;
        assert_eq!(body["github_client_id"], "public-id");
        assert_eq!(body["github_secret_set"], true);
        assert!(body.get("github_client_secret").is_none());
        assert!(!body.to_string().contains("super-secret"));
    }
}

//! The panel and public-status HTTP surface.

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent_ws::Agent;
use crate::auth::{authed, hash_password, issue_session, random_token, with_cookies};
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
        "sort": node.sort,
        "public": node.public,
        "online": current.is_some(),
        // The live entry while it is connected, the stored one after it goes:
        // "offline" is worth much more with a "since when" attached.
        // Zero means connected but not yet reporting, which is not a time:
        // fall back to the stored one so "offline since" survives the gap.
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
    // Raw kernel counters are a wire-protocol detail: the hub's accumulated
    // figures above are the truth, and the raw ones expose a machine's whole
    // lifetime traffic to anyone loading the public page.
    if !full {
        if let Some(m) = view["metrics"].as_object_mut() {
            m.remove("boot_id");
            m.remove("net_rx_total");
            m.remove("net_tx_total");
        }
    }
    // Address, private notes and the node's token never leave the panel. The
    // token is here so the install command can be shown whenever it is asked
    // for, rather than reissued to be read — see docs/decisions.md.
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
    // One traffic query and one lock for the whole list, not one of each per
    // node: this list is what every visitor to the public page loads.
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
    // The same rendered frame the browser streams get, for the same reason: a
    // public status page that gets linked somewhere busy would otherwise
    // rebuild every node's row once per visitor, against the connection the
    // agents are writing through.
    ([(axum::http::header::CONTENT_TYPE, "application/json")], live_snapshot(&app, full).as_str().to_owned())
        .into_response()
}

#[derive(Deserialize)]
pub struct Window {
    #[serde(default = "default_hours")]
    hours: i64,
    /// How many points the caller can actually draw. Optional: an old theme,
    /// or curl, gets the full budget.
    points: Option<i64>,
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
    if !readable(&app, &headers, id) {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    let hours = w.hours.clamp(1, 24 * 90);
    let since = Utc::now().timestamp() - hours * 3_600;
    // Probe names ride along with the samples they label, so the chart reads
    // the same for a visitor as for the admin and the page needs no second
    // request. Only the names: targets and assignments stay behind `Admin`.
    let probes = app.db.ping_task_names().unwrap_or_else(|_| json!({}));
    let step = sample_step(hours, w.points);
    // The probe series gets a budget of its own, and a smaller one. It is drawn
    // as a median with the range its answers spanned around it, and a bucket
    // holding two samples has no middle and a band two readings wide -- at a
    // day's width that came out as a scribble with no envelope you could see.
    // Smokeping's daily graph is the shape this lands on: five-minute buckets,
    // 288 of them. An hour and six hours still come back one sample a bucket,
    // which is every ping there is and needs no band at all.
    let probe_step = sample_step(hours, Some(w.points.map_or(PROBE_POINTS, |p| p.min(PROBE_POINTS))));
    match (app.db.metrics(id, since, step), app.db.ping_records(id, since, probe_step)) {
        (Ok(m), Ok(p)) => Json(json!({"metrics": m, "ping": p, "probes": probes})).into_response(),
        (Err(e), _) | (_, Err(e)) => fail(e),
    }
}

/// Seconds between the samples a window is drawn from, so that a chart costs
/// the same whatever it spans: about `SAMPLES` points either way.
///
/// The unthinned month is 43 200 metric rows and several times that in probe
/// results, which is megabytes of JSON built in memory for a request that needs
/// no credentials at all — and a screen has around a thousand pixels to draw it
/// across. Whole minutes, because that is the grid the metric rows sit on.
///
/// `points` is what the caller says it can draw, the way Grafana's panels send
/// their own width: a phone was being sent 720 samples a probe to lay across
/// 280 pixels, which is two and a half samples a pixel of aliasing and a
/// quarter of a megabyte over mobile data to draw it. It only ever lowers the
/// budget — the ceiling is the hub's, not the caller's, because this path
/// takes no credentials.
// ponytail: the budget is per series, so a response is SAMPLES × (1 + probes) --
// bounded by how many probes the admin created, not by the caller. Four probes
// is ~250 KB; if that list ever grows long, scale SAMPLES by the probe count.
/// Points a probe line is drawn from, whatever the screen. Unlike the metric
/// budget this is not a pixel count: it is how coarse a bucket has to be for
/// its median and its spread to describe anything, and that does not change
/// when the window is narrower.
const PROBE_POINTS: i64 = 250;

fn sample_step(hours: i64, points: Option<i64>) -> i64 {
    const SAMPLES: i64 = 720;
    let budget = points.unwrap_or(SAMPLES).clamp(60, SAMPLES);
    60 * (hours * 60 / budget).max(1)
}

/// Guards a per-node read: the panel sees everything, the public page only
/// sees nodes that were explicitly published.
fn readable(app: &App, headers: &HeaderMap, id: i64) -> bool {
    authed(app, headers) || (app.public_page() && app.db.node(id).ok().flatten().is_some_and(|n| n.public))
}

/// Per-connection read buffer for both WebSocket surfaces. The default is
/// 128 KiB, which at a few hundred agents is tens of megabytes of buffer for
/// frames that are a few hundred bytes each.
pub const SOCKET_BUFFER: usize = 4 * 1024;

/// Largest frame either socket will accept, matching the 64 KiB cap the HTTP
/// body already carries — a report is a few hundred bytes, and nothing on
/// either socket is legitimately larger.
///
/// The body limit is a tower layer, so it never applied here: without this the
/// default ceiling is 64 MiB. A node's own token buys the whole of it, and what
/// arrives on it is stored and then handed to every viewer of the public page.
pub const MAX_FRAME: usize = 64 * 1024;

/// How long one rendered snapshot is reused. Just under the push interval, so
/// every tick still rebuilds once and no viewer is served a stale frame twice.
const SNAPSHOT_TTL_MS: i64 = 1_900;

/// The payload every browser stream sends, built at most once per tick no
/// matter how many tabs are watching.
///
/// The public status page is open to anonymous visitors, so the old
/// per-connection build made viewer count a multiplier on database work: each
/// one queried every node's traffic row every two seconds, against the same
/// connection the agents write through. Two slots, because the admin view
/// carries fields the public one must never see.
fn live_snapshot(app: &App, full: bool) -> Utf8Bytes {
    let now = Utc::now().timestamp_millis();
    let slot = usize::from(full);
    let mut cache = app.snapshot.lock().unwrap_or_else(|e| e.into_inner());
    // A cached frame's age has to be an age: non-negative. A wall clock can step
    // backwards -- NTP correcting a fresh boot -- and against a bare upper bound
    // the negative that produces reads as "young", pinning the panel to a stale
    // frame until real time catches up to where the clock had been.
    if (0..SNAPSHOT_TTL_MS).contains(&now.saturating_sub(cache[slot].0)) {
        return cache[slot].1.clone();
    }
    let nodes = visible_nodes(app, full).unwrap_or_default();
    // `admin` rides along so the panel's first fetch and its stream can share
    // one cached frame; the stream's consumers only ever read `nodes`.
    let payload = Utf8Bytes::from(json!({"nodes": nodes, "admin": full}).to_string());
    cache[slot] = (now, payload.clone());
    payload
}

/// Drops the cached frames so the next push rebuilds. A write the panel just
/// made has to be in that push: without this the node it added blinks back out
/// of the list, and the one it deleted reappears, until the frame ages out.
fn invalidate_snapshot(app: &App) {
    for slot in app.snapshot.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
        slot.0 = 0;
    }
}

/// Live stream for the browser. Each connection runs its own timer, which is
/// cheaper to reason about than a fan-out channel; the snapshot behind it is
/// shared, so the timers cost nothing but a send.
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
        // The hub's own public URL, which is what belongs in an install command
        // and in the OAuth callback — not whichever address this browser used
        // to reach the panel, which may well be a loopback port behind a proxy.
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
        // The node is usable straight away: its install command is readable
        // from the node list, so adding and deploying stay separate steps
        // without a reissue standing between them.
        Ok(id) => {
            invalidate_snapshot(&app);
            Json(json!({"id": id})).into_response()
        }
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

/// The list must name every node exactly once. That is checked inside the
/// transaction that does the renumbering rather than here: a duplicate, a
/// missing node and an unknown id are the same three rejections either way, and
/// re-reading the node list first only made the check race the write it guards.
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

/// Issues a fresh token, which immediately invalidates the old one.
///
/// Only ever an explicit act now — rotate a token you think has leaked, and
/// reinstall the agent afterwards. Reading the install command no longer goes
/// through here, so nothing routine lands on it by accident.
pub async fn reset_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    let token = random_token();
    let updated = app.db.node(id).map(|n| n.is_some()).unwrap_or(false);
    if !updated {
        return (StatusCode::NOT_FOUND, "no such node").into_response();
    }
    // The token is only checked during the handshake, so a session opened with
    // the old one would otherwise keep reporting indefinitely — a rotation
    // after a leak has to close the door that token already walked through.
    // Dropping the sender ends the agent's loop; it reconnects and is refused.
    // The live entry goes with it: that loop no longer owns it, so its own
    // teardown will leave it alone.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).remove(&id);
    // The token is part of the admin frame, so the panel would otherwise keep
    // showing an install command for the credential just retired.
    invalidate_snapshot(&app);
    match app.db.reset_token(id, &token) {
        // Just the token: the panel builds the command, and one place that
        // knows its shape is enough.
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
const READABLE_SETTINGS: &[&str] =
    &["site_name", "public_page", "github_client_id", "github_allowed_users", "retention_days", "theme"];

pub async fn themes(_: Admin, State(app): State<Shared>) -> Response {
    match crate::frontend::themes(&app) {
        Ok(themes) => Json(json!({"themes": themes})).into_response(),
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
    Json(Value::Object(out))
}

pub async fn save_settings(_: Admin, State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let Some(map) = body.as_object() else { return bad("expected an object") };
    // Set when the password changed, so the caller can be handed a fresh
    // session instead of being logged out by their own password change.
    let mut reissued = String::new();
    for (key, value) in map {
        let Some(value) = value.as_str() else { continue };
        let stored = match key.as_str() {
            "theme" if !crate::frontend::selectable(&app, value) => return bad("theme is not installed"),
            // Housekeeping clamps whatever it reads back, so anything it cannot
            // parse would be stored, shown back in the panel, and quietly mean
            // 30 days forever. Refuse it here instead.
            "retention_days" if !value.parse::<i64>().is_ok_and(|d| (1..=3_650).contains(&d)) => {
                return bad("retention days must be a number from 1 to 3650")
            }
            k if READABLE_SETTINGS.contains(&k) || k == "github_client_secret" => value,
            // Changing the password logs every existing session out.
            "admin_password" => {
                if value.len() < 12 {
                    return bad("password must be at least 12 characters");
                }
                // Every existing session dies with the old password; the
                // browser doing the change gets a replacement below.
                match hash_password(value).and_then(|h| {
                    app.db.set("admin_password_hash", &h)?;
                    app.db.drop_all_sessions()?;
                    issue_session(&app)
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

    /// A connected agent holding one report, as `App.agents` carries it. The
    /// receiver comes back because dropping it closes the channel, which is
    /// itself the signal `reset_token` is tested for.
    fn connect(app: &App, id: i64, metrics: Value) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut agent = crate::agent_ws::Agent::new(7, tx);
        agent.metrics = metrics;
        agent.last_seen = Utc::now().timestamp();
        app.agents.write().unwrap().insert(id, agent);
        rx
    }

    fn node(app: &App, name: &str, public: bool) -> i64 {
        app.db
            .create_node(
                &Node { name: name.into(), public, remark: "secret note".into(), ..Default::default() },
                &format!("token-of-{name}"),
            )
            .unwrap()
    }

    /// A chart request costs about the same whatever it spans. Nothing on this
    /// path asks for a session, so an unbounded month-wide window is a few
    /// megabytes of JSON that anyone at all can ask the hub to build, over and
    /// over, on the connection the agents are reporting through.
    #[test]
    fn a_history_window_costs_the_same_however_wide_it_is() {
        let app = app();
        let id = node(&app, "n", true);
        let now = Utc::now().timestamp();
        // A month of history at the rate the hub actually writes it: a metric
        // row a minute, and a probe result rather more often than that. Two
        // probes, because each one is a line of its own on the chart and so
        // gets its own samples -- the budget is per series, not per response,
        // and a single-probe fixture would hide that.
        const PROBES: i64 = 2;
        for i in 0..30 * 1440 {
            app.db.insert_metric(id, now - i * 60, &json!({"cpu": 1.0})).unwrap();
            for task in 1..=PROBES {
                app.db.insert_ping(id, task, now - i * 20, 42).unwrap();
            }
        }

        for hours in [1, 6, 24, 168, 2_160] {
            let step = sample_step(hours, None);
            let since = now - hours * 3_600;
            let metrics = app.db.metrics(id, since, step).unwrap();
            let ping = app.db.ping_records(id, since, step).unwrap();
            // One bucket of slack: the window rarely divides evenly.
            let cap = (hours * 3_600 / step) + 1;
            assert!(metrics.len() <= cap as usize, "{hours}h returned {} metric rows", metrics.len());
            assert!(
                ping.len() <= (cap * PROBES) as usize,
                "{hours}h returned {} ping rows for {PROBES} probes",
                ping.len()
            );
            // Thin, but not empty, and not reaching outside the window.
            assert!(!metrics.is_empty() && !ping.is_empty(), "{hours}h returned nothing");
            // A row is stamped with its bucket's start, and a bucket the
            // window opens halfway through starts before the window does.
            assert!(
                metrics.iter().all(|m| m["ts"].as_i64().unwrap() >= since - step),
                "{hours}h reached back too far"
            );
        }
        // The whole point: the widest window is no dearer than a narrow one.
        // Against a month of history, unthinned, this was 43 200 rows.
        assert!(app.db.metrics(id, now - 2_160 * 3_600, sample_step(2_160, None)).unwrap().len() <= 721);

        // A caller may ask for less than it can draw, and no caller may ask for
        // more: the ceiling on this path is the hub's, since it takes no
        // credentials. A phone asking for its 390 pixels gets a coarser step.
        assert!(sample_step(24, Some(390)) > sample_step(24, None));
        assert_eq!(sample_step(24, Some(100_000)), sample_step(24, None));
        assert_eq!(sample_step(24, Some(0)), sample_step(24, Some(60)));

        // The probe budget is a statistic, not a pixel count: a day's bucket
        // has to hold several answers for a median and a spread to describe
        // anything, and a wide screen does not change that. An hour still
        // comes back one sample a bucket, which is every ping there is.
        let probes = |hours, points: Option<i64>| {
            sample_step(hours, Some(points.map_or(PROBE_POINTS, |p: i64| p.min(PROBE_POINTS))))
        };
        assert_eq!(probes(24, Some(1_280)), 300, "a day of probes buckets by five minutes");
        assert_eq!(probes(24, Some(1_280)), probes(24, Some(390)), "the phone gets the same buckets");
        assert_eq!(probes(1, Some(1_280)), 60, "an hour is every sample, and needs no band");
        assert!(probes(24, None) >= sample_step(24, None), "probes are never finer than metrics");
    }

    /// What a thinned bucket is allowed to answer with. Keeping one row of it
    /// and dropping the rest is how the seven-day traffic chart came to
    /// integrate to twice the traffic the minutes hold, and how a probe losing
    /// half its packets came to draw as an unbroken line.
    #[test]
    fn a_thinned_bucket_answers_with_its_mean_and_says_what_it_lost() {
        let app = app();
        let id = node(&app, "n", true);
        // Anchored on a bucket boundary, and a whole bucket into the past.
        // Hung off `now` instead, the rows straddle the boundary or not
        // depending on what second the suite is run at, and the assertions
        // below are green on a developer's machine and red on CI.
        let base = Utc::now().timestamp() / 120 * 120 - 120;
        // One bucket's worth: a quiet minute and a busy one, then a probe that
        // answered once and timed out three times.
        app.db.insert_metric(id, base + 10, &json!({"cpu": 0.0, "net_rx": 0})).unwrap();
        app.db.insert_metric(id, base + 70, &json!({"cpu": 40.0, "net_rx": 1_000})).unwrap();
        for (i, latency) in [30, -1, -1, -1].into_iter().enumerate() {
            app.db.insert_ping(id, 1, base + 10 + i as i64 * 20, latency).unwrap();
        }
        // A second probe that never answered at all, which used to vanish, and
        // a third that answered cleanly.
        app.db.insert_ping(id, 2, base + 10, -1).unwrap();
        app.db.insert_ping(id, 3, base + 10, 12).unwrap();

        let m = &app.db.metrics(id, base, 120).unwrap()[0];
        assert_eq!(m["cpu"], 20.0, "the bucket is its mean, not one row of it");
        assert_eq!(m["net_rx"], 500);
        assert_eq!(m["ts"], base, "stamped with the bucket, so every series shares a grid");

        // By task, not by index: the three rows share a timestamp, so the
        // query's `ORDER BY ts` leaves their order to SQLite.
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
        // A clean bucket carries no loss key at all: it is per row, per probe,
        // on a response that is already a quarter of a megabyte. Which is
        // exactly why the percentage rounds up -- the absence of the key is
        // read as "nothing was lost", so nothing lost has to be the only way
        // to produce it.
        assert!(probe(3).get("loss").is_none(), "{:?}", probe(3));

        // One timeout in a bucket too full for it to be a whole percent.
        // Truncating divides this into the same answer as a clean bucket.
        let wide = node(&app, "wide", true);
        let wide_base = base / 180 * 180;
        for i in 0..180 {
            app.db.insert_ping(wide, 1, wide_base + i, if i == 0 { -1 } else { 20 }).unwrap();
        }
        let rows = app.db.ping_records(wide, wide_base, 180).unwrap();
        assert_eq!(rows.len(), 1, "the fixture has to be one bucket for this to mean anything");
        let row = &rows[0];
        assert_eq!(row["loss"], 1, "a bucket that lost one of 180 has not lost none");

        // What the band is for: the middle reading, and the two ends the bucket
        // actually reached. Drawing 20 here and nothing else is a link that
        // swung 40 ms inside two minutes rendered as a flat point.
        let jitter = node(&app, "jitter", true);
        for (i, latency) in [10, 20, 50, 20, 20].into_iter().enumerate() {
            app.db.insert_ping(jitter, 1, wide_base + i as i64, latency).unwrap();
        }
        let row = &app.db.ping_records(jitter, wide_base, 180).unwrap()[0];
        assert_eq!(row["latency"], 20, "the middle answer, not the mean of 24");
        assert_eq!(row["band"], json!([10, 50]));
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
        // The token joined this list once the panel started reading it back;
        // handing it out would let any visitor impersonate the node.
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
        // The agent loop selects on this receiver; a closed channel is how it
        // learns to go. Asked for with try_recv, because `recv().await` on a
        // channel wrongly left open never returns: the regression this guards
        // would hang the suite rather than fail it.
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

        // A second read inside the window is the cached frame. Two reads over
        // unchanged data prove nothing -- a rebuild returns the same bytes --
        // so the data moves underneath first.
        node(&app, "late", true);
        assert_eq!(live_snapshot(&app, false), public, "the frame is reused, not rebuilt per viewer");
    }

    #[test]
    fn a_clock_stepping_backwards_does_not_pin_a_stale_frame() {
        let app = app();
        node(&app, "first", true);
        live_snapshot(&app, false);

        // NTP correcting a fresh boot leaves the cached stamp in the future.
        // That is not a young frame, and serving it would freeze the panel until
        // real time caught up to where the clock had been.
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
        // The defaults the panel leans on by not sending them. `public` most of
        // all: defaulting the other way would publish a node nobody published.
        assert!(added.public);
        assert_eq!(added.billing_cycle, "monthly");
        assert_eq!(added.traffic_reset_day, 1);

        let created = create_node(Admin, State(app.clone()), Ok(Json(added))).await;
        assert_eq!(created.status(), StatusCode::OK);
        // Frames are cached for nearly two seconds. Without dropping that cache
        // the node the panel just added blinks straight back out of the list.
        assert!(live_snapshot(&app, true).as_str().contains("added"));

        // A name of nothing but spaces is refused, and leaves no node behind.
        let blank = Json(serde_json::from_value::<Node>(json!({"name": "   "})).unwrap());
        let refused = create_node(Admin, State(app.clone()), Ok(blank)).await;
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        assert_eq!(app.db.nodes().unwrap().len(), 2);
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
        // The live entry is gone with the connection; "offline since when" has
        // to come off the node row or the badge has nothing to count from.
        assert_eq!(view["last_seen"], 1_700_000_000);
    }

    #[test]
    fn per_node_reads_follow_the_public_flag_and_the_public_page_switch() {
        let app = app();
        let open = node(&app, "open", true);
        let hidden = node(&app, "hidden", false);
        let anonymous = HeaderMap::new();

        assert!(readable(&app, &anonymous, open), "a published node is readable by anyone");
        assert!(!readable(&app, &anonymous, hidden), "a private node is not");
        assert!(!readable(&app, &anonymous, 9999), "an unknown id is not");

        // Switching the public page off closes even the published node.
        app.db.set("public_page", "off").unwrap();
        assert!(!readable(&app, &anonymous, open));
    }

    #[tokio::test]
    async fn changing_the_password_kills_other_sessions_but_not_the_caller() {
        let app = std::sync::Arc::new(app());
        let stale = random_token();
        app.db.create_session(&sha256(&stale), Utc::now().timestamp() + 3_600).unwrap();

        let body = Json(json!({"admin_password": "a-long-enough-password"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), body).await;

        assert!(!app.db.session_valid(&sha256(&stale)), "sessions must not outlive the old password");

        // The browser that made the change is handed a replacement, so it is
        // not logged out by its own password change.
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("a replacement session")
            .to_str()
            .unwrap();
        let token = cookie.split(';').next().unwrap().split('=').nth(1).unwrap();
        assert!(app.db.session_valid(&sha256(token)), "the replacement session must work");
    }

    #[tokio::test]
    async fn a_short_password_is_refused_and_changes_nothing() {
        let app = std::sync::Arc::new(app());
        let live = random_token();
        app.db.create_session(&sha256(&live), Utc::now().timestamp() + 3_600).unwrap();

        let body = Json(json!({"admin_password": "short"}));
        let response = save_settings(Admin, axum::extract::State(app.clone()), body).await;

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(app.db.get("admin_password_hash").is_none(), "the password must not have changed");
        assert!(app.db.session_valid(&sha256(&live)), "a rejected change must not log anyone out");
    }

    /// Housekeeping clamps whatever it finds, so a value it cannot parse is not
    /// an error anywhere downstream -- it just silently means 30 days, in a box
    /// that goes on displaying what was typed.
    #[tokio::test]
    async fn a_retention_window_that_would_never_apply_is_refused() {
        let app = std::sync::Arc::new(app());
        let put =
            |v: &str| save_settings(Admin, State(app.clone()), Json(json!({"retention_days": v.to_owned()})));
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

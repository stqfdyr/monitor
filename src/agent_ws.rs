//! The agent side of the hub: one WebSocket per node carrying JSON-RPC 2.0
//! notifications. A single long-lived connection on which either end may speak
//! first, with self-describing frames readable via curl or a browser console.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::auth::client_ip;
use crate::{App, Shared};

/// How often a quiet agent is probed, and how long the hub waits for any frame
/// before abandoning the connection.
const HEARTBEAT: Duration = Duration::from_secs(30);
const SILENCE: Duration = Duration::from_secs(120);

/// Distinguishes one agent session on a node from the next. A connection can
/// remain nominally open for up to SILENCE, long enough for the agent to have
/// given up and reconnected; without this tag a late teardown would remove the
/// live session that replaced it.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// One connected agent. Held in memory only, and rebuilt within one report
/// interval of a hub restart.
///
/// A single map, because "the node is online" and "the node has current figures"
/// are the same fact. Split across two, they required manual synchronisation at
/// every call site and diverged: the connection was recorded at the handshake
/// and the metrics at the first report, so a node that had connected but not yet
/// reported appeared offline for a whole `--interval`.
#[derive(Debug)]
pub struct Agent {
    /// Distinguishes one session on a node from the next; see [`release`].
    pub session: u64,
    /// Outbound channel, used to push probe assignments.
    pub tx: mpsc::Sender<String>,
    /// The latest report, or `Null` between connecting and the first one.
    pub metrics: serde_json::Value,
    pub last_seen: i64,
    /// Wall-clock minute this session has already accounted for. A history row
    /// is written when a report arrives past it.
    pub last_minute: i64,
    /// `(monotonic instant, total_rx, total_tx)` as of the last history row, so
    /// the next one carries the average rate over the interval. Without it a row
    /// would hold a single instantaneous reading -- a 1-in-60 sample of the
    /// minute it describes. See [`report`].
    ///
    /// An `Instant` rather than the wall clock the stamp comes from, because this
    /// is a duration. NTP stepping the clock backwards -- a fresh boot correcting
    /// itself, a restored snapshot -- makes a wall-clock difference negative, and
    /// the `.max(1)` guarding the division would then divide a whole minute of
    /// bytes by one second. The agent computes the same quantity against
    /// `std::time::Instant` for the same reason.
    pub mark: Option<(Instant, i64, i64)>,
    /// Running mean of the minute in progress, for the same reason.
    minute: Minute,
}

impl Agent {
    pub fn new(session: u64, tx: mpsc::Sender<String>) -> Self {
        Self {
            session,
            tx,
            metrics: serde_json::Value::Null,
            last_seen: 0,
            // The minute in progress rather than zero. Its row is already on
            // disk, written by the session this one replaces from the mean of a
            // whole minute; a reconnect's first report would otherwise overwrite
            // it with the single sample that opened the new session.
            last_minute: Utc::now().timestamp() / 60,
            mark: None,
            minute: Minute::default(),
        }
    }
}

/// Fields a history row carries as the mean of its minute rather than the single
/// reading that landed on the boundary. A 30-second spike between two samples is
/// real load that a point sample would report as idle.
///
/// `load` is absent because no history row carries it: it is a live figure read
/// from the report. `net_rx` and `net_tx` are absent because [`report`] fills
/// them from the accumulator, which is exact.
const MEAN_FLOAT: [&str; 1] = ["cpu"];
const MEAN_INT: [&str; 6] = ["mem_used", "swap_used", "disk_used", "tcp", "udp", "procs"];

/// Running sums for the minute in progress, one slot per averaged field.
#[derive(Debug, Default)]
struct Minute {
    sums: [f64; MEAN_FLOAT.len() + MEAN_INT.len()],
    reports: f64,
}

impl Minute {
    fn add(&mut self, metrics: &serde_json::Value) {
        for (slot, key) in MEAN_FLOAT.iter().chain(&MEAN_INT).enumerate() {
            self.sums[slot] += metrics.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
        }
        self.reports += 1.0;
    }

    /// Replaces each averaged field with the mean of the reports folded in so
    /// far, keeping integers integral: `insert_metric` reads them with `as_i64`,
    /// which returns nothing for a value carrying a fraction.
    fn write_into(&self, row: &mut serde_json::Value) {
        let Some(obj) = row.as_object_mut() else { return };
        if self.reports == 0.0 {
            return;
        }
        for (slot, key) in MEAN_FLOAT.iter().chain(&MEAN_INT).enumerate() {
            if !obj.contains_key(*key) {
                continue;
            }
            let mean = self.sums[slot] / self.reports;
            let mean = if slot < MEAN_FLOAT.len() { json!(mean) } else { json!(mean.round() as i64) };
            obj.insert((*key).to_owned(), mean);
        }
    }
}

#[derive(Deserialize)]
struct Rpc {
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

pub async fn handler(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing token").into_response();
    };
    let Ok(Some(node_id)) = app.db.node_by_token(token) else {
        // The same response whether the token is malformed or merely unknown.
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    };
    let ip = client_ip(&headers, peer.ip()).to_string();

    upgrade.read_buffer_size(crate::api::SOCKET_BUFFER).max_message_size(crate::api::MAX_FRAME).on_upgrade(
        move |socket| async move {
            if let Err(e) = serve(app, node_id, ip, socket).await {
                debug!("node {node_id} disconnected: {e:#}");
            }
        },
    )
}

/// Extracts the node token from `Authorization: Bearer <token>`.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").filter(|t| !t.is_empty())
}

async fn serve(app: Shared, node_id: i64, ip: String, mut socket: WebSocket) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<String>(16);
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    // Online from the handshake rather than the first report: a panel reporting
    // otherwise for a whole interval would describe the hub's bookkeeping rather
    // than the machine.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).insert(node_id, Agent::new(session, tx));
    info!("node {node_id} connected from {ip}");

    // Send the probe list before the first report arrives.
    let _ = socket.send(Message::Text(ping_tasks_message(&app, node_id).into())).await;

    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await; // The first tick completes immediately.
    let mut last_frame = Instant::now();

    let outcome = loop {
        tokio::select! {
            outbound = rx.recv() => match outbound {
                Some(text) => socket.send(Message::Text(text.into())).await?,
                None => break Ok(()),
            },
            // A machine that leaves the network without closing its socket would
            // leave this receive pending until the kernel abandons the TCP session
            // hours later, with the node reading online and its metrics frozen. A
            // ping every HEARTBEAT proves the path in both directions; any frame
            // in return, the pong included, counts as a sign of life.
            _ = heartbeat.tick() => {
                let quiet = last_frame.elapsed();
                if quiet > SILENCE {
                    break Err(anyhow::anyhow!("silent for {}s", quiet.as_secs()));
                }
                socket.send(Message::Ping(Vec::new().into())).await?;
            }
            inbound = socket.recv() => {
                last_frame = Instant::now();
                match inbound {
                // Every report contends for the single database connection, which
                // a restore or vacuum can hold for seconds. Without this, agents
                // would park every worker thread on that lock and starve the rest
                // of the runtime -- the panel, the public page, the shutdown
                // signal.
                Some(Ok(Message::Text(text))) =>
                    match tokio::task::block_in_place(|| dispatch(&app, node_id, &ip, &text)) {
                    Ok(true) => locate(app.clone(), node_id, ip.clone()),
                    Ok(false) => {}
                    Err(e) => warn!("node {node_id} sent an unusable message: {e:#}"),
                },
                Some(Ok(Message::Close(_))) | None => break Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                }
            }
        }
    };

    if release(&app, node_id, session) {
        info!("node {node_id} went offline");
    }
    outcome
}

/// Drops a node's connection state, but only while `session` is still the one
/// holding it. Returns whether anything was released.
///
/// A teardown can arrive up to SILENCE after the agent gave up, by which time a
/// reconnect may have installed a newer session under the same node id; clearing
/// that one would mark a node offline while it is reporting normally.
fn release(app: &App, node_id: i64, session: u64) -> bool {
    let mut agents = app.agents.write().unwrap_or_else(|e| e.into_inner());
    if !agents.get(&node_id).is_some_and(|a| a.session == session) {
        return false;
    }
    agents.remove(&node_id);
    true
}

/// Handles one inbound frame and reports whether the node is now owed a country
/// lookup. The lookup itself is an outbound request and happens off this path;
/// see `locate`.
fn dispatch(app: &App, node_id: i64, ip: &str, text: &str) -> Result<bool> {
    let rpc: Rpc = serde_json::from_str(text)?;
    match rpc.method.as_str() {
        "hello" => return app.db.save_facts(node_id, &rpc.params, ip),
        "report" => report(app, node_id, rpc.params)?,
        "ping.result" => {
            let task_id = rpc.params.get("task_id").and_then(|v| v.as_i64()).unwrap_or(0);
            // A missing reading is not a reading of -1: `close_bucket` counts
            // every negative latency as a lost packet, so defaulting here would
            // render a malformed frame as an outage. The accumulator follows the
            // same rule for a counter it cannot read.
            let latency = rpc.params.get("latency_ms").and_then(|v| v.as_i64());
            if let (true, Some(latency)) = (task_id > 0, latency) {
                app.db.insert_ping(node_id, task_id, Utc::now().timestamp(), latency)?;
            }
        }
        other => debug!("node {node_id} sent unknown method {other}"),
    }
    Ok(false)
}

/// When each node was last looked up.
///
/// A failed lookup leaves the country column empty, so `save_facts` continues to
/// report the node as owed one; without this gate an agent reconnecting every few
/// seconds -- a poor link, or two machines sharing a token -- would issue one
/// outbound request per reconnect indefinitely. Keying on the address cannot
/// cover the second case: the two machines connect from different addresses, so
/// every reconnect reads as a new question and the gate never closes. Only the
/// time is recorded. The cost is that a node genuinely changing address within
/// the hour waits for its next hello to acquire a badge, and an empty column is
/// already a permitted state.
static ASKED: OnceLock<Mutex<HashMap<i64, Instant>>> = OnceLock::new();
const LOCATE_RETRY: Duration = Duration::from_secs(3_600);

/// Resolves the address a node connects from to a country, at most once per hour
/// per node.
///
/// The answer comes from a third party and appears on the public page, so only
/// two ASCII letters are ever stored. Anything else -- a private address where
/// agent and hub share a network, an outage, a rate limit -- leaves the column
/// empty and the badge hidden.
///
/// ponytail: no backoff beyond that one window, and the record is per process. A
/// hub restart repeats the lookup once per node.
fn locate(app: Shared, node_id: i64, ip: String) {
    let mut asked = ASKED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if asked.get(&node_id).is_some_and(|at| at.elapsed() < LOCATE_RETRY) {
        return;
    }
    asked.insert(node_id, Instant::now());
    drop(asked);

    tokio::spawn(async move {
        let lookup = async {
            let url = format!("https://ipinfo.io/{ip}/country");
            anyhow::Ok(app.http.get(url).send().await?.error_for_status()?.text().await?)
        };
        let cc = match lookup.await {
            Ok(body) => body.trim().to_ascii_uppercase(),
            Err(e) => return debug!("node {node_id}: no country for {ip}: {e:#}"),
        };
        if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_uppercase()) {
            return debug!("node {node_id}: {ip} resolved to no country");
        }
        if let Err(e) = app.db.set_country(node_id, &cc, &ip) {
            warn!("node {node_id}: storing country {cc} failed: {e:#}");
        }
    });
}

/// Figures the hub folds into a report on the way out. They never arrive from an
/// agent and are therefore not part of the contract one must meet.
const INJECTED: [&str; 4] = ["total_rx", "total_tx", "month_rx", "month_tx"];

/// Everything an agent must send, derived from the public view rather than
/// restated a third time: this list, `api::PUBLIC_METRICS` and the check below
/// must agree, and only one of them is an independent fact.
///
/// The measure is what the hub depends on, not what it stores. `uptime`,
/// `mem_total`, `swap_total` and `disk_total` never reach the `metric` table but
/// go straight to the browser, and the default theme blanks a node's entire live
/// view when one is absent. Derived from the stored columns instead, this list
/// left those four uncovered, so an agent renaming one blanked every card on the
/// page with nothing in any log to explain it.
///
/// Hub and agent ship as two binaries from two repositories, and every reader
/// here ends in `unwrap_or(0)`: a field the agent renames does not fail, it
/// records zero until someone examines that chart.
fn report_fields() -> impl Iterator<Item = &'static str> {
    ["boot_id", "net_rx_total", "net_tx_total"]
        .into_iter()
        .chain(crate::api::PUBLIC_METRICS.iter().copied().filter(|k| !INJECTED.contains(k)))
}

/// Those carrying a plain number. `boot_id` is a string and `load` an array of
/// three; each is checked separately.
fn numeric_fields() -> impl Iterator<Item = &'static str> {
    report_fields().filter(|k| !matches!(*k, "boot_id" | "load"))
}

/// Reports, once per connection, when a report omits fields the hub depends on.
/// A version number cannot serve here: an agent that renames a field carries a
/// higher version, not a lower one.
fn check_contract(node_id: i64, metrics: &serde_json::Value) {
    let missing: Vec<&str> = report_fields().filter(|k| metrics.get(k).is_none()).collect();
    if !missing.is_empty() {
        warn!("node {node_id} reports without {missing:?}: those columns will read zero and the default theme will void this node's live view, so this agent and this hub are out of step");
    }
}

fn report(app: &App, node_id: i64, mut metrics: serde_json::Value) -> Result<()> {
    // Missing fields remain compatible with older agents, while malformed values
    // must not become a live frame that can crash a browser. Counter validation
    // is separate: a missing or null kernel reading must not alter its
    // baseline.
    let number = |v: &serde_json::Value| v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.0);
    anyhow::ensure!(metrics.is_object(), "report must be an object");
    for key in numeric_fields() {
        anyhow::ensure!(metrics.get(key).is_none_or(number), "invalid report field {key}");
    }
    if let Some(load) = metrics.get("load") {
        anyhow::ensure!(
            load.as_array().is_some_and(|v| v.len() == 3 && v.iter().all(number)),
            "invalid load"
        );
    }
    let now = Utc::now().timestamp();
    // Read once, alongside the wall clock: the stamp is a point in time taken
    // from `now`, while the rate below is a duration taken from this.
    let tick = Instant::now();
    // A placeholder rather than the empty string, which `accumulate` reads as
    // the absence of a baseline. An agent sending no boot_id -- an older build,
    // or a host without the file -- would otherwise realign on every report and
    // never book a byte.
    let boot_id = metrics.get("boot_id").and_then(|v| v.as_str()).filter(|b| !b.is_empty()).unwrap_or("-");
    // No reading is not a reading of zero; see `accumulate`. Anything that is
    // not a non-negative i64 is likewise no reading -- a u64 beyond the signed
    // range, a float, or a negative value. Negatives are rejected above and must
    // not survive here either: `accumulate` stores whatever it receives as the
    // next baseline, and a negative baseline would make the following report's
    // delta the counter plus its magnitude.
    let counter = |k: &str| metrics.get(k).and_then(|v| v.as_i64()).filter(|n| *n >= 0);
    let counters = counter("net_rx_total").zip(counter("net_tx_total"));
    let traffic = app.db.accumulate(node_id, boot_id, counters)?;

    // The UI displays the hub's accumulated figures, so they are folded into the
    // live payload while the raw kernel counters remain a wire-protocol detail.
    if let Some(obj) = metrics.as_object_mut() {
        obj.insert("total_rx".into(), json!(traffic.total_rx));
        obj.insert("total_tx".into(), json!(traffic.total_tx));
        obj.insert("month_rx".into(), json!(traffic.month_rx));
        obj.insert("month_tx".into(), json!(traffic.month_tx));
    }

    let minute = now / 60;
    let mut agents = app.agents.write().unwrap_or_else(|e| e.into_inner());
    // Absence means the session was retired mid-flight: the panel rotated the
    // token, or the socket is unwinding. The bytes above remain booked; there is
    // simply no longer a session to attribute them to.
    let Some(entry) = agents.get_mut(&node_id) else { return Ok(()) };
    let first = entry.last_seen == 0;
    if first {
        check_contract(node_id, &metrics);
    }
    // History holds one row per minute; the live view receives every report.
    let store = entry.last_minute != minute;
    entry.metrics = metrics.clone();
    entry.last_seen = now;
    entry.minute.add(&metrics);

    // The stored row summarises the interval since the previous row rather than
    // the instant it is stamped with: the network rate from the totals this hub
    // observed climb, every other averaged field from the mean of the reports in
    // between. This is what makes the chart integrate to the totals beside it.
    // The live view retains the report as it arrived.
    let row = store.then(|| {
        let mut row = metrics.clone();
        entry.minute.write_into(&mut row);
        if let (Some((since, rx0, tx0)), Some(obj)) = (entry.mark, row.as_object_mut()) {
            let elapsed = tick.saturating_duration_since(since).as_secs().max(1) as i64;
            obj.insert("net_rx".into(), json!((traffic.total_rx - rx0).max(0) / elapsed));
            obj.insert("net_tx".into(), json!((traffic.total_tx - tx0).max(0) / elapsed));
        }
        entry.last_minute = minute;
        entry.mark = Some((tick, traffic.total_rx, traffic.total_tx));
        entry.minute = Minute::default();
        row
    });
    // A session that has just started measures the next row's rate from its own
    // first report; without a mark the row would carry the agent's instantaneous
    // reading rather than the average over the interval.
    entry.mark.get_or_insert((tick, traffic.total_rx, traffic.total_tx));
    drop(agents);

    if let Some(row) = &row {
        app.db.insert_metric(node_id, minute * 60, row)?;
    }
    // "Offline since" is read from this column, so a session ending before its
    // first minute boundary must still leave a mark.
    if row.is_some() || first {
        app.db.touch_seen(node_id, now)?;
    }
    Ok(())
}

fn ping_tasks_message(app: &App, node_id: i64) -> String {
    let tasks = app.db.ping_tasks_for(node_id).unwrap_or_default();
    json!({"jsonrpc": "2.0", "method": "ping.tasks", "params": tasks}).to_string()
}

/// Pushes the current probe list to every connected agent, so a panel edit takes
/// effect without waiting for a reconnect.
pub fn push_ping_tasks(app: &App) {
    let connected: Vec<(i64, mpsc::Sender<String>)> = app
        .agents
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, agent)| (*id, agent.tx.clone()))
        .collect();
    for (node_id, sender) in connected {
        // The queue carries only these messages, so a full one indicates an agent
        // that has stopped reading its socket. It is dropped within SILENCE and
        // reconnects onto the current list; what must not happen is the panel
        // reporting a push that never occurred.
        if sender.try_send(ping_tasks_message(app, node_id)).is_err() {
            warn!("node {node_id} is not draining its queue; it gets the new probe list when it reconnects");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Node, PingTask};

    fn app() -> App {
        App::for_test(Db::open(":memory:").unwrap())
    }

    fn node(app: &App) -> i64 {
        app.db
            .create_node(&Node { name: "n".into(), traffic_reset_day: 1, ..Default::default() }, "tok")
            .unwrap()
    }

    /// A connected agent, the precondition for filing any report: the session
    /// holds the node's live state.
    fn connect(app: &App) -> (i64, mpsc::Receiver<String>) {
        let id = node(app);
        let (tx, rx) = mpsc::channel(4);
        app.agents.write().unwrap().insert(id, Agent::new(1, tx));
        (id, rx)
    }

    fn report_json(boot: &str, rx: i64, tx: i64) -> String {
        json!({
            "jsonrpc": "2.0", "method": "report",
            "params": {"boot_id": boot, "cpu": 12.5, "load": [0.5, 0.4, 0.3],
                       "mem_used": 100, "net_rx_total": rx, "net_tx_total": tx}
        })
        .to_string()
    }

    #[test]
    fn malformed_reports_leave_the_last_good_frame_and_counters_untouched() {
        let app = app();
        let (id, _held) = connect(&app);
        dispatch(&app, id, "ip", &report_json("boot", 1_000, 500)).unwrap();
        let good = app.agents.read().unwrap()[&id].metrics.clone();
        for bad in [json!({"load":null}), json!({"load":[1,"bad",3]}), json!({"cpu":"bad"}), json!([])] {
            assert!(report(&app, id, bad).is_err());
            assert_eq!(app.agents.read().unwrap()[&id].metrics, good);
        }
        dispatch(&app, id, "ip", &report_json("boot", 2_000, 600)).unwrap();
        assert_eq!(app.db.all_traffic()[&id].total_rx, 1_000);
    }

    /// The lifetime total must never decrease, and must never book bytes nobody
    /// moved. The two figures behind it arrive from another repository's binary
    /// and are the only report fields that mutate state outliving the
    /// connection.
    #[test]
    fn a_hostile_counter_can_neither_inflate_the_total_nor_wrap_it() {
        let app = app();
        let (id, _held) = connect(&app);
        let total = || app.db.all_traffic()[&id].total_rx;

        // Both counters, always: `report` pairs them, so omitting one makes the
        // pair unreadable and every assertion below pass for that reason rather
        // than the one under test.
        let send = |boot: &str, rx: serde_json::Value| {
            report(&app, id, json!({"boot_id": boot, "net_rx_total": rx, "net_tx_total": 0}))
        };

        // A negative reading is rejected and, critically, does not survive as the
        // baseline the next report subtracts from, which would make that report's
        // delta its own value plus 5 GB.
        assert!(send("b", json!(-5_000_000_000i64)).is_err());
        send("b", json!(1_000)).unwrap();
        assert_eq!(total(), 0, "a node that moved nothing books nothing");

        // Nor does a u64 beyond the signed range, which `as_i64` cannot read: no
        // reading, so the baseline is unchanged.
        send("b", json!(u64::MAX)).unwrap();
        send("b", json!(2_000)).unwrap();
        assert_eq!(total(), 1_000, "only the 1 000 bytes this hub watched climb");

        // The total saturates rather than wrapping. A plain `+=` would wrap to
        // i64::MIN in release builds, where overflow checks are disabled,
        // producing a lifetime figure that has decreased.
        app.db
            .set_traffic(id, &crate::db::TrafficPatch { total_rx: Some(i64::MAX - 10), ..Default::default() })
            .unwrap();
        send("c", json!(0)).unwrap();
        send("c", json!(i64::MAX)).unwrap();
        assert_eq!(total(), i64::MAX, "the total clamps; it never goes backwards");
    }

    /// The contract check is what makes a cross-repository rename visible.
    /// Derived from the columns the hub stores, it missed four fields that never
    /// reach the `metric` table but do reach the browser; the default theme
    /// blanks a node's entire live view if one is absent, so the drift surfaced
    /// as empty cards and no log output.
    #[test]
    fn the_contract_covers_every_field_the_browser_needs_not_just_the_stored_ones() {
        let fields: Vec<&str> = report_fields().collect();
        for needed in ["uptime", "mem_total", "swap_total", "disk_total"] {
            assert!(fields.contains(&needed), "{needed} reaches the theme, so a rename has to warn");
        }
        // boot_id and the two kernel counters extend the contract beyond the
        // public view; the four the hub folds in are not the agent's
        // responsibility.
        for injected in INJECTED {
            assert!(!fields.contains(&injected), "{injected} is the hub's own, not part of the contract");
        }
        assert!(fields.contains(&"boot_id") && fields.contains(&"net_rx_total"));
        // The numeric list is the same list minus the two that are not plain
        // numbers, so neither can drift from the other.
        let numeric: Vec<&str> = numeric_fields().collect();
        assert_eq!(numeric.len(), fields.len() - 2);
        assert!(!numeric.contains(&"load") && !numeric.contains(&"boot_id"));
    }

    /// A burst of reports within one minute: each advances the live view and the
    /// running totals, while history takes one row on the minute boundary.
    #[test]
    fn a_burst_of_reports_moves_the_live_view_but_writes_one_history_row() {
        let app = app();
        let (id, _held) = connect(&app);
        let minute = Utc::now().timestamp() / 60 * 60;
        // A session already running when this minute opened: the first report of
        // a new one lands within a minute already accounted for, which is the
        // reconnect case below.
        app.agents.write().unwrap().get_mut(&id).unwrap().last_minute -= 1;

        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 1_000, 500)).unwrap();
        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 3_000, 1_500)).unwrap();

        let live = app.agents.read().unwrap();
        let entry = live.get(&id).unwrap();
        assert_eq!(entry.metrics["cpu"], 12.5);
        // The first report establishes the baseline, so only the second counts.
        assert_eq!(entry.metrics["total_rx"], 2_000);
        assert_eq!(entry.metrics["total_tx"], 1_000);
        assert_eq!(entry.metrics["month_rx"], 2_000);
        assert_eq!(entry.last_minute, minute / 60, "the minute already written is remembered");
        drop(live);

        // History rows are keyed by (node, ts), so counting them proves nothing on
        // its own: reports a second apart collapse onto one row with or without
        // the minute gate. The stamp is what demonstrates it.
        let rows = app.db.metrics(id, 0, 60).unwrap();
        assert_eq!(rows.len(), 1, "a minute of reports is one row");
        assert_eq!(rows[0]["ts"], minute, "stamped on the minute, not on the report");
        // Written on the same branch, and the offline badge is measured from it.
        assert!(app.db.node(id).unwrap().unwrap().last_seen >= minute, "last_seen is written too");
    }

    /// A history row describes the minute preceding it rather than the instant it
    /// is stamped with: the network rate from the totals the hub observed climb,
    /// everything else from the mean of the reports in between.
    #[test]
    fn a_history_row_describes_its_whole_minute_not_one_instant() {
        let app = app();
        let (id, _held) = connect(&app);
        let burst = |rx: i64, instant: i64, cpu: f64, mem: i64| {
            json!({"jsonrpc": "2.0", "method": "report",
                   "params": {"boot_id": "boot-a", "net_rx_total": rx, "net_tx_total": 0,
                              // What the agent measured over its own last second.
                              "net_rx": instant, "net_tx": 0, "cpu": cpu, "mem_used": mem}})
            .to_string()
        };

        // Busy for half the minute, then idle. The first reading is also the
        // traffic baseline: nothing is booked until a second arrives.
        dispatch(&app, id, "ip", &burst(1_000, 0, 100.0, 100)).unwrap();
        // Rewind the bookkeeping by a minute so the next report crosses the
        // boundary with a minute of elapsed time behind it. The mark is an
        // `Instant` precisely because a wall-clock difference can be negative when
        // NTP steps the clock; reverting the field to a timestamp fails to
        // compile.
        {
            let mut agents = app.agents.write().unwrap();
            let entry = agents.get_mut(&id).unwrap();
            entry.last_minute -= 1;
            entry.mark = Some((Instant::now() - Duration::from_secs(60), 0, 0));
        }
        // 60 MB arrived and the machine was busy for half the minute; by the next
        // sample both have ended.
        dispatch(&app, id, "ip", &burst(1_000 + 60_000_000, 0, 0.0, 201)).unwrap();

        let row = &app.db.metrics(id, 0, 60).unwrap()[0];
        assert_eq!(row["net_rx"], 1_000_000, "60 MB over 60 s is 1 MB/s, not the agent's 0");
        assert_eq!(row["cpu"], 50.0, "the mean of the minute, not the idle second it ended on");
        // Integers remain integral: the column is read with as_i64, which returns
        // nothing for the 150.5 the raw mean would produce.
        assert_eq!(row["mem_used"], 151);
        // The live view still shows the instantaneous reading, which is its
        // purpose.
        assert_eq!(app.agents.read().unwrap()[&id].metrics["net_rx"], 0);
    }

    /// A reconnect arrives mid-minute, and that minute's row already holds the
    /// mean of the preceding session. Replacing it with the single sample that
    /// opened the new session would stop the chart integrating to the totals
    /// printed beside it.
    #[test]
    fn a_reconnect_leaves_the_minute_it_lands_in_alone() {
        let app = app();
        let (id, _held) = connect(&app);
        app.agents.write().unwrap().get_mut(&id).unwrap().last_minute -= 1;
        dispatch(&app, id, "ip", &report_json("boot-a", 1_000, 500)).unwrap();
        let before = app.db.metrics(id, 0, 60).unwrap();
        assert_eq!(before.len(), 1, "the running session wrote the row for this minute");

        // The socket drops and the agent returns within the same minute.
        let (tx, _rx) = mpsc::channel(4);
        app.agents.write().unwrap().insert(id, Agent::new(2, tx));
        let loud = json!({"jsonrpc": "2.0", "method": "report",
                          "params": {"boot_id": "boot-a", "cpu": 99.0, "net_rx_total": 9_000,
                                     "net_tx_total": 4_500}})
        .to_string();
        dispatch(&app, id, "ip", &loud).unwrap();

        assert_eq!(app.db.metrics(id, 0, 60).unwrap(), before, "the row keeps the minute it described");
        // The bytes are still booked; only the history row is left untouched.
        assert_eq!(app.agents.read().unwrap()[&id].metrics["total_rx"], 8_000);
    }

    /// An agent sending no boot_id -- an older build, or a host without the file
    /// -- still has its traffic accumulated. Reading the empty string as the
    /// absence of a baseline would realign on every report and book nothing
    /// indefinitely, with no outward sign.
    #[test]
    fn traffic_accumulates_for_an_agent_that_sends_no_boot_id() {
        let app = app();
        let (id, _held) = connect(&app);
        let report = |rx: i64| {
            json!({"jsonrpc": "2.0", "method": "report",
                   "params": {"cpu": 1.0, "net_rx_total": rx, "net_tx_total": 0}})
            .to_string()
        };
        dispatch(&app, id, "ip", &report(1_000)).unwrap();
        dispatch(&app, id, "ip", &report(3_000)).unwrap();
        assert_eq!(app.agents.read().unwrap()[&id].metrics["total_rx"], 2_000);

        // A report with no counters books nothing and, crucially, leaves the
        // baseline unchanged so the next one is a delta.
        let blind = json!({"jsonrpc": "2.0", "method": "report", "params": {"cpu": 1.0}}).to_string();
        dispatch(&app, id, "ip", &blind).unwrap();
        dispatch(&app, id, "ip", &report(4_000)).unwrap();
        assert_eq!(
            app.agents.read().unwrap()[&id].metrics["total_rx"],
            3_000,
            "a missing reading must not re-baseline the counter to zero"
        );
    }

    #[test]
    fn hello_stores_the_facts_and_the_observed_address() {
        let app = app();
        let id = node(&app);
        let hello = json!({
            "jsonrpc": "2.0", "method": "hello",
            "params": {"hostname": "vps-1", "os": "Debian 12", "cpu_cores": 4, "mem_total": 2048}
        });
        dispatch(&app, id, "198.51.100.4", &hello.to_string()).unwrap();

        let n = app.db.node(id).unwrap().unwrap();
        assert_eq!(n.hostname, "vps-1");
        assert_eq!(n.cpu_cores, 4);
        assert_eq!(n.ip, "198.51.100.4");
    }

    #[test]
    fn ping_results_are_recorded_and_bad_ones_ignored() {
        let app = app();
        let id = node(&app);
        // Assigned probes: a result is readable only through a node's current
        // assignments.
        let probe = |name: &str| {
            app.db
                .save_ping_task(&PingTask {
                    id: 0,
                    name: name.into(),
                    target: "1.1.1.1:443".into(),
                    interval: 60,
                    nodes: vec![id],
                })
                .unwrap()
        };
        let (one, two) = (probe("one"), probe("two"));
        let result = |task, latency| {
            json!({"jsonrpc": "2.0", "method": "ping.result",
                   "params": {"task_id": task, "latency_ms": latency}})
            .to_string()
        };
        dispatch(&app, id, "ip", &result(one, 42)).unwrap();
        // The rejected results carry task ids of their own: a bare count would be
        // satisfied by the key collapsing them onto a valid row.
        dispatch(&app, id, "ip", &result(two, 15)).unwrap();
        dispatch(&app, id, "ip", &result(0, 42)).unwrap(); // no such task
        dispatch(&app, id, "ip", &result(-1, 42)).unwrap(); // nor this one
                                                            // A frame carrying no reading. Defaulting to -1 would file it as a lost
                                                            // packet, rendering a malformed frame as an outage.
        dispatch(
            &app,
            id,
            "ip",
            &json!({"jsonrpc": "2.0", "method": "ping.result",
                                         "params": {"task_id": one}})
            .to_string(),
        )
        .unwrap();

        // Sorted rather than indexed: both rows land in the same second and the
        // query orders by timestamp.
        let mut seen: Vec<(i64, i64)> = app
            .db
            .ping_records(id, 0, 60)
            .unwrap()
            .0
            .iter()
            .map(|r| (r["task_id"].as_i64().unwrap(), r["latency"].as_i64().unwrap()))
            .collect();
        seen.sort();
        assert_eq!(seen, vec![(one, 42), (two, 15)], "each real task keeps its own result, and only those");
    }

    #[test]
    fn the_token_is_read_from_the_authorization_header_only() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None, "no header means no token");
        h.insert("authorization", "Bearer abc123".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc123"));
        h.insert("authorization", "abc123".parse().unwrap());
        assert_eq!(bearer(&h), None, "a bare value is not a bearer token");
        h.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(bearer(&h), None, "an empty token is not accepted");
    }

    #[test]
    fn a_late_teardown_leaves_the_reconnected_session_alone() {
        let app = app();
        let id = node(&app);
        let live = || app.agents.read().unwrap().contains_key(&id);
        // release() reads the session tag rather than the channel, so a dropped
        // receiver changes nothing.
        let connect = |session| {
            let (tx, _) = mpsc::channel(1);
            app.agents.write().unwrap().insert(id, Agent::new(session, tx));
        };

        // The ordinary case: the session ending is the one on record.
        connect(1);
        assert!(release(&app, id, 1));
        assert!(!live(), "its own teardown clears the node");

        // The race: the agent gave up and reconnected while the old socket was
        // half-open, so session 2 is live when session 1 unwinds.
        connect(1);
        connect(2);
        assert!(!release(&app, id, 1), "a stale session must release nothing");
        assert!(live(), "the reconnected agent stays online");
        assert!(app.agents.read().unwrap().contains_key(&id), "and keeps receiving probe pushes");
    }

    #[test]
    fn junk_from_an_agent_is_rejected_without_taking_the_connection_down() {
        let app = app();
        let id = node(&app);
        assert!(dispatch(&app, id, "ip", "not json").is_err());
        // Unknown methods are ignored.
        assert!(dispatch(&app, id, "ip", r#"{"method":"whatever"}"#).is_ok());
    }
}

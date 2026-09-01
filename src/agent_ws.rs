//! The agent side of the hub: one WebSocket per node carrying JSON-RPC 2.0
//! notifications. One long-lived connection either end can speak first on, and
//! frames that name their own method, readable with curl or a browser console.

use std::sync::atomic::{AtomicU64, Ordering};
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

/// How often a quiet agent is poked, and how long the hub waits for any frame
/// at all before it gives up on the connection.
const HEARTBEAT: Duration = Duration::from_secs(30);
const SILENCE: Duration = Duration::from_secs(120);

/// Tells one agent session on a node apart from the next. A connection can
/// outlive its usefulness by up to SILENCE, long enough for the agent to have
/// given up and reconnected; without this tag a late teardown would remove the
/// live session that replaced it.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// One connected agent. In memory only: it is rebuilt within a report interval
/// of a hub restart.
///
/// One map, because "the node is online" and "the node has current figures" are
/// the same fact. Split across two they had to be kept in step by hand at every
/// site touching either, and they disagreed: the connection went in at the
/// handshake and the metrics at the first report, so a node that had connected
/// and not yet reported read as offline for a whole `--interval`.
#[derive(Debug)]
pub struct Agent {
    /// Tells one session on a node apart from the next; see [`release`].
    pub session: u64,
    /// Outbound channel, used to push probe assignments.
    pub tx: mpsc::Sender<String>,
    /// The latest report, or `Null` between connecting and the first one.
    pub metrics: serde_json::Value,
    pub last_seen: i64,
    /// Wall-clock minute of the last row written to `metric`.
    pub last_minute: i64,
    /// `(unix seconds, total_rx, total_tx)` as they stood at the last history
    /// row, so the next one carries the average rate over the gap. Without it
    /// the row held one instantaneous reading -- a 1-in-60 sample of the minute
    /// it claims to describe. See [`report`].
    pub mark: Option<(i64, i64, i64)>,
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
            last_minute: 0,
            mark: None,
            minute: Minute::default(),
        }
    }
}

/// Fields a history row carries as the mean of its minute rather than the one
/// reading that landed on the boundary. A 30-second spike between two samples
/// is real load; a point sample says the machine was idle.
///
/// `load1` is absent because the kernel already averages it over the last
/// minute, and averaging again smears it across two. `net_rx` and `net_tx` are
/// absent because [`report`] fills them from the accumulator, which is exact.
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
    /// far, keeping whole numbers whole: `insert_metric` reads those with
    /// `as_i64`, which answers nothing for a value carrying a fraction.
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
        // Same response whether the token is malformed or simply unknown.
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
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").filter(|t| !t.is_empty())
}

async fn serve(app: Shared, node_id: i64, ip: String, mut socket: WebSocket) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<String>(16);
    let session = SESSION.fetch_add(1, Ordering::Relaxed);
    // Online from the handshake, not from the first report: a panel that says
    // otherwise for a whole report interval is describing the hub's
    // bookkeeping rather than the machine.
    app.agents.write().unwrap_or_else(|e| e.into_inner()).insert(node_id, Agent::new(session, tx));
    info!("node {node_id} connected from {ip}");

    // Tell the agent what to probe before the first report arrives.
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
            // A machine that drops off the network without closing its socket
            // leaves this receive waiting until the kernel gives up on the TCP
            // session hours later, with the node reading as online and its
            // metrics frozen. A ping every HEARTBEAT proves the path both ways;
            // any frame back, the pong included, counts as a sign of life.
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
                Some(Ok(Message::Text(text))) => {
                    if let Err(e) = dispatch(&app, node_id, &ip, &text) {
                        warn!("node {node_id} sent an unusable message: {e:#}");
                    }
                }
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
/// reconnect may have installed a newer session under the same node id.
/// Clearing that one marks a node offline while it is reporting normally.
fn release(app: &App, node_id: i64, session: u64) -> bool {
    let mut agents = app.agents.write().unwrap_or_else(|e| e.into_inner());
    if !agents.get(&node_id).is_some_and(|a| a.session == session) {
        return false;
    }
    agents.remove(&node_id);
    true
}

fn dispatch(app: &App, node_id: i64, ip: &str, text: &str) -> Result<()> {
    let rpc: Rpc = serde_json::from_str(text)?;
    match rpc.method.as_str() {
        "hello" => app.db.save_facts(node_id, &rpc.params, ip)?,
        "report" => report(app, node_id, rpc.params)?,
        "ping.result" => {
            let task_id = rpc.params.get("task_id").and_then(|v| v.as_i64()).unwrap_or(0);
            let latency = rpc.params.get("latency_ms").and_then(|v| v.as_i64()).unwrap_or(-1);
            if task_id > 0 {
                app.db.insert_ping(node_id, task_id, Utc::now().timestamp(), latency)?;
            }
        }
        other => debug!("node {node_id} sent unknown method {other}"),
    }
    Ok(())
}

/// Everything a report has to carry for the hub to store a complete row.
///
/// Hub and agent ship as two binaries from two repositories, and every reader
/// here ends in `unwrap_or(0)`: a field the agent renames does not fail, it
/// records zero until somebody looks at that chart. Worth one line in the log.
const REPORT_FIELDS: [&str; 13] = [
    "boot_id",
    "cpu",
    "load",
    "mem_used",
    "swap_used",
    "disk_used",
    "net_rx",
    "net_tx",
    "net_rx_total",
    "net_tx_total",
    "tcp",
    "udp",
    "procs",
];

/// Says so, once per connection, when a report is missing fields the hub
/// stores. A version number cannot do this: the agent that renames a field
/// carries a higher version, not a lower one.
fn check_contract(node_id: i64, metrics: &serde_json::Value) {
    let missing: Vec<&str> = REPORT_FIELDS.iter().copied().filter(|k| metrics.get(k).is_none()).collect();
    if !missing.is_empty() {
        warn!("node {node_id} reports without {missing:?}: those columns will read zero, so this agent and this hub are out of step");
    }
}

fn report(app: &App, node_id: i64, mut metrics: serde_json::Value) -> Result<()> {
    let now = Utc::now().timestamp();
    let boot_id = metrics.get("boot_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let rx = metrics.get("net_rx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let tx = metrics.get("net_tx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let traffic = app.db.accumulate(node_id, &boot_id, rx, tx)?;

    // The UI shows the hub's accumulated figures, so they are folded into the
    // live payload and the raw kernel counters stay a wire-protocol detail.
    if let Some(obj) = metrics.as_object_mut() {
        obj.insert("total_rx".into(), json!(traffic.total_rx));
        obj.insert("total_tx".into(), json!(traffic.total_tx));
        obj.insert("month_rx".into(), json!(traffic.month_rx));
        obj.insert("month_tx".into(), json!(traffic.month_tx));
    }

    let minute = now / 60;
    let mut agents = app.agents.write().unwrap_or_else(|e| e.into_inner());
    // Gone means the session was retired mid-flight: the panel rotated the
    // token, or the socket is unwinding. The bytes above are real and stay
    // booked; there is no longer a session to file them under.
    let Some(entry) = agents.get_mut(&node_id) else { return Ok(()) };
    if entry.last_seen == 0 {
        check_contract(node_id, &metrics);
    }
    // History is one row per minute; the live view gets every report.
    let store = entry.last_minute != minute;
    entry.metrics = metrics.clone();
    entry.last_seen = now;
    entry.minute.add(&metrics);

    // The stored row summarises the interval since the previous row, not the
    // instant it is stamped on: the network rate comes from the totals this hub
    // watched climb, every other averaged field from the mean of the reports in
    // between. That is what makes the chart integrate to the totals beside it.
    // The live view keeps the report as it arrived.
    let row = store.then(|| {
        let mut row = metrics.clone();
        entry.minute.write_into(&mut row);
        if let (Some((since, rx0, tx0)), Some(obj)) = (entry.mark, row.as_object_mut()) {
            let elapsed = (now - since).max(1);
            obj.insert("net_rx".into(), json!((traffic.total_rx - rx0).max(0) / elapsed));
            obj.insert("net_tx".into(), json!((traffic.total_tx - tx0).max(0) / elapsed));
        }
        entry.last_minute = minute;
        entry.mark = Some((now, traffic.total_rx, traffic.total_tx));
        entry.minute = Minute::default();
        row
    });
    drop(agents);

    if let Some(row) = row {
        app.db.insert_metric(node_id, minute * 60, &row)?;
        app.db.touch_seen(node_id, now)?;
    }
    Ok(())
}

fn ping_tasks_message(app: &App, node_id: i64) -> String {
    let tasks = app.db.ping_tasks_for(node_id).unwrap_or_default();
    json!({"jsonrpc": "2.0", "method": "ping.tasks", "params": tasks}).to_string()
}

/// Pushes the current probe list to every connected agent, so a panel edit
/// takes effect without waiting for a reconnect.
pub fn push_ping_tasks(app: &App) {
    let connected: Vec<(i64, mpsc::Sender<String>)> = app
        .agents
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, agent)| (*id, agent.tx.clone()))
        .collect();
    for (node_id, sender) in connected {
        let _ = sender.try_send(ping_tasks_message(app, node_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Node};

    fn app() -> App {
        App::for_test(Db::open(":memory:").unwrap())
    }

    fn node(app: &App) -> i64 {
        app.db
            .create_node(&Node { name: "n".into(), traffic_reset_day: 1, ..Default::default() }, "tok")
            .unwrap()
    }

    /// A connected agent, which is the precondition for filing a report at
    /// all: the session holds the node's live state.
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

    /// A burst of reports inside one minute: each moves the live view and the
    /// running totals, while history takes one row on the minute boundary.
    #[test]
    fn a_burst_of_reports_moves_the_live_view_but_writes_one_history_row() {
        let app = app();
        let (id, _held) = connect(&app);
        let minute = Utc::now().timestamp() / 60 * 60;

        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 1_000, 500)).unwrap();
        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 3_000, 1_500)).unwrap();

        let live = app.agents.read().unwrap();
        let entry = live.get(&id).unwrap();
        assert_eq!(entry.metrics["cpu"], 12.5);
        // The first report is the baseline, so only the second counts.
        assert_eq!(entry.metrics["total_rx"], 2_000);
        assert_eq!(entry.metrics["total_tx"], 1_000);
        assert_eq!(entry.metrics["month_rx"], 2_000);
        assert_eq!(entry.last_minute, minute / 60, "the minute already written is remembered");
        drop(live);

        // History rows are keyed by (node, ts), so counting them proves
        // nothing on its own: reports a second apart collapse onto one row
        // with or without the minute gate. The stamp is what shows it.
        let rows = app.db.metrics(id, 0, 60).unwrap();
        assert_eq!(rows.len(), 1, "a minute of reports is one row");
        assert_eq!(rows[0]["ts"], minute, "stamped on the minute, not on the report");
        // Written on the same branch, and the offline badge counts from it.
        assert!(app.db.node(id).unwrap().unwrap().last_seen >= minute, "last_seen is written too");
    }

    /// A history row describes the minute behind it, not the instant it is
    /// stamped on: the network rate from the totals the hub watched climb,
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

        // A connection's first report opens a row and clears the running
        // mean, so the minute under test starts after it.
        dispatch(&app, id, "ip", &burst(1_000, 0, 0.0, 0)).unwrap();
        // Busy half the minute, then quiet.
        dispatch(&app, id, "ip", &burst(1_000, 0, 100.0, 100)).unwrap();
        // Rewind the bookkeeping a minute, so the next report crosses the
        // boundary with a minute of elapsed time behind it.
        let now = Utc::now().timestamp();
        {
            let mut agents = app.agents.write().unwrap();
            let entry = agents.get_mut(&id).unwrap();
            entry.last_minute -= 1;
            entry.mark = Some((now - 60, 0, 0));
        }
        // 60 MB arrived and the machine was busy for half the minute; by the
        // next sample both are over.
        dispatch(&app, id, "ip", &burst(1_000 + 60_000_000, 0, 0.0, 201)).unwrap();

        let row = &app.db.metrics(id, 0, 60).unwrap()[0];
        assert_eq!(row["net_rx"], 1_000_000, "60 MB over 60 s is 1 MB/s, not the agent's 0");
        assert_eq!(row["cpu"], 50.0, "the mean of the minute, not the idle second it ended on");
        // Whole numbers stay whole: the column is read with as_i64, which
        // answers nothing for the 150.5 the raw mean would be.
        assert_eq!(row["mem_used"], 151);
        // And the live view still shows the instant, which is what it is for.
        assert_eq!(app.agents.read().unwrap()[&id].metrics["net_rx"], 0);
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
        let result = |task, latency| {
            json!({"jsonrpc": "2.0", "method": "ping.result",
                   "params": {"task_id": task, "latency_ms": latency}})
            .to_string()
        };
        dispatch(&app, id, "ip", &result(7, 42)).unwrap();
        // The rejected results carry task ids of their own: a bare count would
        // be satisfied by the key collapsing them onto a good row.
        dispatch(&app, id, "ip", &result(8, 15)).unwrap();
        dispatch(&app, id, "ip", &result(0, 42)).unwrap(); // no such task
        dispatch(&app, id, "ip", &result(-1, 42)).unwrap(); // nor this one

        // Sorted rather than indexed: both rows land in the same second and
        // the query orders by timestamp.
        let mut seen: Vec<(i64, i64)> = app
            .db
            .ping_records(id, 0, 60)
            .unwrap()
            .iter()
            .map(|r| (r["task_id"].as_i64().unwrap(), r["latency"].as_i64().unwrap()))
            .collect();
        seen.sort();
        assert_eq!(seen, vec![(7, 42), (8, 15)], "each real task keeps its own result, and only those");
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
        // release() reads the session tag, not the channel, so the receiver
        // going away changes nothing.
        let connect = |session| {
            let (tx, _) = mpsc::channel(1);
            app.agents.write().unwrap().insert(id, Agent::new(session, tx));
        };

        // The ordinary case: the session that ends is the one on record.
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
        // Unknown methods are simply ignored.
        assert!(dispatch(&app, id, "ip", r#"{"method":"whatever"}"#).is_ok());
    }
}

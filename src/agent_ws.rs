//! The agent side of the hub: one WebSocket per node carrying JSON-RPC 2.0
//! notifications. One long-lived connection either end can speak first on, and
//! a frame that names its own method, which `curl` and a browser console read.

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

/// Tells one agent session on a node apart from the next. A connection can now
/// outlive its usefulness by up to SILENCE, which is long enough for the agent
/// to have given up and reconnected; without this tag the late teardown would
/// remove the live session that replaced it and strand a node that is in fact
/// reporting normally.
static SESSION: AtomicU64 = AtomicU64::new(0);

/// One node's current state. Lives in memory only: it is rebuilt within a
/// report interval of a hub restart, so persisting it would buy nothing.
#[derive(Clone, Debug, Default)]
pub struct Live {
    pub metrics: serde_json::Value,
    pub last_seen: i64,
    /// Wall-clock minute of the last row written to `metric`.
    pub last_minute: i64,
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
    app.agents.lock().unwrap_or_else(|e| e.into_inner()).insert(node_id, (session, tx));
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
            // leaves the hub waiting on a receive that will never return: the
            // node reads as online with metrics frozen at the moment it died,
            // until the kernel eventually gives up on the TCP session hours
            // later. A ping every HEARTBEAT proves the path both ways, and any
            // frame coming back — including the pong — counts as a sign of life.
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
/// holding it. Returns whether anything was actually released.
///
/// A teardown can arrive late — up to SILENCE after the agent gave up — by
/// which time a reconnect may already have installed a newer session under the
/// same node id. Clearing that one would mark a node offline and cut it off
/// from probe pushes while it is reporting perfectly well.
fn release(app: &App, node_id: i64, session: u64) -> bool {
    let mut agents = app.agents.lock().unwrap_or_else(|e| e.into_inner());
    if !agents.get(&node_id).is_some_and(|(id, _)| *id == session) {
        return false;
    }
    agents.remove(&node_id);
    drop(agents);
    app.live.write().unwrap_or_else(|e| e.into_inner()).remove(&node_id);
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

fn report(app: &App, node_id: i64, mut metrics: serde_json::Value) -> Result<()> {
    let now = Utc::now().timestamp();
    let boot_id = metrics.get("boot_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let rx = metrics.get("net_rx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let tx = metrics.get("net_tx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let traffic = app.db.accumulate(node_id, &boot_id, rx, tx)?;

    // The hub's accumulated figures are what the UI shows, so fold them into
    // the live payload and let the raw kernel counters stay an implementation
    // detail of the wire protocol.
    if let Some(obj) = metrics.as_object_mut() {
        obj.insert("total_rx".into(), json!(traffic.total_rx));
        obj.insert("total_tx".into(), json!(traffic.total_tx));
        obj.insert("month_rx".into(), json!(traffic.month_rx));
        obj.insert("month_tx".into(), json!(traffic.month_tx));
    }

    let minute = now / 60;
    let mut live = app.live.write().unwrap_or_else(|e| e.into_inner());
    let entry = live.entry(node_id).or_default();
    // History is one row per minute; the live view gets every report.
    let store = entry.last_minute != minute;
    entry.metrics = metrics.clone();
    entry.last_seen = now;
    if store {
        entry.last_minute = minute;
    }
    drop(live);

    if store {
        app.db.insert_metric(node_id, minute * 60, &metrics)?;
        app.db.touch_seen(node_id, now)?;
    }
    Ok(())
}

fn ping_tasks_message(app: &App, node_id: i64) -> String {
    let tasks = app.db.ping_tasks_for(node_id).unwrap_or_default();
    json!({"jsonrpc": "2.0", "method": "ping.tasks", "params": tasks}).to_string()
}

/// Pushes the current probe list to every connected agent. Called after the
/// panel edits tasks so changes take effect without waiting for a reconnect.
pub fn push_ping_tasks(app: &App) {
    let connected: Vec<(i64, mpsc::Sender<String>)> = app
        .agents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(id, (_, tx))| (*id, tx.clone()))
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

    fn report_json(boot: &str, rx: i64, tx: i64) -> String {
        json!({
            "jsonrpc": "2.0", "method": "report",
            "params": {"boot_id": boot, "cpu": 12.5, "load": [0.5, 0.4, 0.3],
                       "mem_used": 100, "net_rx_total": rx, "net_tx_total": tx}
        })
        .to_string()
    }

    /// A burst of reports inside one minute: every one of them moves the live
    /// view and the running totals, while history takes a single row stamped on
    /// the minute boundary.
    #[test]
    fn a_burst_of_reports_moves_the_live_view_but_writes_one_history_row() {
        let app = app();
        let id = node(&app);
        let minute = Utc::now().timestamp() / 60 * 60;

        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 1_000, 500)).unwrap();
        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 3_000, 1_500)).unwrap();

        let live = app.live.read().unwrap();
        let entry = live.get(&id).unwrap();
        assert_eq!(entry.metrics["cpu"], 12.5);
        // First report is the baseline, so only the second one counts.
        assert_eq!(entry.metrics["total_rx"], 2_000);
        assert_eq!(entry.metrics["total_tx"], 1_000);
        assert_eq!(entry.metrics["month_rx"], 2_000);
        assert_eq!(entry.last_minute, minute / 60, "the minute already written is remembered");
        drop(live);

        // History rows are keyed by (node, ts), so counting them proves nothing
        // on its own: five reports a second apart collapse onto one row whether
        // the minute gate is there or not. The stamp is what shows the gate.
        let rows = app.db.metrics(id, 0, 60).unwrap();
        assert_eq!(rows.len(), 1, "a minute of reports is one row");
        assert_eq!(rows[0]["ts"], minute, "stamped on the minute, not on the report");
        // Written on the same branch, and the offline badge counts from it.
        assert!(app.db.node(id).unwrap().unwrap().last_seen >= minute, "last_seen is written too");
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
        // Records are keyed by (node, task, ts), so the rejected ones carry a
        // task id of their own: a bare count would be satisfied by the key
        // collapsing them onto the good row.
        dispatch(&app, id, "ip", &result(8, 15)).unwrap();
        dispatch(&app, id, "ip", &result(0, 42)).unwrap(); // no such task
        dispatch(&app, id, "ip", &result(-1, 42)).unwrap(); // nor this one

        // Sorted rather than indexed: both rows land in the same second, and
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
        let live = || app.live.read().unwrap().contains_key(&id);
        // release() reads the session tag, not the channel, so the receiver
        // going away here changes nothing.
        let connect = |session| {
            let (tx, _) = mpsc::channel(1);
            app.agents.lock().unwrap().insert(id, (session, tx));
            app.live.write().unwrap().insert(id, Live::default());
        };

        // The ordinary case: the session that ends is the one on record.
        connect(1);
        assert!(release(&app, id, 1));
        assert!(!live(), "its own teardown clears the node");

        // The race: the agent gave up and reconnected while the old socket sat
        // half-open, so session 2 is live when session 1 finally unwinds.
        connect(1);
        connect(2);
        assert!(!release(&app, id, 1), "a stale session must release nothing");
        assert!(live(), "the reconnected agent stays online");
        assert!(app.agents.lock().unwrap().contains_key(&id), "and keeps receiving probe pushes");
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

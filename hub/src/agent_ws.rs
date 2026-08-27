//! The agent side of the hub: one WebSocket per node carrying JSON-RPC 2.0
//! notifications, the same transport komari and NodeGet settled on.

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

use crate::auth::sha256;
use crate::{App, Shared};

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
    let Ok(Some(node_id)) = app.db.node_by_token(&sha256(token)) else {
        // Same response whether the token is malformed or simply unknown.
        return (StatusCode::UNAUTHORIZED, "invalid token").into_response();
    };
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_owned())
        .unwrap_or_else(|| peer.ip().to_string());

    upgrade.on_upgrade(move |socket| async move {
        if let Err(e) = serve(app, node_id, ip, socket).await {
            debug!("node {node_id} disconnected: {e:#}");
        }
    })
}

/// Extracts the node token from `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ").filter(|t| !t.is_empty())
}

async fn serve(app: Shared, node_id: i64, ip: String, mut socket: WebSocket) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<String>(16);
    app.agents.lock().unwrap_or_else(|e| e.into_inner()).insert(node_id, tx);
    info!("node {node_id} connected from {ip}");

    // Tell the agent what to probe before the first report arrives.
    let _ = socket.send(Message::Text(ping_tasks_message(&app, node_id).into())).await;

    let outcome = loop {
        tokio::select! {
            outbound = rx.recv() => match outbound {
                Some(text) => socket.send(Message::Text(text.into())).await?,
                None => break Ok(()),
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(text))) => {
                    if let Err(e) = dispatch(&app, node_id, &ip, &text) {
                        warn!("node {node_id} sent an unusable message: {e:#}");
                    }
                }
                Some(Ok(Message::Close(_))) | None => break Ok(()),
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
            },
        }
    };

    app.agents.lock().unwrap_or_else(|e| e.into_inner()).remove(&node_id);
    app.live.write().unwrap_or_else(|e| e.into_inner()).remove(&node_id);
    info!("node {node_id} went offline");
    outcome
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
    let reset_day = app
        .db
        .node(node_id)?
        .map(|n| n.traffic_reset_day)
        .unwrap_or(1);

    let boot_id = metrics.get("boot_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
    let rx = metrics.get("net_rx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let tx = metrics.get("net_tx_total").and_then(|v| v.as_i64()).unwrap_or(0);
    let traffic = app.db.accumulate(node_id, &boot_id, rx, tx, reset_day)?;

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
    let connected: Vec<i64> = app.agents.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    for node_id in connected {
        let message = ping_tasks_message(app, node_id);
        let sender = app.agents.lock().unwrap_or_else(|e| e.into_inner()).get(&node_id).cloned();
        if let Some(sender) = sender {
            let _ = sender.try_send(message);
        }
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
            .create_node(&Node { name: "n".into(), traffic_reset_day: 1, ..Default::default() }, "hash")
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

    #[test]
    fn a_report_updates_live_state_and_accumulated_traffic() {
        let app = app();
        let id = node(&app);

        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 1_000, 500)).unwrap();
        dispatch(&app, id, "1.2.3.4", &report_json("boot-a", 3_000, 1_500)).unwrap();

        let live = app.live.read().unwrap();
        let m = &live.get(&id).unwrap().metrics;
        assert_eq!(m["cpu"], 12.5);
        // First report is the baseline, so only the second one counts.
        assert_eq!(m["total_rx"], 2_000);
        assert_eq!(m["total_tx"], 1_000);
        assert_eq!(m["month_rx"], 2_000);
    }

    #[test]
    fn history_is_written_once_a_minute_not_once_a_report() {
        let app = app();
        let id = node(&app);
        for _ in 0..5 {
            dispatch(&app, id, "ip", &report_json("boot-a", 1_000, 1_000)).unwrap();
        }
        assert_eq!(app.db.metrics(id, 0).unwrap().len(), 1, "five reports in one minute is one row");
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
        dispatch(&app, id, "ip", &result(1, 42)).unwrap();
        dispatch(&app, id, "ip", &result(0, 42)).unwrap(); // no such task
        let records = app.db.ping_records(id, 0).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["latency"], 42);
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
    fn junk_from_an_agent_is_rejected_without_taking_the_connection_down() {
        let app = app();
        let id = node(&app);
        assert!(dispatch(&app, id, "ip", "not json").is_err());
        // Unknown methods are simply ignored.
        assert!(dispatch(&app, id, "ip", r#"{"method":"whatever"}"#).is_ok());
    }
}

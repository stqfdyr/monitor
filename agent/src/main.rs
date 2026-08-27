//! monitor-agent: reports one Linux host to a monitor hub over WebSocket.

mod collect;

use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use collect::Collector;

struct Args {
    server: String,
    token: String,
    interval: u64,
    skip_ifaces: Vec<String>,
}

fn usage() -> ! {
    eprintln!(
        "monitor-agent {}\n\n\
         Usage: monitor-agent --server <url> --token <token> [options]\n\n\
         Options:\n  \
           --server <url>       Hub base URL, e.g. https://hub.example.com\n  \
           --token <token>      Node token from the hub panel\n  \
           --interval <secs>    Report interval (default 2)\n  \
           --skip-iface <name>  Extra interface prefix to exclude (repeatable)\n",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2)
}

fn parse_args() -> Result<Args> {
    let (mut server, mut token, mut interval, mut skip_ifaces) = (None, None, 2u64, Vec::new());
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || it.next().unwrap_or_else(|| usage());
        match arg.as_str() {
            "--server" => server = Some(value()),
            "--token" => token = Some(value()),
            "--interval" => interval = value().parse().unwrap_or(2),
            "--skip-iface" => skip_ifaces.push(value()),
            "-h" | "--help" => usage(),
            other => bail!("unknown argument: {other}"),
        }
    }
    let server = server.or_else(|| std::env::var("MONITOR_SERVER").ok()).unwrap_or_else(|| usage());
    let token = token.or_else(|| std::env::var("MONITOR_TOKEN").ok()).unwrap_or_else(|| usage());
    Ok(Args { server, token, interval: interval.clamp(1, 3600), skip_ifaces })
}

/// `https://host/path` -> `wss://host/path/api/agent/ws`.
///
/// The token travels in an Authorization header rather than the query string,
/// so it stays out of reverse-proxy access logs.
fn ws_url(server: &str) -> Result<String> {
    let base = server.trim_end_matches('/');
    let base = match base.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        Some(("wss" | "ws", _)) => base.to_owned(),
        _ => format!("wss://{base}"),
    };
    if base.starts_with("ws://") && !is_loopback(&base) {
        bail!("refusing plaintext ws:// to a remote hub; the token would travel in the clear");
    }
    Ok(format!("{base}/api/agent/ws"))
}

fn is_loopback(url: &str) -> bool {
    let host = url.split("://").nth(1).unwrap_or("").split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("127.")
}

#[derive(Deserialize)]
struct Rpc {
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Deserialize, Clone, Debug)]
struct PingTask {
    id: i64,
    target: String,
    interval: u64,
}

fn notify(method: &str, params: serde_json::Value) -> Message {
    Message::Text(
        serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string().into(),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("MONITOR_LOG").add_directive("info".parse()?),
        )
        .init();

    let args = parse_args()?;
    let url = ws_url(&args.server)?;
    let mut collector = Collector::new(args.skip_ifaces.clone());
    let mut backoff = 1u64;

    loop {
        match session(&url, &args.token, &mut collector, args.interval).await {
            Ok(()) => backoff = 1,
            Err(e) => warn!("session ended: {e:#}"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

/// One connection: say hello, then report until the socket dies.
async fn session(url: &str, token: &str, collector: &mut Collector, interval: u64) -> Result<()> {
    let mut request = url.into_client_request()?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().context("token is not header-safe")?);
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.context("connect")?;
    info!("connected");

    ws.send(notify("hello", serde_json::to_value(collector.facts())?)).await?;

    let (result_tx, mut result_rx) = mpsc::channel::<Message>(64);
    let mut ping_tasks: Vec<(PingTask, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(interval));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let result = loop {
        tokio::select! {
            _ = ticker.tick() => {
                let m = serde_json::to_value(collector.collect())?;
                if let Err(e) = ws.send(notify("report", m)).await { break Err(e.into()); }
            }
            Some(msg) = result_rx.recv() => {
                if let Err(e) = ws.send(msg).await { break Err(e.into()); }
            }
            incoming = ws.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(rpc) = serde_json::from_str::<Rpc>(&text) {
                        if rpc.method == "ping.tasks" {
                            if let Ok(tasks) = serde_json::from_value::<Vec<PingTask>>(rpc.params) {
                                respawn_ping_tasks(&mut ping_tasks, tasks, &result_tx);
                            }
                        }
                    }
                }
                Some(Ok(Message::Ping(p))) => { let _ = ws.send(Message::Pong(p)).await; }
                Some(Ok(_)) => {}
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
        }
    };

    for (_, handle) in ping_tasks {
        handle.abort();
    }
    result
}

/// Replaces the running probe loops with the hub's current task list, leaving
/// unchanged tasks alone so their timers do not restart on every push.
fn respawn_ping_tasks(
    running: &mut Vec<(PingTask, tokio::task::JoinHandle<()>)>,
    wanted: Vec<PingTask>,
    tx: &mpsc::Sender<Message>,
) {
    running.retain(|(task, handle)| {
        let keep =
            wanted.iter().any(|w| w.id == task.id && w.target == task.target && w.interval == task.interval);
        if !keep {
            handle.abort();
        }
        keep
    });
    for task in wanted {
        if running.iter().any(|(t, _)| t.id == task.id) {
            continue;
        }
        let (tx, spawned) = (tx.clone(), task.clone());
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(spawned.interval.clamp(5, 3600)));
            loop {
                ticker.tick().await;
                let latency = tcp_ping(&spawned.target).await;
                let msg =
                    notify("ping.result", serde_json::json!({"task_id": spawned.id, "latency_ms": latency}));
                if tx.send(msg).await.is_err() {
                    return;
                }
            }
        });
        running.push((task, handle));
    }
}

/// Connect time to a TCP port in milliseconds; -1 when unreachable.
async fn tcp_ping(target: &str) -> i32 {
    let started = std::time::Instant::now();
    let connect = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(target)).await;
    match connect {
        Ok(Ok(_)) => started.elapsed().as_millis().min(i32::MAX as u128) as i32,
        _ => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_upgrades_scheme_and_refuses_plaintext_to_remote() {
        assert_eq!(ws_url("https://hub.example.com/").unwrap(), "wss://hub.example.com/api/agent/ws");
        assert_eq!(ws_url("http://127.0.0.1:8080").unwrap(), "ws://127.0.0.1:8080/api/agent/ws");
        // Bare host defaults to TLS rather than silently leaking the token.
        assert!(ws_url("hub.example.com").unwrap().starts_with("wss://"));
        assert!(ws_url("http://hub.example.com").is_err());
        // No token anywhere in the URL: it rides in a header instead.
        assert!(!ws_url("https://hub.example.com").unwrap().contains("token"));
    }

    #[tokio::test]
    async fn tcp_ping_measures_success_and_reports_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        assert!(tcp_ping(&addr.to_string()).await >= 0);
        assert_eq!(tcp_ping("127.0.0.1:1").await, -1);
    }

    #[test]
    fn ping_tasks_keep_their_timers_unless_the_task_changed() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let (tx, _rx) = mpsc::channel(8);
        let mut running = Vec::new();
        let task = |id, target: &str, interval| PingTask { id, target: target.into(), interval };

        respawn_ping_tasks(&mut running, vec![task(1, "a:1", 60), task(2, "b:2", 60)], &tx);
        assert_eq!(running.len(), 2);
        let first = running[0].1.id();

        // Task 1 unchanged, task 2 retargeted, task 3 added.
        respawn_ping_tasks(
            &mut running,
            vec![task(1, "a:1", 60), task(2, "c:3", 60), task(3, "d:4", 60)],
            &tx,
        );
        assert_eq!(running.len(), 3);
        assert_eq!(running[0].1.id(), first, "unchanged task must not be restarted");
    }
}

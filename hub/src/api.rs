//! The panel and public-status HTTP surface.

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{authed, hash_password, random_token, sha256};
use crate::db::{Node, PingTask};
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
fn node_view(app: &App, node: &Node, full: bool) -> Value {
    let live = app.live.read().unwrap_or_else(|e| e.into_inner());
    let current = live.get(&node.id);
    let traffic = app.db.traffic(node.id);
    let mut view = json!({
        "id": node.id,
        "name": node.name,
        "sort": node.sort,
        "public": node.public,
        "online": current.is_some(),
        "last_seen": current.map(|l| l.last_seen).unwrap_or(0),
        "metrics": current.map(|l| l.metrics.clone()).unwrap_or(Value::Null),
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
    });
    // Address and private notes never leave the panel.
    if full {
        view["hostname"] = json!(node.hostname);
        view["ip"] = json!(node.ip);
        view["remark"] = json!(node.remark);
    }
    view
}

fn visible_nodes(app: &App, full: bool) -> Result<Vec<Value>, anyhow::Error> {
    Ok(app
        .db
        .nodes()?
        .iter()
        .filter(|n| full || n.public)
        .map(|n| node_view(app, n, full))
        .collect())
}

pub async fn nodes(State(app): State<Shared>, headers: HeaderMap) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    match visible_nodes(&app, full) {
        Ok(list) => Json(json!({"nodes": list, "admin": full})).into_response(),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct Window {
    #[serde(default = "default_hours")]
    hours: i64,
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
    match readable(&app, &headers, id) {
        Err(r) => r,
        Ok(()) => {
            let since = Utc::now().timestamp() - w.hours.clamp(1, 24 * 90) * 3_600;
            match (app.db.metrics(id, since), app.db.ping_records(id, since)) {
                (Ok(m), Ok(p)) => Json(json!({"metrics": m, "ping": p})).into_response(),
                (Err(e), _) | (_, Err(e)) => fail(e),
            }
        }
    }
}

/// Guards a per-node read: the panel sees everything, the public page only
/// sees nodes that were explicitly published.
fn readable(app: &App, headers: &HeaderMap, id: i64) -> Result<(), Response> {
    if authed(app, headers) {
        return Ok(());
    }
    let public = app.public_page() && app.db.node(id).ok().flatten().is_some_and(|n| n.public);
    if public {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "sign-in required").into_response())
    }
}

/// Live stream for the browser. Each connection runs its own timer, which is
/// cheaper to reason about than a fan-out channel at this scale.
pub async fn live_ws(State(app): State<Shared>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let full = authed(&app, &headers);
    if !full && !app.public_page() {
        return (StatusCode::UNAUTHORIZED, "sign-in required").into_response();
    }
    upgrade.on_upgrade(move |socket| stream_live(app, socket, full))
}

async fn stream_live(app: Shared, mut socket: WebSocket, full: bool) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        ticker.tick().await;
        let Ok(list) = visible_nodes(&app, full) else { break };
        let payload = json!({"nodes": list}).to_string();
        if socket.send(Message::Text(payload.into())).await.is_err() {
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
    }))
}

pub async fn create_node(
    _: Admin,
    State(app): State<Shared>,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    let Ok(Json(node)) = body else { return bad("invalid node") };
    if node.name.trim().is_empty() {
        return bad("name is required");
    }
    let token = random_token();
    match app.db.create_node(&node, &sha256(&token)) {
        // The plaintext token is shown exactly once, here.
        Ok(id) => Json(json!({"id": id, "token": token, "install": app.install_command(&token)})).into_response(),
        Err(e) => fail(e),
    }
}

pub async fn update_node(
    _: Admin,
    State(app): State<Shared>,
    Path(id): Path<i64>,
    body: Result<Json<Node>, JsonRejection>,
) -> Response {
    let Ok(Json(node)) = body else { return bad("invalid node") };
    match app.db.update_node(id, &node) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => fail(e),
    }
}

pub async fn delete_node(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    match app.db.delete_node(id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => fail(e),
    }
}

/// Issues a fresh token, which immediately invalidates the old one.
pub async fn reset_token(_: Admin, State(app): State<Shared>, Path(id): Path<i64>) -> Response {
    let token = random_token();
    let updated = app.db.node(id).map(|n| n.is_some()).unwrap_or(false);
    if !updated {
        return (StatusCode::NOT_FOUND, "no such node").into_response();
    }
    match app.db.reset_token(id, &sha256(&token)) {
        Ok(()) => Json(json!({"token": token, "install": app.install_command(&token)})).into_response(),
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
        Ok(()) => Json(json!({"ok": true})).into_response(),
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
];

pub async fn settings(_: Admin, State(app): State<Shared>) -> Json<Value> {
    let mut out = serde_json::Map::new();
    for key in READABLE_SETTINGS {
        out.insert((*key).to_owned(), json!(app.db.get(key).unwrap_or_default()));
    }
    out.insert("github_secret_set".into(), json!(app.db.get("github_client_secret").is_some_and(|v| !v.is_empty())));
    Json(Value::Object(out))
}

pub async fn save_settings(_: Admin, State(app): State<Shared>, Json(body): Json<Value>) -> Response {
    let Some(map) = body.as_object() else { return bad("expected an object") };
    for (key, value) in map {
        let Some(value) = value.as_str() else { continue };
        let stored = match key.as_str() {
            k if READABLE_SETTINGS.contains(&k) || k == "github_client_secret" => value,
            // Changing the password logs every existing session out.
            "admin_password" => {
                if value.len() < 12 {
                    return bad("password must be at least 12 characters");
                }
                match hash_password(value).and_then(|h| {
                    app.db.set("admin_password_hash", &h)?;
                    app.db.drop_all_sessions()?;
                    Ok(())
                }) {
                    Ok(()) => continue,
                    Err(e) => return fail(e),
                }
            }
            _ => return bad(&format!("unknown setting: {key}")),
        };
        if let Err(e) = app.db.set(key, stored) {
            return fail(e);
        }
    }
    Json(json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn app() -> App {
        App::for_test(Db::open(":memory:").unwrap())
    }

    fn node(app: &App, name: &str, public: bool) -> i64 {
        app.db
            .create_node(
                &Node { name: name.into(), public, remark: "secret note".into(), ..Default::default() },
                &random_token(),
            )
            .unwrap()
    }

    #[test]
    fn the_public_view_hides_private_nodes_and_sensitive_fields() {
        let app = app();
        let open = node(&app, "open", true);
        node(&app, "hidden", false);
        app.db.save_facts(open, &json!({"hostname": "vps-1"}), "198.51.100.9").unwrap();

        let public = visible_nodes(&app, false).unwrap();
        assert_eq!(public.len(), 1, "a node marked private must not be listed");
        assert_eq!(public[0]["name"], "open");
        for hidden in ["ip", "remark", "hostname"] {
            assert!(public[0].get(hidden).is_none(), "{hidden} must not be public");
        }

        let admin = visible_nodes(&app, true).unwrap();
        assert_eq!(admin.len(), 2);
        assert_eq!(admin[0]["ip"], "198.51.100.9");
        assert_eq!(admin[0]["remark"], "secret note");
    }

    #[test]
    fn a_node_view_carries_traffic_even_while_offline() {
        let app = app();
        let id = node(&app, "n", true);
        app.db.accumulate(id, "b", 100, 100, 1).unwrap();
        app.db.accumulate(id, "b", 900, 500, 1).unwrap();

        let view = &visible_nodes(&app, true).unwrap()[0];
        assert_eq!(view["online"], false);
        assert_eq!(view["metrics"], Value::Null);
        assert_eq!(view["total_rx"], 800, "traffic is stored, not derived from the live state");
        assert_eq!(view["total_tx"], 400);
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

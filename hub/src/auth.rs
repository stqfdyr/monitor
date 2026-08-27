//! Sessions, the local emergency password, and GitHub single sign-on.
//!
//! GitHub is the normal way in. The local password exists so a broken OAuth
//! app or an unreachable github.com cannot lock the owner out of their own hub.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::App;

pub const COOKIE: &str = "monitor_session";
const STATE_COOKIE: &str = "monitor_oauth_state";
const SESSION_DAYS: i64 = 14;
/// Failed password attempts allowed per address before it is shut out.
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(900);

pub fn sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

pub fn random_token() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt =
        SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| anyhow::anyhow!("salt: {e}"))?;
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("hash password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, stored: &str) -> bool {
    PasswordHash::new(stored)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

/// Per-address failure counter for the password endpoint.
#[derive(Default)]
pub struct Throttle(Mutex<HashMap<IpAddr, (u32, Instant)>>);

impl Throttle {
    fn locked(&self, ip: IpAddr) -> bool {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&ip) {
            Some((n, since)) if since.elapsed() < LOCKOUT => *n >= MAX_ATTEMPTS,
            Some(_) => {
                map.remove(&ip);
                false
            }
            None => false,
        }
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(ip).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= LOCKOUT {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }

    fn clear(&self, ip: IpAddr) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(&ip);
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_owned())
}

/// True when the request carries a live session cookie.
pub fn authed(app: &App, headers: &HeaderMap) -> bool {
    cookie_value(headers, COOKIE).is_some_and(|token| app.db.session_valid(&sha256(&token)))
}

fn set_cookie(name: &str, value: &str, max_age: i64, secure: bool) -> String {
    let mut cookie = format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

pub fn issue_session(app: &App) -> Result<String> {
    let token = random_token();
    app.db.create_session(&sha256(&token), Utc::now().timestamp() + SESSION_DAYS * 86_400)?;
    Ok(set_cookie(COOKIE, &token, SESSION_DAYS * 86_400, app.secure_cookies()))
}

#[derive(Deserialize)]
pub struct LoginBody {
    password: String,
}

pub async fn login(
    State(app): State<crate::Shared>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    let ip = client_ip(&headers, peer.ip());
    if app.throttle.locked(ip) {
        return (StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later").into_response();
    }
    let Some(stored) = app.db.get("admin_password_hash") else {
        return (StatusCode::FORBIDDEN, "password login is disabled").into_response();
    };
    if !verify_password(&body.password, &stored) {
        app.throttle.record_failure(ip);
        return (StatusCode::UNAUTHORIZED, "invalid password").into_response();
    }
    app.throttle.clear(ip);
    match issue_session(&app) {
        Ok(cookie) => with_cookies(Json(serde_json::json!({"ok": true})), [cookie]),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn logout(State(app): State<crate::Shared>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, COOKIE) {
        let _ = app.db.drop_session(&sha256(&token));
    }
    with_cookies(Json(serde_json::json!({"ok": true})), [set_cookie(COOKIE, "", 0, app.secure_cookies())])
}

/// Step one of the OAuth dance: hand the browser a state nonce and send it to
/// GitHub. The nonce comes back in step two and must match.
pub async fn github_start(State(app): State<crate::Shared>) -> Response {
    let Some(client_id) = app.db.get("github_client_id").filter(|v| !v.is_empty()) else {
        return (StatusCode::PRECONDITION_FAILED, "GitHub sign-in is not configured").into_response();
    };
    let state = random_token();
    let url = format!(
        "https://github.com/login/oauth/authorize?client_id={client_id}&scope=read:user&state={state}"
    );
    with_cookies(Redirect::to(&url), [set_cookie(STATE_COOKIE, &state, 600, app.secure_cookies())])
}

#[derive(Deserialize)]
pub struct Callback {
    code: String,
    state: String,
}

pub async fn github_callback(
    State(app): State<crate::Shared>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    // Reject a callback the browser did not initiate.
    if cookie_value(&headers, STATE_COOKIE).as_deref() != Some(query.state.as_str()) {
        return sign_in_failed(&app, "state mismatch; start again from the sign-in page");
    }
    if let Err(e) = github_login(&app, &query.code).await {
        return sign_in_failed(&app, &e.to_string());
    }
    let session = match issue_session(&app) {
        Ok(cookie) => cookie,
        Err(e) => return sign_in_failed(&app, &e.to_string()),
    };
    with_cookies(Redirect::to("/admin"), [clear_state(&app), session])
}

/// Sends the browser back to the sign-in page carrying the reason, so the
/// failure is readable in the UI instead of being a bare plain-text 401 at a
/// callback URL the user cannot navigate away from.
fn sign_in_failed(app: &App, reason: &str) -> Response {
    let target = format!("/admin?login_error={}", urlencode(reason));
    with_cookies(Redirect::to(&target), [clear_state(app), String::new()])
}

fn clear_state(app: &App) -> String {
    set_cookie(STATE_COOKIE, "", 0, app.secure_cookies())
}

/// Attaches several `Set-Cookie` headers to one response.
///
/// An array of header tuples cannot be used here: axum applies those with
/// `HeaderMap::insert`, so a second `Set-Cookie` silently replaces the first
/// rather than adding to it. Empty entries are skipped.
pub fn with_cookies<const N: usize>(response: impl IntoResponse, cookies: [String; N]) -> Response {
    let mut response = response.into_response();
    for cookie in cookies {
        if cookie.is_empty() {
            continue;
        }
        match cookie.parse() {
            Ok(value) => {
                response.headers_mut().append(header::SET_COOKIE, value);
            }
            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "bad cookie").into_response(),
        }
    }
    response
}

/// Percent-encodes everything outside the unreserved set, which is enough for
/// dropping an arbitrary message into a query string.
fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Exchanges the code for a token and checks the login against the allow list.
async fn github_login(app: &App, code: &str) -> Result<()> {
    let (Some(id), Some(secret)) = (app.db.get("github_client_id"), app.db.get("github_client_secret"))
    else {
        bail!("not configured");
    };
    let allowed = app.db.get("github_allowed_users").unwrap_or_default();
    let allowed: Vec<String> =
        allowed.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
    if allowed.is_empty() {
        // Without an allow list every GitHub account on earth could sign in.
        bail!("no allowed GitHub users configured");
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: Option<String>,
        error_description: Option<String>,
    }
    let token: TokenResponse = app
        .http
        .post("https://github.com/login/oauth/access_token")
        .header(header::ACCEPT, "application/json")
        .json(&serde_json::json!({"client_id": id, "client_secret": secret, "code": code}))
        .send()
        .await
        .context("token request")?
        .json()
        .await
        .context("token response")?;
    let Some(access) = token.access_token else {
        bail!("{}", token.error_description.unwrap_or_else(|| "no access token".into()));
    };

    #[derive(Deserialize)]
    struct GithubUser {
        login: String,
    }
    let user: GithubUser = app
        .http
        .get("https://api.github.com/user")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await
        .context("user request")?
        .json()
        .await
        .context("user response")?;

    if !allowed.contains(&user.login.to_lowercase()) {
        bail!("{} is not on the allowed list", user.login);
    }
    Ok(())
}

/// Peer address, or the first hop in X-Forwarded-For when a reverse proxy is
/// trusted. Only consulted for throttling, never for authorization.
fn client_ip(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trips_and_rejects_wrong_input() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("Correct horse battery staple", &hash));
        assert!(!verify_password("", &hash));
        // A corrupt or empty stored hash must fail closed, not open.
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", ""));
    }

    #[test]
    fn each_hash_gets_its_own_salt() {
        assert_ne!(hash_password("same").unwrap(), hash_password("same").unwrap());
    }

    #[test]
    fn throttle_locks_an_address_then_forgets_it_on_success() {
        let t = Throttle::default();
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();

        for _ in 0..MAX_ATTEMPTS {
            assert!(!t.locked(ip));
            t.record_failure(ip);
        }
        assert!(t.locked(ip), "address must be locked out after repeated failures");
        assert!(!t.locked(other), "lockout must not spread to other addresses");

        t.clear(ip);
        assert!(!t.locked(ip));
    }

    #[test]
    fn several_cookies_all_survive_on_one_response() {
        // axum applies an array of header tuples with insert(), which silently
        // drops all but the last Set-Cookie. This helper must append instead.
        let response = with_cookies(StatusCode::OK, ["a=1".to_owned(), "b=2".to_owned()]);
        let set: Vec<_> = response.headers().get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(set.len(), 2, "both cookies must reach the browser");
        // Empty entries are skipped rather than emitting a blank header.
        let response = with_cookies(StatusCode::OK, ["a=1".to_owned(), String::new()]);
        assert_eq!(response.headers().get_all(header::SET_COOKIE).iter().count(), 1);
    }

    #[test]
    fn a_failure_reason_survives_the_trip_through_the_query_string() {
        assert_eq!(
            urlencode("no allowed GitHub users configured"),
            "no%20allowed%20GitHub%20users%20configured"
        );
        // Characters that would otherwise break out of the query string.
        assert_eq!(urlencode("a&b=c#d"), "a%26b%3Dc%23d");
        assert_eq!(urlencode("用户"), "%E7%94%A8%E6%88%B7");
    }

    #[test]
    fn cookies_are_parsed_out_of_a_shared_header() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "other=1; monitor_session=abc123; x=2".parse().unwrap());
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc123"));
        assert_eq!(cookie_value(&h, "missing"), None);
        assert_eq!(cookie_value(&HeaderMap::new(), COOKIE), None);
    }

    #[test]
    fn session_cookie_is_locked_down() {
        let c = set_cookie(COOKIE, "v", 3600, true);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Lax") && c.contains("Secure"));
        assert!(!set_cookie(COOKIE, "v", 3600, false).contains("Secure"));
    }

    #[test]
    fn forwarded_header_is_used_only_when_present() {
        let peer: IpAddr = "10.0.0.1".parse().unwrap();
        assert_eq!(client_ip(&HeaderMap::new(), peer), peer);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "198.51.100.9, 10.0.0.2".parse().unwrap());
        assert_eq!(client_ip(&h, peer).to_string(), "198.51.100.9");
    }
}

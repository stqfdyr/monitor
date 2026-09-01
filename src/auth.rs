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
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::App;

pub const COOKIE: &str = "monitor_session";
const STATE_COOKIE: &str = "monitor_oauth_state";
const SESSION_DAYS: i64 = 14;
/// Failed password attempts allowed per address before it is shut out.
const MAX_ATTEMPTS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(900);

/// How many password checks may run at once.
///
/// argon2 is deliberately expensive: one attempt costs 19 MiB and a tenth of a
/// second of a core. Unbounded, that cost is a lever rather than a defence --
/// the lockout below bounds attempts per address while nothing bounds the
/// addresses, which on IPv6 is a /64 the caller already owns.
///
/// One, because any number at or above what the machine can run at once is not
/// a limit: argon2 saturates a core, so a gate of four on a three-core hub
/// never had four in flight to refuse and passed a flood through untouched --
/// 570 MB against a unit file allowing 256. At one, 633 of 640 attempts are
/// turned away at the door. Tying it to the core count would re-open the hole
/// on smaller machines.
///
/// Refused rather than queued: a queue lets the flood in anyway, only later.
/// The cost is that two simultaneous sign-ins mean one retries.
const PASSWORD_CHECKS: usize = 1;
static PASSWORD_GATE: Semaphore = Semaphore::const_new(PASSWORD_CHECKS);

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
pub struct Throttle {
    seen: Mutex<HashMap<IpAddr, (u32, Instant)>>,
    /// How long a failure is remembered. A field rather than the constant so a
    /// test can watch a lockout expire without sleeping for 15 minutes.
    window: Duration,
}

impl Default for Throttle {
    fn default() -> Self {
        Self { seen: Mutex::default(), window: LOCKOUT }
    }
}

impl Throttle {
    fn locked(&self, ip: IpAddr) -> bool {
        let mut map = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(&ip) {
            Some((n, since)) if since.elapsed() < self.window => *n >= MAX_ATTEMPTS,
            Some(_) => {
                map.remove(&ip);
                false
            }
            None => false,
        }
    }

    fn record_failure(&self, ip: IpAddr) {
        let mut map = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        // Addresses past their window are dropped here rather than left to
        // accumulate, which also restarts the count for a returning one.
        map.retain(|_, (_, since)| since.elapsed() < self.window);
        map.entry(ip).or_insert((0, Instant::now())).0 += 1;
    }

    fn clear(&self, ip: IpAddr) {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(&ip);
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
    // Held across the check below, which is the point of it.
    let Ok(_permit) = PASSWORD_GATE.try_acquire() else {
        return (StatusCode::TOO_MANY_REQUESTS, "too many attempts, try again later").into_response();
    };
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

/// Every field is optional. With required fields axum rejects a malformed
/// callback before the handler runs, returning a bare 400 and logging nothing
/// -- and GitHub reports a refusal with `error` and no `code` at all.
#[derive(Deserialize, Default)]
pub struct Callback {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

pub async fn github_callback(
    State(app): State<crate::Shared>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Response {
    // GitHub reports a refusal in the query string rather than in the body.
    if let Some(error) = &query.error {
        let reason = query.error_description.as_deref().unwrap_or(error);
        return sign_in_failed(&app, &format!("GitHub returned {error}: {reason}"));
    }
    // Reject a callback the browser did not initiate.
    let state = query.state.as_deref().unwrap_or_default();
    if state.is_empty() || cookie_value(&headers, STATE_COOKIE).as_deref() != Some(state) {
        return sign_in_failed(&app, "state mismatch or missing; start again from the sign-in page");
    }
    let Some(code) = query.code.as_deref().filter(|c| !c.is_empty()) else {
        return sign_in_failed(&app, "GitHub sent no authorization code");
    };
    if let Err(e) = github_login(&app, code).await {
        return sign_in_failed(&app, &e.to_string());
    }
    let session = match issue_session(&app) {
        Ok(cookie) => cookie,
        Err(e) => return sign_in_failed(&app, &e.to_string()),
    };
    with_cookies(Redirect::to("/admin"), [clear_state(&app), session])
}

/// Sends the browser back to the sign-in page with the reason, rather than
/// leaving a bare 401 at a callback URL there is no way out of.
fn sign_in_failed(app: &App, reason: &str) -> Response {
    // A rejected sign-in must leave a server-side trace: the browser only sees
    // the redirect.
    warn!("GitHub sign-in rejected: {reason}");
    let target = format!("/admin?login_error={}", urlencode(reason));
    with_cookies(Redirect::to(&target), [clear_state(app), String::new()])
}

fn clear_state(app: &App) -> String {
    set_cookie(STATE_COOKIE, "", 0, app.secure_cookies())
}

/// Attaches several `Set-Cookie` headers to one response. An array of header
/// tuples will not do: axum applies those with `HeaderMap::insert`, so a second
/// `Set-Cookie` replaces the first. Empty entries are skipped.
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
    let response = app
        .http
        .get("https://api.github.com/user")
        .header(header::AUTHORIZATION, format!("Bearer {access}"))
        .header(header::USER_AGENT, "monitor-hub")
        .send()
        .await
        .context("user request")?;
    let status = response.status();
    let body = response.text().await.context("user response")?;
    // Decoding an error page into GithubUser reports "missing field login"
    // instead of what GitHub said.
    let user: GithubUser = serde_json::from_str(&body).with_context(|| {
        format!("user response ({status}): {}", body.chars().take(200).collect::<String>())
    })?;

    if !allowed.contains(&user.login.to_lowercase()) {
        // The list stays in the log and out of the reason, which rides back in
        // a query string: anyone with a GitHub account can reach that page, and
        // a reason carrying the allow list hands them the names worth phishing.
        warn!("GitHub user {} is not on the allowed list {allowed:?}", user.login);
        bail!("GitHub user {} is not on the allowed list", user.login);
    }
    info!("GitHub sign-in accepted for {}", user.login);
    Ok(())
}

/// Peer address, or the first hop in X-Forwarded-For when the request came
/// through a local reverse proxy. Used for throttling and for the address shown
/// next to a node, never for authorization.
///
/// The header is honoured only when the peer is itself local. Otherwise a
/// caller mints a fresh identity per request, walking past the lockout and
/// growing the throttle map without bound.
pub fn client_ip(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
    if !behind_local_proxy(peer) {
        return peer;
    }
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer)
}

/// Loopback or a private network: where a reverse proxy actually sits.
fn behind_local_proxy(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // Unique-local (fc00::/7) and link-local (fe80::/10); the stable
        // standard library has no predicate for either.
        IpAddr::V6(v6) => {
            let head = v6.segments()[0];
            v6.is_loopback() || head & 0xfe00 == 0xfc00 || head & 0xffc0 == 0xfe80
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_fails_closed_and_never_repeats_a_salt() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("Correct horse battery staple", &hash));
        assert!(!verify_password("", &hash));
        // A corrupt or empty stored hash must fail closed.
        assert!(!verify_password("anything", "not-a-hash"));
        assert!(!verify_password("anything", ""));
        // The salt is per hash, so one cracked row does not identify every
        // other row using that password.
        assert_ne!(hash_password("same").unwrap(), hash_password("same").unwrap());
    }

    /// One address through the whole lockout lifecycle: attempts up to the
    /// limit are allowed, the next shuts the address out, the window runs out
    /// on its own, and a success clears it early. The window is shortened so
    /// that expiry is reachable at all.
    #[test]
    fn a_lockout_lands_expires_on_its_own_and_clears_on_success() {
        let window = Duration::from_millis(60);
        let t = Throttle { window, ..Default::default() };
        let ip: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        let stale: IpAddr = "203.0.113.9".parse().unwrap();
        let held = || t.seen.lock().unwrap().len();

        t.record_failure(stale);
        for _ in 0..MAX_ATTEMPTS {
            assert!(!t.locked(ip), "attempts up to the limit are still allowed");
            t.record_failure(ip);
        }
        assert!(t.locked(ip), "the attempt past the limit is shut out");
        assert!(!t.locked(other), "the lockout must not spread to other addresses");

        // A lockout is a delay, not a ban: the address is let back in by
        // itself.
        std::thread::sleep(window * 2);
        assert!(!t.locked(ip), "an expired lockout must lift on its own");

        // `stale` is never asked about, so only the sweep on the way in can
        // drop it. Without that the map grows one entry per address presented,
        // for the life of the process.
        assert_eq!(held(), 1, "the expired lockout is gone, stale is still held");
        t.record_failure(other);
        assert_eq!(held(), 1, "the stale address is swept, not carried");

        // A correct password clears the count, so two typos do not leave the
        // next slip a lockout.
        t.clear(other);
        assert_eq!(held(), 0);
    }

    /// The gate has to refuse rather than queue: a queue lets the flood arrive
    /// anyway, and each attempt that lands costs 19 MiB that stays in a
    /// thread's arena for the life of the process.
    #[test]
    fn the_password_gate_refuses_a_flood_rather_than_queueing_it() {
        let held: Vec<_> =
            (0..PASSWORD_CHECKS).map(|_| PASSWORD_GATE.try_acquire().expect("up to the limit")).collect();
        assert!(PASSWORD_GATE.try_acquire().is_err(), "the attempt past the limit must be refused");
        drop(held);
        assert!(PASSWORD_GATE.try_acquire().is_ok(), "permits come back when the checks finish");
    }

    /// Both ends of the same redirect: every shape GitHub can send has to
    /// parse, or it never reaches the handler and can be neither logged nor
    /// explained; and the reason sent back has to survive its query string.
    #[test]
    fn every_callback_shape_parses_and_a_failure_reason_survives_the_round_trip() {
        let parse = |q: &str| serde_urlencoded::from_str::<Callback>(q);

        let ok = parse("code=abc&state=xyz").expect("the happy path");
        assert_eq!(ok.code.as_deref(), Some("abc"));
        assert_eq!(ok.state.as_deref(), Some("xyz"));

        // GitHub reports a refusal with no code at all.
        let denied = parse("error=access_denied&error_description=the+user+said+no&state=xyz")
            .expect("a refusal must parse, not 400");
        assert_eq!(denied.error.as_deref(), Some("access_denied"));
        assert_eq!(denied.error_description.as_deref(), Some("the user said no"));
        assert!(denied.code.is_none());

        // Truncated or empty callbacks must still land in the handler.
        assert!(parse("state=xyz").is_ok());
        assert!(parse("").is_ok());

        // Anything that would break out of the query string has to be encoded
        // on the way in, or the reason arrives truncated at the first stray
        // separator.
        assert_eq!(urlencode("a&b=c#d"), "a%26b%3Dc%23d");
        assert_eq!(urlencode("用户"), "%E7%94%A8%E6%88%B7");
        let reason = "no allowed GitHub users configured (a&b=c)";
        let back = parse(&format!("error={}", urlencode(reason))).expect("a reason must parse");
        assert_eq!(back.error.as_deref(), Some(reason), "the whole reason comes back");
    }

    /// A session cookie's round trip: the flags it goes out with, riding a
    /// response beside a second cookie, and being picked back out of the one
    /// header the browser returns them all in.
    #[test]
    fn a_session_cookie_goes_out_locked_down_alongside_others_and_parses_back() {
        let session = set_cookie(COOKIE, "abc123", 3_600, true);
        assert!(session.contains("HttpOnly") && session.contains("SameSite=Lax"));
        assert!(session.contains("Secure"));
        assert!(!set_cookie(COOKIE, "abc123", 3_600, false).contains("Secure"));

        // axum applies an array of header tuples with insert(), dropping all
        // but the last Set-Cookie; this helper appends.
        let response = with_cookies(StatusCode::OK, [session, set_cookie(STATE_COOKIE, "s", 0, true)]);
        let set: Vec<_> = response.headers().get_all(header::SET_COOKIE).iter().collect();
        assert_eq!(set.len(), 2, "both cookies must reach the browser");
        // Empty entries are skipped rather than emitting a blank header.
        let response = with_cookies(StatusCode::OK, ["a=1".to_owned(), String::new()]);
        assert_eq!(response.headers().get_all(header::SET_COOKIE).iter().count(), 1);

        // And back. They come home crammed into one header, not one each.
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "other=1; monitor_session=abc123; x=2".parse().unwrap());
        assert_eq!(cookie_value(&h, COOKIE).as_deref(), Some("abc123"));
        assert_eq!(cookie_value(&h, "missing"), None);
        assert_eq!(cookie_value(&HeaderMap::new(), COOKIE), None);
    }

    #[test]
    fn forwarded_header_is_trusted_only_behind_a_local_proxy() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "198.51.100.9, 10.0.0.2".parse().unwrap());
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();

        // A proxy on loopback or a private range: the header is the client.
        assert_eq!(client_ip(&h, ip("127.0.0.1")).to_string(), "198.51.100.9");
        assert_eq!(client_ip(&h, ip("10.0.0.1")).to_string(), "198.51.100.9");
        assert_eq!(client_ip(&h, ip("::1")).to_string(), "198.51.100.9");
        assert_eq!(client_ip(&h, ip("fd00::1")).to_string(), "198.51.100.9");
        // Straight off the internet the header is whatever the caller typed,
        // and honouring it walks past the lockout for free.
        assert_eq!(client_ip(&h, ip("203.0.113.5")), ip("203.0.113.5"));
        assert_eq!(client_ip(&h, ip("2001:db8::5")), ip("2001:db8::5"));
        // No header at all: the peer, wherever it is.
        assert_eq!(client_ip(&HeaderMap::new(), ip("10.0.0.1")), ip("10.0.0.1"));
    }
}

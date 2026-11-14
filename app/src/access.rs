//! Cloudflare Access sign-in in the user's own browser (off unless Settings turns it on).
//!
//! The user's logins (Google, passkeys, password manager) live in their normal browser, not in
//! the viewer. So when Access intercepts the webmail, the login is handed to the default browser
//! the way `cloudflared access login` does it (cloudflared's token/transfer.go, token/encrypt.go):
//!
//! 1. We make an ephemeral NaCl box key pair and open
//!    `<webmail origin>/cdn-cgi/access/cli?aud=…&token=<our public key>&…` in the browser.
//! 2. The user signs in there. Access encrypts the application token to our public key and
//!    gives it to the transfer service (login.cloudflareaccess.org).
//! 3. We long-poll `<service>/transfer/<our public key>`. The answer is base64 of
//!    `nonce(24) || box`, with the service's public key in the `service-public-key` header;
//!    it opens to `{"app_token": …, "org_token": …}`.
//! 4. `app_token` is the JWT Access keeps in the `CF_Authorization` cookie of the webmail host.
//!    We set that cookie in the viewer and load the webmail.
//!
//! The token is a bearer credential: it stays in memory, is never logged and never put in a
//! URL, and the cookie goes to the webmail origin only. The org token (the team-wide SSO
//! credential the same answer carries) is dropped unread. The only IPC is one command, callable
//! by the local "finish signing in" page and by nothing remote (see `capabilities/access.json`).

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{
    engine::{general_purpose::GeneralPurpose, general_purpose::GeneralPurposeConfig, DecodePaddingMode},
    alphabet, Engine,
};
use crypto_box::{
    aead::{Aead, OsRng},
    Nonce, PublicKey, SalsaBox, SecretKey,
};
use serde::Deserialize;
use tauri::{webview::cookie::{self, Cookie, SameSite}, AppHandle, Manager, Url, Webview};
use tokio::sync::Notify;

use crate::{viewer, AppState};

const COOKIE: &str = "CF_Authorization";
const TEAM_SUFFIX: &str = ".cloudflareaccess.com";
const TRANSFER_BASE: &str = "https://login.cloudflareaccess.org/";
/// The local page shown while the browser has the user.
const WAITING_PAGE: &str = "access.html";
/// How long the browser sign-in may take (cloudflared gives it ten polls of about a minute).
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);
/// One poll: the service holds a request for about a minute before answering 404.
const POLL_TIMEOUT: Duration = Duration::from_secs(90);
/// A page can't make the app open browser tabs in a loop.
const MIN_START_INTERVAL: Duration = Duration::from_secs(10);

/// Where the transfer service is. A debug build can point it (and the "browser") at a local
/// mock with POSTERN_ACCESS_MOCK=http://localhost:PORT; a release build never reads it.
struct Endpoints {
    transfer: String,
    mock: bool,
}

fn endpoints() -> Endpoints {
    #[cfg(debug_assertions)]
    if let Ok(m) = std::env::var("POSTERN_ACCESS_MOCK") {
        return Endpoints { transfer: format!("{}/", m.trim_end_matches('/')), mock: true };
    }
    Endpoints { transfer: TRANSFER_BASE.into(), mock: false }
}

// ---- pure parts ------------------------------------------------------------------------

/// If `url` is Access's login for the application at `app_host`, its application audience.
///
/// Access answers a request for a protected host with a redirect to
/// `https://<team>.cloudflareaccess.com/cdn-cgi/access/login/<app host>?kid=<audience>&…`.
/// Anything else (another team's login, another host, a look-alike domain) is not ours to take.
fn login_aud(url: &Url, app_host: &str, mock: bool) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let known = if mock {
        url.scheme() == "http" && host == "localhost"
    } else {
        url.scheme() == "https" && host.len() > TEAM_SUFFIX.len() && host.ends_with(TEAM_SUFFIX)
    };
    if !known {
        return None;
    }
    let mut segments = url.path_segments()?;
    if !(segments.next() == Some("cdn-cgi") && segments.next() == Some("access") && segments.next() == Some("login")) {
        return None;
    }
    if !segments.next()?.eq_ignore_ascii_case(app_host) || segments.next().is_some() {
        return None;
    }
    // Exactly one `kid`: with two, we and Cloudflare could read different ones.
    let mut kids = url.query_pairs().filter(|(k, _)| k == "kid").map(|(_, v)| v.into_owned());
    let kid = kids.next()?;
    let valid = (16..=128).contains(&kid.len()) && kid.bytes().all(|b| b.is_ascii_alphanumeric());
    if kids.next().is_some() || !valid {
        return None;
    }
    Some(kid)
}

/// Padding or not, as the services send it.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const LENIENT_URL: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// The ephemeral key pair of one sign-in. The public key doubles as the unguessable name under
/// which the transfer service holds the answer, so it is made fresh each time.
struct Keys {
    secret: SecretKey,
}

impl Keys {
    fn generate() -> Keys {
        Keys { secret: SecretKey::generate(&mut OsRng) }
    }

    /// base64url with padding, as cloudflared sends it.
    fn public(&self) -> String {
        base64::engine::general_purpose::URL_SAFE.encode(self.secret.public_key().as_bytes())
    }

    /// Opens what the service stored: `body` is base64 of nonce(24) || box, `service_key` the
    /// base64url public key from its `service-public-key` header.
    fn open(&self, body: &[u8], service_key: &str) -> Result<Vec<u8>, &'static str> {
        let raw = LENIENT.decode(String::from_utf8_lossy(body).trim()).map_err(|_| "answer is not base64")?;
        if raw.len() < 24 {
            return Err("answer too short");
        }
        let key: [u8; 32] = LENIENT_URL
            .decode(service_key.trim())
            .map_err(|_| "service key is not base64")?
            .try_into()
            .map_err(|_| "service key has the wrong length")?;
        SalsaBox::new(&PublicKey::from(key), &self.secret)
            .decrypt(Nonce::from_slice(&raw[..24]), &raw[24..])
            .map_err(|_| "answer did not decrypt")
    }
}

/// What the browser is sent to: the app's `/cdn-cgi/access/cli`, with the same parameters as
/// cloudflared (which also adds `redirect_url` pointing back at the app's host).
fn cli_url(app_origin: &Url, aud: &str, public_key: &str) -> Url {
    let mut base = app_origin.clone();
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    let mut back = base.clone();
    back.query_pairs_mut().append_pair("aud", aud).append_pair("token", public_key);
    let mut url = base;
    url.set_path("/cdn-cgi/access/cli");
    url.query_pairs_mut()
        .append_pair("aud", aud)
        .append_pair("edge_token_transfer", "true")
        .append_pair("redirect_url", back.as_str())
        .append_pair("send_org_token", "true")
        .append_pair("token", public_key);
    url
}

fn transfer_url(base: &str, public_key: &str) -> String {
    format!("{base}transfer/{public_key}")
}

/// The application token and when it expires (unix seconds).
struct AppToken {
    jwt: String,
    exp: i64,
}

/// Checks the shape of the answer before it becomes a cookie: a JWT for this application that
/// has not run out. The signature is Cloudflare's to check; the edge does it on every request.
fn app_token(plain: &[u8], aud: &str, now: i64) -> Result<AppToken, &'static str> {
    #[derive(Deserialize)]
    struct Answer {
        app_token: String,
    }
    #[derive(Deserialize)]
    struct Claims {
        exp: i64,
        aud: serde_json::Value,
    }
    let Answer { app_token: jwt } = serde_json::from_slice(plain).map_err(|_| "answer is not the expected JSON")?;
    let mut parts = jwt.split('.');
    let (Some(_), Some(payload), Some(_), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("token is not a JWT");
    };
    if !jwt.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err("token has unexpected characters");
    }
    let claims: Claims = LENIENT_URL
        .decode(payload)
        .ok()
        .and_then(|p| serde_json::from_slice(&p).ok())
        .ok_or("token claims unreadable")?;
    let for_us = match &claims.aud {
        serde_json::Value::String(a) => a == aud,
        serde_json::Value::Array(all) => all.iter().any(|a| a.as_str() == Some(aud)),
        _ => false,
    };
    if !for_us {
        return Err("token is for another application");
    }
    if claims.exp <= now + 30 {
        return Err("token has already expired");
    }
    Ok(AppToken { jwt, exp: claims.exp })
}

/// The cookie Access itself would set: a host cookie for the webmail, HttpOnly, and Secure on https.
fn session_cookie(host: &str, https: bool, token: &AppToken) -> Result<Cookie<'static>, &'static str> {
    let expires = cookie::time::OffsetDateTime::from_unix_timestamp(token.exp).map_err(|_| "bad expiry")?;
    Ok(Cookie::build((COOKIE, token.jwt.clone()))
        .domain(host.to_string())
        .path("/")
        .secure(https)
        .http_only(true)
        .same_site(SameSite::Lax)
        .expires(expires)
        .build())
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

// ---- the flow --------------------------------------------------------------------------

struct Active {
    id: u64,
    cancel: Arc<Notify>,
    /// Where the window goes when the sign-in is over.
    resume: Url,
}

#[derive(Default)]
struct State {
    next_id: u64,
    active: Option<Active>,
    /// The user (or a failure) chose the window: its next login stays in it.
    in_window: bool,
    /// A token was just handed over: if Access still sends the window to login, the hand-off
    /// does not work for this app, and looping through the browser would only annoy.
    handed_off: bool,
    last_start: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State { next_id: 0, active: None, in_window: false, handed_off: false, last_start: None });

pub enum Start {
    /// The browser has it; the navigation is dropped.
    Handled,
    /// Let the login load in the window.
    InWindow,
}

/// The audience of the Access login a navigation is heading for, when the option is on.
pub fn login_target(app: &AppHandle, to: &Url) -> Option<String> {
    let viewer_url = {
        let state = app.state::<AppState>();
        let cfg = state.config.lock().unwrap();
        if !cfg.access_browser_signin {
            return None;
        }
        cfg.viewer_url.clone()
    };
    let base = Url::parse(viewer_url.trim()).ok()?;
    login_aud(to, base.host_str()?, endpoints().mock)
}

/// The local page that says the browser has the user. Only while a sign-in is running.
pub fn is_waiting_page(url: &Url) -> bool {
    is_local_page(url) && STATE.lock().unwrap().active.is_some()
}

/// Where the app's own pages are served from (Tauri's asset protocol; WebView2 needs the http form).
const LOCAL_ORIGIN: &str = if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" };

/// Not `Url::origin`: for a custom scheme like tauri:// that is opaque and never equal.
fn is_local_page(url: &Url) -> bool {
    let local = Url::parse(LOCAL_ORIGIN).expect("static url");
    url.scheme() == local.scheme()
        && url.host_str() == local.host_str()
        && url.port() == local.port()
        && url.username().is_empty()
        && url.path() == format!("/{WAITING_PAGE}")
}

fn waiting_url(host: &str) -> Option<Url> {
    let mut url = Url::parse(&format!("{LOCAL_ORIGIN}/{WAITING_PAGE}")).ok()?;
    url.query_pairs_mut().append_pair("host", host);
    Some(url)
}

/// A fresh viewer starts over: nothing running, no earlier choice.
pub fn reset() {
    cancel_flow();
    let mut st = STATE.lock().unwrap();
    st.in_window = false;
    st.handed_off = false;
}

/// A page of the webmail finished loading: signed in, so the next expiry may use the browser again.
pub fn signed_in() {
    let mut st = STATE.lock().unwrap();
    st.in_window = false;
    st.handed_off = false;
}

fn cancel_flow() -> Option<Url> {
    let old = STATE.lock().unwrap().active.take()?;
    old.cancel.notify_one();
    Some(old.resume)
}

/// Starts the browser sign-in for a navigation that was heading for Access's login.
pub fn begin(app: &AppHandle, aud: String, resume: Url) -> Start {
    let viewer_url = app.state::<AppState>().config.lock().unwrap().viewer_url.clone();
    let Ok(origin) = Url::parse(viewer_url.trim()) else { return Start::InWindow };
    let mock = endpoints().mock;
    if !(origin.scheme() == "https" || (mock && origin.scheme() == "http")) || origin.host_str().is_none() {
        return Start::InWindow; // never hand a token to a page that is not https
    }
    let (id, cancel) = {
        let mut st = STATE.lock().unwrap();
        if st.active.is_some() {
            drop(st);
            show_waiting(app, origin.host_str().unwrap_or_default());
            return Start::Handled;
        }
        if st.in_window {
            return Start::InWindow;
        }
        if st.handed_off {
            crate::log("access: sent to login again right after the browser hand-off; signing in in the window instead");
            st.in_window = true;
            return Start::InWindow;
        }
        if st.last_start.is_some_and(|t| t.elapsed() < MIN_START_INTERVAL) {
            crate::log("access: the browser was opened moments ago; signing in in the window instead");
            return Start::InWindow;
        }
        st.last_start = Some(Instant::now());
        st.next_id += 1;
        let cancel = Arc::new(Notify::new());
        st.active = Some(Active { id: st.next_id, cancel: cancel.clone(), resume: resume.clone() });
        (st.next_id, cancel)
    };
    crate::log("access: signing in to Cloudflare Access in the browser");
    show_waiting(app, origin.host_str().unwrap_or_default());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let outcome = tokio::select! {
            r = tokio::time::timeout(SIGN_IN_TIMEOUT, fetch_token(&origin, &aud)) => r.unwrap_or(Err("timed out".into())),
            _ = cancel.notified() => return,
        };
        finish(&app, id, &origin, outcome);
    });
    Start::Handled
}

/// Off the navigation handler (it runs inside the webview's own decision): the page swap happens
/// a moment later on another thread.
fn show_waiting(app: &AppHandle, host: &str) {
    let (app, host) = (app.clone(), host.to_string());
    tauri::async_runtime::spawn(async move {
        // Not if the sign-in already ended: the page would cover the webmail it just loaded.
        if STATE.lock().unwrap().active.is_none() {
            return;
        }
        if let (Some(w), Some(url)) = (app.get_webview_window(viewer::LABEL), waiting_url(&host)) {
            let _ = w.navigate(url);
        }
    });
}

async fn fetch_token(origin: &Url, aud: &str) -> Result<AppToken, String> {
    let ep = endpoints();
    let keys = Keys::generate();
    let public = keys.public();
    let open = cli_url(origin, aud, &public);
    if ep.mock {
        // A test stands in for the user and their browser: it is the request that signs in.
        let c = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().map_err(|e| e.to_string())?;
        c.get(open).send().await.map_err(|e| format!("mock browser: {e}"))?;
    } else if let Err(e) = tauri_plugin_opener::open_url(open.as_str(), None::<&str>) {
        return Err(format!("no browser: {e}"));
    }
    let client = reqwest::Client::builder()
        .timeout(POLL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("Postern/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let url = transfer_url(&ep.transfer, &public);
    let mut errors = 0;
    loop {
        let started = Instant::now();
        match client.get(&url).send().await {
            // Anything but 200 and a server error means "not yet": the user is still signing in.
            Ok(r) if r.status() == reqwest::StatusCode::OK => {
                let service_key = r.headers().get("service-public-key").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                let body = r.bytes().await.map_err(|e| e.to_string())?;
                let plain = keys.open(&body, &service_key).map_err(str::to_string)?;
                return app_token(&plain, aud, now()).map_err(str::to_string);
            }
            Ok(r) if r.status().is_server_error() => return Err(format!("transfer service answered {}", r.status().as_u16())),
            Ok(_) => errors = 0,
            Err(e) => {
                errors += 1;
                if errors > 3 {
                    return Err(format!("transfer service unreachable: {}", e.without_url()));
                }
            }
        }
        // The service holds a request for about a minute; if it answers at once, don't spin.
        if started.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

/// The sign-in ended: with the token, set the cookie and load the webmail; without, the window.
fn finish(app: &AppHandle, id: u64, origin: &Url, outcome: Result<AppToken, String>) {
    let resume = {
        let mut st = STATE.lock().unwrap();
        if st.active.as_ref().map(|a| a.id) != Some(id) {
            return; // cancelled meanwhile
        }
        st.active.take().map(|a| a.resume)
    };
    let Some(resume) = resume else { return };
    let token = match outcome {
        Ok(t) => t,
        Err(e) => {
            crate::log(&format!("access: browser sign-in failed ({e}); signing in in the window"));
            to_window(app, resume);
            return;
        }
    };
    let host = origin.host_str().unwrap_or_default().to_string();
    let https = origin.scheme() == "https";
    let app = app.clone();
    // Off the async threads: the cookie store call waits for the webview's own thread.
    tauri::async_runtime::spawn_blocking(move || {
        let Some(w) = app.get_webview_window(viewer::LABEL) else {
            crate::log("access: the window was closed; the token is dropped");
            return;
        };
        match session_cookie(&host, https, &token).and_then(|c| w.set_cookie(c).map_err(|_| "the webview refused the cookie")) {
            Ok(()) => {
                STATE.lock().unwrap().handed_off = true;
                crate::log("access: signed in through the browser; loading the webmail");
                let _ = w.navigate(resume);
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
            Err(e) => {
                crate::log(&format!("access: could not set the session ({e}); signing in in the window"));
                to_window(&app, resume);
            }
        }
    });
}

/// Falls back to Access's own login inside the window.
fn to_window(app: &AppHandle, resume: Url) {
    STATE.lock().unwrap().in_window = true;
    if let Some(w) = app.get_webview_window(viewer::LABEL) {
        let _ = w.navigate(resume);
    }
}

/// "Sign in here instead": ends the browser sign-in and lets the window do it.
#[tauri::command]
pub fn access_use_window(app: AppHandle, webview: Webview) {
    // Only the local waiting page in the viewer; the webmail has no way in (see the capability).
    if webview.label() != viewer::LABEL || !webview.url().is_ok_and(|u| is_local_page(&u)) {
        return;
    }
    crate::log("access: signing in in the window instead of the browser");
    let resume = cancel_flow();
    if let Some(resume) = resume {
        to_window(&app, resume);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUD: &str = "54a2e7fba37e9db300252dcee0956a4b8674d31a02b1d39b29a69423ec69123d";

    fn login(host: &str, path: &str, query: &str) -> Url {
        Url::parse(&format!("https://{host}{path}?{query}")).unwrap()
    }

    #[test]
    fn only_this_apps_access_login_is_taken() {
        let ok = login("team.cloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com", &format!("kid={AUD}&redirect_url=%2F"));
        assert_eq!(login_aud(&ok, "webmail.example.com", false).as_deref(), Some(AUD));
        assert_eq!(login_aud(&ok, "Webmail.Example.com", false).as_deref(), Some(AUD));
        // Another application, a look-alike domain, plain http, a link with no login path.
        assert!(login_aud(&ok, "other.example.com", false).is_none());
        let lookalike = login("team.cloudflareaccess.com.evil.example", "/cdn-cgi/access/login/webmail.example.com", &format!("kid={AUD}"));
        assert!(login_aud(&lookalike, "webmail.example.com", false).is_none());
        let suffix = login("evilcloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com", &format!("kid={AUD}"));
        assert!(login_aud(&suffix, "webmail.example.com", false).is_none());
        let bare = login(TEAM_SUFFIX.trim_start_matches('.'), "/cdn-cgi/access/login/webmail.example.com", &format!("kid={AUD}"));
        assert!(login_aud(&bare, "webmail.example.com", false).is_none());
        let http = Url::parse(&format!("http://team.cloudflareaccess.com/cdn-cgi/access/login/webmail.example.com?kid={AUD}")).unwrap();
        assert!(login_aud(&http, "webmail.example.com", false).is_none());
        let page = login("team.cloudflareaccess.com", "/docs", &format!("kid={AUD}"));
        assert!(login_aud(&page, "webmail.example.com", false).is_none());
        let deeper = login("team.cloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com/x", &format!("kid={AUD}"));
        assert!(login_aud(&deeper, "webmail.example.com", false).is_none());
        // The audience must be there exactly once and look like one.
        let none = login("team.cloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com", "redirect_url=%2F");
        assert!(login_aud(&none, "webmail.example.com", false).is_none());
        let twice = login("team.cloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com", &format!("kid={AUD}&kid={AUD}"));
        assert!(login_aud(&twice, "webmail.example.com", false).is_none());
        let odd = login("team.cloudflareaccess.com", "/cdn-cgi/access/login/webmail.example.com", "kid=a%26b%3Dc");
        assert!(login_aud(&odd, "webmail.example.com", false).is_none());
    }

    #[test]
    fn the_mock_login_is_localhost_only_and_only_in_mock_mode() {
        let mock = Url::parse(&format!("http://localhost:4000/cdn-cgi/access/login/127.0.0.1?kid={AUD}")).unwrap();
        assert!(login_aud(&mock, "127.0.0.1", true).is_some());
        assert!(login_aud(&mock, "127.0.0.1", false).is_none());
    }

    #[test]
    fn the_browser_url_matches_cloudflared() {
        let origin = Url::parse("https://webmail.example.com/some/path?x=1").unwrap();
        let url = cli_url(&origin, AUD, "KEY-_=");
        assert_eq!(url.origin(), origin.origin());
        assert_eq!(url.path(), "/cdn-cgi/access/cli");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["aud"], AUD);
        assert_eq!(q["token"], "KEY-_=");
        assert_eq!(q["edge_token_transfer"], "true");
        assert_eq!(q["send_org_token"], "true");
        let back = Url::parse(&q["redirect_url"]).unwrap();
        assert_eq!(back.origin(), origin.origin());
        assert_eq!(back.path(), "/");
        assert!(back.query().unwrap().contains("token=KEY-_%3D"));
        assert_eq!(transfer_url(TRANSFER_BASE, "KEY-_="), "https://login.cloudflareaccess.org/transfer/KEY-_=");
    }

    /// What the service does: seal to our public key from its own key pair, nonce in front.
    fn seal(to: &str, plain: &[u8]) -> (String, String) {
        use crypto_box::aead::AeadCore;
        let service = SecretKey::generate(&mut OsRng);
        let to: [u8; 32] = LENIENT_URL.decode(to).unwrap().try_into().unwrap();
        let salsa = SalsaBox::new(&PublicKey::from(to), &service);
        let nonce = SalsaBox::generate_nonce(&mut OsRng);
        let mut raw = nonce.to_vec();
        raw.extend(salsa.encrypt(&nonce, plain).unwrap());
        (
            base64::engine::general_purpose::STANDARD.encode(raw),
            base64::engine::general_purpose::URL_SAFE.encode(service.public_key().as_bytes()),
        )
    }

    fn jwt(aud: serde_json::Value, exp: i64) -> String {
        let enc = |v: serde_json::Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        format!("{}.{}.c2ln", enc(serde_json::json!({"alg": "RS256"})), enc(serde_json::json!({"aud": aud, "exp": exp})))
    }

    #[test]
    fn an_answer_sealed_to_our_key_opens_and_nothing_else_does() {
        let keys = Keys::generate();
        let (body, service_key) = seal(&keys.public(), b"hello");
        assert_eq!(keys.open(body.as_bytes(), &service_key).unwrap(), b"hello");
        assert_eq!(keys.open(format!("{body}\n").as_bytes(), &service_key).unwrap(), b"hello");
        // Someone else's key pair, a different sender, a cut-off or damaged answer.
        assert!(Keys::generate().open(body.as_bytes(), &service_key).is_err());
        let (_, other_service) = seal(&keys.public(), b"x");
        assert!(keys.open(body.as_bytes(), &other_service).is_err());
        assert!(keys.open(b"AAAA", &service_key).is_err());
        assert!(keys.open(b"!!!", &service_key).is_err());
        assert!(keys.open(body.as_bytes(), "short").is_err());
        let mut raw = LENIENT.decode(&body).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        assert!(keys.open(base64::engine::general_purpose::STANDARD.encode(raw).as_bytes(), &service_key).is_err());
    }

    #[test]
    fn the_token_must_be_a_live_jwt_for_this_app() {
        let answer = |jwt: &str| format!(r#"{{"app_token":"{jwt}","org_token":"ignored"}}"#);
        let good = jwt(serde_json::json!([AUD]), 5000);
        let t = app_token(answer(&good).as_bytes(), AUD, 1000).unwrap();
        assert_eq!((t.jwt.as_str(), t.exp), (good.as_str(), 5000));
        assert!(app_token(answer(&jwt(serde_json::json!(AUD), 5000)).as_bytes(), AUD, 1000).is_ok());
        assert!(app_token(answer(&jwt(serde_json::json!(["other"]), 5000)).as_bytes(), AUD, 1000).is_err());
        assert!(app_token(answer(&good).as_bytes(), AUD, 6000).is_err());
        assert!(app_token(answer(&jwt(serde_json::json!([AUD]), 1010)).as_bytes(), AUD, 1000).is_err());
        assert!(app_token(answer("not-a-jwt").as_bytes(), AUD, 1000).is_err());
        assert!(app_token(answer("a.b.c;d").as_bytes(), AUD, 1000).is_err());
        assert!(app_token(b"{}", AUD, 1000).is_err());
        assert!(app_token(b"nope", AUD, 1000).is_err());
    }

    #[test]
    fn only_our_waiting_page_is_the_local_page() {
        let url = waiting_url("webmail.example.com").unwrap();
        assert!(is_local_page(&url));
        assert!(is_local_page(&Url::parse(&format!("{LOCAL_ORIGIN}/{WAITING_PAGE}")).unwrap()));
        assert!(!is_local_page(&Url::parse(&format!("{LOCAL_ORIGIN}/settings.html")).unwrap()));
        assert!(!is_local_page(&Url::parse("https://webmail.example.com/access.html").unwrap()));
        assert!(!is_local_page(&Url::parse("tauri://localhost.evil.example/access.html").unwrap()));
        assert!(!is_local_page(&Url::parse("http://tauri.localhost@evil.example/access.html").unwrap()));
    }

    #[test]
    fn the_cookie_is_a_host_cookie_for_the_webmail() {
        let t = AppToken { jwt: "a.b.c".into(), exp: 1_800_000_000 };
        let c = session_cookie("webmail.example.com", true, &t).unwrap();
        assert_eq!((c.name(), c.value()), ("CF_Authorization", "a.b.c"));
        assert_eq!(c.domain(), Some("webmail.example.com"));
        assert_eq!(c.path(), Some("/"));
        assert_eq!((c.secure(), c.http_only(), c.same_site()), (Some(true), Some(true), Some(SameSite::Lax)));
        assert_eq!(c.expires_datetime().map(|d| d.unix_timestamp()), Some(1_800_000_000));
        assert_eq!(session_cookie("127.0.0.1", false, &t).unwrap().secure(), Some(false));
    }
}

//! OAuth 2.0 sign-in to Stalwart, no passwords stored anywhere (RFC 8252 native-app flow).
//!
//! - Discovery: `<server>/.well-known/oauth-authorization-server`.
//! - Client: registered on the fly (RFC 7591) as a public client. Stalwart answers with a
//!   stateless, signed client_id; it allows that for loopback redirects without an admin step.
//! - Sign-in: authorization code + PKCE (S256) in the user's default browser, so whatever the
//!   server asks for there (password, 2FA, passkey, SSO) stays between the browser and Stalwart.
//!   The code comes back to a one-shot listener on 127.0.0.1 (any port matches a loopback
//!   registration).
//! - Tokens: only the refresh token is kept, in the OS keyring. Stalwart rotates it on every
//!   refresh, so the new one is stored each time. Access tokens live in memory.

use std::{
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Manager};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

use crate::{config, AppState};

const CLIENT_NAME: &str = "Postern";
const REDIRECT_PATH: &str = "/callback";
/// Mail and calendars for JMAP; offline_access for the refresh token. Nothing administrative.
const SCOPES: &str = "openid offline_access urn:ietf:params:oauth:scope:mail urn:ietf:params:oauth:scope:calendars";
const KEYRING_REFRESH: &str = config::KEYRING_REFRESH;
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug)]
pub enum AuthError {
    /// Not signed in, or the server no longer accepts the grant: the user has to sign in again.
    SignInNeeded(String),
    /// Server unreachable or answered oddly: try again later.
    Temporary(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::SignInNeeded(e) | AuthError::Temporary(e) => f.write_str(e),
        }
    }
}

/// An access token in memory, with when to stop using it.
#[derive(Clone)]
pub struct AccessToken {
    pub token: String,
    pub renew_at: Instant,
}

#[derive(Deserialize)]
struct Metadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    error_description: Option<String>,
}

/// One client for every OAuth request, so connections are reused.
static HTTP: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
});

fn http() -> Result<reqwest::Client, String> {
    HTTP.clone()
}

/// A rotated refresh token the keyring would not take. The server has already retired the
/// previous one, so this is the only valid grant: used before the keyring and saved again on
/// the next refresh. Guarded by `AppState::token` like everything else in a refresh.
static UNSAVED_REFRESH: Mutex<Option<String>> = Mutex::new(None);

/// How long one loopback connection may take to send its request line before it is dropped.
const LOOPBACK_READ_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(5) };

/// https, or plain http only to this machine (a test server). Everything the app talks to or
/// opens for a sign-in goes through this, including endpoints a server's metadata names.
pub fn secure_url(s: &str) -> Result<Url, String> {
    let url = Url::parse(s.trim()).map_err(|e| e.to_string())?;
    let local = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match url.scheme() {
        "https" if url.host().is_some() => Ok(url),
        "http" if local => Ok(url),
        _ => Err("must use https".into()),
    }
}

/// `scheme://host[:port]/` of the mail server, whatever path the configured URL has.
pub fn server_base(jmap_url: &str) -> Result<Url, String> {
    let url = Url::parse(jmap_url.trim()).map_err(|e| format!("Mail server URL: {e}"))?;
    let host = url.host_str().ok_or("Mail server URL has no host")?;
    let mut base = format!("{}://{}", url.scheme(), host);
    if let Some(port) = url.port() {
        base.push_str(&format!(":{port}"));
    }
    Url::parse(&base).map_err(|e| e.to_string())
}

async fn discover(jmap_url: &str) -> Result<Metadata, String> {
    let url = server_base(jmap_url)?.join("/.well-known/oauth-authorization-server").map_err(|e| e.to_string())?;
    let resp = http()?.get(url).send().await.map_err(|e| format!("OAuth discovery: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("OAuth discovery: HTTP {} (does the server support OAuth?)", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("OAuth discovery: {e}"))
}

async fn register(meta: &Metadata) -> Result<String, String> {
    let endpoint = meta.registration_endpoint.as_deref().ok_or("the server does not offer client registration")?;
    let body = serde_json::json!({
        "client_name": CLIENT_NAME,
        "redirect_uris": [format!("http://127.0.0.1{REDIRECT_PATH}")],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": SCOPES,
    });
    let resp = http()?
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("client registration: {e}"))?;
    let status = resp.status();
    let value: serde_json::Value =
        serde_json::from_slice(&resp.bytes().await.map_err(|e| e.to_string())?).unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "client registration: HTTP {status}: {}",
            value["error_description"].as_str().or(value["error"].as_str()).unwrap_or("no details")
        ));
    }
    value["client_id"].as_str().map(str::to_string).ok_or_else(|| "client registration: no client_id".into())
}

pub fn random_url_safe(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

fn form(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

async fn token_request(endpoint: &str, pairs: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
    let endpoint = secure_url(endpoint).map_err(|e| AuthError::SignInNeeded(format!("token endpoint {e}")))?;
    let resp = http()
        .map_err(AuthError::Temporary)?
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(pairs))
        .send()
        .await
        .map_err(|e| AuthError::Temporary(format!("token request: {e}")))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| AuthError::Temporary(e.to_string()))?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|e| AuthError::Temporary(format!("token response: {e}")));
    }
    match serde_json::from_slice::<TokenError>(&bytes) {
        // RFC 6749 5.2: these mean the grant itself is no good any more.
        Ok(e) if matches!(e.error.as_str(), "invalid_grant" | "invalid_client" | "unauthorized_client") => {
            Err(AuthError::SignInNeeded(format!("{}: {}", e.error, e.error_description.unwrap_or_default())))
        }
        Ok(e) => Err(AuthError::Temporary(format!("{}: {}", e.error, e.error_description.unwrap_or_default()))),
        Err(_) => Err(AuthError::Temporary(format!("token request: HTTP {status}"))),
    }
}

fn keep(token: &TokenResponse) -> AccessToken {
    // Renew a minute early, and never trust a lifetime under two minutes for longer than half of it.
    // A lifetime under 30 s (or 0) is taken as 30 s: otherwise renewal would loop. If the token
    // really dies sooner, the server's 401 drops it (watcher::is_auth_error).
    // Capped at a day: a huge value must not overflow the Instant below.
    let life = token.expires_in.unwrap_or(3600).clamp(30, 86_400);
    let margin = if life > 120 { 60 } else { life / 2 };
    AccessToken { token: token.access_token.clone(), renew_at: Instant::now() + Duration::from_secs(life - margin) }
}

/// What came back to a loopback listener.
#[derive(Debug, PartialEq)]
pub enum LoopbackError {
    /// The other side said no (`error=`), e.g. `access_denied`.
    Refused(String),
    NoCode,
    Io(String),
}

impl std::fmt::Display for LoopbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoopbackError::Refused(e) => write!(f, "sign-in refused: {e}"),
            LoopbackError::NoCode => f.write_str("the sign-in reply had no code"),
            LoopbackError::Io(e) => write!(f, "local listener: {e}"),
        }
    }
}

/// Reads up to the end of the request head. `None` for a connection that sends nothing in time
/// (browsers open spare "preconnect" sockets that never carry a request) or breaks.
async fn read_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let read = async {
        let mut buf = vec![0u8; 8192];
        let mut len = 0;
        while len < buf.len() {
            let n = stream.read(&mut buf[len..]).await.ok()?;
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        (len > 0).then(|| String::from_utf8_lossy(&buf[..len]).into_owned())
    };
    tokio::time::timeout(LOOPBACK_READ_TIMEOUT, read).await.ok().flatten()
}

/// Waits for the browser to come back to the loopback listener at `path`. Returns the `code`.
/// `who` names the app on the page the browser shows afterwards. Connections are served one at
/// a time, each with a short read deadline; anything that isn't a reply with our `state` is
/// answered and ignored, so a stray request can't end the wait.
pub async fn wait_for_code(listener: TcpListener, path: &str, state: &str, who: &str) -> Result<String, LoopbackError> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(|e| LoopbackError::Io(e.to_string()))?;
        let Some(head) = read_head(&mut stream).await else { continue };
        let target = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let url = match Url::parse(&format!("http://127.0.0.1{target}")) {
            Ok(u) if u.path() == path => u,
            _ => {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue; // favicon and the like
            }
        };
        let param = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        if param("state").as_deref() != Some(state) {
            // Not this sign-in's reply (an old tab, another program): keep waiting for ours.
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        }
        let result = if let Some(err) = param("error") {
            Err(LoopbackError::Refused(err))
        } else {
            param("code").ok_or(LoopbackError::NoCode)
        };
        let (title, text) = match &result {
            Ok(_) => ("Signed in", format!("{who} is signed in. You can close this tab.")),
            Err(LoopbackError::Refused(_)) => ("Not signed in", format!("{who} was not signed in. You can close this tab.")),
            Err(_) => ("Sign-in failed", format!("{who} could not sign in. You can close this tab and try again.")),
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title><body style=\"font:16px system-ui;margin:15vh auto;max-width:28em;padding:0 1em\"><h1>{title}</h1><p>{text}</p>"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return result;
    }
}

pub struct SignedIn {
    pub username: String,
}

/// Full interactive sign-in. Stores the client and endpoints in `cfg` (the caller saves it),
/// the refresh token in the keyring, and the access token in memory.
pub async fn sign_in(app: &AppHandle, cfg: &mut config::Config) -> Result<SignedIn, String> {
    let meta = discover(&cfg.jmap_url).await?;
    let check = |what: &str, u: &str| secure_url(u).map(|_| ()).map_err(|e| format!("the server's {what} endpoint {e}"));
    check("authorization", &meta.authorization_endpoint)?;
    check("token", &meta.token_endpoint)?;
    if let Some(u) = &meta.registration_endpoint {
        check("registration", u)?;
    }
    if let Some(u) = &meta.revocation_endpoint {
        check("revocation", u)?;
    }
    // Stateless client ids are cheap; register again whenever the server or endpoints changed.
    if cfg.oauth_client_id.is_empty() || cfg.oauth_token_endpoint != meta.token_endpoint {
        cfg.oauth_client_id = register(&meta).await?;
    }
    cfg.oauth_token_endpoint = meta.token_endpoint.clone();
    cfg.oauth_revocation_endpoint = meta.revocation_endpoint.clone().unwrap_or_default();

    let verifier = random_url_safe(32)?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_safe(16)?;
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| format!("local listener: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}{REDIRECT_PATH}");

    let mut authorize = secure_url(&meta.authorization_endpoint).map_err(|e| format!("authorization endpoint: {e}"))?;
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &cfg.oauth_client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    crate::log("sign-in: opening the browser");
    tauri_plugin_opener::open_url(authorize.as_str(), None::<&str>).map_err(|e| format!("opening the browser: {e}"))?;

    let code = tokio::time::timeout(SIGN_IN_TIMEOUT, wait_for_code(listener, REDIRECT_PATH, &state, CLIENT_NAME))
        .await
        .map_err(|_| "no answer from the browser within 5 minutes".to_string())?
        .map_err(|e| e.to_string())?;

    let token = token_request(
        &cfg.oauth_token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &cfg.oauth_client_id),
            ("code_verifier", &verifier),
        ],
    )
    .await
    .map_err(|e| e.to_string())?;
    let refresh = token.refresh_token.as_deref().ok_or("the server issued no refresh token")?;
    let state = app.state::<AppState>();
    let mut cached = state.token.lock().await;
    // The grant is good even if the keyring is not: keep it in memory and retry, as on refresh.
    match config::set_secret(KEYRING_REFRESH, refresh) {
        Ok(()) => *UNSAVED_REFRESH.lock().unwrap() = None,
        Err(e) => {
            crate::log(&format!("keyring: could not save the token ({e}); keeping it in memory, will retry"));
            *UNSAVED_REFRESH.lock().unwrap() = Some(refresh.to_string());
        }
    }
    *cached = Some(keep(&token));
    drop(cached);
    crate::log("sign-in: done");
    Ok(SignedIn { username: token.username.unwrap_or_default() })
}

/// The refresh token in use: one the keyring refused to take, else the keyring's. An error
/// means the keyring could not be read just now (not that there is no token).
fn current_refresh() -> Result<Option<String>, String> {
    if let Some(t) = UNSAVED_REFRESH.lock().unwrap().clone() {
        return Ok(Some(t));
    }
    config::secret(KEYRING_REFRESH)
}

/// Tries again to save a refresh token the keyring refused earlier (the caller holds the token lock).
fn retry_unsaved() {
    let mut unsaved = UNSAVED_REFRESH.lock().unwrap();
    if let Some(token) = unsaved.as_deref() {
        if config::set_secret(KEYRING_REFRESH, token).is_ok() {
            crate::log("keyring: saved the renewed token");
            *unsaved = None;
        }
    }
}

/// Forgets the grant locally (the caller holds the token lock).
fn forget(cached: &mut Option<AccessToken>) {
    config::delete_secret(KEYRING_REFRESH);
    *UNSAVED_REFRESH.lock().unwrap() = None;
    *cached = None;
}

pub fn is_signed_in(cfg: &config::Config) -> bool {
    // A keyring that can't be read right now is not a sign-out.
    !cfg.oauth_client_id.is_empty() && !matches!(current_refresh(), Ok(None))
}

/// A valid access token, refreshing (and rotating the stored refresh token) when needed.
/// The refresh runs as its own task: if the caller gives up half-way (the watcher restarts),
/// a token the server has already rotated is still stored instead of being lost.
pub async fn access_token(app: &AppHandle) -> Result<AccessToken, AuthError> {
    let app = app.clone();
    tauri::async_runtime::spawn(async move { refresh_or_cached(&app).await })
        .await
        .unwrap_or_else(|e| Err(AuthError::Temporary(format!("token task: {e}"))))
}

async fn refresh_or_cached(app: &AppHandle) -> Result<AccessToken, AuthError> {
    let state = app.state::<AppState>();
    let mut cached = state.token.lock().await; // held across the refresh: one rotation at a time
    if let Some(t) = cached.as_ref().filter(|t| Instant::now() < t.renew_at) {
        retry_unsaved();
        return Ok(t.clone());
    }
    let cfg = state.config.lock().unwrap().clone();
    let refresh = match current_refresh() {
        Ok(Some(r)) if !cfg.oauth_client_id.is_empty() => r,
        Ok(_) => return Err(AuthError::SignInNeeded("not signed in".into())),
        Err(e) => return Err(AuthError::Temporary(format!("keyring: {e}"))),
    };
    let token = match token_request(
        &cfg.oauth_token_endpoint,
        &[("grant_type", "refresh_token"), ("refresh_token", &refresh), ("client_id", &cfg.oauth_client_id)],
    )
    .await
    {
        Ok(t) => t,
        Err(AuthError::SignInNeeded(e)) => {
            // The server no longer takes this grant: drop it, so Settings offers Sign in again.
            forget(&mut cached);
            return Err(AuthError::SignInNeeded(e));
        }
        Err(e) => return Err(e),
    };
    // The new access token is good whatever happens to the keyring below.
    let t = keep(&token);
    *cached = Some(t.clone());
    if let Some(rotated) = token.refresh_token.as_deref() {
        // The server has retired the old refresh token: the rotated one must not be lost.
        match config::set_secret(KEYRING_REFRESH, rotated) {
            Ok(()) => *UNSAVED_REFRESH.lock().unwrap() = None,
            Err(e) => {
                crate::log(&format!("keyring: could not save the renewed token ({e}); keeping it in memory, will retry"));
                *UNSAVED_REFRESH.lock().unwrap() = Some(rotated.to_string());
            }
        }
    }
    crate::log("access token renewed");
    Ok(t)
}

/// Forgets the grant locally without telling the server (a sign-in that can't be kept).
pub async fn discard(app: &AppHandle) {
    let state = app.state::<AppState>();
    let mut cached = state.token.lock().await;
    forget(&mut cached);
}

/// Revokes the refresh token on the server (best effort) and forgets everything locally.
/// Holds the token lock throughout, so a refresh in flight can't store a new grant afterwards.
/// Returns whether the server confirmed the revocation.
pub async fn sign_out(app: &AppHandle) -> bool {
    let state = app.state::<AppState>();
    let mut cached = state.token.lock().await;
    let cfg = state.config.lock().unwrap().clone();
    let mut revoked = false;
    if let (Ok(Some(refresh)), Ok(endpoint)) = (current_refresh(), secure_url(&cfg.oauth_revocation_endpoint)) {
        if let Ok(client) = http() {
            let resp = client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(form(&[("token", &refresh), ("token_type_hint", "refresh_token"), ("client_id", &cfg.oauth_client_id)]))
                .send()
                .await;
            match resp {
                Ok(r) if r.status().is_success() => revoked = true,
                Ok(r) => crate::log(&format!("sign-out: revocation answered HTTP {}", r.status())),
                Err(e) => crate::log(&format!("sign-out: revocation failed: {e}")),
            }
        }
    }
    forget(&mut cached);
    revoked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_base_strips_path() {
        assert_eq!(server_base("https://mail.example.com/jmap/session").unwrap().as_str(), "https://mail.example.com/");
        assert_eq!(server_base("https://mail.example.com:8443").unwrap().as_str(), "https://mail.example.com:8443/");
    }

    #[test]
    fn only_https_or_loopback_http() {
        assert!(secure_url("https://mail.example.com/x").is_ok());
        assert!(secure_url("http://localhost:8080").is_ok());
        assert!(secure_url("http://127.0.0.1/").is_ok());
        assert!(secure_url("http://[::1]/").is_ok());
        assert!(secure_url("http://mail.example.com").is_err());
        assert!(secure_url("http://localhost.evil.com").is_err());
        assert!(secure_url("javascript:alert(1)").is_err());
        assert!(secure_url("file:///C:/x").is_err());
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 Appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_listener_returns_code_and_checks_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(listener, "/callback", "s1", "Test").await });
        let send = |path: String| async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).await.unwrap();
            out
        };
        assert!(send("/favicon.ico".into()).await.starts_with("HTTP/1.1 404"));
        assert!(send("/callback?code=abc&state=s1".into()).await.contains("Signed in"));
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");

        // Bulwark's "Not now" in the browser
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port3 = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(listener, "/bulwark-desktop/callback", "s1", "Test").await });
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port3)).await.unwrap();
        s.write_all(b"GET /bulwark-desktop/callback?error=access_denied&state=s1 HTTP/1.1\r\n\r\n").await.unwrap();
        assert_eq!(waiter.await.unwrap(), Err(LoopbackError::Refused("access_denied".into())));

    }

    async fn get(port: u16, path: &str) -> String {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_listener_ignores_wrong_state_and_keeps_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(listener, "/callback", "s1", "Test").await });
        assert!(get(port, "/callback?code=evil&state=forged").await.starts_with("HTTP/1.1 400"));
        assert!(get(port, "/callback?error=access_denied").await.starts_with("HTTP/1.1 400"));
        assert!(get(port, "/callback?code=abc&state=s1").await.contains("Signed in"));
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loopback_listener_survives_an_idle_preconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiter = tokio::spawn(async move { wait_for_code(listener, "/callback", "s1", "Test").await });
        // A browser's spare socket: connects first, never sends anything.
        let _idle = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(3), get(port, "/callback?code=abc&state=s1")).await;
        assert!(reply.expect("the real request was not served behind the idle socket").contains("Signed in"));
        assert_eq!(waiter.await.unwrap().unwrap(), "abc");
    }

    #[test]
    fn tiny_or_zero_lifetimes_do_not_renew_at_once() {
        let token = |life| TokenResponse { access_token: "t".into(), expires_in: Some(life), refresh_token: None, username: None };
        for life in [0, 1, 10] {
            assert!(keep(&token(life)).renew_at >= Instant::now() + Duration::from_secs(14));
        }
        assert!(keep(&token(3600)).renew_at >= Instant::now() + Duration::from_secs(3539));
    }
}

//! "Sign in with your browser" for the Bulwark window.
//!
//! Bulwark's login page offers that button when its desktop app's bridge is present
//! (`window.bulwarkDesktop`, version 1, `auth.claim()`; upstream lib/desktop/browser-sign-in.ts).
//! Our viewer provides only that part of the bridge. The flow is Bulwark's own "instance mode":
//!
//! 1. The page calls `begin`: we bind a one-shot listener on 127.0.0.1 and return its
//!    redirect URI (`/bulwark-desktop/callback`, the only path Bulwark accepts) and a state.
//! 2. The page builds `<instance>/<locale>/login?mobile_redirect_uri=…&mobile_state=…&mobile_code_challenge=…`
//!    and calls `complete`: we open it in the default browser, where the user signs in to
//!    Bulwark however it allows (password manager, passkey, Stalwart OAuth, SSO). Bulwark parks
//!    the session server-side and sends the browser to our listener with a one-time code.
//! 3. The page redeems the code at `/api/auth/desktop-handoff/redeem` with the PKCE verifier
//!    only it holds, which writes Bulwark's session cookies into the viewer.
//!
//! With Bulwark's admin switches "OAuth only" and "Auto SSO" on, the browser skips Bulwark's
//! page and goes straight to the mail server's sign-in (upstream keeps the hand-off through
//! /api/auth/sso/start). The viewer itself is kept from auto-redirecting (see `init_script`).
//!
//! The tray never sees a password or a Bulwark token, only the one-time code, which is useless
//! without the page's verifier. The capability that lets the viewer reach these three commands
//! (and nothing else) is added at runtime for the configured Bulwark origin only, and each
//! command checks again that it was called from the viewer on that origin.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tauri::{ipc::CapabilityBuilder, AppHandle, Manager, Url, Webview};
use tokio::{net::TcpListener, sync::Notify};

use crate::{auth, viewer, AppState};

const CALLBACK_PATH: &str = "/bulwark-desktop/callback";
const COMMANDS: [&str; 3] = ["viewer_sign_in_begin", "viewer_sign_in_complete", "viewer_sign_in_cancel"];

struct Flow {
    listener: Option<TcpListener>,
    state: String,
    redirect_uri: String,
    cancel: Arc<Notify>,
}

static FLOWS: LazyLock<Mutex<HashMap<String, Flow>>> = LazyLock::new(Default::default);
static GRANTED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
/// When the browser was last opened for a sign-in: a script on the page can't open it in a loop.
static LAST_OPEN: Mutex<Option<Instant>> = Mutex::new(None);
const MIN_OPEN_INTERVAL: Duration = Duration::from_secs(3);
/// When a flow was last begun: each one binds a port, so a page can't make them in a loop.
static LAST_BEGIN: Mutex<Option<Instant>> = Mutex::new(None);
const MIN_BEGIN_INTERVAL: Duration = Duration::from_millis(500);

/// Runs in the viewer before Bulwark's own scripts, on every page load (main frame only), and
/// does nothing on any origin but Bulwark's (`origin`, e.g. `https://webmail.example.com`).
pub fn init_script(origin: &str) -> String {
    let origin = serde_json::to_string(origin).unwrap_or_else(|_| "\"\"".into());
    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    format!(
        r#"(() => {{
  if (location.origin !== {origin}) return;
  // With Bulwark's "Auto SSO" on, its login page goes straight to the provider. That should
  // happen in the browser this window opens, not in here: the flag Bulwark's own loop guard
  // checks keeps this window on its login page, with "Sign in with your browser".
  try {{ sessionStorage.setItem("sso_attempted", "1"); }} catch (_) {{}}
  if (window.bulwarkDesktop || !window.__TAURI_INTERNALS__) return;
  const call = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args || {{}})
    .catch((e) => ({{ ok: false, error: String(e) }}));
  const none = async () => ({{ ok: false, error: "unavailable" }});
  const controls = Object.freeze({{
    begin: (request) => request && request.kind === "instance" ? call("viewer_sign_in_begin") : none(),
    complete: (flowId, url) => call("viewer_sign_in_complete", {{ flowId, url }}),
    useScheme: async () => null,
    cancel: (flowId) => {{ call("viewer_sign_in_cancel", {{ flowId }}); }},
    beginPassword: none,
    nextPassword: none,
    answerPassword: () => {{}},
    outcome: none,
  }});
  let claimed = false;
  const bridge = Object.freeze({{
    version: 1,
    platform: "{platform}",
    mode: "instance",
    auth: Object.freeze({{ claim: () => {{ if (claimed) return null; claimed = true; return controls; }} }}),
  }});
  Object.defineProperty(window, "bulwarkDesktop", {{ value: bridge, writable: false, configurable: false }});
}})();"#
    )
}

fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

fn viewer_base(app: &AppHandle) -> Result<Url, String> {
    let s = app.state::<AppState>().config.lock().unwrap().viewer_url.trim().to_string();
    Url::parse(&s).map_err(|e| format!("Webmail URL: {e}"))
}

/// Lets the viewer, on the configured Bulwark origin, call the three commands above. Once per origin.
pub fn grant(app: &AppHandle) {
    let Ok(base) = viewer_base(app) else { return };
    let origin = origin(&base);
    let mut granted = GRANTED.lock().unwrap();
    if granted.contains(&origin) {
        return;
    }
    let mut cap = CapabilityBuilder::new(format!("viewer-sign-in-{}", granted.len()))
        .remote(format!("{origin}/*"))
        .local(false)
        .window(viewer::LABEL);
    for c in COMMANDS {
        cap = cap.permission(format!("allow-{}", c.replace('_', "-")));
    }
    match app.add_capability(cap) {
        Ok(()) => {
            granted.insert(origin);
        }
        Err(e) => crate::log(&format!("viewer sign-in: could not grant the capability: {e}")),
    }
}

/// Only the viewer, showing the configured Bulwark, may drive this.
fn check_caller(app: &AppHandle, webview: &Webview) -> Result<Url, String> {
    let base = viewer_base(app)?;
    let here = webview.url().map_err(|e| e.to_string())?;
    if webview.label() != viewer::LABEL || origin(&here) != origin(&base) {
        return Err("invalid".into());
    }
    Ok(base)
}

fn fail(error: &str) -> Value {
    json!({ "ok": false, "error": error })
}

#[tauri::command]
pub async fn viewer_sign_in_begin(app: AppHandle, webview: Webview) -> Value {
    if check_caller(&app, &webview).is_err() {
        return fail("invalid");
    }
    {
        let mut last = LAST_BEGIN.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < MIN_BEGIN_INTERVAL) {
            return fail("busy");
        }
        *last = Some(Instant::now());
    }
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => return fail(&format!("listener: {e}")),
    };
    let Ok(port) = listener.local_addr().map(|a| a.port()) else { return fail("listener") };
    let (Ok(flow_id), Ok(state)) = (auth::random_url_safe(16), auth::random_url_safe(24)) else { return fail("random") };
    let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    {
        let mut flows = FLOWS.lock().unwrap();
        // One sign-in at a time: a new click ends any earlier one.
        for (_, old) in flows.drain() {
            old.cancel.notify_one();
        }
        flows.insert(
            flow_id.clone(),
            Flow { listener: Some(listener), state: state.clone(), redirect_uri: redirect_uri.clone(), cancel: Arc::new(Notify::new()) },
        );
    }
    // A flow the page never completes (it reloaded, went away) must not keep its port open.
    let stale = flow_id.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(auth::SIGN_IN_TIMEOUT).await;
        let mut flows = FLOWS.lock().unwrap();
        if flows.get(&stale).is_some_and(|f| f.listener.is_some()) {
            flows.remove(&stale);
        }
    });
    json!({ "ok": true, "flowId": flow_id, "redirectUri": redirect_uri, "state": state })
}

/// Only Bulwark's own login page on the same instance, carrying this flow's redirect and state.
fn check_target(base: &Url, url: &str, redirect_uri: &str, state: &str) -> Result<Url, &'static str> {
    let target = Url::parse(url).map_err(|_| "invalid")?;
    // Exactly one of each: with two, the server and this check could read different ones.
    let param = |k: &str| {
        let mut values = target.query_pairs().filter(|(n, _)| n == k).map(|(_, v)| v.into_owned());
        let first = values.next();
        if values.next().is_some() { None } else { first }
    };
    let ok = origin(&target) == origin(base)
        && target.path().trim_end_matches('/').ends_with("/login")
        && param("mobile_redirect_uri").as_deref() == Some(redirect_uri)
        && param("mobile_state").as_deref() == Some(state)
        && param("mobile_code_challenge").is_some_and(|c| c.len() == 43);
    if ok { Ok(target) } else { Err("invalid") }
}

#[tauri::command]
pub async fn viewer_sign_in_complete(app: AppHandle, webview: Webview, flow_id: String, url: String) -> Value {
    let base = match check_caller(&app, &webview) {
        Ok(b) => b,
        Err(e) => return fail(&e),
    };
    let (listener, state, redirect_uri, cancel) = {
        let mut flows = FLOWS.lock().unwrap();
        let Some(flow) = flows.get_mut(&flow_id) else { return fail("cancelled") };
        let Some(listener) = flow.listener.take() else { return fail("invalid") };
        (listener, flow.state.clone(), flow.redirect_uri.clone(), flow.cancel.clone())
    };
    let finish = |v: Value| {
        FLOWS.lock().unwrap().remove(&flow_id);
        v
    };
    let target = match check_target(&base, &url, &redirect_uri, &state) {
        Ok(t) => t,
        Err(e) => return finish(fail(e)),
    };
    {
        let mut last = LAST_OPEN.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < MIN_OPEN_INTERVAL) {
            crate::log("viewer sign-in: refused, the browser was opened moments ago");
            return finish(fail("invalid"));
        }
        *last = Some(Instant::now());
    }
    crate::log("viewer sign-in: opening the webmail login in the browser");
    if let Err(e) = tauri_plugin_opener::open_url(target.as_str(), None::<&str>) {
        crate::log(&format!("viewer sign-in: no browser: {e}"));
        return finish(fail("no_browser"));
    }
    let outcome = tokio::select! {
        r = tokio::time::timeout(auth::SIGN_IN_TIMEOUT, auth::wait_for_code(listener, CALLBACK_PATH, &state, "Postern")) => r,
        _ = cancel.notified() => return finish(fail("cancelled")),
    };
    let result = match outcome {
        Err(_) => fail("timeout"),
        Ok(Ok(code)) => json!({ "ok": true, "code": code, "state": state }),
        Ok(Err(auth::LoopbackError::Refused(e))) if e == "access_denied" => fail("denied"),
        Ok(Err(e)) => fail(&e.to_string()),
    };
    crate::log(&format!(
        "viewer sign-in: {}",
        if result["ok"] == true { "code received, the page redeems it" } else { result["error"].as_str().unwrap_or("failed") }
    ));
    // Back to the app window, where the page finishes the sign-in.
    if let Some(w) = app.get_webview_window(viewer::LABEL) {
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
    finish(result)
}

#[tauri::command]
pub fn viewer_sign_in_cancel(app: AppHandle, webview: Webview, flow_id: String) {
    if check_caller(&app, &webview).is_err() {
        return;
    }
    if let Some(flow) = FLOWS.lock().unwrap().remove(&flow_id) {
        flow.cancel.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_must_be_this_instances_login_with_this_flow() {
        let base = Url::parse("https://webmail.example.com/").unwrap();
        let redirect = "http://127.0.0.1:51234/bulwark-desktop/callback";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let make = |host: &str, path: &str, state: &str| {
            let mut u = Url::parse(&format!("https://{host}{path}")).unwrap();
            u.query_pairs_mut()
                .append_pair("mobile_redirect_uri", redirect)
                .append_pair("mobile_state", state)
                .append_pair("mobile_code_challenge", challenge);
            u.to_string()
        };
        assert!(check_target(&base, &make("webmail.example.com", "/en/login", "s"), redirect, "s").is_ok());
        assert!(check_target(&base, &make("evil.example.com", "/en/login", "s"), redirect, "s").is_err());
        assert!(check_target(&base, &make("webmail.example.com", "/en/settings", "s"), redirect, "s").is_err());
        assert!(check_target(&base, &make("webmail.example.com", "/en/login", "other"), redirect, "s").is_err());
        let twice = format!("{}&mobile_redirect_uri=http%3A%2F%2Fevil.example.com%2F", make("webmail.example.com", "/en/login", "s"));
        assert!(check_target(&base, &twice, redirect, "s").is_err());
    }
}

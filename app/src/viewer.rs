//! The Bulwark window. Created only when needed and destroyed when closed, which on Windows
//! ends every WebView2 process within ~1–2.5 s (see experiments/wv2-lifecycle).

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tauri::{webview::{NewWindowResponse, PageLoadEvent}, AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use crate::AppState;

pub const LABEL: &str = "viewer";

/// Bumped whenever the viewer is shown again, so a pending delayed destroy knows to stand down.
static HIDE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn target_url(app: &AppHandle, path: &str) -> Result<Url, String> {
    let viewer_url = app.state::<AppState>().config.lock().unwrap().viewer_url.clone();
    let mut s = viewer_url.trim().to_string();
    if s.is_empty() {
        return Err(NO_URL.into());
    }
    if !s.ends_with('/') {
        s.push('/'); // so join() keeps a sub-path deploy such as /webmail/
    }
    let base = Url::parse(&s).map_err(|e| format!("Webmail URL: {e}"))?;
    base.join(path.trim_start_matches('/')).map_err(|e| e.to_string())
}

const NO_URL: &str = "no webmail URL set";

/// A web link the viewer won't load itself goes to the default browser; anything else is dropped.
fn open_outside(url: &Url) {
    if matches!(url.scheme(), "http" | "https") {
        crate::log(&format!("viewer: opening {} in the browser", url.origin().ascii_serialization()));
        if let Err(e) = tauri_plugin_opener::open_url(url.as_str(), None::<&str>) {
            crate::log(&format!("viewer: no browser: {e}"));
        }
    }
}

/// Tells a sign-in gateway apart from a link to another site.
///
/// A gateway in front of Bulwark (Cloudflare Access, Authelia, oauth2-proxy, …) answers a request
/// for Bulwark with a redirect to its own login, and often on to an identity provider. That whole
/// chain has to stay in the window: its cookies must land here, and a callback that ends up in
/// the browser finds no session. A link the user follows starts from a Bulwark page that has
/// finished loading; a gateway's redirect arrives before one has.
///
/// The exception is Cloudflare Access with "sign in in my browser" on (see `access`): its login
/// is not loaded at all; the browser signs in and the session is handed to the window.
#[derive(Default)]
struct Gate {
    /// A page of Bulwark (or its mail server) has finished loading since the last navigation to it.
    settled: bool,
    /// Away at a gateway's login: stay in the window until it sends us back.
    gateway: bool,
}

/// What to do with a navigation.
#[derive(Debug, PartialEq)]
enum Nav {
    Load,
    /// Another site: the browser's.
    Refuse,
    /// The first hop of a gateway's redirect, to a login the browser could take over.
    Login,
}

impl Gate {
    /// `home`: it goes to Bulwark or its mail server. `login`: it goes to an Access login that
    /// the browser may take over.
    fn navigate(&mut self, home: bool, https: bool, login: bool) -> Nav {
        if home {
            *self = Gate::default();
            return Nav::Load;
        }
        if https && login && !self.gateway && !self.settled {
            return Nav::Login;
        }
        if https && (self.gateway || !self.settled) {
            self.gateway = true;
            return Nav::Load;
        }
        Nav::Refuse
    }

    /// The window does the login after all.
    fn stay(&mut self) {
        self.gateway = true;
    }

    /// A page finished loading (or failed to); `home` as above.
    fn loaded(&mut self, home: bool) {
        if home {
            self.settled = true;
        }
    }
}

/// WebKitGTK keeps a window's web process (hundreds of MB) running after the window is gone,
/// one more per open (tauri-runtime-wry holds the shared WebContext for the life of the app).
/// Ending it here is what WebView2 does by itself on Windows.
fn end_web_process(w: &tauri::WebviewWindow) {
    #[cfg(target_os = "linux")]
    let _ = w.with_webview(|webview| {
        use webkit2gtk::WebViewExt;
        webview.inner().terminate_web_process();
    });
    #[cfg(not(target_os = "linux"))]
    let _ = w;
}

/// Closes the viewer for good (e.g. its URL changed: it must be rebuilt for the new origin).
pub fn destroy(app: &AppHandle) {
    crate::access::reset();
    if let Some(w) = app.get_webview_window(LABEL) {
        HIDE_GENERATION.fetch_add(1, Ordering::SeqCst);
        end_web_process(&w);
        let _ = w.destroy();
    }
}

/// Opens (or focuses) the viewer on your Bulwark instance. `path` is app-relative: "" for its
/// start page, `mail/message/<id>`, `calendar/event/<id>`, `protocol/mailto?url=…`.
pub fn open(app: &AppHandle, path: &str) {
    if let Err(e) = try_open(app, path) {
        crate::log(&format!("viewer: {e}"));
        if e == NO_URL {
            open_settings(app);
        }
    }
}

fn try_open(app: &AppHandle, path: &str) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(LABEL) {
        HIDE_GENERATION.fetch_add(1, Ordering::SeqCst);
        if !path.is_empty() {
            window.navigate(target_url(app, path)?).map_err(|e| e.to_string())?;
        }
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    let url = target_url(app, path)?;
    let bulwark = url.origin();
    let popup_origin = bulwark.clone();
    // The mail server too: Bulwark's own "Sign in with Stalwart" may run in the window.
    let server = Url::parse(app.state::<AppState>().config.lock().unwrap().jmap_url.trim()).ok().map(|u| u.origin());
    let gate = Arc::new(Mutex::new(Gate::default()));
    let loaded_gate = gate.clone();
    // Where a browser sign-in resumes: the last page of Bulwark the window was sent to.
    let resume: Arc<Mutex<Url>> = Arc::new(Mutex::new(url.clone()));
    let nav_app = app.clone();
    crate::access::reset();
    let (loaded_bulwark, loaded_server) = (bulwark.clone(), server.clone());
    crate::viewer_auth::grant(app);
    let builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(url))
        .initialization_script(crate::viewer_auth::init_script(&bulwark.ascii_serialization()));
    // Every frame, on any origin: a plugin's sandboxed iframe must not reach the Ed25519 crash.
    #[cfg(target_os = "linux")]
    let builder = builder.initialization_script_for_all_frames(crate::viewer_auth::WEBKIT_ED25519_GUARD);
    let window = builder
        // Only Bulwark, its mail server and a sign-in gateway in front of them load in here (see
        // `Gate`); every other link opens in the browser.
        .on_navigation(move |to| {
            if to.scheme() == "about" {
                return true;
            }
            // The page that says the browser has the user (only while it does).
            if crate::access::is_waiting_page(to) {
                return true;
            }
            let home = to.origin() == bulwark || server.as_ref() == Some(&to.origin());
            if to.origin() == bulwark {
                *resume.lock().unwrap() = to.clone();
            }
            let login = if home { None } else { crate::access::login_target(&nav_app, to) };
            // (`login` is only ever Some for a scheme `access` accepts: https, bar its test mock.)
            let nav = gate.lock().unwrap().navigate(home, to.scheme() == "https" || login.is_some(), login.is_some());
            match (nav, login) {
                (Nav::Load, _) => return true,
                (Nav::Login, Some(aud)) => {
                    let back = resume.lock().unwrap().clone();
                    match crate::access::begin(&nav_app, aud, back) {
                        crate::access::Start::Handled => return false,
                        crate::access::Start::InWindow => {
                            gate.lock().unwrap().stay();
                            return true;
                        }
                    }
                }
                _ => {}
            }
            open_outside(to);
            false
        })
        .on_page_load(move |_, payload| {
            if payload.event() == PageLoadEvent::Finished {
                let at = payload.url().origin();
                let home = at == loaded_bulwark || loaded_server.as_ref() == Some(&at);
                loaded_gate.lock().unwrap().loaded(home);
                if at == loaded_bulwark {
                    crate::access::signed_in();
                }
            }
        })
        .on_new_window(move |to, _features| {
            // Bulwark's own blob: pages (attachments, print previews) may open; web links go
            // to the browser; anything else (about:, data:, javascript:) is refused.
            if to.scheme() == "blob" && to.origin() == popup_origin {
                return NewWindowResponse::Allow;
            }
            open_outside(&to);
            NewWindowResponse::Deny
        })
        // Files dropped on the window go to Bulwark's own drop zones (attachments), not to Tauri.
        .disable_drag_drop_handler()
        .title("Postern")
        .inner_size(1280.0, 860.0)
        .min_inner_size(480.0, 400.0)
        .visible(false)
        .build()
        .map_err(|e| e.to_string())?;
    if crate::window_state::restore(&window) {
        let _ = window.maximize();
    }
    let _ = window.show();
    let _ = window.set_focus();

    let handle = app.clone();
    window.on_window_event(move |event| {
        if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
            if let Some(w) = handle.get_webview_window(LABEL) {
                crate::window_state::track(&w, false);
            }
            return;
        }
        if matches!(event, WindowEvent::Destroyed) {
            crate::access::reset(); // a browser sign-in for a window that is gone has nowhere to go
            return;
        }
        if let WindowEvent::CloseRequested { api, .. } = event {
            let Some(w) = handle.get_webview_window(LABEL) else { return };
            crate::window_state::track(&w, true);
            let keep = handle.state::<AppState>().config.lock().unwrap().keep_viewer_secs;
            if keep == 0 {
                if cfg!(target_os = "linux") {
                    // Stop the web process first, then close the window ourselves.
                    api.prevent_close();
                    end_web_process(&w);
                    let _ = w.destroy();
                }
                return; // default close: the window and its WebView are destroyed
            }
            api.prevent_close();
            let _ = w.hide();
            let generation = HIDE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            let handle = handle.clone();
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(keep)).await;
                if HIDE_GENERATION.load(Ordering::SeqCst) != generation {
                    return; // shown again meanwhile
                }
                if let Some(w) = handle.get_webview_window(LABEL) {
                    if !w.is_visible().unwrap_or(true) {
                        end_web_process(&w);
                        let _ = w.destroy();
                    }
                }
            });
        }
    });
    Ok(())
}

/// Forgets everything the Bulwark window stored (cookies, Cloudflare Access sessions, local data)
/// and starts it again from the sign-in page. For a login stuck on a stale or half-finished session.
pub fn reset_session(app: &AppHandle) {
    crate::access::reset();
    open(app, "");
    let app = app.clone();
    // Off the main thread: WebView2's browsing-data calls can deadlock there.
    std::thread::spawn(move || {
        let Some(w) = app.get_webview_window(LABEL) else { return };
        if let Err(e) = w.clear_all_browsing_data() {
            crate::log(&format!("viewer: reset failed: {e}"));
            return;
        }
        if let Ok(url) = target_url(&app, "") {
            let _ = w.navigate(url);
        }
        crate::log("viewer: session reset");
    });
}

/// Opens Bulwark's composer for a `mailto:` URL through its own handler route.
pub fn open_mailto(app: &AppHandle, mailto: &str) {
    let mut path = Url::parse("http://x/protocol/mailto").expect("static url");
    path.query_pairs_mut().append_pair("url", mailto);
    open(app, &format!("protocol/mailto?{}", path.query().unwrap_or_default()));
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn open_message(app: &AppHandle, email_id: &str) {
    let mut u = Url::parse("http://x/mail/message/").expect("static url");
    u.path_segments_mut().expect("base").pop_if_empty().push(email_id);
    open(app, u.path().trim_start_matches('/'));
}

/// A calendar alert: the event itself, or the task list for a task.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn open_calendar_item(app: &AppHandle, event_id: &str, is_task: bool) {
    if is_task {
        open(app, "calendar/tasks");
        return;
    }
    let mut u = Url::parse("http://x/calendar/event/").expect("static url");
    u.path_segments_mut().expect("base").pop_if_empty().push(event_id);
    open(app, u.path().trim_start_matches('/'));
}

pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // Tall enough for the whole page with room for a three-line message (measured: 647 px of
    // content, 714 with the AppImage-only update row, plus 19 for the longer message).
    let height = if crate::is_appimage() { 760.0 } else { 700.0 };
    let result = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Postern settings")
        .inner_size(460.0, height)
        .resizable(false)
        .build();
    match result {
        Ok(window) if cfg!(target_os = "linux") => {
            let w = window.clone();
            window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    end_web_process(&w);
                    let _ = w.destroy();
                }
            });
        }
        Ok(_) => {}
        Err(e) => crate::log(&format!("settings window: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, Nav};

    #[test]
    fn a_gateway_login_stays_in_the_window_and_links_do_not() {
        let mut g = Gate::default();
        // Opening Bulwark: the gateway redirects to its login, then to the identity provider.
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, false), Nav::Load);
        g.loaded(false); // the login page
        assert_eq!(g.navigate(false, true, false), Nav::Load); // on to the identity provider
        assert_eq!(g.navigate(true, true, false), Nav::Load); // and back
        g.loaded(true);
        // From a loaded Bulwark page, another site is a link: the browser's.
        assert_eq!(g.navigate(false, true, false), Nav::Refuse);
        assert_eq!(g.navigate(false, true, false), Nav::Refuse);
        // A session that ran out: the next load of Bulwark is redirected again.
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, false), Nav::Load);
    }

    #[test]
    fn a_gateway_must_be_https() {
        let mut g = Gate::default();
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, false, false), Nav::Refuse);
        assert_eq!(g.navigate(false, false, true), Nav::Refuse);
    }

    #[test]
    fn an_access_login_is_only_taken_over_on_the_first_hop() {
        let mut g = Gate::default();
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, true), Nav::Login);
        // A link from a settled page is still a link, even to a look-alike login.
        g.loaded(true);
        assert_eq!(g.navigate(false, true, true), Nav::Refuse);
        // Mid-way through a login in the window nothing is taken over.
        let mut g = Gate::default();
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, true), Nav::Load);
        // The window chose to do the login itself.
        let mut g = Gate::default();
        assert_eq!(g.navigate(true, true, false), Nav::Load);
        assert_eq!(g.navigate(false, true, true), Nav::Login);
        g.stay();
        assert_eq!(g.navigate(false, true, true), Nav::Load);
    }
}

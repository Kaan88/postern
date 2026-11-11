//! The Bulwark window. Created only when needed and destroyed when closed, which on Windows
//! ends every WebView2 process within ~1–2.5 s (see experiments/wv2-lifecycle).

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tauri::{webview::NewWindowResponse, AppHandle, Manager, Url, WebviewUrl, WebviewWindowBuilder, WindowEvent};

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
    let base = Url::parse(&s).map_err(|e| format!("Bulwark URL: {e}"))?;
    base.join(path.trim_start_matches('/')).map_err(|e| e.to_string())
}

const NO_URL: &str = "no Bulwark URL set";

/// A web link the viewer won't load itself goes to the default browser; anything else is dropped.
fn open_outside(url: &Url) {
    if matches!(url.scheme(), "http" | "https") {
        crate::log(&format!("viewer: opening {} in the browser", url.origin().ascii_serialization()));
        if let Err(e) = tauri_plugin_opener::open_url(url.as_str(), None::<&str>) {
            crate::log(&format!("viewer: no browser: {e}"));
        }
    }
}

/// Closes the viewer for good (e.g. its URL changed: it must be rebuilt for the new origin).
pub fn destroy(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        HIDE_GENERATION.fetch_add(1, Ordering::SeqCst);
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
    crate::viewer_auth::grant(app);
    let window = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(url))
        .initialization_script(crate::viewer_auth::init_script(&bulwark.ascii_serialization()))
        // Only Bulwark (and its mail server) load in here; every other link opens in the browser.
        .on_navigation(move |to| {
            if to.origin() == bulwark || server.as_ref() == Some(&to.origin()) || to.scheme() == "about" {
                return true;
            }
            open_outside(to);
            false
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
        .title("Bulwark")
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
        if let WindowEvent::CloseRequested { api, .. } = event {
            if let Some(w) = handle.get_webview_window(LABEL) {
                crate::window_state::track(&w, true);
            }
            let keep = handle.state::<AppState>().config.lock().unwrap().keep_viewer_secs;
            if keep == 0 {
                return; // default close: the window and its WebView are destroyed
            }
            api.prevent_close();
            let Some(w) = handle.get_webview_window(LABEL) else { return };
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
                        let _ = w.destroy();
                    }
                }
            });
        }
    });
    Ok(())
}

/// Opens Bulwark's composer for a `mailto:` URL through its own handler route.
pub fn open_mailto(app: &AppHandle, mailto: &str) {
    let mut path = Url::parse("http://x/protocol/mailto").expect("static url");
    path.query_pairs_mut().append_pair("url", mailto);
    open(app, &format!("protocol/mailto?{}", path.query().unwrap_or_default()));
}

pub fn open_message(app: &AppHandle, email_id: &str) {
    let mut u = Url::parse("http://x/mail/message/").expect("static url");
    u.path_segments_mut().expect("base").pop_if_empty().push(email_id);
    open(app, u.path().trim_start_matches('/'));
}

/// A calendar alert: the event itself, or the task list for a task.
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
    let result = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Postern settings")
        .inner_size(520.0, 860.0)
        .resizable(false)
        .build();
    if let Err(e) = result {
        crate::log(&format!("settings window: {e}"));
    }
}

//! Postern: a resident notifier for a JMAP mailbox, with Bulwark opened in a webview only
//! while you use it. Closing the window frees the WebView entirely; the tray process stays small.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod appimage;
mod access;
mod auth;
mod calendar;
mod config;
mod notify;
mod tray;
#[cfg(target_os = "linux")]
mod updates;
mod viewer;
mod viewer_auth;
mod window_state;
mod watcher;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, RunEvent, State};
use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::Notify;

/// The log file, once the app knows where its log folder is.
static LOG_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
/// Past this the log starts over (the previous one is kept as postern.old.log).
const LOG_MAX_BYTES: u64 = 1024 * 1024;

/// Timestamped line on stderr and in postern.log in the app's log folder. Lines say what
/// happened, not to whom: no addresses, names, subjects or tokens.
pub fn log(msg: &str) {
    let line = format!("[{}] {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    eprintln!("{line}");
    let Some(path) = LOG_FILE.get() else { return };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
        let _ = std::fs::rename(path, path.with_file_name("postern.old.log"));
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn init_log(app: &AppHandle) {
    if let Ok(dir) = app.path().app_log_dir() {
        if std::fs::create_dir_all(&dir).is_ok() {
            let _ = LOG_FILE.set(dir.join("postern.log"));
        }
    }
    log(&format!("Postern {} starting", app.package_info().version));
}

pub struct AppState {
    pub config: Mutex<config::Config>,
    /// Wakes the watcher to reconnect with fresh settings.
    pub restart: Arc<Notify>,
    /// The current OAuth access token (memory only). Async mutex: held across a refresh.
    pub token: tokio::sync::Mutex<Option<auth::AccessToken>>,
}

/// What the settings page edits. The OAuth fields are the app's own business.
#[derive(Deserialize)]
struct SettingsInput {
    jmap_url: String,
    viewer_url: String,
    notifications: bool,
    calendar_alerts: bool,
    keep_viewer_secs: u64,
    auto_update: bool,
    access_browser_signin: bool,
}

#[derive(Serialize)]
struct SettingsView {
    config: config::Config,
    signed_in: bool,
    /// Whether the page should offer the automatic-update setting.
    self_updating: bool,
}

/// Running as an AppImage, which is where the menu entry and self-update apply.
pub fn is_appimage() -> bool {
    #[cfg(target_os = "linux")]
    return appimage::path().is_some();
    #[cfg(not(target_os = "linux"))]
    false
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let config = state.config.lock().unwrap().clone();
    let signed_in = auth::is_signed_in(&config);
    Ok(SettingsView { config, signed_in, self_updating: is_appimage() })
}

fn validate(input: &SettingsInput) -> Result<(), String> {
    auth::secure_url(&input.jmap_url).map_err(|e| format!("Mail server URL {e}"))?;
    auth::secure_url(&input.viewer_url).map_err(|e| format!("Webmail URL {e}"))?;
    Ok(())
}

/// Applies the page's fields to the stored config. A different mail server drops the sign-in.
async fn apply(app: &AppHandle, input: SettingsInput) -> Result<config::Config, String> {
    validate(&input)?;
    let current = app.state::<AppState>().config.lock().unwrap().clone();
    let server_changed = current.jmap_url.trim() != input.jmap_url.trim();
    if server_changed && auth::is_signed_in(&current) {
        auth::sign_out(app).await;
    }
    let mut next = config::Config {
        jmap_url: input.jmap_url.trim().to_string(),
        viewer_url: input.viewer_url.trim().to_string(),
        notifications: input.notifications,
        calendar_alerts: input.calendar_alerts,
        keep_viewer_secs: input.keep_viewer_secs.min(3600),
        auto_update: input.auto_update,
        access_browser_signin: input.access_browser_signin,
        ..current
    };
    if server_changed {
        next.username.clear();
        next.oauth_client_id.clear();
        next.oauth_token_endpoint.clear();
        next.oauth_revocation_endpoint.clear();
    }
    Ok(next)
}

fn store(app: &AppHandle, config: config::Config) -> Result<(), String> {
    config::save(app, &config)?;
    let state = app.state::<AppState>();
    let old = std::mem::replace(&mut *state.config.lock().unwrap(), config.clone());
    if old.viewer_url.trim() != config.viewer_url.trim() {
        // Its navigation rules and bridge are tied to the old origin: rebuild on next open.
        viewer::destroy(app);
    }
    // Only another account needs a new connection; the toggles are read live.
    if old.jmap_url.trim() != config.jmap_url.trim() || old.oauth_client_id != config.oauth_client_id {
        state.restart.notify_one();
    }
    Ok(())
}

#[tauri::command]
async fn save_settings(app: AppHandle, settings: SettingsInput) -> Result<(), String> {
    let next = apply(&app, settings).await?;
    store(&app, next)
}

/// Saves the fields, then runs the browser sign-in and checks the account over JMAP.
#[tauri::command]
async fn sign_in(app: AppHandle, settings: SettingsInput) -> Result<String, String> {
    let mut signing = apply(&app, settings).await?;
    store(&app, signing.clone())?;
    let signed = auth::sign_in(&app, &mut signing).await?;
    // The browser step can take minutes: keep whatever was saved meanwhile, add only the sign-in.
    let mut next = app.state::<AppState>().config.lock().unwrap().clone();
    if next.jmap_url.trim() != signing.jmap_url.trim() {
        auth::discard(&app).await;
        return Err("The mail server URL changed during the sign-in. Sign in again.".into());
    }
    next.oauth_client_id = signing.oauth_client_id;
    next.oauth_token_endpoint = signing.oauth_token_endpoint;
    next.oauth_revocation_endpoint = signing.oauth_revocation_endpoint;
    if !signed.username.is_empty() {
        next.username = signed.username;
    }
    if let Err(e) = store(&app, next.clone()) {
        auth::discard(&app).await;
        return Err(e);
    }
    // Also after signing in again to the same account (e.g. from "Sign-in failed").
    app.state::<AppState>().restart.notify_one();
    let who = if next.username.is_empty() { String::new() } else { format!(" as {}", next.username) };
    // Signed in from here on: a failed check is reported, not turned into a sign-in error.
    let check = async {
        let token = auth::access_token(&app).await?;
        let client = watcher::connect(&next, &token.token).await?;
        let inbox = watcher::inbox_id(&client).await?;
        watcher::inbox_unread(&client, &inbox).await
    };
    let checked: Result<usize, watcher::Failure> = check.await;
    Ok(match checked {
        Ok(unread) => format!("Signed in{who}. {unread} unread in the Inbox."),
        Err(e) => format!("Signed in{who}, but the Inbox could not be read yet: {e}"),
    })
}

/// Returns whether the server confirmed it revoked the sign-in.
#[tauri::command]
async fn sign_out(app: AppHandle) -> Result<bool, String> {
    let revoked = auth::sign_out(&app).await;
    let mut next = app.state::<AppState>().config.lock().unwrap().clone();
    next.username.clear();
    next.oauth_client_id.clear();
    next.oauth_token_endpoint.clear();
    next.oauth_revocation_endpoint.clear();
    store(&app, next)?;
    Ok(revoked)
}

fn mailto_arg(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| a.to_ascii_lowercase().starts_with("mailto:"))
}

fn main() {
    let builder = tauri::Builder::default()
        // A second launch (e.g. the OS opening a mailto: link) is handed to the running copy.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| match mailto_arg(&args) {
            Some(m) => viewer::open_mailto(app, m),
            None => viewer::open(app, ""),
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None));
    // Its endpoint and public key are in tauri.linux.conf.json. The deb and rpm are updated by
    // the package manager, so only an AppImage gets the plugin.
    #[cfg(target_os = "linux")]
    let builder = if is_appimage() { builder.plugin(tauri_plugin_updater::Builder::new().build()) } else { builder };
    let app = builder
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            sign_in,
            sign_out,
            viewer_auth::viewer_sign_in_begin,
            viewer_auth::viewer_sign_in_complete,
            viewer_auth::viewer_sign_in_cancel,
            access::access_use_window
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            init_log(&handle);
            #[cfg(target_os = "linux")]
            appimage::integrate();
            config::migrate_legacy(&handle);
            let cfg = config::load(&handle);
            let complete = cfg.is_complete() && auth::is_signed_in(&cfg) && !cfg.viewer_url.trim().is_empty();
            let restart = Arc::new(Notify::new());
            app.manage(AppState {
                config: Mutex::new(cfg),
                restart: restart.clone(),
                token: tokio::sync::Mutex::new(None),
            });

            tray::build(&handle)?;
            watcher::spawn(handle.clone(), restart);
            #[cfg(target_os = "linux")]
            if is_appimage() {
                updates::spawn(handle.clone());
            }

            let args: Vec<String> = std::env::args().collect();
            if let Some(m) = mailto_arg(&args) {
                viewer::open_mailto(&handle, m);
            } else if !complete {
                viewer::open_settings(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the app");

    app.run(|_app, event| {
        // Closing the last window must not end the app: only Quit (exit with a code) does.
        if let RunEvent::ExitRequested { api, code, .. } = event {
            if code.is_none() {
                api.prevent_exit();
            }
        }
    });
}

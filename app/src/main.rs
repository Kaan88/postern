//! Postern: a resident notifier for a JMAP mailbox, with Bulwark opened in a webview only
//! while you use it. Closing the window frees the WebView entirely; the tray process stays small.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
mod calendar;
mod config;
mod notify;
mod tray;
mod viewer;
mod viewer_auth;
mod watcher;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, RunEvent, State};
use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::Notify;

/// Timestamped line on stderr (visible with `cargo run`, or redirect it to a file).
pub fn log(msg: &str) {
    eprintln!("[{}] {msg}", chrono::Local::now().format("%H:%M:%S"));
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
}

#[derive(Serialize)]
struct SettingsView {
    config: config::Config,
    signed_in: bool,
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let config = state.config.lock().unwrap().clone();
    let signed_in = auth::is_signed_in(&config);
    Ok(SettingsView { config, signed_in })
}

fn validate(input: &SettingsInput) -> Result<(), String> {
    let jmap = tauri::Url::parse(input.jmap_url.trim()).map_err(|e| format!("Mail server URL: {e}"))?;
    if jmap.scheme() != "https" && jmap.host_str() != Some("localhost") {
        return Err("Mail server URL must use https".into());
    }
    let viewer = tauri::Url::parse(input.viewer_url.trim()).map_err(|e| format!("Bulwark URL: {e}"))?;
    if viewer.scheme() != "https" && viewer.host_str() != Some("localhost") {
        return Err("Bulwark URL must use https".into());
    }
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
    *state.config.lock().unwrap() = config;
    state.restart.notify_one();
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
    let mut next = apply(&app, settings).await?;
    store(&app, next.clone())?;
    let signed = auth::sign_in(&app, &mut next).await?;
    if !signed.username.is_empty() {
        next.username = signed.username;
    }
    store(&app, next.clone())?;
    let token = auth::access_token(&app).await.map_err(|e| e.to_string())?;
    let client = watcher::connect(&next, &token.token).await?;
    let inbox = watcher::inbox_id(&client).await?;
    let unread = watcher::inbox_unread(&client, &inbox).await?;
    Ok(format!("Signed in as {}. {unread} unread in the Inbox.", next.username))
}

#[tauri::command]
async fn sign_out(app: AppHandle) -> Result<(), String> {
    auth::sign_out(&app).await;
    let mut next = app.state::<AppState>().config.lock().unwrap().clone();
    next.username.clear();
    next.oauth_client_id.clear();
    store(&app, next)
}

fn mailto_arg(args: &[String]) -> Option<&str> {
    args.iter().map(String::as_str).find(|a| a.to_ascii_lowercase().starts_with("mailto:"))
}

fn main() {
    let app = tauri::Builder::default()
        // A second launch (e.g. the OS opening a mailto: link) is handed to the running copy.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| match mailto_arg(&args) {
            Some(m) => viewer::open_mailto(app, m),
            None => viewer::open(app, ""),
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, None))
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            sign_in,
            sign_out,
            viewer_auth::viewer_sign_in_begin,
            viewer_auth::viewer_sign_in_complete,
            viewer_auth::viewer_sign_in_cancel
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            config::migrate_legacy(&handle);
            notify::register(&handle);
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

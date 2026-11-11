//! Settings on disk (config.json in the app config dir) and secrets in the OS keyring
//! (only the OAuth refresh token; no passwords are stored).

use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};
use tauri::{AppHandle, Manager};

const KEYRING_SERVICE: &str = "postern";
/// Before the rename to Postern (0.1.x): moved over once by `migrate_legacy`.
const LEGACY_KEYRING_SERVICE: &str = "bulwark-native";
const LEGACY_CONFIG_DIR: &str = "dev.bulwark-native.tray";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// JMAP session URL or server base, e.g. `https://mail.example.com` (`/.well-known/jmap` is found by the client).
    pub jmap_url: String,
    /// Who the server says signed in (display only).
    pub username: String,
    /// OAuth public client registered with the server at sign-in (stateless id, not a secret).
    pub oauth_client_id: String,
    pub oauth_token_endpoint: String,
    pub oauth_revocation_endpoint: String,
    /// Base URL of your Bulwark instance, e.g. `https://webmail.example.com`; the viewer loads it.
    pub viewer_url: String,
    /// Show a toast for each new unread mail in the Inbox.
    pub notifications: bool,
    /// Show the server's calendar and task reminders (Stalwart DISPLAY alarms, JMAP CalendarAlert).
    pub calendar_alerts: bool,
    /// Keep the viewer alive (hidden) this many seconds after its window is closed, so a quick
    /// reopen is instant. 0 destroys it at once, which frees all WebView memory.
    pub keep_viewer_secs: u64,
}

/// Also what fields missing from an older config.json fall back to.
impl Default for Config {
    fn default() -> Self {
        Config {
            jmap_url: String::new(),
            username: String::new(),
            oauth_client_id: String::new(),
            oauth_token_endpoint: String::new(),
            oauth_revocation_endpoint: String::new(),
            viewer_url: String::new(),
            notifications: true,
            calendar_alerts: true,
            keep_viewer_secs: 0,
        }
    }
}

impl Config {
    /// Enough for the watcher (which never needs Bulwark itself), once a token is in the keyring.
    pub fn is_complete(&self) -> bool {
        !self.jmap_url.trim().is_empty() && !self.oauth_client_id.is_empty()
    }
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("config.json"))
}

pub fn load(app: &AppHandle) -> Config {
    let Some(p) = path(app) else { return default_config() };
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!("config: {} is not valid ({e}); using defaults", p.display());
            default_config()
        }),
        Err(_) => default_config(),
    }
}

fn default_config() -> Config {
    Config::default()
}

pub fn save(app: &AppHandle, config: &Config) -> Result<(), String> {
    let p = path(app).ok_or("no config directory")?;
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    fs::write(&p, json).map_err(|e| e.to_string())
}

/// Carries a 0.1.x ("Bulwark Tray") install over to Postern, once: its config.json from the
/// old app-config folder, and its OAuth refresh token from the old keyring service. Nothing is
/// overwritten if Postern already has its own. The Bulwark window's cookies (a separate WebView
/// profile) don't move: one "Sign in with your browser" brings them back.
pub fn migrate_legacy(app: &AppHandle) {
    let (Some(new_cfg), Ok(dir)) = (path(app), app.path().app_config_dir()) else { return };
    if !new_cfg.exists() {
        if let Some(old) = dir.parent().map(|p| p.join(LEGACY_CONFIG_DIR).join("config.json")).filter(|p| p.exists()) {
            let moved = new_cfg.parent().map(fs::create_dir_all).transpose().and_then(|_| fs::copy(&old, &new_cfg));
            match moved {
                Ok(_) => crate::log(&format!("migrated settings from {}", old.display())),
                Err(e) => crate::log(&format!("could not migrate settings from {}: {e}", old.display())),
            }
        }
    }
    for name in ["oauth-refresh-token"] {
        if secret(name).is_some() {
            continue;
        }
        let Ok(old) = keyring::Entry::new(LEGACY_KEYRING_SERVICE, name) else { continue };
        if let Ok(value) = old.get_password() {
            if set_secret(name, &value).is_ok() {
                let _ = old.delete_credential();
                crate::log("migrated the sign-in token to Postern's keyring entry");
            }
        }
    }
}

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| e.to_string())
}

pub fn secret(name: &str) -> Option<String> {
    entry(name).ok()?.get_password().ok()
}

pub fn set_secret(name: &str, value: &str) -> Result<(), String> {
    entry(name)?.set_password(value).map_err(|e| e.to_string())
}

pub fn delete_secret(name: &str) {
    if let Ok(e) = entry(name) {
        let _ = e.delete_credential();
    }
}

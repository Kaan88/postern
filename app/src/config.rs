//! Settings on disk (config.json in the app config dir) and secrets in the OS keyring
//! (only the OAuth refresh token; no passwords are stored).

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tauri::{AppHandle, Manager};

const KEYRING_SERVICE: &str = "postern";
/// The keyring entry that holds the OAuth refresh token.
pub const KEYRING_REFRESH: &str = "oauth-refresh-token";
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
    let Some(p) = path(app) else { return Config::default() };
    crate::log(&format!("config: {}", p.display()));
    match fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            crate::log(&format!("config: {} is not valid ({e}); using defaults", p.display()));
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// Writes `bytes` to a temporary file next to `path` and renames it over `path`, so a crash or
/// power cut mid-write leaves the old file rather than a truncated one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

pub fn save(app: &AppHandle, config: &Config) -> Result<(), String> {
    let p = path(app).ok_or("no config directory")?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    write_atomic(&p, json.as_bytes()).map_err(|e| e.to_string())
}

/// Carries a 0.1.x ("Bulwark Tray") install over to Postern, once: its config.json from the
/// old app-config folder and, with it, its OAuth refresh token from the old keyring service
/// (copied, not moved, so the old version still works if Postern is removed). Nothing is
/// migrated if Postern already has its own settings: a token only makes sense next to the
/// client id in the config it came with. The Bulwark window's cookies (a separate WebView
/// profile) don't move: one "Sign in with your browser" brings them back.
pub fn migrate_legacy(app: &AppHandle) {
    let (Some(new_cfg), Ok(dir)) = (path(app), app.path().app_config_dir()) else { return };
    if new_cfg.exists() {
        return;
    }
    let Some(old) = dir.parent().map(|p| p.join(LEGACY_CONFIG_DIR).join("config.json")).filter(|p| p.exists()) else {
        return;
    };
    let copied = fs::read(&old).and_then(|bytes| write_atomic(&new_cfg, &bytes));
    if let Err(e) = copied {
        crate::log(&format!("could not migrate settings from {}: {e}", old.display()));
        return;
    }
    crate::log(&format!("migrated settings from {} to {}", old.display(), new_cfg.display()));
    if !matches!(secret(KEYRING_REFRESH), Ok(None)) {
        return; // already has one, or the keyring can't be read: leave it
    }
    let Ok(old_entry) = keyring::Entry::new(LEGACY_KEYRING_SERVICE, KEYRING_REFRESH) else { return };
    if let Ok(value) = old_entry.get_password() {
        match set_secret(KEYRING_REFRESH, &value) {
            Ok(()) => crate::log("migrated the sign-in token to Postern's keyring entry"),
            Err(e) => crate::log(&format!("could not migrate the sign-in token: {e}")),
        }
    }
}

fn entry(name: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| e.to_string())
}

/// `Ok(None)` when there is no such entry; `Err` when the keyring could not be read.
pub fn secret(name: &str) -> Result<Option<String>, String> {
    match entry(name)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

pub fn set_secret(name: &str, value: &str) -> Result<(), String> {
    entry(name)?.set_password(value).map_err(|e| e.to_string())
}

pub fn delete_secret(name: &str) {
    if let Ok(e) = entry(name) {
        let _ = e.delete_credential();
    }
}

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
    /// The sound played with a notification: "bulwark" (the same as the webmail's own, see
    /// `bulwark_sound`), "off", or one of Bulwark's (see sound.rs). Any other value counts as off.
    pub notification_sound: String,
    /// What the webmail was last seen to use for new mail: "off" or one of Bulwark's ids, or
    /// empty if it never reported. Reported by the viewer (viewer_auth.rs); not a setting of
    /// this app.
    pub bulwark_sound: String,
    /// The same for calendar and task reminders (Bulwark has a sound switch for each).
    pub bulwark_reminder_sound: String,
    /// Keep the viewer alive (hidden) this many seconds after its window is closed, so a quick
    /// reopen is instant. 0 destroys it at once, which frees all WebView memory.
    pub keep_viewer_secs: u64,
    /// AppImage only: look for a new version daily and install it in the background.
    pub auto_update: bool,
    /// Behind Cloudflare Access: sign in in the default browser and hand the session to the
    /// window, instead of running Access's login in the window (see access.rs).
    pub access_browser_signin: bool,
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
            notification_sound: crate::sound::DEFAULT_SETTING.to_string(),
            bulwark_sound: crate::sound::UNKNOWN.to_string(),
            bulwark_reminder_sound: crate::sound::UNKNOWN.to_string(),
            keep_viewer_secs: 0,
            auto_update: true,
            access_browser_signin: false,
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
    // A name of its own per write, so two writers never share (or truncate) one temp file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}-{n}.tmp", std::process::id()));
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
/// (copied, not moved; but tokens rotate, so after Postern's first refresh the old version
/// needs to sign in again). Nothing is
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_follows_bulwark_by_default_and_old_files_still_load() {
        assert_eq!(Config::default().notification_sound, "bulwark");
        assert_eq!(Config::default().bulwark_sound, "");
        assert_eq!(Config::default().bulwark_reminder_sound, "");
        // A config.json from before the setting existed.
        let old: Config = serde_json::from_str(r#"{"jmap_url":"https://m.example.com","notifications":false}"#).unwrap();
        assert_eq!(old.notification_sound, "bulwark");
        assert_eq!(old.bulwark_sound, "");
        assert!(!old.notifications);
        // An explicit choice from the first version of the setting is kept.
        let explicit: Config = serde_json::from_str(r#"{"notification_sound":"swift"}"#).unwrap();
        assert_eq!(explicit.notification_sound, "swift");
        assert_eq!(explicit.bulwark_sound, "");
    }

    #[test]
    fn a_remembered_sound_from_before_reminders_had_their_own_still_loads() {
        let c: Config = serde_json::from_str(r#"{"bulwark_sound":"cheerful"}"#).unwrap();
        assert_eq!(c.bulwark_sound, "cheerful");
        assert_eq!(c.bulwark_reminder_sound, "");
    }

    #[test]
    fn an_unknown_sound_means_off() {
        let c: Config = serde_json::from_str(r#"{"notification_sound":"klaxon"}"#).unwrap();
        assert!(crate::sound::find(&c.notification_sound).is_none());
    }
}

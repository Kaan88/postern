//! AppImage only: updates itself in place. The updater plugin fetches `latest.json` from the
//! GitHub release, downloads the new AppImage, checks its signature against the public key in
//! tauri.linux.conf.json and writes it over the file we run from, so the path (and with it the
//! menu entry and "Start at login") stays valid. The running copy keeps working until the user
//! restarts from the tray; nothing here restarts on its own.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::Duration,
};

use tauri::{AppHandle, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::{notify, tray, AppState};

const FIRST_CHECK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Without a limit a stalled connection would block every later check.
const CHECK_LIMIT: Duration = Duration::from_secs(60);
const DOWNLOAD_LIMIT: Duration = Duration::from_secs(15 * 60);

/// One check or download at a time.
static BUSY: AtomicBool = AtomicBool::new(false);
/// The version already written to disk and waiting for a restart.
static INSTALLED: Mutex<Option<String>> = Mutex::new(None);

enum Outcome {
    UpToDate,
    /// Installed now, or earlier in this run.
    Installed { version: String, fresh: bool },
}

/// Checks soon after start-up and then daily, unless automatic updates are off.
pub fn spawn(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(FIRST_CHECK).await;
        loop {
            if app.state::<AppState>().config.lock().unwrap().auto_update {
                check(&app, false).await;
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

/// The tray's "Check for updates": works whatever the automatic setting says.
pub fn check_now(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move { check(&app, true).await });
}

async fn check(app: &AppHandle, manual: bool) {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let result = run(app).await;
    BUSY.store(false, Ordering::Release);
    match result {
        Ok(Outcome::Installed { version, fresh }) => {
            if fresh {
                crate::log(&format!("update {version} installed; restart to use it"));
            }
            if fresh || manual {
                notify::info(app, "Postern", &format!("Postern {version} installed — restart to use it"));
            }
            tray::show_restart(app, &version);
        }
        Ok(Outcome::UpToDate) => {
            crate::log("no update available");
            if manual {
                notify::info(app, "Postern", "Postern is up to date");
            }
        }
        Err(e) => {
            crate::log(&format!("update check failed: {e}"));
            if manual {
                notify::info(app, "Postern", "Could not check for updates");
            }
        }
    }
}

async fn run(app: &AppHandle) -> Result<Outcome, String> {
    let mut builder = app.updater_builder();
    // Debug builds only, for trying updates against a local server: a release build never
    // lets the environment decide where updates come from.
    if cfg!(debug_assertions) {
        if let Ok(url) = std::env::var("POSTERN_UPDATE_URL") {
            let url = url.parse().map_err(|e| format!("POSTERN_UPDATE_URL: {e}"))?;
            builder = builder.endpoints(vec![url]).map_err(|e| e.to_string())?;
        }
    }
    let updater = builder.build().map_err(|e| e.to_string())?;

    // Only a strictly newer version counts as an update (the plugin's default comparison).
    let found = tokio::time::timeout(CHECK_LIMIT, updater.check()).await.map_err(|_| "timed out")?;
    let Some(update) = found.map_err(|e| e.to_string())? else {
        // Nothing newer than what is running; an update already installed still waits.
        return Ok(match INSTALLED.lock().unwrap().clone() {
            Some(version) => Outcome::Installed { version, fresh: false },
            None => Outcome::UpToDate,
        });
    };
    if INSTALLED.lock().unwrap().as_deref() == Some(update.version.as_str()) {
        return Ok(Outcome::Installed { version: update.version, fresh: false });
    }

    crate::log(&format!("update {} found; downloading", update.version));
    tokio::time::timeout(DOWNLOAD_LIMIT, update.download_and_install(|_, _| {}, || {}))
        .await
        .map_err(|_| "download timed out")?
        .map_err(|e| e.to_string())?;
    *INSTALLED.lock().unwrap() = Some(update.version.clone());
    Ok(Outcome::Installed { version: update.version, fresh: true })
}

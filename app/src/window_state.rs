//! Remembers where the Bulwark window was and how big, across closes and restarts.
//! Kept in `window.json` next to the config, in physical pixels. The normal (restored)
//! bounds are kept while maximised, so un-maximising after a restart lands somewhere sane.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewWindow};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

static CURRENT: Mutex<Option<Bounds>> = Mutex::new(None);
static SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);
const SAVE_DELAY: Duration = Duration::from_millis(600);

fn path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("window.json"))
}

fn load(app: &AppHandle) -> Option<Bounds> {
    let text = std::fs::read_to_string(path(app)?).ok()?;
    serde_json::from_str(&text).ok()
}

fn write(app: &AppHandle, b: Bounds) {
    let Some(p) = path(app) else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(&b) {
        let _ = std::fs::write(p, text);
    }
}

/// True when at least a 100 × 50 px piece of the window's top edge (its title bar) lies on
/// some monitor, so the window can be grabbed and moved.
fn reachable(b: &Bounds, monitors: &[(i32, i32, u32, u32)]) -> bool {
    let (left, right, top) = (b.x, b.x + b.width as i32, b.y);
    monitors.iter().any(|&(mx, my, mw, mh)| {
        let (mr, mb) = (mx + mw as i32, my + mh as i32);
        let overlap = right.min(mr) - left.max(mx);
        overlap >= 100 && top >= my && top + 50 <= mb
    })
}

/// Applies the saved bounds to a freshly built, still hidden window. Returns whether it
/// should be maximised when shown.
pub fn restore(window: &WebviewWindow) -> bool {
    let app = window.app_handle();
    let Some(b) = load(app) else { return false };
    if b.width < 200 || b.height < 200 {
        return false;
    }
    let monitors: Vec<_> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| (m.position().x, m.position().y, m.size().width, m.size().height))
        .collect();
    if reachable(&b, &monitors) {
        let _ = window.set_size(PhysicalSize::new(b.width, b.height));
        let _ = window.set_position(PhysicalPosition::new(b.x, b.y));
    } else {
        let _ = window.set_size(PhysicalSize::new(b.width, b.height));
        let _ = window.center();
    }
    *CURRENT.lock().unwrap() = Some(b);
    b.maximized
}

/// Records the window's current bounds; with `now`, writes them at once, otherwise after
/// the window has stopped moving for a moment.
pub fn track(window: &WebviewWindow, now: bool) {
    if window.is_minimized().unwrap_or(false) || !window.is_visible().unwrap_or(false) {
        return;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    let mut current = CURRENT.lock().unwrap();
    let mut b = current.unwrap_or_default();
    b.maximized = maximized;
    if !maximized {
        let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) else { return };
        b.x = pos.x;
        b.y = pos.y;
        b.width = size.width;
        b.height = size.height;
    }
    if b.width == 0 || b.height == 0 {
        return;
    }
    *current = Some(b);
    drop(current);

    let app = window.app_handle().clone();
    let generation = SAVE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if now {
        write(&app, b);
        return;
    }
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(SAVE_DELAY).await;
        if SAVE_GENERATION.load(Ordering::SeqCst) == generation {
            write(&app, b);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reachable_needs_title_bar_on_a_monitor() {
        let mons = [(0, 0, 1920, 1080), (1920, 0, 2560, 1440)];
        let b = |x, y| Bounds { x, y, width: 1280, height: 860, maximized: false };
        assert!(reachable(&b(100, 100), &mons));
        assert!(reachable(&b(2000, 200), &mons)); // second monitor
        assert!(!reachable(&b(5000, 100), &mons)); // monitor unplugged
        assert!(!reachable(&b(100, -400), &mons)); // title bar above the screen
        assert!(!reachable(&b(-1250, 100), &mons)); // only 30 px showing
    }
}

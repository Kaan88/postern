//! The Linux tray: our own StatusNotifierItem over D-Bus (ksni).
//!
//! Tauri's Linux tray goes through libappindicator, which marks the item as "menu only" and has
//! no click events, so hosts open the menu on a left click and there is no tooltip. Here a left
//! click is `Activate` and opens the webmail, as on Windows; the menu is the right click.
//!
//! ksni calls us on its own (tokio) thread. Anything that touches windows or the app is handed
//! to the main thread, as Tauri's own menu events are.

use std::sync::{Mutex, OnceLock};

use ksni::{menu::{CheckmarkItem, StandardItem}, Category, Handle, Icon, MenuItem, OfflineReason, ToolTip, Tray};
use tauri::AppHandle;

use crate::{
    tray::{autostart_on, icon, on_menu_event},
    viewer,
    watcher::Status,
};

/// What the menu and icon show. Static so a status that arrives before the tray has registered
/// is not lost: the tray reads it whenever the host asks.
struct View {
    status: Status,
    text: String,
    autostart: bool,
    /// The version a "Restart to update" item is offered for.
    restart: Option<String>,
}

static VIEW: Mutex<Option<View>> = Mutex::new(None);
static HANDLE: OnceLock<Handle<PosternTray>> = OnceLock::new();

fn with_view<R>(f: impl FnOnce(&mut View) -> R) -> R {
    let mut guard = VIEW.lock().unwrap();
    f(guard.get_or_insert_with(|| View {
        status: Status::Connecting,
        text: "Starting…".into(),
        autostart: false,
        restart: None,
    }))
}

struct PosternTray {
    app: AppHandle,
}

impl PosternTray {
    /// A menu item that runs `on_menu_event(id)` on the main thread.
    fn item(label: &str, id: &'static str) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            activate: Box::new(move |t: &mut Self| t.dispatch(id)),
            ..Default::default()
        }
        .into()
    }

    fn dispatch(&self, id: &'static str) {
        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || {
            on_menu_event(&app, id);
            if id == "autostart" {
                let on = autostart_on(&app);
                with_view(|v| v.autostart = on);
                refresh();
            }
        });
    }
}

impl Tray for PosternTray {
    fn id(&self) -> String {
        "postern".into()
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn title(&self) -> String {
        "Postern".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        with_view(|v| pixmap(&v.status))
    }

    fn tool_tip(&self) -> ToolTip {
        with_view(|v| ToolTip {
            title: format!("Postern: {}", v.text),
            icon_pixmap: pixmap(&v.status),
            ..Default::default()
        })
    }

    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        let app = self.app.clone();
        let _ = self.app.run_on_main_thread(move || viewer::open(&app, ""));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let (text, autostart, restart) = with_view(|v| (v.text.clone(), v.autostart, v.restart.clone()));
        let mut items = vec![
            StandardItem { label: text, enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            Self::item("Open webmail", "open"),
            Self::item("New message", "compose"),
            MenuItem::Separator,
            Self::item("Settings…", "settings"),
            Self::item("Reset webmail session", "reset"),
            CheckmarkItem {
                label: "Start at login".into(),
                checked: autostart,
                activate: Box::new(|t: &mut Self| t.dispatch("autostart")),
                ..Default::default()
            }
            .into(),
        ];
        if let Some(version) = restart {
            items.push(Self::item(&format!("Restart to update to {version}"), "update_restart"));
        }
        // AppImage only: the others get their updates from the package manager.
        if crate::is_appimage() {
            items.push(Self::item("Check for updates", "update_check"));
        }
        items.push(MenuItem::Separator);
        items.push(Self::item("Quit", "quit"));
        items
    }

    // Without a StatusNotifierWatcher (a bare window manager, or the panel not up yet) the tray
    // waits for one to appear; ksni registers again by itself.
    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        crate::log(&format!("tray: no StatusNotifierWatcher ({reason:?}); waiting for one"));
        true
    }

    fn watcher_online(&self) {
        crate::log("tray: StatusNotifierWatcher is back");
    }
}

/// SNI wants ARGB32, most significant byte first (network order); the PNGs decode to RGBA.
fn pixmap(status: &Status) -> Vec<Icon> {
    let image = icon(status);
    let data = image.rgba().chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
    vec![Icon { width: image.width() as i32, height: image.height() as i32, data }]
}

/// Tells the host that the view changed.
fn refresh() {
    if let Some(handle) = HANDLE.get() {
        let handle = handle.clone();
        tauri::async_runtime::spawn(async move {
            handle.update(|_| {}).await;
        });
    }
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    use ksni::TrayMethods;
    let on = autostart_on(app);
    with_view(|v| v.autostart = on);
    let tray = PosternTray { app: app.clone() };
    // `assume_sni_available`: a missing watcher is not an error but a wait (see watcher_offline).
    tauri::async_runtime::spawn(async move {
        match tray.assume_sni_available(true).spawn().await {
            Ok(handle) => {
                let _ = HANDLE.set(handle);
                // A status may have come in while it was registering.
                refresh();
            }
            Err(e) => crate::log(&format!("tray: {e}")),
        }
    });
    Ok(())
}

pub fn set_status(_app: &AppHandle, status: &Status, text: String) {
    with_view(|v| {
        v.status = status.clone();
        v.text = text;
    });
    refresh();
}

/// Adds (or re-words) "Restart to update to <version>" right above "Check for updates".
pub fn show_restart(_app: &AppHandle, version: &str) {
    with_view(|v| v.restart = Some(version.to_string()));
    refresh();
}

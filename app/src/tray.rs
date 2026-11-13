//! Tray icon and menu. Left click opens the webmail; the menu has the rest.
//!
//! On Linux the tray is a StatusNotifierItem (libappindicator): there are no click events and
//! no tooltip, so the menu is the only way in. It already carries "Open webmail" and the status
//! line, which stands in for the tooltip there.

use std::sync::{LazyLock, Mutex};

use tauri::{
    image::Image,
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

use crate::{viewer, watcher::Status};

const TRAY_ID: &str = "main";

pub struct TrayItems {
    status: MenuItem<Wry>,
    /// Where "Restart to update" is added once an update is installed.
    #[cfg(target_os = "linux")]
    menu: Menu<Wry>,
}

/// The "Restart to update" item, once there is one.
#[cfg(target_os = "linux")]
static RESTART_ITEM: Mutex<Option<MenuItem<Wry>>> = Mutex::new(None);

/// Decoded once: plain, unread, offline.
static ICONS: LazyLock<[Image<'static>; 3]> = LazyLock::new(|| {
    [
        include_bytes!("../icons/tray.png").as_slice(),
        include_bytes!("../icons/tray-unread.png").as_slice(),
        include_bytes!("../icons/tray-offline.png").as_slice(),
    ]
    .map(|bytes| Image::from_bytes(bytes).expect("bundled tray icon"))
});

/// The last status shown, so a refresh that changes nothing doesn't touch the tray.
static SHOWN: Mutex<Option<Status>> = Mutex::new(None);

fn icon(status: &Status) -> Image<'static> {
    let i = match status {
        Status::Online { unread } if *unread > 0 => 1,
        // Connecting is normal (start-up, token renewal): not worth the offline look.
        Status::Online { .. } | Status::Connecting => 0,
        _ => 2,
    };
    ICONS[i].clone()
}

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let open = MenuItem::with_id(app, "open", "Open webmail", true, None::<&str>)?;
    let compose = MenuItem::with_id(app, "compose", "New message", true, None::<&str>)?;
    let settings = MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart = CheckMenuItem::with_id(app, "autostart", "Start at login", true, autostart_on, None::<&str>)?;
    let reset = MenuItem::with_id(app, "reset", "Reset webmail session", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let sep3 = PredefinedMenuItem::separator(app)?;
    // AppImage only: the others get their updates from the package manager.
    let check = crate::is_appimage()
        .then(|| MenuItem::with_id(app, "update_check", "Check for updates", true, None::<&str>))
        .transpose()?;
    let mut items: Vec<&dyn IsMenuItem<Wry>> =
        vec![&status, &sep1, &open, &compose, &sep2, &settings, &reset, &autostart];
    if let Some(check) = &check {
        items.push(check);
    }
    items.extend([&sep3 as &dyn IsMenuItem<Wry>, &quit]);
    let menu = Menu::with_items(app, &items)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(&Status::Connecting))
        .tooltip("Postern")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => viewer::open(app, ""),
            "compose" => viewer::open_mailto(app, "mailto:"),
            "settings" => viewer::open_settings(app),
            "reset" => viewer::reset_session(app),
            "autostart" => {
                let launcher = app.autolaunch();
                let on = launcher.is_enabled().unwrap_or(false);
                let result = if on { launcher.disable() } else { launcher.enable() };
                if let Err(e) = result {
                    crate::log(&format!("autostart: {e}"));
                }
                let _ = autostart.set_checked(launcher.is_enabled().unwrap_or(false));
            }
            #[cfg(target_os = "linux")]
            "update_check" => crate::updates::check_now(app),
            #[cfg(target_os = "linux")]
            "update_restart" => app.request_restart(),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                viewer::open(tray.app_handle(), "");
            }
        })
        .build(app)?;

    app.manage(TrayItems {
        status,
        #[cfg(target_os = "linux")]
        menu,
    });
    Ok(())
}

/// Adds (or re-words) "Restart to update to <version>" right above "Check for updates".
#[cfg(target_os = "linux")]
pub fn show_restart(app: &AppHandle, version: &str) {
    let text = format!("Restart to update to {version}");
    let handle = app.clone();
    // Menus are changed on the main (GTK) thread.
    let _ = app.run_on_main_thread(move || {
        let mut slot = RESTART_ITEM.lock().unwrap();
        if let Some(item) = slot.as_ref() {
            let _ = item.set_text(text);
            return;
        }
        let Some(items) = handle.try_state::<TrayItems>() else { return };
        let Ok(item) = MenuItem::with_id(&handle, "update_restart", text, true, None::<&str>) else { return };
        // Third from the end is "Check for updates" (then the separator and Quit).
        let at = items.menu.items().map_or(0, |all| all.len().saturating_sub(3));
        if items.menu.insert(&item, at).is_ok() {
            *slot = Some(item);
        }
    });
}

pub fn set_status(app: &AppHandle, status: &Status) {
    {
        let mut shown = SHOWN.lock().unwrap();
        if shown.as_ref() == Some(status) {
            return;
        }
        *shown = Some(status.clone());
    }
    let text = match status {
        Status::NotConfigured => "Not set up: open Settings".to_string(),
        Status::Connecting => "Connecting…".to_string(),
        Status::Online { unread: 0 } => "No unread mail".to_string(),
        Status::Online { unread: 1 } => "1 unread message".to_string(),
        Status::Online { unread } => format!("{unread} unread messages"),
        Status::Offline(e) => format!("Offline: {}", e.chars().take(80).collect::<String>()),
        Status::AuthFailed => "Sign-in failed: check Settings".to_string(),
    };
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icon(status)));
        let _ = tray.set_tooltip(Some(format!("Postern: {text}")));
    }
    if let Some(items) = app.try_state::<TrayItems>() {
        let _ = items.status.set_text(text);
    }
}

//! Tray icon and menu. Left click opens the webmail; the menu has the rest.

use std::sync::{LazyLock, Mutex};

use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Wry,
};
use tauri_plugin_autostart::ManagerExt;

use crate::{viewer, watcher::Status};

const TRAY_ID: &str = "main";

pub struct TrayItems {
    status: MenuItem<Wry>,
}

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
    let menu = Menu::with_items(
        app,
        &[&status, &sep1, &open, &compose, &sep2, &settings, &reset, &autostart, &sep3, &quit],
    )?;

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
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                viewer::open(tray.app_handle(), "");
            }
        })
        .build(app)?;

    app.manage(TrayItems { status });
    Ok(())
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

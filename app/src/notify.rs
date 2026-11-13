//! Toasts for new mail and calendar/task reminders. A click opens the message or event in the
//! viewer (Windows and Linux; on macOS the toast is informational for now).

use tauri::AppHandle;

pub struct NewMail {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub preview: String,
}

pub fn new_mail(app: &AppHandle, mail: NewMail) {
    let title = if mail.from.is_empty() { "New mail".to_string() } else { mail.from.clone() };
    let subject = if mail.subject.is_empty() { "(no subject)".to_string() } else { mail.subject.clone() };
    show(app, &title, &subject, &mail.preview, Some(Target::Message(mail.id)));
}

pub fn summary(app: &AppHandle, more: usize) {
    show(app, "New mail", &format!("{more} more new messages"), "", None);
}

pub fn calendar_alert(app: &AppHandle, item: crate::calendar::Item, event_id: String) {
    let target = if item.is_task { Target::Task } else { Target::Event(event_id) };
    show(app, &item.title, &item.when, &item.location, Some(target));
}

/// What a click on the toast opens. Unread on macOS, where the toast is informational.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub enum Target {
    Message(String),
    Event(String),
    Task,
}

#[cfg(windows)]
fn show(app: &AppHandle, title: &str, line1: &str, line2: &str, target: Option<Target>) {
    use tauri_winrt_notification::Toast;

    let app_id = windows_app_id(app);
    let (title, line1, line2) = (title.to_string(), line1.to_string(), line2.to_string());
    let handle = app.clone();
    // WinRT wants an apartment-initialised thread; the main (UI) thread is one.
    let _ = app.run_on_main_thread(move || {
        let click = handle.clone();
        let result = Toast::new(&app_id)
            .title(&title)
            .text1(&line1)
            .text2(&line2)
            .on_activated(move |_action| {
                match &target {
                    Some(Target::Message(id)) => crate::viewer::open_message(&click, id),
                    Some(Target::Event(id)) => crate::viewer::open_calendar_item(&click, id, false),
                    Some(Target::Task) => crate::viewer::open_calendar_item(&click, "", true),
                    None => crate::viewer::open(&click, "mail"),
                }
                Ok(())
            })
            .show();
        match result {
            Ok(()) => crate::log("toast shown"),
            Err(e) => crate::log(&format!("toast failed: {e:?}")),
        }
    });
}

/// Windows shows a toast under an app ID only if a Start Menu shortcut carries it. Both
/// installers make one with our identifier: NSIS at `Programs\Postern.lnk` (per user), the MSI
/// at `Programs\Postern\Postern.lnk` (all users). For a plain `cargo run` without either,
/// borrow PowerShell's, which Windows always knows.
#[cfg(windows)]
fn windows_app_id(app: &AppHandle) -> String {
    use tauri_winrt_notification::Toast;
    let name = &app.package_info().name;
    let shortcut_exists = || {
        [std::env::var("APPDATA").ok(), std::env::var("ProgramData").ok()]
            .into_iter()
            .flatten()
            .any(|root| {
                let programs = std::path::Path::new(&root).join(r"Microsoft\Windows\Start Menu\Programs");
                programs.join(format!("{name}.lnk")).exists() || programs.join(name).join(format!("{name}.lnk")).exists()
            })
    };
    if shortcut_exists() {
        app.config().identifier.clone()
    } else {
        Toast::POWERSHELL_APP_ID.to_string()
    }
}

/// XDG notification: a "default" action is what a click on the body sends. Servers that don't
/// support actions just show the notification.
#[cfg(target_os = "linux")]
fn show(app: &AppHandle, title: &str, line1: &str, line2: &str, target: Option<Target>) {
    let body = if line2.is_empty() { line1.to_string() } else { format!("{line1}\n{line2}") };
    // The body is markup to most servers (links, images), and here it is a stranger's subject
    // and preview: shown as text, never as markup.
    let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let (title, app) = (title.to_string(), app.clone());
    // wait_for_action blocks until the notification is clicked, closed or expires, so each one
    // gets a thread of its own; that ends with the notification, so they don't accumulate.
    std::thread::spawn(move || {
        let shown = notify_rust::Notification::new()
            .appname(&app.package_info().name)
            .summary(&title)
            .body(&body)
            // The icon the .deb installs (named after the binary) and the Postern.desktop that
            // Tauri's bundler writes (named after productName), so the shell can match them.
            .icon("postern")
            .hint(notify_rust::Hint::DesktopEntry("Postern".into()))
            .action("default", "Open")
            .show();
        match shown {
            Ok(handle) => handle.wait_for_action(|action| {
                if action != "default" {
                    return; // "__closed": dismissed or expired
                }
                // Window creation has to happen on the main (GTK) thread.
                let click = app.clone();
                let _ = app.run_on_main_thread(move || match &target {
                    Some(Target::Message(id)) => crate::viewer::open_message(&click, id),
                    Some(Target::Event(id)) => crate::viewer::open_calendar_item(&click, id, false),
                    Some(Target::Task) => crate::viewer::open_calendar_item(&click, "", true),
                    None => crate::viewer::open(&click, "mail"),
                });
            }),
            Err(e) => crate::log(&format!("notification: {e}")),
        }
    });
}

/// A plain notification: no click action. Only the updater uses it.
#[cfg(target_os = "linux")]
pub fn info(app: &AppHandle, title: &str, body: &str) {
    let (title, body, name) = (title.to_string(), body.to_string(), app.package_info().name.clone());
    // show() talks to the notification server over D-Bus: not for the main thread.
    std::thread::spawn(move || {
        let shown = notify_rust::Notification::new()
            .appname(&name)
            .summary(&title)
            .body(&body)
            .icon("postern")
            .hint(notify_rust::Hint::DesktopEntry("Postern".into()))
            .show();
        if let Err(e) = shown {
            crate::log(&format!("notification: {e}"));
        }
    });
}

#[cfg(not(any(windows, target_os = "linux")))]
fn show(_app: &AppHandle, title: &str, line1: &str, line2: &str, _target: Option<Target>) {
    let body = if line2.is_empty() { line1.to_string() } else { format!("{line1}\n{line2}") };
    if let Err(e) = notify_rust::Notification::new().summary(title).body(&body).show() {
        crate::log(&format!("notification: {e}"));
    }
}

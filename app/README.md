# Postern: technical notes

The user-facing overview is in [../README.md](../README.md). This file covers how it works and what has been verified.

A small resident app for a Stalwart (JMAP) account. It sits in the tray, gets new-mail push and calendar/task reminders straight from the server, and opens your hosted Bulwark in a webview only while you use it. Closing the window frees the webview completely; the tray process stays at a few MB. Background: [../FEASIBILITY.md](../FEASIBILITY.md) option D.

The log (connection, each push event, each alert and toast; no addresses, names or subjects) goes to stderr with `cargo run` and to `%LOCALAPPDATA%\app.postern\logs\postern.log`, which starts over past 1 MB.

## What it does

- **Tray:** an icon with a red dot when the Inbox has unread mail, and a status line in the menu ("3 unread messages", "Offline: …"). Left click opens Bulwark. The menu also has New message, Settings…, Start at login and Quit.
- **Push:** one JMAP EventSource for Email, Mailbox and CalendarAlert (`src/watcher.rs`). It re-arms every 5 minutes so a connection that died silently (sleep, network change) is replaced, and backs off from 5 s to 5 min while the server can't be reached. When the access token is renewed, the new stream is opened before the old one is closed, and a catch-up read covers the swap. A server that keeps refusing fresh tokens (HTTP 401) gets "Sign-in failed" after four tries instead of a retry loop.
- **Mail toasts:** up to 3 per burst for new unread Inbox mail received in the last 24 h, then "N more". A click opens that message.
- **Reminder toasts** (`src/calendar.rs`): Stalwart fires DISPLAY alarms itself and pushes a `CalendarAlert`. The app fetches the event or task title, time and location and shows them, converting the event's time zone to this computer's (floating times are shown as written) and following a moved instance of a recurring event. A click opens the event, or the task list for a task, while Postern is running. These arrive whether or not Bulwark is open.
- **Viewer** (`src/viewer.rs`): your Bulwark instance in a webview window created on demand. Closing the window destroys it, unless "Keep the window in memory after closing" is set to a number of seconds (`keep_viewer_secs`, at most 3600). Only Bulwark's origin and the mail server's load in it; other links open in the default browser and new windows are refused. The window's size and position are kept in `window.json`. The webview profile, with Bulwark's cookies, is kept on disk across closes. Sign in once with **"Sign in with your browser"** on Bulwark's login page (see below), or with the form and "Remember me" ticked. Either way Bulwark sets a 30-day cookie that the webview profile keeps across closes; without "Remember me" it is a browser-session cookie, lost every time the window is destroyed.
- **Browser sign-in for Bulwark** (`src/viewer_auth.rs`): Bulwark's login page offers "Sign in with your browser" when it finds its desktop app's bridge (`window.bulwarkDesktop`, version 1). The viewer provides only the sign-in part of it. Bulwark's login opens in your default browser (password manager, passkeys, SSO all work there); Bulwark parks the session on its server and sends a one-time code to a listener on `127.0.0.1/bulwark-desktop/callback`; the page in the viewer redeems it with a PKCE verifier only it holds. The tray sees only that code. The three commands behind it are granted at run time to the viewer on the configured Bulwark origin only, and each checks the caller again. Side effects of the bridge being present: Bulwark treats the window as its desktop app (no browser push sign-up, which the tray replaces anyway, and some "app" wording). The Bulwark admin can turn this off (`desktopBrowserSignIn`). To skip Bulwark's page in the browser and go straight to Stalwart's sign-in (passkeys), turn on "OAuth only" and "Auto SSO" in Bulwark's admin (Authentication); the viewer is kept from auto-redirecting itself, so it still shows the button. Bulwark's session and the tray's are separate sign-ins: each OAuth client has its own rotating refresh token, so they can't share one.
- **Single instance:** a second launch hands its arguments to the running copy. A `mailto:` argument opens Bulwark's composer (`/protocol/mailto?url=…`).
- **Sign-in (OAuth, `src/auth.rs`):** no password is stored or seen. Settings > Sign in reads the server's `/.well-known/oauth-authorization-server`, registers a public client (dynamic registration, loopback redirect `http://127.0.0.1/callback`), and opens your browser at Stalwart's login page. The answer comes back to a one-shot listener on `127.0.0.1` that checks `state`; the code is exchanged with PKCE (S256). Scopes: `openid offline_access` plus JMAP mail and calendars. Only the refresh token is kept, in the OS keyring (Windows Credential Manager, service `postern`, entry `oauth-refresh-token`); Stalwart rotates it on each refresh and the app stores the new one. The access token lives in memory and the push connection is renewed just before it expires. A rejected refresh token shows "Sign-in failed" in the tray. Sign out asks the server to revoke the token (best effort: Settings says whether the server confirmed it) and always forgets it locally. The server's OAuth endpoints, like the URLs in Settings, must use https (plain http is accepted only for this machine). The other settings are in `%APPDATA%\app.postern\config.json`. A 0.1.x install (`dev.bulwark-native.tray`, keyring service `bulwark-native`) is migrated once at start-up (`config::migrate_legacy`), only when Postern has no settings of its own yet; the old token is copied, not moved. Settings files are written atomically.
- **Isolation:** only the local settings window can call the app's settings commands (`capabilities/settings.json`), under a strict content security policy. Bulwark's pages get only the three browser sign-in commands, on Bulwark's origin.

## Server requirements

- Stalwart with JMAP and its built-in OAuth (on by default; open client registration for loopback redirects). No app password is needed.
- For reminders: calendar alarms enabled on the server (web admin: Calendar Alarms > "Enable E-mail Alarms", which despite its name gates all alarms). "Minimum Alarm Interval" is the least time between two alarms of one event: with the default 1 hour, a second reminder on the same event within the hour is skipped. Stalwart skips alarms when they are off, for draft events, and for calendars you're not subscribed to. Only alarms with action DISPLAY are pushed; Stalwart emails EMAIL alarms itself.

## Build and run

```bash
cargo run
```

```bash
cargo build --release
```

Installers, laid out like Bulwark Desktop's releases (NSIS per user and MSI per machine, `Postern-Setup-<v>-<arch>.exe`, `Postern-<v>-<arch>.msi`, a `.sha256` for each). They also add the Start Menu shortcut that notifications need:

```bash
cargo install tauri-cli --version "^2" --locked
```

```powershell
powershell -File scripts/release.ps1 -Arch x64
```

ARM64 needs Visual Studio's "MSVC ARM64 build tools" (the TLS library aws-lc compiles C code); the GitHub workflow builds it on a Windows ARM runner. Icons and installer bitmaps come from `scripts/make-icons.py`, then `cargo tauri icon icons/icon-1024.png -o icons`.

Without the installed shortcut (NSIS: `Start Menu\Programs\Postern.lnk`; MSI: `Start Menu\Programs\Postern\Postern.lnk`), toasts appear under PowerShell's name (see `src/notify.rs`).

## Status

Verified on Windows 11 with WebView2 157:
- It builds with no warnings; unit tests pass (reminder text and time zones, PKCE against the RFC 7636 vector, the loopback listener rejecting a forged `state`, https-only endpoints, duplicate sign-in parameters, window placement).
- On first run it opens Settings, and the settings page loads its values through IPC.
- A second launch, with or without a `mailto:` argument, reaches the running copy.
- With no Bulwark URL set, opening the viewer brings up Settings instead.
- **Against a real Stalwart (2026-11-11):** connected with the calendars capability present. An event reminder and a task reminder each arrived as a `CalendarAlert` within a second of their time, and both were shown as toasts with Bulwark closed. A new mail was toasted about 1 s after delivery.
- Closing a window takes WebView2 from 6 processes (~150–200 MB private) to 0 within ~4 s, while the app keeps running at ~7.8 MB private (debug build).

- **Installer (2026-11-11):** the x64 NSIS setup (2.4 MB) installed silently per user into `%LOCALAPPDATA%\Postern` with a Start Menu shortcut. On first start it moved the 0.1.x settings and token over and connected to the server, running at 7.8 MB.

Not yet verified:
- the MSI and the ARM64 builds; notifications under the installed shortcut's name;
- renewing the access token mid-session when it expires (renewal at start-up works);
- clicking a toast to open the message or event;
- tray menu clicks;
- acknowledging a reminder back to the server (not implemented, so Bulwark may show it again if it's open);
- registering as the system mailto: handler (not implemented);
- macOS and Linux.

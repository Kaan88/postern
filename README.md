<p align="center"><img src="app/icons/128x128.png" width="96" alt=""></p>

<h1 align="center">Postern</h1>

<p align="center">A light tray companion for <b>Bulwark Mail</b> on a <b>Stalwart</b> server.<br>
Notifications and reminders come straight from your server; Bulwark opens only while you use it.</p>

---

Postern sits in the system tray (Windows and Linux) and keeps a single push connection to your Stalwart server. New mail, calendar reminders and task reminders arrive as desktop notifications, whether or not Bulwark is open. Click one, or the tray icon, and your own Bulwark opens in a window. Close the window and its browser engine is shut down completely, so Postern goes back to a few megabytes on Windows (on Linux a WebKit network process of about 70 MB stays; see the [technical notes](app/README.md)).

> Postern is an independent project. It is not made, endorsed or supported by the Bulwark project or by Stalwart Labs. "Bulwark" and "Stalwart" are used only to say what Postern works with.

## How it differs from Bulwark Desktop

[Bulwark Desktop](https://github.com/bulwarkmail/desktop) is the official app: a full desktop client built on Electron, with its own windows, menus, offline mode, policies and auto-updates. Postern does far less, on purpose.

| | Postern | Bulwark Desktop |
|---|---|---|
| What stays running | A small tray process (about 7 MB of memory, measured on Windows 11) | The Electron app |
| Browser engine | The system's WebView2 (Windows) or WebKitGTK (Linux), started when you open Bulwark and shut down when you close it (all its processes end within about 2.5 s) | Chromium, bundled with the app |
| Where reminders come from | Your Stalwart server fires them (JMAP `CalendarAlert` push), so they arrive with no Bulwark page open | Bulwark's own reminder logic |
| New-mail notifications | JMAP push from Stalwart, about 1 s after delivery in testing | Bulwark's own notifications |
| Sign-in | OAuth with PKCE in your normal browser, so passkeys and your password manager work. Only a renewable token is kept, in Windows Credential Manager or the Linux Secret Service (KWallet, GNOME Keyring) | Bulwark's sign-in, including its own "Sign in with your browser" |
| What it needs | A Stalwart server and a hosted Bulwark | Any JMAP server, with or without a Bulwark instance |
| Platforms | Windows (tested). Linux (new: tested on KDE Plasma, Wayland). The code builds for macOS but is untested there | Windows, macOS, Linux |

Use Bulwark Desktop if you want a full mail client on your desktop. Use Postern if you mostly want to know when something arrives, and open your webmail now and then.

## Features

- **Tray icon** with an unread dot, plus a status line: "3 unread messages", "Offline: …", "Sign-in failed: check Settings".
- **Notifications** for new Inbox mail (up to three per burst, then a summary) and for calendar and task reminders, with times shown in your own time zone. While Postern is running, clicking one opens that message, event or task list in Bulwark.
- **Bulwark on demand:** your hosted Bulwark in a window that exists only while it's open. It reopens where you left it. Optionally kept warm for a few minutes for instant reopening. Links to other sites open in your normal browser.
- **Works behind a sign-in gateway:** if Cloudflare Access, Authelia, oauth2-proxy or similar sits in front of your Bulwark, its login runs inside the window, so the session lands where it's needed. "Reset webmail session" in the tray menu clears a stuck one.
- **Browser sign-in for Bulwark too:** Bulwark's login page shows "Sign in with your browser" inside Postern. It uses Bulwark's own hand-off, so your passkey or single sign-on happens in your normal browser.
- **No passwords stored:** OAuth 2.0 authorization code with PKCE (RFC 8252), dynamic client registration with Stalwart, refresh-token rotation. Signing out asks the server to revoke the token and tells you whether it did.
- **Start at login**, and one instance at a time. A `mailto:` link passed to Postern opens Bulwark's composer. Registering Postern as the default mail app isn't implemented yet.

## Requirements

- **Stalwart** with OAuth (on by default) and, for reminders, calendar alarms turned on (web admin › Calendar › Alarms; the "Enable E-mail Alarms" switch turns on all alarms). Only DISPLAY alarms are pushed.
- **Bulwark Mail** hosted somewhere you can reach. For browser sign-in, the instance must allow it (the `desktopBrowserSignIn` admin setting, on by default).
- **Windows 10 1809 or later** with the WebView2 runtime (built into Windows 11), or
- **Linux** with WebKitGTK 4.1, a tray that shows StatusNotifier/AppIndicator icons (KDE Plasma does; GNOME needs the AppIndicator extension) and a Secret Service keyring.

## Install

Download from [Releases](../../releases):

- `Postern-Setup-<version>-x64.exe`: for most people. Installs for your user only, without admin rights.
- `Postern-<version>-x64.msi`: for IT. Installs for all users in Program Files.
- `-arm64` variants for Windows on ARM.

For Linux (`-x64` or `-arm64`):

- `Postern-<version>-x64.AppImage`: runs anywhere, no installation. Make it executable and start it. On first start it adds itself to your application menu, and it **updates itself**: it looks for a new release daily, checks the download's signature, replaces its own file and offers "Restart to update" in the tray menu. Turn that off in Settings; "Check for updates" in the tray menu works either way. Keep the file somewhere permanent (for example `~/Applications`) before you start it.
- `Postern-<version>-x64.deb` (Debian, Ubuntu) and `.rpm` (Fedora, openSUSE): installed and updated through your package manager.

Check a Linux download with `sha256sum -c Postern-<version>-x64.AppImage.sha256`, or with `gh attestation verify` as below.

The Windows installers are not code-signed yet, so Windows SmartScreen will warn you. Check the download against its `.sha256` file first:

```powershell
(Get-FileHash .\Postern-Setup-0.3.1-x64.exe -Algorithm SHA256).Hash
```

Or, with the [GitHub CLI](https://cli.github.com/), check that it was built by this repository's release workflow:

```bash
gh attestation verify Postern-Setup-0.3.1-x64.exe --repo Kaan88/postern
```

To uninstall, sign out in Settings first, so the token is revoked and removed from Windows Credential Manager; then remove Postern from Windows Settings › Apps. Your settings stay in `%APPDATA%\app.postern` until you delete that folder. On Linux, remove the package, or delete the AppImage together with `~/.local/share/applications/Postern.desktop` and the `postern.png` icons under `~/.local/share/icons/hicolor`; settings are in `~/.config/app.postern` and data in `~/.local/share/app.postern`.

## Build

```bash
cd app
cargo test
cargo tauri build
```

To build both architectures with release file names and checksums, as on the Releases page:

```powershell
powershell -File app/scripts/release.ps1
```

On Linux, for the host architecture (all three packages, or name the ones you want):

```bash
bash app/scripts/release.sh deb,rpm
```

Technical notes are in [app/README.md](app/README.md), and the design background is in [FEASIBILITY.md](FEASIBILITY.md).

## Audit

Postern 0.2.0 was reviewed before release by Fable (Anthropic's Claude Fable 5.2): two passes covering correctness, security, reliability and performance. Every finding was fixed or is listed as a known limitation in the [technical notes](app/README.md).

## License

MIT. Postern contains no code from Bulwark or Stalwart. It talks to them over their public protocols and interfaces (JMAP, OAuth, and Bulwark's documented desktop sign-in bridge). It uses Stalwart Labs' `jmap-client` crate (Apache-2.0 or MIT) and Tauri (Apache-2.0 or MIT).

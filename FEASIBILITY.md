# Bulwark → native Rust desktop: feasibility

Researched 2026-11-11 against `bulwarkmail/webmail` @ `73d82df` (v1.21.1).

## Verdict

A **native Rust rewrite** with feature parity that keeps up with upstream is **not feasible**.
A **Rust (Tauri) shell around the upstream web build** is feasible and cheap, and it keeps up with upstream almost on its own.

## What upstream is

| | |
|---|---|
| Stack | Next.js 16 + React 19 + TypeScript, Zustand, TipTap, pdf.js, Tailwind 4 |
| Size | ~405k LOC non-test TS/TSX (lib 180k, components 150k, stores 32k, app 31k, hooks 12k), 2,144 test files, 32 locales |
| Server side | 100 Next API routes (auth/OAuth/SSO, admin, plugins, marketplace, push, settings sync, CalDAV discovery, iCal fetch, BIMI, WOPI…) |
| Extensibility | JS plugins in sandboxed iframes, CSS themes, extension marketplace |
| Licence | AGPL-3.0-only (private use: no obligations) |
| Editions | **Full** (Node server) and **Lite** (`npm run build:lite`, static export, no admin/OAuth/plugins/settings sync) |
| Official desktop | `bulwarkmail/desktop`: Electron 45 shell (~75k LOC TS) embedding the full Next server, plus a small **Rust** native helper (Windows Hello, OS keystore for S/MIME). v0.1.0, betas on every push, no stable release yet |

### How fast it moves

- Releases: 1.9 → 1.21.1 between 2026-08-26 and 2026-11-11 (a release every 2–4 days).
- Commits/month: Aug 303, Sep 360, Oct 1,536, Nov (11 days) 1,990.
- Last 30 days: 4,059 files changed, +738k / −54k lines. ~95% of commits come from one maintainer.

## Option A: native Rust rewrite (iced / Slint / egui / GPUI / Dioxus)

Why it fails on parity and upstream tracking:

1. **Scale.** ~400k LOC of client logic (JMAP mail, JSCalendar with recurrence, JSContact/vCard, WebDAV files, Sieve filters, S/MIME/PGP, PST/EML/TNEF import, templates, scheduling, offline store) would need rewriting. That is years of work for one person.
2. **Nothing to sync with.** Upstream changes about 25k lines a day in TypeScript UI code. A Rust port can't merge those changes. Each one has to be re-implemented by hand, and the backlog grows faster than one person can clear it.
3. **You'd still need a web engine.** HTML email rendering, the rich-text composer (TipTap/ProseMirror has no mature Rust equivalent), pdf.js previews, JS plugins and CSS themes all need a browser engine. So "native" ends up embedding a webview anyway.
4. **Rust ecosystem gaps.** `jmap-client` (Stalwart, v0.4.3, Sep 2026) covers core JMAP mail, but not the calendars/contacts/files layer Bulwark uses.

A **reduced-scope** native client is realistic as a separate project: mail-only, `jmap-client` + iced/Slint, plain text plus a webview for HTML bodies. But it would be a different app that borrows ideas, not a port.

## Option B: Tauri 2 shell (Rust host + system WebView). Recommended if you want Rust

| Variant | Content | Node needed | Parity | Effort |
|---|---|---|---|---|
| B1 Instance window | Load your own Bulwark server URL | No | Full (server-side) | Days |
| B2 Embedded Lite | Ship `build:lite` output as Tauri assets | No | Lite feature set (no plugins, OAuth, admin, settings sync) | ~1–2 weeks |
| B3 Embedded Full | `build:desktop` Next server as a Node sidecar | Yes (sidecar) | Full | Weeks; you re-do what the Electron repo already does |

Tracking upstream: rebuild the pinned webmail tag and repackage. You don't maintain any of the UI.

The work is in the native bridge. The webmail expects `window.bulwarkDesktop` (contract in `lib/desktop/types.ts`, ~1.4k lines; ~7.3k lines of page-side glue in `lib/desktop/`). Without it you get a plain webview: no tray, native notifications, default-mail-app handling, Outlook/Thunderbird import, edit-attachment, keystore. You can implement a subset in Rust over Tauri IPC, but **this contract changes upstream too**, so this is where the ongoing maintenance lands.

Risks:
- WebView2 (Windows) and WebKitGTK/WKWebView differ from Electron's Chromium. Upstream only tests Chromium, so expect rendering and feature gaps on macOS/Linux; Windows (WebView2 = Chromium) is close.
- CORS/origin: the Lite build calls JMAP from `tauri://`/`http://tauri.localhost`. Either allow that origin on Stalwart or proxy JMAP through a Rust custom protocol handler.
- The service worker, PWA bits and web push behave differently in an embedded webview.

## Option C: use the official Electron desktop app

It already has full parity and tracks upstream by design (same maintainers, built from webmail `main`/tag), and it already uses Rust for its native helper. The cost is Electron's footprint (~150–250 MB RAM, a large install) and pre-1.0 maturity.

## Option D: resident Rust daemon + on-demand webview viewer (the lightest that keeps parity)

Split the app into a resident part and an on-demand viewer. **Measured (below): on Windows, one Tauri process is enough.** Destroying the window frees the whole WebView2 tree, so the two-binary split is optional.

```
postern  (always running; the webview is created only on demand)
  ├─ tray icon + unread badge         tray-icon / muda
  ├─ JMAP push (WebSocket/EventSource) jmap-client ("websockets" feature; event_source module)
  ├─ new-mail toasts                   notify-rust / Windows toast via winrt
  ├─ mailto: handler, autostart        registry / desktop file; auto-launch crate
  └─ credentials                        keyring crate (OAuth refresh token only)

bulwark-view  (spawned on demand, exits when its window closes)
  └─ Tauri/wry window loading your hosted Bulwark instance,
     with a persistent profile dir so the login and IndexedDB cache survive
```

- **Idle cost (measured, Windows, test harness):** one process with ~6.5 MB private and ~27–29 MB working set after a webview has been used and closed; ~3 MB private and ~13 MB working set before the first one. The real daemon adds push/tray code on top. Electron stays at ~150–250 MB all day.
- **Open cost:** a cold start of WebView2 plus Bulwark loading from your server (estimated ~1–2 s; its IndexedDB cache makes later opens faster). Optionally keep the viewer alive for N minutes after closing.
- **Deep links:** a toast click spawns the viewer at `/mail/message/<id>` (upstream `lib/deep-links.ts`). A mailto: link goes to the webmail's own mailto handler (`lib/protocol-handlers/mailto.ts`).
- **One process or two:** one Tauri process that keeps running with no windows (`RunEvent::ExitRequested` → `prevent_exit()`) releases WebView2 fully on Windows; see the measurement below. Two binaries remain an option for isolation (a crash in the viewer can't take the notifier down), not for memory.
- **Upstream tracking:**
  - The daemon depends only on the JMAP RFCs (8620/8621/8887), which are stable.
  - The viewer is your hosted Bulwark, so updating the server updates the viewer; nothing to rebuild.
  - No `window.bulwarkDesktop` bridge is needed. The daemon covers the OS integration that matters (tray, toasts, badge, mailto).
- **What you give up:**
  - Electron-only extras: Outlook/Thunderbird import, edit-attachment round-trip, Windows Hello, OS keystore for S/MIME.
- **Two logins:** the daemon holds its own credential, and the viewer logs in once with remember-me.
- **Effort:** daemon ~1–2k lines of Rust, about 1–2 weeks part-time. Viewer ~100 lines.

### Measurement: WebView2 cleanup in one long-lived Tauri process

Harness: [experiments/wv2-lifecycle](experiments/wv2-lifecycle). Setup: Tauri 2 (tao 0.37, webview2-com 0.39), WebView2 runtime 157.0.4341.42, Windows 11, release build, CPU idle (<30%) before each run. The app starts with no windows and runs 3 cycles: open a window with a page holding ~240 MB of JS heap plus 20k DOM nodes, wait 12 s, close it, wait 25 s. It then idles 60 s with no windows. A sampler records every process descended from the app about once a second.

| | While open | ~1–2.5 s after close | Idle 60 s after last close |
|---|---|---|---|
| WebView2 processes | 6 | **0** | 0 |
| WebView2 tree private / working set | ~460–520 / ~610–640 MB | 0 | 0 |
| Host process private / working set | ~6.5–7 / ~27–30 MB | unchanged | ~6.5 / ~27–29 MB |

- Same result for `WebviewWindow::destroy()` and `close()`, which is the X-button path. The data is in `experiments/wv2-lifecycle/results/`.
- No growth from one cycle to the next. The host's one-time step (+~4 MB private, +~15 MB working set) is WebView2/loader state that stays after the first webview.
- The window reopens without noticeable delay (all 6 processes up within the ~1 s sample). Bulwark's own load time comes on top and is not measured.
- **Not covered:** macOS (WKWebView) and Linux (WebKitGTK); a real Bulwark page with a service worker (once no webview is left the browser process exits, so a service worker can't keep it alive, but this wasn't tested with Bulwark); long soak runs.

**Decision (2026-11-11):** no Lite; the viewer loads the hosted instance. Built in [app/](app/README.md), which also shows the server's calendar/task reminders (below).

### Calendar and task reminders: Bulwark vs. Stalwart

Bulwark (webmail @ `73d82df`) computes reminders in the page: `hooks/use-calendar-alerts.ts`, or `hooks/use-store-reminders.ts` for accounts with the offline store. Consequences:
- **No Bulwark tab, no reminder.** Its web push subscribes to `EmailDelivery` only (`lib/web-push.ts`, `PUSH_TYPES`), and the service worker only shows reminders a page handed it.
- **System notifications** need the browser's notification permission, which the reminder code never asks for (`lib/calendar-reminders/system-notification.ts`). They also only show when the tab is hidden or unfocused; otherwise you get the in-app toast.
- **Task reminders** without the offline store come from `useTaskStore().tasks`. That list is only loaded when the tasks view, tasks-on-calendar or the side panel is opened in that session (`components/calendar/calendar-app.tsx`). Usually it's empty, so tasks never ring.
- Only `display` alerts ring; `email` alerts are left to the server.

Stalwart fires DISPLAY alarms server-side (`crates/services/src/task_manager/tasks/alarm.rs`) and pushes a JMAP `CalendarAlert` to push subscribers that request that type (`crates/common/src/ipc/subscribers.rs`). It's on by the server's `alarms_enabled`, and skipped for drafts and unsubscribed calendars. The tray app subscribes to it, so reminders work with Bulwark closed. Tested against a real server on 2026-11-11: an event reminder and a task reminder each arrived on time and were shown with Bulwark closed.

## Recommendation

1. For daily use now: **C** (official desktop app beta), or just the PWA.
2. If the goal is a lighter Rust-hosted app: start with **B1** (an afternoon) or **B2**. Implement the bridge only for what you miss (notifications, tray, mailto). Pin a webmail tag and bump it with a script.
3. Treat a true native Rust UI as a separate, mail-only hobby client, not a port.

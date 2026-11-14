# Notification sounds

These four MP3 files are Bulwark Mail's notification sounds, copied unchanged so that Postern
plays the same sound as Bulwark does:

- Repository: https://github.com/bulwarkmail/webmail
- Path: `public/notification/` (the choices are listed in `lib/notification-sound.ts`)
- Upstream commit: e94849668eb1786cc1b3c4cfda088c0dfc043a89

| File | Bulwark's id |
| --- | --- |
| `cheerful-527.mp3` | cheerful |
| `involved-notification.mp3` | involved (Bulwark's default) |
| `notification-tone-swift-gesture.mp3` | swift |
| `relax-message-tone.mp3` | relax |

Bulwark's fifth choice, "default" (a beep), is not a file; Postern synthesizes it
(`src/sound.rs`).

**These files are not covered by Postern's MIT licence.** Bulwark's webmail is licensed
AGPL-3.0-only. The sound files carry no licence or attribution of their own upstream, and where
they originally came from is not stated there. Postern includes them because its owner decided
to; **they will be removed on request** (open an issue or write to the maintainer), and Postern
then plays no sound for those choices.

`app/scripts/sync-sounds.sh` refreshes this folder from upstream (and `--check` only reports
whether it is out of date); it is run when preparing a release.

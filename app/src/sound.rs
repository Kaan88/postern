//! The notification sound: the same ones Bulwark offers (its lib/notification-sound.ts), played
//! by Postern itself when it shows a notification. The files are in app/sounds (see its README
//! for where they come from and how they are refreshed).
//!
//! Nothing here stays open: each sound opens the audio device on a thread of its own, plays,
//! and closes the device again, so an idle Postern holds no audio resources.

use std::{
    io::Cursor,
    num::NonZero,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use rodio::{buffer::SamplesBuffer, Decoder, DeviceSinkBuilder, Player, Source};
use tauri::{AppHandle, Manager};

/// What a setting value maps to. `bytes` is None for the synthesized beep.
pub struct Sound {
    pub id: &'static str,
    /// Only the tests read it: the sync script reads the same names from the mp3! lines.
    #[allow(dead_code)]
    pub file: Option<&'static str>,
    bytes: Option<&'static [u8]>,
}

/// Bulwark's "default" sound, synthesized (see `beep`).
macro_rules! beep {
    ($id:literal) => {
        Sound { id: $id, file: None, bytes: None }
    };
}

/// One of Bulwark's files. scripts/sync-sounds.sh reads these lines (and the beep! one), so
/// keep one entry per line in this exact form.
macro_rules! mp3 {
    ($id:literal, $file:literal) => {
        Sound { id: $id, file: Some($file), bytes: Some(include_bytes!(concat!("../sounds/", $file))) }
    };
}

/// The ids and files of Bulwark's NOTIFICATION_SOUNDS.
static SOUNDS: [Sound; 5] = [
    beep!("default"),
    mp3!("cheerful", "cheerful-527.mp3"),
    mp3!("involved", "involved-notification.mp3"),
    mp3!("swift", "notification-tone-swift-gesture.mp3"),
    mp3!("relax", "relax-message-tone.mp3"),
];

/// The setting that turns the sound off. Any other value that is not an id also means off.
pub const OFF: &str = "off";
/// The setting "Same as Bulwark": the sound is whatever Bulwark was last seen to use, for mail
/// and for reminders separately (`Config::bulwark_sound`, `Config::bulwark_reminder_sound`,
/// reported by the viewer, see viewer_auth.rs).
pub const BULWARK: &str = "bulwark";
/// The default setting.
pub const DEFAULT_SETTING: &str = BULWARK;
/// What Bulwark plays when its user chose nothing; assumed until the webmail has reported.
pub const BULWARK_DEFAULT: &str = "involved";
/// What the remembered Bulwark values hold until the webmail has reported: "not seen yet", which
/// is not the same as Bulwark being set to its default.
pub const UNKNOWN: &str = "";

/// Which kind of notification a sound is for. Bulwark has a sound switch for each.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Mail,
    Reminder,
}

/// Bulwark plays its files at this volume (`audio.volume = 0.3`).
const FILE_VOLUME: f32 = 0.3;
/// A sound is cut off after this long, so a device that stops consuming audio cannot keep the
/// next sounds from playing.
const MAX_PLAY: Duration = Duration::from_secs(15);
/// The device keeps about 100 ms of audio queued after the player is empty; closing it at once
/// would clip the end.
const TAIL: Duration = Duration::from_millis(250);

pub fn find(id: &str) -> Option<&'static Sound> {
    SOUNDS.iter().find(|s| s.id == id)
}

/// What Bulwark can be using, and so what the webmail may report: "off" or one of the ids.
pub fn is_choice(value: &str) -> bool {
    value == OFF || find(value).is_some()
}

/// A setting value that the settings page may store: "same as Bulwark", "off" or one of the ids.
pub fn is_valid_setting(value: &str) -> bool {
    value == BULWARK || is_choice(value)
}

/// The sound a setting means, given what Bulwark was last seen to use for this kind of
/// notification (`UNKNOWN` if never). None is silence.
pub fn resolve(setting: &str, bulwark: &str) -> Option<&'static Sound> {
    let bulwark = if bulwark == UNKNOWN { BULWARK_DEFAULT } else { bulwark };
    find(if setting == BULWARK { bulwark } else { setting })
}

/// What Bulwark was last seen to use for this kind of notification.
pub fn remembered(config: &crate::config::Config, kind: Kind) -> &str {
    match kind {
        Kind::Mail => &config.bulwark_sound,
        // A config from before reminders were reported separately knows only the mail value.
        Kind::Reminder if config.bulwark_reminder_sound == UNKNOWN => &config.bulwark_sound,
        Kind::Reminder => &config.bulwark_reminder_sound,
    }
}

/// A notification is being shown: play the chosen sound, unless the webmail window is in use.
/// Never waits for the audio.
pub fn notify(app: &AppHandle, kind: Kind) {
    let Some(sound) = chosen(app, kind) else { return };
    // The one place for this rule: Bulwark plays its own sound in an open window, and two at
    // once is wrong. A viewer kept alive hidden (keep_viewer_secs) is a closed window to the
    // user, so only a visible one counts. If the state can't be read, play: a missed
    // notification sound is worse than a doubled one.
    let viewer_open = app.get_webview_window(crate::viewer::LABEL).is_some_and(|w| w.is_visible().unwrap_or(false));
    if viewer_open {
        return;
    }
    play(sound);
}

/// The sound the saved settings mean right now for this kind of notification.
pub fn chosen(app: &AppHandle, kind: Kind) -> Option<&'static Sound> {
    let state = app.state::<crate::AppState>();
    let config = state.config.lock().unwrap();
    resolve(&config.notification_sound, remembered(&config, kind))
}

/// Plays one sound now (the settings page's Play button), whatever the window state.
pub fn play(sound: &'static Sound) {
    // One at a time: a request that arrives while a sound is playing is dropped.
    static BUSY: AtomicBool = AtomicBool::new(false);
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            BUSY.store(false, Ordering::SeqCst);
        }
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    // The device handle is not Send on every platform and must outlive the playback, so all of
    // it happens on this one thread.
    let spawned = std::thread::Builder::new().name("sound".into()).spawn(move || {
        let _release = Release;
        if let Err(e) = run(sound) {
            crate::log(&format!("sound: {e}"));
        }
    });
    if let Err(e) = spawned {
        BUSY.store(false, Ordering::SeqCst);
        crate::log(&format!("sound: could not start a thread: {e}"));
    }
}

fn run(sound: &Sound) -> Result<(), String> {
    // Decoded before the device is opened: a bad file costs nothing.
    let file = match sound.bytes {
        Some(bytes) => Some(Decoder::new(Cursor::new(bytes)).map_err(|e| format!("{}: {e}", sound.id))?),
        None => None,
    };
    let mut device = DeviceSinkBuilder::open_default_sink().map_err(|e| describe(&e))?;
    device.log_on_drop(false); // rodio would print "Dropping DeviceSink" on every sound
    let player = Player::connect_new(device.mixer());
    match file {
        Some(decoded) => {
            player.set_volume(FILE_VOLUME);
            player.append(decoded);
        }
        None => player.append(beep()),
    }
    let started = Instant::now();
    while !player.empty() && started.elapsed() < MAX_PLAY {
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(TAIL);
    Ok(()) // the player, then the device, are dropped here: the stream closes
}

/// The error with its cause (rodio's own messages are generic; the cause names the device problem).
fn describe(e: &dyn std::error::Error) -> String {
    let mut text = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        text.push_str(&format!(": {c}"));
        cause = c.source();
    }
    text
}

const BEEP_RATE: u32 = 44_100;
const BEEP_SECS: f32 = 0.45;
const BEEP_HZ: f32 = 800.0;
const BEEP_PEAK: f32 = 0.12;
const BEEP_FLOOR: f32 = 0.0001;
const BEEP_RISE_SECS: f32 = 0.04;
const BEEP_FALL_SECS: f32 = 0.08;

/// Bulwark's beep: an 800 Hz sine of 0.45 s whose gain rises exponentially from 0.0001 to 0.12
/// in 40 ms, holds, and falls the same way over the last 80 ms (WebAudio's
/// exponentialRampToValueAtTime). Its peak is already at the level Bulwark plays it at.
fn beep_samples() -> Vec<f32> {
    let n = (BEEP_SECS * BEEP_RATE as f32) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / BEEP_RATE as f32;
            let gain = if t < BEEP_RISE_SECS {
                BEEP_FLOOR * (BEEP_PEAK / BEEP_FLOOR).powf(t / BEEP_RISE_SECS)
            } else if t < BEEP_SECS - BEEP_FALL_SECS {
                BEEP_PEAK
            } else {
                BEEP_PEAK * (BEEP_FLOOR / BEEP_PEAK).powf((t - (BEEP_SECS - BEEP_FALL_SECS)) / BEEP_FALL_SECS)
            };
            gain * (2.0 * std::f32::consts::PI * BEEP_HZ * t).sin()
        })
        .collect()
}

fn beep() -> impl Source + Send + 'static {
    SamplesBuffer::new(NonZero::new(1).unwrap(), NonZero::new(BEEP_RATE).unwrap(), beep_samples())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_map_to_bulwarks_files() {
        let expected = [
            ("default", None),
            ("cheerful", Some("cheerful-527.mp3")),
            ("involved", Some("involved-notification.mp3")),
            ("swift", Some("notification-tone-swift-gesture.mp3")),
            ("relax", Some("relax-message-tone.mp3")),
        ];
        for (id, file) in expected {
            let s = find(id).unwrap_or_else(|| panic!("no sound {id}"));
            assert_eq!(s.file, file);
            assert_eq!(s.bytes.is_some(), file.is_some());
        }
        assert_eq!(SOUNDS.len(), expected.len());
        assert!(find("DEFAULT").is_none() && find("off").is_none() && find("").is_none());
        assert!(is_valid_setting(DEFAULT_SETTING));
    }

    #[test]
    fn settings_validate() {
        for ok in ["bulwark", "off", "default", "cheerful", "involved", "swift", "relax"] {
            assert!(is_valid_setting(ok), "{ok}");
        }
        for bad in ["", "Off", "Bulwark", "loud", "involved ", "../x"] {
            assert!(!is_valid_setting(bad), "{bad}");
        }
    }

    #[test]
    fn the_webmail_may_report_only_ids_and_off() {
        for ok in ["off", "default", "cheerful", "involved", "swift", "relax"] {
            assert!(is_choice(ok), "{ok}");
        }
        for bad in ["bulwark", "", "Off", "loud", "involved ", "<script>", "involved\0"] {
            assert!(!is_choice(bad), "{bad}");
        }
    }

    #[test]
    fn same_as_bulwark_follows_what_bulwark_reported() {
        assert_eq!(resolve(BULWARK, "cheerful").map(|s| s.id), Some("cheerful"));
        assert_eq!(resolve(BULWARK, "default").map(|s| s.id), Some("default"));
        assert!(resolve(BULWARK, "off").is_none());
        assert!(resolve(BULWARK, "garbage").is_none());
        // Never reported: Bulwark's own default, for an explicit choice nothing changes.
        assert_eq!(resolve(BULWARK, UNKNOWN).map(|s| s.id), Some(BULWARK_DEFAULT));
        assert_eq!(resolve("swift", UNKNOWN).map(|s| s.id), Some("swift"));
        assert!(resolve("off", UNKNOWN).is_none());
        // An explicit choice ignores Bulwark's.
        assert_eq!(resolve("swift", "off").map(|s| s.id), Some("swift"));
        assert!(resolve("off", "cheerful").is_none());
        assert!(find(BULWARK_DEFAULT).is_some());
    }

    #[test]
    fn mail_and_reminders_are_remembered_separately() {
        let c = crate::config::Config {
            bulwark_sound: "cheerful".into(),
            bulwark_reminder_sound: "off".into(),
            ..Default::default()
        };
        assert_eq!(resolve(BULWARK, remembered(&c, Kind::Mail)).map(|s| s.id), Some("cheerful"));
        assert!(resolve(BULWARK, remembered(&c, Kind::Reminder)).is_none());
        // An explicit choice applies to both.
        assert_eq!(resolve("swift", remembered(&c, Kind::Reminder)).map(|s| s.id), Some("swift"));
        assert_eq!(remembered(&crate::config::Config::default(), Kind::Reminder), UNKNOWN);
    }

    #[test]
    fn every_embedded_file_decodes() {
        for s in SOUNDS.iter().filter(|s| s.bytes.is_some()) {
            let decoder = Decoder::new(Cursor::new(s.bytes.unwrap())).unwrap_or_else(|e| panic!("{}: {e}", s.id));
            let channels = decoder.channels().get() as usize;
            let rate = decoder.sample_rate().get() as usize;
            let samples = decoder.count();
            let secs = samples as f32 / (channels * rate) as f32;
            assert!((0.3..10.0).contains(&secs), "{}: {secs} s", s.id);
        }
    }

    #[test]
    fn beep_has_the_envelope() {
        let b = beep_samples();
        assert_eq!(b.len(), (BEEP_SECS * BEEP_RATE as f32) as usize); // 0.45 s
        let peak = |from: f32, to: f32| {
            b[(from * BEEP_RATE as f32) as usize..(to * BEEP_RATE as f32) as usize].iter().fold(0f32, |m, x| m.max(x.abs()))
        };
        assert!(b[0].abs() < 1e-3);
        assert!(peak(0.0, 0.005) < 0.01, "starts near silence");
        assert!(peak(0.04, 0.2) > 0.119 && peak(0.0, BEEP_SECS) <= BEEP_PEAK + 1e-6, "peaks at 0.12");
        assert!(peak(0.0, 0.02) < peak(0.02, 0.04), "rises");
        assert!(peak(BEEP_SECS - 0.005, BEEP_SECS) < 0.01, "ends near silence");
        // 800 Hz: count rising zero crossings in 0.3 s of the hold.
        let hold = &b[(0.05 * BEEP_RATE as f32) as usize..(0.35 * BEEP_RATE as f32) as usize];
        let crossings = hold.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((239..=241).contains(&crossings), "{crossings} cycles in 0.3 s");
    }
}

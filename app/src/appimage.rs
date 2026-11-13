//! AppImage only: puts Postern in the user's application menu. An AppImage has no installer, so
//! a menu entry and its icons are written on start-up (the deb and rpm bring their own).
//!
//! The entry is `Postern.desktop`: notifications name that desktop entry (see notify.rs), and
//! `Icon`/`StartupWMClass` are the same as in the deb's entry. Nothing is written when the
//! files are already as they should be, so a normal start touches nothing; moving the AppImage
//! changes its Exec line and the next start fixes it.

use std::{
    fs,
    path::{Path, PathBuf},
};

const DESKTOP_FILE: &str = "Postern.desktop";
/// Marks an entry as ours. One without it (made by hand, or by a launcher that integrates
/// AppImages itself) is left alone.
const MARKER: &str = "X-Postern-AppImage=true";

/// Sizes the bundled PNGs really have (checked with `file`, not trusted from their names).
const ICONS: [(u32, &[u8]); 5] = [
    (32, include_bytes!("../icons/32x32.png")),
    (64, include_bytes!("../icons/64x64.png")),
    (128, include_bytes!("../icons/128x128.png")),
    (256, include_bytes!("../icons/128x128@2x.png")),
    (512, include_bytes!("../icons/icon.png")),
];

/// The AppImage file we are running from, if we are.
///
/// `APPIMAGE` alone isn't proof: a Postern from a package, started by something that is itself
/// an AppImage (a terminal, a launcher), inherits that program's variables. Taking them at their
/// word would point the menu entry at the other program and let an update overwrite its file.
/// So our own executable has to sit inside the mounted image (`APPDIR`).
pub fn path() -> Option<PathBuf> {
    let var = |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (image, dir) = (var("APPIMAGE")?, var("APPDIR")?.canonicalize().ok()?);
    let inside = std::env::current_exe().is_ok_and(|exe| exe.starts_with(&dir));
    (inside && image.is_file()).then_some(image)
}

/// `$XDG_DATA_HOME`, or `~/.local/share`. The spec ignores a relative XDG_DATA_HOME.
fn data_home() -> Option<PathBuf> {
    let from_env = |name| std::env::var_os(name).map(PathBuf::from).filter(|p| p.is_absolute());
    from_env("XDG_DATA_HOME").or_else(|| from_env("HOME").map(|h| h.join(".local/share")))
}

/// Writes the menu entry and icons for the AppImage we are running from. Never fatal.
pub fn integrate() {
    let (Some(appimage), Some(data)) = (path(), data_home()) else { return };
    match install(&data, &appimage) {
        Ok(true) => crate::log("menu entry and icons updated"),
        Ok(false) => {}
        Err(e) => crate::log(&format!("menu entry: {e}")),
    }
}

/// Returns whether anything was written.
fn install(data: &Path, appimage: &Path) -> Result<bool, String> {
    let desktop = desktop_entry(appimage).ok_or("the AppImage path has characters a menu entry cannot hold")?;
    let entry = data.join("applications").join(DESKTOP_FILE);
    match fs::read_to_string(&entry) {
        Ok(old) if !old.lines().any(|l| l == MARKER) => {
            return Err(format!("{} is not ours; left alone", entry.display()));
        }
        _ => {}
    }

    let mut changed = false;
    let mut write = |path: PathBuf, bytes: &[u8]| -> Result<(), String> {
        if fs::read(&path).is_ok_and(|old| old == bytes) {
            return Ok(());
        }
        crate::config::write_atomic(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        changed = true;
        Ok(())
    };
    // Icons first, so the entry never points at one that isn't there yet.
    for (size, png) in ICONS {
        write(data.join(format!("icons/hicolor/{size}x{size}/apps/postern.png")), png)?;
    }
    write(entry, desktop.as_bytes())?;
    Ok(changed)
}

/// Same keys as the deb's entry, plus TryExec (the entry hides itself once the file is gone)
/// and the marker. None if the path can't be written into a desktop file.
fn desktop_entry(appimage: &Path) -> Option<String> {
    let path = appimage.to_str().filter(|p| !p.chars().any(char::is_control))?;
    Some(format!(
        "[Desktop Entry]\n\
         Categories=Network;Email;\n\
         Comment=Postern: a light tray companion for Bulwark Mail\n\
         Exec={}\n\
         TryExec={}\n\
         StartupWMClass=Postern\n\
         Icon=postern\n\
         Name=Postern\n\
         Terminal=false\n\
         Type=Application\n\
         {MARKER}\n",
        exec_arg(path),
        path.replace('\\', "\\\\"),
    ))
}

/// One argument for an Exec line. The spec has two layers: the argument is double-quoted, with
/// `"`, `` ` ``, `$` and `\` backslash-escaped inside the quotes and `%` doubled (it starts a
/// field code); then, as with every string value, each backslash is doubled again.
fn exec_arg(arg: &str) -> String {
    let mut quoted = String::from('"');
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_arg_plain_and_spaces() {
        assert_eq!(exec_arg("/home/a/Postern.AppImage"), r#""/home/a/Postern.AppImage""#);
        assert_eq!(exec_arg("/home/a b/Postern.AppImage"), r#""/home/a b/Postern.AppImage""#);
    }

    #[test]
    fn exec_arg_escapes_both_layers() {
        // a quote and a dollar need a backslash inside the quotes, which is then doubled
        assert_eq!(exec_arg(r#"/a"b"#), r#""/a\\"b""#);
        assert_eq!(exec_arg("/a$b`c"), r#""/a\\$b\\`c""#);
        // a literal backslash becomes four
        assert_eq!(exec_arg(r"/a\b"), r#""/a\\\\b""#);
        // a percent sign would otherwise be a field code
        assert_eq!(exec_arg("/100%/p"), r#""/100%%/p""#);
    }

    #[test]
    fn desktop_entry_content() {
        let d = desktop_entry(Path::new("/opt/My Apps/Postern.AppImage")).unwrap();
        assert!(d.starts_with("[Desktop Entry]\n"));
        assert!(d.contains("\nExec=\"/opt/My Apps/Postern.AppImage\"\n"));
        assert!(d.contains("\nTryExec=/opt/My Apps/Postern.AppImage\n"));
        assert!(d.contains("\nIcon=postern\n") && d.contains("\nStartupWMClass=Postern\n"));
        assert!(d.contains("\nName=Postern\n") && d.ends_with(&format!("\n{MARKER}\n")));
    }

    #[test]
    fn desktop_entry_refuses_control_characters() {
        assert!(desktop_entry(Path::new("/a\nExec=evil")).is_none());
    }

    #[test]
    fn install_writes_once_and_leaves_foreign_entries() {
        let dir = std::env::temp_dir().join(format!("postern-appimage-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let image = dir.join("Postern.AppImage");

        assert_eq!(install(&dir, &image), Ok(true));
        assert!(dir.join("icons/hicolor/256x256/apps/postern.png").is_file());
        // nothing changed: nothing written
        assert_eq!(install(&dir, &image), Ok(false));
        // moved: the entry follows
        assert_eq!(install(&dir, &dir.join("moved.AppImage")), Ok(true));
        let entry = dir.join("applications").join(DESKTOP_FILE);
        assert!(fs::read_to_string(&entry).unwrap().contains("moved.AppImage"));

        // an entry without our marker is not touched
        fs::write(&entry, "[Desktop Entry]\nName=Mine\n").unwrap();
        assert!(install(&dir, &image).is_err());
        assert_eq!(fs::read_to_string(&entry).unwrap(), "[Desktop Entry]\nName=Mine\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn icon_sizes_match_their_pixels() {
        // a PNG's IHDR holds width then height, big-endian, right after the 8-byte signature
        for (size, png) in ICONS {
            let dim = |at: usize| u32::from_be_bytes(png[at..at + 4].try_into().unwrap());
            assert_eq!((dim(16), dim(20)), (size, size));
        }
    }
}

fn main() {
    // The exe embeds icons/icon.ico; without this a changed icon isn't picked up until a clean build.
    println!("cargo:rerun-if-changed=icons");
    // App commands are denied unless a capability grants them. The settings window gets the
    // first four (capabilities/settings.json); the viewer, on the configured Bulwark origin
    // only, the sign-in three, added at run time (src/viewer_auth.rs); and the viewer's own local
    // "finish signing in" page the last one (capabilities/access.json), which Bulwark can't call.
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "get_settings",
            "save_settings",
            "sign_in",
            "sign_out",
            "viewer_sign_in_begin",
            "viewer_sign_in_complete",
            "viewer_sign_in_cancel",
            "access_use_window",
        ]),
    ))
    .expect("tauri build");
}

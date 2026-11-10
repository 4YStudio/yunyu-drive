fn main() {
    // `tauri-codegen` embeds the window/tray icon at compile time, but
    // `tauri-build` only emits `rerun-if-changed` for `tauri.conf.json` and
    // `capabilities/`. Without the line below, replacing an icon does *not*
    // invalidate the cached codegen output — the binary (and therefore the
    // taskbar/window icon) silently keeps the old artwork while the files under
    // /usr/share/icons are already new. That is exactly how a "I replaced the
    // logo but the icon is still the old one" bug happens.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}

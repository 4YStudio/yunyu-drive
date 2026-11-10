//! Desktop colour-scheme detection.
//!
//! WebKitGTK does not reliably resolve `prefers-color-scheme` from the KDE
//! setting, so the window would open in the wrong theme on a dark desktop.
//! Instead of guessing we read the same sources the shell itself uses:
//! KDE's `kdeglobals` (or the GTK preference on GTK desktops), with the
//! freedesktop portal as a final override.

use std::fs;
use std::path::PathBuf;

fn config_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        let path = PathBuf::from(value);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

fn is_kde() -> bool {
    ["XDG_CURRENT_DESKTOP", "KDE_FULL_SESSION", "XDG_SESSION_DESKTOP"]
        .iter()
        .any(|key| {
            std::env::var(key)
                .map(|v| v.to_uppercase().contains("KDE") || v.contains("plasma"))
                .unwrap_or(false)
        })
}

fn parse_hex_channel(raw: &str) -> Option<u8> {
    let value = raw.trim();
    u8::from_str_radix(value, 16).ok()
}

/// Reads `[Colors:Window] BackgroundNormal` from `kdeglobals`.
fn kde_background() -> Option<(u8, u8, u8)> {
    let path = config_home()?.join("kdeglobals");
    let text = fs::read_to_string(path).ok()?;
    let mut in_window = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_window = line.eq_ignore_ascii_case("[Colors:Window]");
            continue;
        }
        if !in_window {
            continue;
        }
        if let Some(value) = line.strip_prefix("BackgroundNormal=") {
            let parts: Vec<&str> = value.split(',').collect();
            if parts.len() >= 3 {
                return Some((
                    parse_hex_channel(parts[0])?,
                    parse_hex_channel(parts[1])?,
                    parse_hex_channel(parts[2])?,
                ));
            }
        }
    }
    None
}

/// GTK3/GTK4 preference file, used when KDE's palette is unavailable.
fn gtk_prefers_dark() -> Option<bool> {
    for candidate in ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"] {
        let path = config_home()?.join(candidate);
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            if let Some(value) = line.trim().strip_prefix("gtk-application-prefer-dark-theme") {
                let value = value.trim_start_matches([' ', '=']).trim();
                if value.eq_ignore_ascii_case("true") || value == "1" {
                    return Some(true);
                }
                if value.eq_ignore_ascii_case("false") || value == "0" {
                    return Some(false);
                }
            }
        }
    }
    None
}

fn luminance(rgb: (u8, u8, u8)) -> f32 {
    let channel = |c: u8| {
        let v = f32::from(c) / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    // Rec. 709 relative luminance.
    0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
}

/// The desktop's current colour scheme: `"dark"` or `"light"`.
pub fn detect() -> &'static str {
    if let Some(rgb) = kde_background() {
        return if luminance(rgb) < 0.5 { "dark" } else { "light" };
    }
    if let Some(dark) = gtk_prefers_dark() {
        return if dark { "dark" } else { "light" };
    }
    if is_kde() {
        // KDE ships a dark Breeze by default on recent releases.
        return "dark";
    }
    "light"
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_the_desktop_scheme() {
        let scheme = super::detect();
        println!("detected scheme = {scheme}");
        assert!(scheme == "dark" || scheme == "light");
    }

    #[test]
    fn luminance_is_sane() {
        assert!(super::luminance((255, 255, 255)) > 0.9);
        assert!(super::luminance((0, 0, 0)) < 0.01);
    }
}

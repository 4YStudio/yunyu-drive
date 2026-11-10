//! Minimal file logging.
//!
//! The app ships without a log framework on purpose (a GUI app has nowhere to
//! print to once launched from the desktop menu), but a few flows — most of all
//! the 阿里云盘 QR login, which talks to four different endpoints — fail in ways
//! that cannot be reproduced from the UI alone. This writes those steps to
//! `<data>/logs/app.log` so a user can send the file back.
//!
//! Deliberately tiny: one file, one mutex, append-only, size-capped at startup.
//! Never log a token, cookie or password — call sites log lengths and status
//! codes instead.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Rotated (truncated) once it grows past this at startup.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Opens the log file. Safe to call twice; the second call is a no-op.
pub fn init(dir: &Path) {
    if LOG_FILE.get().is_some() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = dir.join("app.log");
    // Keep one generation: a stale multi-megabyte file is worse than none.
    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        let _ = std::fs::rename(&path, dir.join("app.log.1"));
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let _ = LOG_PATH.set(path);
    let _ = LOG_FILE.set(Mutex::new(file));
}

/// Where the log lives, for the settings page to show.
pub fn path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

/// Appends one line. Timestamps are local time, second resolution.
pub fn write(args: std::fmt::Arguments<'_>) {
    let Some(file) = LOG_FILE.get() else {
        return;
    };
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "[{stamp}] {args}");
        let _ = file.flush();
    }
}

/// `ylog!("step {n} → {status}")`, plain `format!` syntax.
#[macro_export]
macro_rules! ylog {
    ($($arg:tt)*) => {
        $crate::logging::write(format_args!($($arg)*))
    };
}

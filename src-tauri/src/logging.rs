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

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

use serde::Serialize;

/// Rotated (truncated) once it grows past this at startup.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// How many lines the in-app viewer can scroll back through.
const MAX_LINES: usize = 4000;

/// One line of the in-app log console.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    /// `HH:MM:SS.mmm`, local time.
    pub at: String,
    /// `log` | `ok` | `warn` | `error` | `invoke`.
    pub level: String,
    /// Who logged it: a module name, or the Tauri command for `invoke` lines.
    pub target: String,
    pub message: String,
}

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LINES: OnceLock<Mutex<VecDeque<LogLine>>> = OnceLock::new();
static NEXT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn lines() -> &'static Mutex<VecDeque<LogLine>> {
    LINES.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

/// Appends one line to the in-memory ring buffer the UI reads.
pub fn push(level: &str, target: &str, message: impl Into<String>) {
    let line = LogLine {
        seq: NEXT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        at: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
        level: level.to_string(),
        target: target.to_string(),
        message: message.into(),
    };
    if let Ok(mut buf) = lines().lock() {
        if buf.len() >= MAX_LINES {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

/// Lines newer than `since` (a `seq`), oldest first.
pub fn tail(since: u64, limit: usize) -> Vec<LogLine> {
    let Ok(buf) = lines().lock() else {
        return Vec::new();
    };
    let cap = limit.clamp(1, MAX_LINES);
    let mut out: Vec<LogLine> = buf
        .iter()
        .filter(|line| line.seq > since)
        .cloned()
        .collect();
    if out.len() > cap {
        out.drain(..out.len() - cap);
    }
    out
}

pub fn clear() {
    if let Ok(mut buf) = lines().lock() {
        buf.clear();
    }
}

/// One HTTP exchange, in the shape a browser's network panel would show it.
///
/// Logged at `invoke` level so the console can filter request chatter away from
/// real errors. The query string is dropped on purpose: provider URLs carry
/// session keys and signed tokens, and this panel is meant to be pasted into a
/// bug report.
pub fn http(target: &str, method: &str, url: &str, status: Option<u16>, ms: u128, bytes: usize) {
    let path = match url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(at) => rest[at..].split('?').next().unwrap_or("/"),
            None => "/",
        },
        None => url,
    };
    let status = match status {
        Some(code) => code.to_string(),
        None => "无响应".to_string(),
    };
    push(
        "invoke",
        target,
        format!("{method} {path} → {status} · {ms} ms · {bytes} B"),
    );
}

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
    let message = args.to_string();
    push("log", "app", message.clone());
    let Some(file) = LOG_FILE.get() else {
        return;
    };
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    if let Ok(mut file) = file.lock() {
        let _ = writeln!(file, "[{stamp}] {message}");
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

/// Lines newer than `since`, for the in-app console.
#[tauri::command]
pub fn logs_tail(since: u64, limit: Option<usize>) -> Vec<LogLine> {
    tail(since, limit.unwrap_or(500))
}

/// Empties the in-memory buffer (the file on disk is kept).
#[tauri::command]
pub fn logs_clear() {
    clear();
}

/// Where the on-disk log lives, so the viewer can point at it.
#[tauri::command]
pub fn logs_file() -> Option<String> {
    path().map(|path| path.display().to_string())
}

/// A line from the frontend: `console.*`, a Vue error, or a finished command.
#[tauri::command]
pub fn log_emit(level: String, target: String, message: String) {
    let level = match level.as_str() {
        "ok" | "warn" | "error" | "invoke" | "log" => level,
        _ => "log".to_string(),
    };
    push(&level, &target, message);
}

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

/// Directory name under `~/.config`, `~/.cache` and `~/.local/share`.
///
/// Renamed from `quark-desktop` when the app grew into a multi-drive client.
/// [`AppPaths::resolve`] migrates the old directory once, so an existing user
/// keeps their cookie, settings and transfer history instead of being logged out.
pub const APP_DIR_NAME: &str = "yunyu-drive";

/// Name used before the rebrand; only read during migration.
const LEGACY_DIR_NAME: &str = "quark-desktop";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub download_dir: String,
    pub upload_conflict: String,
    pub concurrent_transfers: u32,
    pub confirm_delete: bool,
    pub api_base_url: String,
    pub show_hidden: bool,
    pub list_density: String,
    pub last_dir_fid: String,
    /// Keep running in the tray when the window is closed.
    pub close_to_tray: bool,
    /// Desktop notifications for finished/failed transfers and share links.
    pub notifications: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            upload_conflict: "rename".to_string(),
            concurrent_transfers: 3,
            confirm_delete: true,
            api_base_url: "https://drive.quark.cn".to_string(),
            show_hidden: false,
            list_density: "comfortable".to_string(),
            last_dir_fid: "0".to_string(),
            close_to_tray: true,
            notifications: true,
        }
    }
}

fn default_download_dir() -> String {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
    // `xdg-user-dir` honours localised folder names (e.g. ~/下载 on zh_CN),
    // so ask it first and only then fall back to the conventional names.
    if let Ok(out) = std::process::Command::new("xdg-user-dir").arg("DOWNLOAD").output() {
        if out.status.success() {
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if dir != home.to_string_lossy() && Path::new(&dir).is_dir() {
                return dir;
            }
        }
    }
    for name in ["Downloads", "下载", "Téléchargements", "Загрузки"] {
        let candidate = home.join(name);
        if candidate.is_dir() {
            return candidate.to_string_lossy().to_string();
        }
    }
    home.to_string_lossy().to_string()
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> AppResult<Self> {
        let dirs = ProjectDirs::from("", "", APP_DIR_NAME).ok_or_else(|| {
            AppError::new("无法定位系统配置目录（XDG_CONFIG_HOME 未设置）")
        })?;
        let paths = Self {
            config_dir: dirs.config_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
            log_dir: dirs.data_local_dir().join("logs"),
        };
        paths.migrate_legacy();
        Ok(paths)
    }

    /// Copies the pre-rename `quark-desktop` directories over the first time the
    /// renamed build starts.
    ///
    /// Copy (not move) on purpose: if anything goes wrong half-way the old data
    /// is still intact, and a user who downgrades keeps their session. The
    /// marker file makes this a one-shot that never overwrites newer data.
    fn migrate_legacy(&self) {
        let marker = self.config_dir.join(".migrated-from-quark-desktop");
        if marker.exists() {
            return;
        }
        let Some(legacy) = ProjectDirs::from("", "", LEGACY_DIR_NAME) else {
            return;
        };
        let moved = copy_dir_if_missing(legacy.config_dir(), &self.config_dir);
        let _ = copy_dir_if_missing(legacy.cache_dir(), &self.cache_dir);
        if moved > 0 {
            let _ = std::fs::write(
                &marker,
                format!("migrated {moved} files from {LEGACY_DIR_NAME}\n"),
            );
        }
    }

    pub fn cookie_file(&self) -> PathBuf {
        self.config_dir.join("cookie.txt")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    pub fn transfers_file(&self) -> PathBuf {
        self.config_dir.join("transfers.json")
    }

    pub fn ensure(&self) -> AppResult<()> {
        for dir in [&self.config_dir, &self.cache_dir, &self.log_dir] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// Copies files from `from` into `to`, skipping anything that already exists.
/// Returns how many files were actually copied.
fn copy_dir_if_missing(from: &Path, to: &Path) -> usize {
    if !from.is_dir() {
        return 0;
    }
    let _ = std::fs::create_dir_all(to);
    let mut copied = 0;
    let Ok(entries) = std::fs::read_dir(from) else {
        return 0;
    };
    for entry in entries.flatten() {
        let src = entry.path();
        if !src.is_file() {
            continue;
        }
        let Some(name) = src.file_name() else { continue };
        let dst = to.join(name);
        if dst.exists() {
            continue;
        }
        if std::fs::copy(&src, &dst).is_ok() {
            restrict_permissions(&dst);
            copied += 1;
        }
    }
    copied
}

/// Cookie resolution order mirrors the CLI: env var first, then the config file.
pub fn read_cookie(paths: &AppPaths) -> Option<String> {
    if let Ok(value) = std::env::var("QUARK_COOKIE") {
        let trimmed = value.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    read_cookie_file(&paths.cookie_file())
}

pub fn read_cookie_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub fn write_cookie(paths: &AppPaths, cookie: &str) -> AppResult<()> {
    paths.ensure()?;
    let path = paths.cookie_file();
    std::fs::write(&path, format!("{}\n", cookie.trim()))?;
    restrict_permissions(&path);
    Ok(())
}

pub fn clear_cookie(paths: &AppPaths) -> AppResult<()> {
    let path = paths.cookie_file();
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
/// Same as the internal helper, exposed for adapters that write their own
/// credential files (139's token, 123 云盘's session).
pub fn restrict_permissions_pub(path: &Path) {
    restrict_permissions(path)
}

fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

pub fn load_settings(paths: &AppPaths) -> Settings {
    let path = paths.settings_file();
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save_settings(paths: &AppPaths, settings: &Settings) -> AppResult<()> {
    paths.ensure()?;
    let text = serde_json::to_string_pretty(settings)?;
    std::fs::write(paths.settings_file(), text)?;
    Ok(())
}

/// Best-effort masking for displaying a stored cookie without leaking it.
pub fn mask_cookie(cookie: &str) -> String {
    let mut parts = Vec::new();
    for pair in cookie.split(';') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((key, value)) if value.len() > 8 => {
                parts.push(format!("{key}={}…{}", &value[..4], &value[value.len() - 4..]));
            }
            Some((key, _)) => parts.push(format!("{key}=***")),
            None => parts.push("***".to_string()),
        }
    }
    parts.join("; ")
}

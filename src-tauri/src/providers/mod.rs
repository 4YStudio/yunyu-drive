//! The multi-drive abstraction.
//!
//! Every screen, transfer and share operation in the app talks to a
//! [`DriveProvider`] instead of a concrete client, so adding a cloud drive means
//! implementing one trait rather than editing the transfer engine and every
//! command.
//!
//! Two conventions matter everywhere below:
//!
//! * **Ids are namespaced.** A drive's own file id is only meaningful to that
//!   drive (Quark uses hex fids, 139 uses long integers, 123 云盘 uses int64).
//!   Every id that crosses the IPC boundary is `"<provider>:<native>"`; see
//!   [`EntryId`]. The frontend only ever sees the composite form.
//! * **The provider owns the network, the manager owns the policy.** Downloads
//!   hand back a byte stream and let [`crate::transfer`] do resume, retry,
//!   pause and progress. Uploads run inside the provider because rapid-upload
//!   and chunking differ wildly, but they report progress through the same
//!   counters, so the UI is uniform.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};

pub use aliyun::AliyunProvider;
pub use baidu::BaiduProvider;
pub use pan123::Pan123Provider;
pub use quark::QuarkProvider;
pub use yun139::Yun139Provider;

pub mod aliyun;
pub mod aliyun_qr;
pub mod baidu;
pub mod dto;
pub mod pan123;
pub mod quark;
pub mod yun139;

pub use dto::{DirListing, DownloadRequest, EntryDto, PathNode, QuotaInfo, RecycleEntry, SearchHit};

// ---------------------------------------------------------------- identity ---

/// The cloud drives this app can talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderId {
    /// 夸克网盘
    Quark,
    /// 中国移动云盘 (139)
    Yun139,
    /// 阿里云盘
    Aliyun,
    /// 123 云盘
    Pan123,
    /// 百度网盘
    Baidu,
}

impl ProviderId {
    /// Stable key used in ids, settings and the frontend.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Quark => "quark",
            ProviderId::Yun139 => "yun139",
            ProviderId::Aliyun => "aliyun",
            ProviderId::Pan123 => "pan123",
            ProviderId::Baidu => "baidu",
        }
    }

    /// Human name for menus, error messages and the transfer list.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::Quark => "夸克网盘",
            ProviderId::Yun139 => "中国移动云盘",
            ProviderId::Aliyun => "阿里云盘",
            ProviderId::Pan123 => "123 云盘",
            ProviderId::Baidu => "百度网盘",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "quark" => Some(ProviderId::Quark),
            "yun139" | "139" | "cloud139" => Some(ProviderId::Yun139),
            "aliyun" | "aliyundrive" | "alipan" => Some(ProviderId::Aliyun),
            "pan123" | "123pan" | "123" => Some(ProviderId::Pan123),
            "baidu" | "baidupan" | "pan_baidu" | "bdpan" => Some(ProviderId::Baidu),
            _ => None,
        }
    }

    /// Every drive, in the order the UI lists them.
    pub fn all() -> [ProviderId; 5] {
        [
            ProviderId::Quark,
            ProviderId::Yun139,
            ProviderId::Aliyun,
            ProviderId::Pan123,
            ProviderId::Baidu,
        ]
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A file id qualified with the drive it belongs to: `"quark:abc123"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryId {
    pub provider: ProviderId,
    pub native: String,
}

impl EntryId {
    #[allow(dead_code)]
    pub fn new(provider: ProviderId, native: impl Into<String>) -> Self {
        Self {
            provider,
            native: native.into(),
        }
    }

    /// Splits `"<provider>:<native>"`. A bare id is rejected: silently guessing
    /// the provider is how a delete lands on the wrong drive.
    pub fn parse(raw: &str) -> AppResult<Self> {
        let raw = raw.trim();
        let (head, native) = raw
            .split_once(':')
            .ok_or_else(|| AppError::new(format!("无效的文件 ID：{raw}")))?;
        let provider = ProviderId::parse(head)
            .ok_or_else(|| AppError::new(format!("未知的网盘：{head}")))?;
        if native.is_empty() {
            return Err(AppError::new(format!("无效的文件 ID：{raw}")));
        }
        Ok(Self {
            provider,
            native: native.to_string(),
        })
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider.as_str(), self.native)
    }
}

/// Qualifies a native id for the IPC boundary.
pub fn qualify(provider: ProviderId, native: &str) -> String {
    format!("{}:{}", provider.as_str(), native)
}

/// Strips a `<provider>:` prefix, leaving bare ids untouched.
///
/// Bare ids are the normal case internally: `root`, `0` and `/` are passed
/// around as drive-native markers, and each caller already knows which drive it
/// is talking to, so anything without a recognised provider prefix is returned
/// as-is.
pub fn native_of(raw: &str) -> String {
    match raw.split_once(':') {
        Some((head, native)) if ProviderId::parse(head).is_some() => native.to_string(),
        _ => raw.to_string(),
    }
}

/// Sorts a listing the way every drive should present it: folders first, then
/// case-insensitive by name.
///
/// Quark always did this; the other four drives returned whatever order the API
/// happened to use, so the same folder looked shuffled depending on the drive.
/// Sorting here (instead of only in the UI) also fixes the order used by
/// `search`, whose hits are built from listings.
/// Reads a timestamp that a drive may send as epoch seconds, epoch millis, or an
/// ISO-8601 string — 123 sends `"2026-11-09T15:12:31+08:00"`, Baidu sends
/// `server_mtime` in seconds, and 139 mixes both shapes. Reading only one shape
/// silently produced `1970-01-01` in the file list.
pub fn parse_time(value: &serde_json::Value) -> u64 {
    if let Some(number) = value.as_i64() {
        if number <= 0 {
            return 0;
        }
        // Anything below 1e11 is seconds (1e11 ms is 1973, 1e11 s is year 5138).
        return if number < 100_000_000_000 {
            number as u64 * 1000
        } else {
            number as u64
        };
    }
    let Some(raw) = value.as_str().filter(|s| !s.is_empty()) else {
        return 0;
    };
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return dt.timestamp_millis().max(0) as u64;
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            return dt.and_utc().timestamp_millis().max(0) as u64;
        }
        if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, fmt) {
            return date
                .and_hms_opt(0, 0, 0)
                .map(|dt| dt.and_utc().timestamp_millis().max(0) as u64)
                .unwrap_or(0);
        }
    }
    0
}

pub fn sort_entries(entries: &mut [EntryDto]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Validates a name the user typed for a folder or a rename.
///
/// A path separator would silently create a nested path on drives that address
/// files by path (139), and `.`/`..` would target the parent; both are rejected
/// here so every drive behaves the same.
pub fn check_name(name: &str) -> AppResult<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::new("名称不能为空"));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(AppError::new("名称不能包含 / 或 \\"));
    }
    if name == "." || name == ".." {
        return Err(AppError::new("名称无效"));
    }
    Ok(name)
}

// ------------------------------------------------------------- capabilities ---

/// What a drive can actually do, so the UI can hide what it cannot.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    /// Open someone else's share link.
    pub share_open: bool,
    /// Create a share of your own files.
    pub share_create: bool,
    #[serde(rename = "move")]
    pub move_files: bool,
    pub copy: bool,
    /// Server-side search.
    pub search: bool,
    pub rename: bool,
    /// Nested folders (all of them have this, but the UI checks anyway).
    pub folders: bool,
    /// Has a recycle bin we can list and restore from.
    pub recycle: bool,
}

impl Capabilities {
    /// Everything a personal-drive API gives us.
    pub const FULL: Capabilities = Capabilities {
        share_open: true,
        share_create: true,
        move_files: true,
        copy: true,
        search: true,
        rename: true,
        folders: true,
        recycle: true,
    };

    /// Browse + transfer only.
    pub const BASIC: Capabilities = Capabilities {
        share_open: false,
        share_create: false,
        move_files: true,
        copy: true,
        search: true,
        rename: true,
        folders: true,
        recycle: false,
    };
}

// ------------------------------------------------------------------- state ---

/// Login state of one drive, as reported to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthState {
    pub provider: ProviderId,
    pub authenticated: bool,
    /// `"file"`, `"env"`, `"none"`.
    pub source: String,
    /// Masked credential, safe to show.
    pub masked: Option<String>,
    /// Where the credential is stored, for the settings screen.
    pub credential_path: String,
    /// Nickname once known.
    pub nickname: Option<String>,
    /// Avatar inlined as a `data:` URL (the CSP forbids remote images).
    ///
    /// Only set when the drive reported one and it could be inlined; the UI
    /// falls back to the drive logo / the first letter of the nickname.
    pub avatar: Option<String>,
    pub capabilities: Capabilities,
}

/// One streamed download, already positioned at `start`.
pub struct DownloadHandle {
    /// Full size of the file, if the drive told us.
    pub size: u64,
    /// Expected md5 when the drive publishes a trustworthy one. Quark shares do
    /// not (the md5 does not match the served bytes), so it stays `None` there.
    pub md5: Option<String>,
    pub stream: BoxStream,
}

pub type BoxStream = Box<dyn Stream<Item = Result<Bytes, AppError>> + Send + Unpin + 'static>;

/// Error text a provider returns when the CDN ignored a resume `Range` and
/// answered with the whole file.
///
/// The transfer engine treats this as "restart from zero" rather than as a
/// failure, so it must stay byte-identical wherever it is produced.
pub const RANGE_IGNORED: &str = "服务器不支持断点续传（忽略 Range）";

/// One downloadable file inside a share, as the provider addresses it.
pub struct ShareLink {
    /// Where to read the bytes from. Minted fresh for every attempt: most CDNs
    /// sign it with a short lifetime.
    pub url: String,
    /// Headers the CDN insists on (`Referer`, `User-Agent`, a cookie, ...).
    pub headers: Vec<(String, String)>,
    pub size: u64,
    /// Set when the drive could not hand out a direct link and the bytes only
    /// became reachable by copying the file into the user's own drive first.
    /// The transfer engine deletes that copy once the download finishes.
    pub cleanup_fid: Option<String>,
}

/// One file found while walking a share folder.
pub struct ShareFile {
    /// Path relative to the folder the user picked.
    pub relative: std::path::PathBuf,
    pub fid: String,
    pub token: String,
    pub name: String,
    pub size: u64,
}

/// Progress + phase reporter handed to [`DriveProvider::upload`].
///
/// The provider runs the whole upload itself (rapid upload, chunks, whatever
/// the drive needs) but reports through this, so the transfer list stays
/// uniform across drives.
pub trait UploadSink: Send + Sync {
    /// `"hashing"` while the local file is being prepared, `"uploading"` once
    /// bytes are moving. The manager maps this onto the task status.
    fn phase(&self, phase: &str);
    /// Absolute number of bytes hashed / uploaded so far.
    fn progress(&self, bytes: u64);
}

// ------------------------------------------------------------------- trait ---

/// One cloud drive.
///
/// Implementations hold their own credential and HTTP client; the app keeps one
/// instance per drive in [`crate::state::AppState`].
#[async_trait]
pub trait DriveProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn capabilities(&self) -> Capabilities;

    /// Current login state. Must not hit the network.
    async fn auth_state(&self) -> AuthState;

    /// Drops the stored credential.
    async fn logout(&self) -> AppResult<()>;

    /// Nickname + avatar, fetched lazily. `None` when not logged in.
    #[allow(dead_code)]
    async fn account(&self) -> AppResult<Option<crate::share::AccountInfo>>;

    /// Stores a profile the app fetched elsewhere so the sidebar can show it.
    #[allow(dead_code)]
    async fn cache_account(&self, _info: crate::share::AccountInfo) {}

    /// Nickname + avatar for the sidebar, hitting the network at most once.
    ///
    /// Distinct from [`DriveProvider::account`], which never touches the
    /// network: this one is what the UI calls after a login or a refresh, and
    /// it caches into the same slot `account` reads.
    async fn profile(&self) -> AppResult<Option<crate::share::AccountInfo>> {
        self.account().await
    }

    /// Lists one folder. `dir` is a *native* id (`"0"` for the root).
    async fn list(&self, dir: &str) -> AppResult<DirListing>;

    /// Ancestor chain of `dir`, root first. Empty for the root itself.
    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>>;

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String>;

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()>;

    async fn delete(&self, fids: &[String]) -> AppResult<()>;

    /// Moves (`copy == false`) or copies entries into `to`.
    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize>;

    /// Breadth-first name search from `dir`.
    async fn search(&self, dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>>;

    /// Opens `fid` for reading at byte offset `start`.
    ///
    /// Returning a positioned stream is what lets the transfer engine own
    /// resume, retry and pause for every drive at once.
    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle>;

    /// Uploads `local` into `parent` as `name`, reporting through `sink`.
    /// Returns the new file's native id when the drive reports one.
    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>>;

    /// Asks the drive for a fresh credential if it refreshes tokens.
    /// Default: nothing to do.
    #[allow(dead_code)]
    async fn refresh(&self) -> AppResult<()> {
        Ok(())
    }

    /// Optional quota, shown in the sidebar footer when available.
    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        Ok(None)
    }

    // ----------------------------------------------------------- recycle ---
    //
    // What the drives put in the bin, and what they let us do with it, differs
    // enough that the trait only promises the three calls below. A drive that
    // cannot answer leaves the default, which reports why.

    /// Lists the recycle bin, newest first. `offset`/`limit` are in rows.
    async fn recycle_list(&self, _offset: u32, _limit: u32) -> AppResult<Vec<RecycleEntry>> {
        Err(AppError::new(format!(
            "{}暂不支持回收站",
            self.id().display_name()
        )))
    }

    /// Puts entries back where they came from. Returns how many were restored.
    async fn recycle_restore(&self, _entries: &[RecycleEntry]) -> AppResult<u32> {
        Err(AppError::new(format!(
            "{}暂不支持从回收站恢复",
            self.id().display_name()
        )))
    }

    /// Deletes entries for good.
    async fn recycle_purge(&self, _entries: &[RecycleEntry]) -> AppResult<u32> {
        Err(AppError::new(format!(
            "{}暂不支持彻底删除回收站文件",
            self.id().display_name()
        )))
    }

    /// Empties the whole bin. Some drives only offer this instead of a
    /// per-item purge, so the UI keeps a separate button for it.
    async fn recycle_clear(&self) -> AppResult<()> {
        Err(AppError::new(format!(
            "{}暂不支持清空回收站",
            self.id().display_name()
        )))
    }

    // ------------------------------------------------------------- shares ---
    //
    // A share is someone else's folder tree, reachable without the drive's
    // normal file endpoints. Every drive spells that differently, so the
    // provider implements the protocol and `crate::share` only routes.

    /// Resolves a link into a session (title, owner, counters, token).
    async fn share_open(&self, _id: &str, _passcode: &str) -> AppResult<crate::share::ShareSession> {
        Err(AppError::new(format!(
            "{}暂不支持打开分享链接",
            self.id().display_name()
        )))
    }

    /// Lists one folder inside a share.
    async fn share_list(
        &self,
        _share: &crate::transfer::ShareRef,
        _dir: &str,
    ) -> AppResult<crate::share::ShareListing> {
        Err(AppError::new(format!(
            "{}暂不支持浏览分享内容",
            self.id().display_name()
        )))
    }

    /// Copies share entries into the user's own drive.
    async fn share_save(
        &self,
        _share: &crate::transfer::ShareRef,
        _entries: &[crate::share::ShareEntry],
        _to: Option<&str>,
    ) -> AppResult<u64> {
        Err(AppError::new(format!(
            "{}暂不支持转存分享文件",
            self.id().display_name()
        )))
    }

    /// Expands a share folder into its files, depth first.
    async fn share_walk(
        &self,
        _share: &crate::transfer::ShareRef,
        _dir: &str,
    ) -> AppResult<Vec<ShareFile>> {
        Err(AppError::new(format!(
            "{}暂不支持下载分享文件夹",
            self.id().display_name()
        )))
    }

    /// Mints a download url for one share file.
    async fn share_link(
        &self,
        _share: &crate::transfer::ShareRef,
        _fid: &str,
    ) -> AppResult<ShareLink> {
        Err(AppError::new(format!(
            "{}暂不支持下载分享文件",
            self.id().display_name()
        )))
    }

    /// Drops a temporary copy made by [`Self::share_link`]. Best effort.
    async fn share_cleanup(&self, _fid: &str) {}
}

// ------------------------------------------------------------------ registry ---

/// All drives the app knows about, logged in or not.
pub struct Providers {
    map: HashMap<ProviderId, Arc<dyn DriveProvider>>,
}

impl Providers {
    pub fn new(list: Vec<Arc<dyn DriveProvider>>) -> Self {
        Self {
            map: list.into_iter().map(|p| (p.id(), p)).collect(),
        }
    }

    pub fn get(&self, id: ProviderId) -> AppResult<Arc<dyn DriveProvider>> {
        self.map
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::new(format!("不支持的网盘：{}", id.display_name())))
    }

    /// Resolves a qualified entry id to `(provider, native id)`.
    pub fn resolve(&self, entry: &str) -> AppResult<(Arc<dyn DriveProvider>, String)> {
        let id = EntryId::parse(entry)?;
        Ok((self.get(id.provider)?, id.native))
    }

    #[allow(dead_code)]
    pub fn all(&self) -> Vec<Arc<dyn DriveProvider>> {
        let mut out: Vec<_> = self.map.values().cloned().collect();
        out.sort_by_key(|p| {
            ProviderId::all()
                .iter()
                .position(|id| *id == p.id())
                .unwrap_or(usize::MAX)
        });
        out
    }
}

/// Shared progress counters an [`UploadSink`] implementation writes into.
///
/// The transfer manager owns one of these per task; the ticker reads it.
pub struct CounterSink {
    pub done: Arc<AtomicU64>,
    pub phase: std::sync::Mutex<String>,
}

impl CounterSink {
    pub fn new(done: Arc<AtomicU64>) -> Self {
        Self {
            done,
            phase: std::sync::Mutex::new("uploading".to_string()),
        }
    }

    pub fn phase(&self) -> String {
        self.phase
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }
}

impl UploadSink for CounterSink {
    fn phase(&self, phase: &str) {
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase.to_string();
        }
    }

    fn progress(&self, bytes: u64) {
        self.done.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_ids_round_trip() {
        let id = EntryId::new(ProviderId::Quark, "abc123");
        assert_eq!(id.to_string(), "quark:abc123");
        assert_eq!(EntryId::parse("quark:abc123").unwrap(), id);
    }

    #[test]
    fn parses_aliases() {
        for (raw, expect) in [
            ("yun139", ProviderId::Yun139),
            ("139", ProviderId::Yun139),
            ("aliyundrive", ProviderId::Aliyun),
            ("alipan", ProviderId::Aliyun),
            ("123pan", ProviderId::Pan123),
            ("123", ProviderId::Pan123),
            ("QUARK", ProviderId::Quark),
        ] {
            assert_eq!(ProviderId::parse(raw), Some(expect), "alias {raw}");
        }
    }

    /// A bare id must never silently pick a drive: that is how a delete lands on
    /// the wrong cloud.
    #[test]
    fn rejects_bare_or_unknown_ids() {
        assert!(EntryId::parse("abc123").is_err());
        assert!(EntryId::parse("dropbox:abc").is_err());
        assert!(EntryId::parse("quark:").is_err());
        assert!(EntryId::parse("").is_err());
    }

    /// Bare ids must survive: `root` / `0` / `/` are drive-native markers.
    #[test]
    fn native_of_unwraps_and_keeps_bare_ids() {
        assert_eq!(native_of("quark:abc"), "abc");
        assert_eq!(native_of("abc"), "abc");
        // A colon inside the native id must not be mistaken for a provider.
        assert_eq!(native_of("weird:thing"), "weird:thing");
    }

    #[test]
    fn names_with_separators_are_rejected() {
        assert!(check_name("正常名称").is_ok());
        assert!(check_name("  spaced  ").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("   ").is_err());
        assert!(check_name("a/b").is_err());
        assert!(check_name("a\\b").is_err());
        assert!(check_name("..").is_err());
    }

    #[test]
    fn capability_flags_are_serialisable() {
        let json = serde_json::to_string(&Capabilities::FULL).unwrap();
        // `move` is a Rust keyword; the JSON name must still be "move".
        assert!(json.contains("\"move\":true"), "{json}");
        assert!(json.contains("\"shareOpen\":true"));
    }

    /// Folders first, then case-insensitive by name — the order Quark always
    /// produced and the other drives now share.
    #[test]
    fn reads_timestamps_in_every_shape_the_drives_send() {
        use serde_json::json;
        // 123 sends ISO with an offset, Baidu sends epoch seconds, Quark sends
        // millis; each one used to be read with a single shape and rendered 1970.
        assert_eq!(parse_time(&json!("2026-11-09T15:12:31+08:00")), 1_794_208_351_000);
        assert_eq!(parse_time(&json!(1_794_213_151i64)), 1_794_213_151_000);
        assert_eq!(parse_time(&json!(1_794_213_151_000i64)), 1_794_213_151_000);
        // Without an offset the string is taken as UTC, which is what the drives mean
        // when they omit one.
        assert_eq!(parse_time(&json!("2026-11-09 15:12:31")), 1_794_237_151_000);
        assert_eq!(parse_time(&json!("2026-11-09")), 1_794_182_400_000);
        assert_eq!(parse_time(&json!(0)), 0);
        assert_eq!(parse_time(&json!("")), 0);
        assert_eq!(parse_time(&serde_json::Value::Null), 0);
    }

    #[test]
    fn listings_sort_folders_first_then_names() {
        let entry = |name: &str, is_dir: bool| EntryDto {
            fid: format!("quark:{name}"),
            name: name.to_string(),
            pdir_fid: "quark:0".to_string(),
            size: 0,
            is_dir,
            format_type: if is_dir { "folder".into() } else { "file".into() },
            updated_at: 0,
            created_at: 0,
            ext: String::new(),
            provider: "quark".to_string(),
            thumb: None,
        };
        let mut entries = vec![
            entry("beta.txt", false),
            entry("Zeta", true),
            entry("alpha.txt", false),
            entry("alpha", true),
        ];
        sort_entries(&mut entries);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha", "Zeta", "alpha.txt", "beta.txt"]);
    }
}

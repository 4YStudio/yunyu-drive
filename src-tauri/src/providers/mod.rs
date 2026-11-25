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
pub use cloud189::Cloud189Provider;
pub use pan115::Pan115Provider;
pub use pan123::Pan123Provider;
pub use quark::QuarkProvider;
pub use yun139::Yun139Provider;

pub mod aliyun;
pub mod aliyun_qr;
pub mod baidu;
pub mod cloud189;
pub mod cloud189_qr;
pub mod dto;
pub mod pan115;
pub mod pan115_crypto;
pub mod pan115_ec;
pub mod pan123;
pub mod quark;
pub mod yun139;

pub use dto::{
    DirListing, DownloadRequest, EntryDto, PathNode, QuotaInfo, RecycleEntry, SearchHit, TrafficInfo,
};

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
    /// 115 网盘
    Pan115,
    /// 123 云盘
    Pan123,
    /// 百度网盘
    Baidu,
    /// 天翼云盘 (189)
    Cloud189,
}

impl ProviderId {
    /// Stable key used in ids, settings and the frontend.
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderId::Quark => "quark",
            ProviderId::Yun139 => "yun139",
            ProviderId::Aliyun => "aliyun",
            ProviderId::Pan115 => "pan115",
            ProviderId::Pan123 => "pan123",
            ProviderId::Baidu => "baidu",
            ProviderId::Cloud189 => "cloud189",
        }
    }

    /// Human name for menus, error messages and the transfer list.
    pub fn display_name(self) -> &'static str {
        match self {
            ProviderId::Quark => "夸克网盘",
            ProviderId::Yun139 => "中国移动云盘",
            ProviderId::Aliyun => "阿里云盘",
            ProviderId::Pan115 => "115 网盘",
            ProviderId::Pan123 => "123 云盘",
            ProviderId::Baidu => "百度网盘",
            ProviderId::Cloud189 => "天翼云盘",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "quark" => Some(ProviderId::Quark),
            "yun139" | "139" | "cloud139" => Some(ProviderId::Yun139),
            "aliyun" | "aliyundrive" | "alipan" => Some(ProviderId::Aliyun),
            "pan115" | "115" | "115pan" => Some(ProviderId::Pan115),
            "pan123" | "123pan" | "123" => Some(ProviderId::Pan123),
            "baidu" | "baidupan" | "pan_baidu" | "bdpan" => Some(ProviderId::Baidu),
            "cloud189" | "189" | "tianyi" | "ctyun" => Some(ProviderId::Cloud189),
            _ => None,
        }
    }

    /// Every drive, in the order the UI lists them.
    pub fn all() -> [ProviderId; 7] {
        [
            ProviderId::Quark,
            ProviderId::Yun139,
            ProviderId::Aliyun,
            ProviderId::Pan123,
            ProviderId::Baidu,
            ProviderId::Cloud189,
            ProviderId::Pan115,
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

/// Parses the first number of a `Content-Range` header (`bytes 0-1023/4096`).
///
/// The start offset is what matters: a CDN that answers a ranged request from a
/// different offset than the one asked for would make the engine write those
/// bytes at the wrong place, producing a file that is the right length and
/// silently corrupt. Returns `None` for a missing or unparsable header, which
/// the caller must treat as "cannot verify" rather than "verified".
pub fn content_range_start(value: Option<&str>) -> Option<u64> {
    let value = value?;
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    let start = rest.split(['-', '/']).next()?.trim();
    start.parse::<u64>().ok()
}

/// Outcome of asking a drive for a byte range.
///
/// A separate type from `AppResult` because the segmented engine has to tell
/// three cases apart that all collapse into one string error otherwise: the
/// link went stale (mint a new one), the CDN ignored the range (give up on
/// segments), and the drive has no ranged endpoint (use one stream).
pub enum RangeResult {
    /// The drive accepted the range and is streaming it.
    Body(DownloadHandle),
    /// The drive answered the range but from a different offset than requested.
    ///
    /// Writing those bytes would put them at the wrong place in the file and
    /// produce a corrupt download of exactly the right length — the worst kind
    /// of failure, because nothing downstream notices. The engine abandons the
    /// attempt instead.
    StartMismatch { asked: u64, served: u64 },
    /// Link expired, signature rejected, or access revoked.
    Expired,
    /// The CDN answered with the whole file instead of the range.
    RangeIgnored,
    /// A genuine failure worth reporting as-is.
    Failed(AppError),
}

impl RangeResult {
    /// Builds the outcome for a ranged response, verifying that the CDN started
    /// where it was asked to.
    ///
    /// `start` of 0 is the whole-file case and needs no check. When the header
    /// is absent there is nothing to verify against, so the response is taken
    /// at its word — refusing every response without a `Content-Range` would
    /// break CDNs that simply do not send one.
    pub fn body(handle: DownloadHandle, start: u64, content_range: Option<&str>) -> Self {
        if start > 0 {
            if let Some(served) = content_range_start(content_range) {
                if served != start {
                    return Self::StartMismatch {
                        asked: start,
                        served,
                    };
                }
            }
        }
        Self::Body(handle)
    }

    /// Maps an `open_download_range` error into the closest outcome.
    pub fn from_error(err: AppError) -> Self {
        if err.0 == RANGE_IGNORED {
            Self::RangeIgnored
        } else {
            Self::Failed(err)
        }
    }
}

/// Error text a provider returns when the CDN ignored a resume `Range` and
/// answered with the whole file.
///
/// The transfer engine treats this as "restart from zero" rather than as a
/// failure, so it must stay byte-identical wherever it is produced.
pub const RANGE_IGNORED: &str = "服务器不支持断点续传（忽略 Range）";

/// One downloadable file inside a share, as the provider addresses it.
///
/// Cloned because the segmented engine caches it: minting a url is cheap on
/// most drives but not on Aliyun, where it copies the file into the user's own
/// drive first.
#[derive(Clone)]
pub struct ShareLink {
    /// Where to read the bytes from. Most CDNs sign it with a short lifetime,
    /// so it is re-minted once it expires.
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
    /// bytes are moving. The manager flips the task from `"hashing"` to
    /// `"running"` when this changes (see `phase_watch` in `transfer.rs`), so a
    /// drive that hashes a large file no longer looks stuck on 校验中.
    fn phase(&self, phase: &str);
    /// Absolute number of bytes hashed / uploaded so far.
    ///
    /// While the phase is `"hashing"` the manager counts these separately (see
    /// [`CounterSink::prepared`]) and does not treat them as uploaded bytes.
    ///
    /// Sync on purpose: providers call it from stream closures that cannot
    /// await. A speed cap therefore waits with `block_in_place` (see
    /// [`crate::transfer::ThrottledSink`]).
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

    /// Opens the byte range `[start, end)` of `fid`.
    ///
    /// The default says "this drive only does one stream at a time", which is
    /// what every drive except Quark and Aliyun does. Those two override it and
    /// the segmented engine uses them; everyone else keeps the single-stream
    /// path and is unaffected.
    ///
    /// Not part of `open_download` because resume already works everywhere:
    /// widening that signature would touch five providers, their tests and the
    /// share path for a capability only two of them have.
    #[allow(dead_code)]
    async fn open_download_range(&self, fid: &str, start: u64, end: Option<u64>) -> RangeResult {
        let _ = end;
        match self.open_download(fid, start).await {
            Ok(handle) => RangeResult::Body(handle),
            Err(err) => RangeResult::from_error(err),
        }
    }

    /// True when [`Self::open_download_range`] serves real ranges.
    ///
    /// Cheaper than probing: the engine asks this before splitting a file, so
    /// a drive without ranged support never pays for a probe request.
    fn supports_ranges(&self) -> bool {
        false
    }

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

    /// Optional monthly download allowance, shown next to the quota.
    ///
    /// Only 123 云盘 meters downloads separately from storage; the rest leave
    /// the default, and the UI hides the row.
    async fn traffic(&self) -> AppResult<Option<TrafficInfo>> {
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

    // ------------------------------------------------------- own shares ---

    /// Creates a share out of the user's own files.
    ///
    /// `fids` are **native** ids of files or folders, `days` is the lifetime in
    /// days (`0` = permanent) and `passcode` the 4-character code a visitor
    /// must type — an empty string asks for a share without one, which not
    /// every drive allows (百度 always needs four characters).
    async fn create_share(
        &self,
        _fids: &[String],
        _title: &str,
        _days: u32,
        _passcode: &str,
    ) -> AppResult<crate::share::ShareCreated> {
        Err(AppError::new(format!(
            "{}暂不支持创建分享链接",
            self.id().display_name()
        )))
    }

    /// Revokes a share created by [`Self::create_share`].
    ///
    /// `share_id` is whatever `ShareCreated::share_id` carried; `fids` are the
    /// native ids the share was made from, which 百度 needs to address it.
    async fn revoke_share(&self, _share_id: &str, _fids: &[String]) -> AppResult<()> {
        Err(AppError::new(format!(
            "{}暂不支持取消分享",
            self.id().display_name()
        )))
    }

    /// Lists the shares the user has already created, newest first.
    async fn share_records(&self) -> AppResult<Vec<crate::share::ShareRecord>> {
        Err(AppError::new(format!(
            "{}暂不支持查看我的分享",
            self.id().display_name()
        )))
    }
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
    /// Bytes the provider has only *read* so far, while it is still preparing
    /// the file (hashing, slicing, precreate). Kept apart from [`Self::done`] so
    /// the progress bar does not jump to 100% before a single byte is uploaded:
    /// Baidu in particular hashes the whole file up front, which used to look
    /// like the transfer had stalled at "校验中".
    pub prepared: Arc<AtomicU64>,
}

impl CounterSink {
    pub fn new(done: Arc<AtomicU64>) -> Self {
        Self {
            done,
            phase: std::sync::Mutex::new("uploading".to_string()),
            prepared: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The last phase the provider reported, e.g. to tell a rapid upload from
    /// one that actually moved bytes.
    pub fn current_phase(&self) -> String {
        self.phase
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// The prepared counter itself, so the ticker can poll it without going
    /// through the sink.
    pub fn prepared_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.prepared)
    }
}

impl UploadSink for CounterSink {
    fn phase(&self, phase: &str) {
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase.to_string();
        }
        if phase != "hashing" {
            self.prepared.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn progress(&self, bytes: u64) {
        // While the file is still being read the bytes are *not* uploaded yet:
        // reporting them as `done` would fill the bar and then make it jump back
        // to zero once the first real byte goes out.
        if self.current_phase() == "hashing" {
            self.prepared
                .store(bytes, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.done.store(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::content_range_start;

    #[test]
    fn content_range_start_reads_the_first_offset() {
        assert_eq!(content_range_start(Some("bytes 0-1023/4096")), Some(0));
        assert_eq!(
            content_range_start(Some("bytes 1024-2047/4096")),
            Some(1024)
        );
        assert_eq!(content_range_start(Some("bytes 2048-4095/*")), Some(2048));
        assert_eq!(
            content_range_start(Some("  bytes=1024-2047/4096  ")),
            Some(1024)
        );
    }

    #[test]
    fn a_body_from_the_wrong_offset_is_refused() {
        use super::{DownloadHandle, RangeResult};
        use bytes::Bytes;
        use futures_util::stream;

        fn handle() -> DownloadHandle {
            DownloadHandle {
                size: 10,
                md5: None,
                stream: Box::new(stream::empty::<Result<Bytes, crate::error::AppError>>()),
            }
        }

        // Served where asked: accepted.
        assert!(matches!(
            RangeResult::body(handle(), 1024, Some("bytes 1024-2047/4096")),
            RangeResult::Body(_)
        ));
        // Served from zero although 1024 was asked: writing this would corrupt
        // the file, so it must be refused rather than streamed.
        match RangeResult::body(handle(), 1024, Some("bytes 0-4095/4096")) {
            RangeResult::StartMismatch { asked, served } => {
                assert_eq!((asked, served), (1024, 0));
            }
            _ => panic!("a wrong start offset must not be accepted"),
        }
        // No header: nothing to verify against, so it is taken at its word.
        assert!(matches!(
            RangeResult::body(handle(), 1024, None),
            RangeResult::Body(_)
        ));
        // The whole-file case needs no check at all.
        assert!(matches!(
            RangeResult::body(handle(), 0, None),
            RangeResult::Body(_)
        ));
    }

    #[test]
    fn content_range_start_refuses_what_it_cannot_read() {
        // Missing or malformed must read as "unverifiable", never as offset 0.
        assert_eq!(content_range_start(None), None);
        assert_eq!(content_range_start(Some("")), None);
        assert_eq!(content_range_start(Some("items 0-10/20")), None);
        assert_eq!(content_range_start(Some("bytes a-b/c")), None);
    }

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

    /// Baidu hashes the whole file before the first byte goes up. Those bytes are
    /// *not* uploaded, so they must not be reported as `done` — the bar used to
    /// jump to 100% during 校验中 and then fall back to 0.
    #[test]
    fn hashing_bytes_do_not_count_as_uploaded() {
        let done = Arc::new(AtomicU64::new(0));
        let sink = CounterSink::new(done.clone());
        sink.phase("hashing");
        sink.progress(1024);
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            sink.prepared_counter()
                .load(std::sync::atomic::Ordering::Relaxed),
            1024
        );
        // Once the upload starts the counter belongs to the transfer again, and
        // the prepared bytes are cleared so they are not counted twice.
        sink.phase("uploading");
        assert_eq!(
            sink.prepared_counter()
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        sink.progress(512);
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 512);
    }
}

// ------------------------------------------------------- live consistency ---

/// Live, cross-drive consistency probe.
///
/// Every drive gets the same scripted sequence in a sandbox folder of our own
/// (`yunyu-probe-pf-<pid>`), and every step is recorded as 通过 / 失败 / 未实现
/// so the result can be diffed against what `Capabilities` promises.
///
/// These talk to the real accounts in `~/.config/yunyu-drive/`, so they are
/// `#[ignore]`d. Run them one at a time:
///
/// ```text
/// cargo test --lib live_provider_probe -- --ignored --nocapture --test-threads=1
/// cargo test --lib live_provider_probe::probe_quark -- --ignored --nocapture
/// ```
///
/// They only ever create/rename/delete names starting with `yunyu-probe-pf-`
/// and remove them again before finishing.
#[cfg(test)]
mod live_provider_probe {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    // ---------------------------------------------------------- reporting ---

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Status {
        Pass,
        Fail,
        Missing,
        Skipped,
    }

    impl Status {
        fn label(self) -> &'static str {
            match self {
                Status::Pass => "通过",
                Status::Fail => "失败",
                Status::Missing => "未实现",
                Status::Skipped => "跳过",
            }
        }
    }

    struct Probe {
        label: String,
        caps: Capabilities,
        rows: Vec<(String, Status, String)>,
        /// Set once the drive answers 登录超时 / 401 / 403: the rest of that
        /// drive is skipped instead of hammering a dead credential.
        halted: bool,
    }

    impl Probe {
        fn new(label: &str, caps: Capabilities) -> Self {
            Self {
                label: label.to_string(),
                caps,
                rows: Vec::new(),
                halted: false,
            }
        }

        fn add(&mut self, action: &str, status: Status, detail: impl Into<String>) {
            let detail = detail.into();
            println!("    [{:>6}] {action:<24} {detail}", status.label());
            self.rows.push((action.to_string(), status, detail));
        }

        fn pass(&mut self, action: &str, detail: impl Into<String>) {
            self.add(action, Status::Pass, detail);
        }

        fn fail(&mut self, action: &str, err: impl std::fmt::Display) {
            let text = err.to_string();
            if looks_like_auth_failure(&text) {
                self.halted = true;
                self.add(
                    action,
                    Status::Skipped,
                    format!("{text}（登录失效/限流，跳过该盘剩余项）"),
                );
            } else {
                self.add(action, Status::Fail, text);
            }
        }

        fn missing(&mut self, action: &str, why: impl Into<String>) {
            self.add(action, Status::Missing, why);
        }

        fn skip(&mut self, action: &str, why: impl Into<String>) {
            self.add(action, Status::Skipped, why);
        }

        fn halted(&self) -> bool {
            self.halted
        }

        fn report(&self) {
            println!("\n===== {} 结果表 =====", self.label);
            println!(
                "capabilities: share_open={} share_create={} move={} copy={} search={} rename={} folders={} recycle={}",
                self.caps.share_open,
                self.caps.share_create,
                self.caps.move_files,
                self.caps.copy,
                self.caps.search,
                self.caps.rename,
                self.caps.folders,
                self.caps.recycle
            );
            println!("| 动作 | 结果 | 关键输出或错误原文 |");
            println!("|---|---|---|");
            for (action, status, detail) in &self.rows {
                println!(
                    "| {action} | {} | {} |",
                    status.label(),
                    detail.replace('|', "\\|").replace('\n', " ")
                );
            }

            let mut notes: Vec<String> = Vec::new();
            for (action, status, detail) in &self.rows {
                match cap_for(action, self.caps) {
                    Some(true) if *status == Status::Fail => notes.push(format!(
                        "⚠ capabilities 声称支持，实测失败：{action} — {detail}"
                    )),
                    Some(true) if *status == Status::Missing => notes.push(format!(
                        "⚠ capabilities 声称支持，但走的是默认实现：{action} — {detail}"
                    )),
                    Some(false) if *status == Status::Pass => notes.push(format!(
                        "⚠ capabilities 声称不支持，实测可用：{action}"
                    )),
                    _ => {}
                }
                if *status == Status::Pass
                    && (action.starts_with("recycle_list") || action == "search")
                    && detail.contains("0 条")
                {
                    notes.push(format!("⚠ 返回空且未报错（可疑实现）：{action}"));
                }
            }
            if self.halted {
                notes.push("⛔ 该盘在登录失效/限流处停止，后面的动作没有执行".to_string());
            }
            println!("\n检测结论（{}）：", self.label);
            if notes.is_empty() {
                println!("- 没有发现 capabilities 与实际行为的偏差");
            }
            for note in notes {
                println!("- {note}");
            }
            println!("\n");
        }
    }

    fn cap_for(action: &str, caps: Capabilities) -> Option<bool> {
        match action {
            a if a.starts_with("recycle_list")
                || a.starts_with("recycle_restore")
                || a.starts_with("recycle_purge") =>
            {
                Some(caps.recycle)
            }
            "search" => Some(caps.search),
            "copy" | "copy 确认" => Some(caps.copy),
            "move" => Some(caps.move_files),
            "rename(目录)" | "rename(文件)" => Some(caps.rename),
            "create_share" => Some(caps.share_create),
            "share_open" => Some(caps.share_open),
            _ => None,
        }
    }

    fn looks_like_auth_failure(text: &str) -> bool {
        [
            "登录超时",
            "登录已失效",
            "登录状态已失效",
            "尚未登录",
            "未登录",
            "401",
            "403",
            "TokenExpired",
            "AccessTokenInvalid",
        ]
        .iter()
        .any(|needle| text.contains(needle))
    }

    fn human(bytes: u64) -> String {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit + 1 < UNITS.len() {
            value /= 1024.0;
            unit += 1;
        }
        format!("{value:.1}{}", UNITS[unit])
    }

    fn truncate(text: &str, max: usize) -> String {
        if text.chars().count() <= max {
            text.to_string()
        } else {
            let head: String = text.chars().take(max).collect();
            format!("{head}…")
        }
    }

    fn config_dir() -> std::path::PathBuf {
        crate::config::AppPaths::resolve()
            .expect("配置目录")
            .config_dir
    }

    // ------------------------------------------------------------ helpers ---

    /// Removes the probe's temp payload when the test ends, success or not.
    struct TempFile(std::path::PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// First `want` bytes of a download stream.
    async fn read_first(mut handle: DownloadHandle, want: usize) -> AppResult<Vec<u8>> {
        let mut out = Vec::with_capacity(want.min(4096));
        while out.len() < want {
            match handle.stream.next().await {
                Some(Ok(chunk)) => {
                    let take = (want - out.len()).min(chunk.len());
                    out.extend_from_slice(&chunk[..take]);
                    if take < chunk.len() {
                        break;
                    }
                }
                Some(Err(err)) => return Err(err),
                None => break,
            }
        }
        if out.is_empty() {
            return Err(AppError::new("下载流立即结束（0 字节）"));
        }
        Ok(out)
    }

    /// Looks a name up in a folder and returns `(native fid, entry)`.
    async fn find_in(
        drive: &dyn DriveProvider,
        dir: &str,
        name: &str,
    ) -> AppResult<Option<(String, EntryDto)>> {
        let listing = drive.list(dir).await?;
        Ok(listing
            .entries
            .into_iter()
            .find(|entry| entry.name == name)
            .map(|entry| (native_of(&entry.fid), entry)))
    }

    /// `find_in` that re-lists a few times before giving up.
    ///
    /// 夸克's `file/sort` is read-after-write inconsistent for ~1–2 s (measured:
    /// a folder created with `POST /1/clouddrive/file` is missing from the very
    /// next listing and shows up ~2 s later), so a single list is not a fair
    /// check right after a mutation.
    async fn find_settled(
        drive: &dyn DriveProvider,
        dir: &str,
        name: &str,
        tries: u32,
    ) -> AppResult<Option<(String, EntryDto)>> {
        let mut last: Option<AppResult<Option<(String, EntryDto)>>> = None;
        for attempt in 0..tries.max(1) {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            match find_in(drive, dir, name).await {
                Ok(Some(hit)) => return Ok(Some(hit)),
                Ok(None) => last = Some(Ok(None)),
                Err(err) => {
                    last = Some(Err(err));
                }
            }
        }
        last.unwrap_or(Ok(None))
    }

    /// Waits for a name to LEAVE a folder: a move that the server applies
    /// asynchronously still shows the entry in the source for a few seconds
    /// (天翼 does), so a single re-list would call a working move a failure.
    async fn gone_settled(
        drive: &dyn DriveProvider,
        dir: &str,
        name: &str,
        tries: u32,
    ) -> bool {
        for attempt in 0..tries.max(1) {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            match find_in(drive, dir, name).await {
                Ok(None) => return true,
                _ => continue,
            }
        }
        false
    }

    /// Bin rows created by this probe, re-listing until they show up.
    ///
    /// Deletes land in the bin asynchronously on 夸克 (same ~2 s lag as
    /// `file/sort`), so one listing right after `delete` reports an empty bin
    /// and makes `recycle_purge` fail with "请先选择要彻底删除的内容".
    async fn bin_mine(
        drive: &dyn DriveProvider,
        prefix: &str,
        tries: u32,
    ) -> AppResult<(usize, Vec<RecycleEntry>)> {
        let tries = tries.max(1);
        let mut last: Option<AppError> = None;
        for attempt in 0..tries {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            match drive.recycle_list(0, 100).await {
                Ok(rows) => {
                    let mine: Vec<RecycleEntry> = rows
                        .iter()
                        .filter(|row| row.name.starts_with(prefix))
                        .cloned()
                        .collect();
                    if !mine.is_empty() || attempt + 1 == tries {
                        return Ok((rows.len(), mine));
                    }
                }
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| AppError::new("回收站不可用")))
    }

    /// Last-resort permanent delete for drives whose trait has no per-item
    /// purge (only 阿里 has one, `/v2/file/delete`).
    async fn hard_delete(aliyun: Option<&AliyunProvider>, qualified: &str) -> Result<(), String> {
        let Some(drive) = aliyun else {
            return Err("该盘没有逐项彻底删除接口".to_string());
        };
        let drive_id = drive.debug_drive_id().await;
        drive
            .debug_call(
                "/v2/file/delete",
                serde_json::json!({
                    "drive_id": drive_id,
                    "file_id": native_of(qualified),
                }),
            )
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    /// Empties the bin of everything whose name starts with `prefix`.
    ///
    /// Order: `recycle_purge` (the trait's own path) → restore + permanent
    /// delete (阿里) → report what could not be removed.
    async fn scrub_bin(
        drive: &dyn DriveProvider,
        prefix: &str,
        aliyun: Option<&AliyunProvider>,
        probe: &mut Probe,
        phase: &str,
    ) {
        let mine: Vec<RecycleEntry> = match bin_mine(drive, prefix, 5).await {
            Ok((_, mine)) => mine,
            Err(_) => return,
        };
        if mine.is_empty() {
            return;
        }
        let purge_err = drive.recycle_purge(&mine).await.err();
        let mut still: Vec<RecycleEntry> = match bin_mine(drive, prefix, 4).await {
            Ok((_, mine)) => mine,
            Err(_) => Vec::new(),
        };
        if still.is_empty() {
            probe.add(
                &format!("清理({phase})"),
                Status::Pass,
                "回收站里的探针条目已清空",
            );
            return;
        }
        // Without a per-item purge the only way to leave nothing behind is to
        // restore and then permanently delete.
        let _ = drive.recycle_restore(&still).await;
        // Restoring hands the entries new bin ids, and 115 only removes the ids
        // it was given — a second purge after the restore clears them.
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        if let Ok((_, again)) = bin_mine(drive, prefix, 4).await {
            if !again.is_empty() {
                let _ = drive.recycle_purge(&again).await;
            }
        }
        let mut errors = Vec::new();
        for entry in &still {
            if let Err(err) = hard_delete(aliyun, &entry.fid).await {
                errors.push(format!("{}：{err}", entry.name));
            }
        }
        still = match bin_mine(drive, prefix, 4).await {
            Ok((_, mine)) => mine,
            Err(_) => still,
        };
        if still.is_empty() {
            probe.add(
                &format!("清理({phase})"),
                Status::Pass,
                format!(
                    "逐项 purge 不可用（{}），改用「恢复 + 硬删除」清掉",
                    purge_err
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "无".to_string())
                ),
            );
        } else {
            probe.add(
                &format!("清理({phase})"),
                Status::Fail,
                format!(
                    "仍有 {} 条留在回收站，请手动清理 {prefix}*：{}",
                    still.len(),
                    errors.join("；")
                ),
            );
        }
    }

    // -------------------------------------------------------------- suite ---

    /// Runs the same script against any drive.
    ///
    /// `aliyun` is only needed for the final hard delete (阿里 exposes no
    /// per-item purge through the trait); every other drive passes `None`.
    /// Opens a share, lists it and asks for one download link.
    ///
    /// Shared by the create-a-share path and the fallback that opens an
    /// existing share, because several drives can list/read shares but fail to
    /// create one (阿里 answers "登录已过期" on `/v2/share_link/create` while the
    /// rest of the API works).
    /// A freshly created share is not immediately readable everywhere: 115
    /// answers `正在生成文件快照（4100021）` until its snapshot lands, so the
    /// open is retried twice at 5 s before the failure is recorded.
    async fn share_open_retry(
        drive: &dyn DriveProvider,
        id: &str,
        passcode: &str,
    ) -> AppResult<crate::share::ShareSession> {
        let mut last = None;
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            match drive.share_open(id, passcode).await {
                Ok(session) => return Ok(session),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| AppError::new("share_open 未执行")))
    }

    /// 转存: copies the first shared file into our own folder and waits for it
    /// to show up there, then deletes the copy again.
    ///
    /// Every drive that can open a share should be able to save from it, so a
    /// drive answering "not supported" here is a real gap rather than a missing
    /// capability flag.
    async fn share_save_step(
        drive: &dyn DriveProvider,
        share_ref: &crate::transfer::ShareRef,
        entries: &[crate::share::ShareEntry],
        save_to: Option<&str>,
        probe: &mut Probe,
    ) {
        let Some(dir) = save_to else {
            probe.skip("share_save", "没有可用的目标目录");
            return;
        };
        let Some(entry) = entries.iter().find(|e| !e.is_dir) else {
            probe.skip("share_save", "分享里没有可直接转存的文件");
            return;
        };
        let name = entry.name.clone();
        match drive
            .share_save(share_ref, std::slice::from_ref(entry), Some(dir))
            .await
        {
            Ok(saved) => match find_settled(drive, dir, &name, 5).await {
                Ok(Some((fid, _))) => {
                    probe.pass(
                        "share_save",
                        format!("转存 {name} 成功（{saved} 个），目标目录里已出现"),
                    );
                    let _ = drive.delete(&[fid]).await;
                }
                Ok(None) => probe.fail("share_save", format!("转存 {name} 后目标目录里没有它")),
                Err(err) => probe.fail("share_save", format!("转存后回查失败：{err}")),
            },
            // Sharing and then saving your own link is refused by most drives
            // (夸克 41017, 百度 "不能转存自己分享的文件"). That is a limit of the
            // test, not of the feature: a real share belongs to someone else.
            Err(err) if is_own_share_block(&err.to_string()) => {
                probe.skip("share_save", format!("该盘不允许转存自己的分享：{err}"))
            }
            Err(err) => probe.fail("share_save", err),
        }
    }

    fn is_own_share_block(message: &str) -> bool {
        let lower = message.to_ascii_lowercase();
        message.contains("自己的分享")
            || message.contains("自己分享")
            || lower.contains("41017")
            || message.contains("不能转存自己")
    }

    async fn share_roundtrip(
        drive: &dyn DriveProvider,
        label: &str,
        id: &str,
        passcode: &str,
        save_to: Option<&str>,
        probe: &mut Probe,
    ) {
        match share_open_retry(drive, id, passcode).await {
            Ok(session) => {
                probe.pass(
                    "share_open",
                    format!(
                        "title={:?} owner={:?} file_num={} total_size={} stoken={} own_avatar={}",
                        session.title,
                        session.owner,
                        session.file_num,
                        session.total_size,
                        if session.stoken.is_empty() { "空" } else { "有" },
                        if session.owner_avatar.is_some() { "有" } else { "空" }
                    ),
                );
                let share_ref = crate::transfer::ShareRef {
                    provider: label.to_string(),
                    pwd_id: session.pwd_id.clone(),
                    stoken: session.stoken.clone(),
                    fid_token: String::new(),
                    passcode: if session.passcode.is_empty() {
                        passcode.to_string()
                    } else {
                        session.passcode.clone()
                    },
                };
                match drive.share_list(&share_ref, "").await {
                    Ok(listing) => {
                        probe.pass(
                            "share_list",
                            format!(
                                "{} 条：{}",
                                listing.entries.len(),
                                listing
                                    .entries
                                    .iter()
                                    .take(5)
                                    .map(|e| e.name.clone())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        );
                        if let Some(entry) = listing.entries.iter().find(|e| !e.is_dir) {
                            // Every real caller stuffs the per-entry token into
                            // `fid_token` before asking for a link
                            // (share.rs:877/918, share_live.rs:516); without it
                            // 夸克 answers code=41020.
                            let mut single = share_ref.clone();
                            single.fid_token = entry.token.clone();
                            match drive.share_link(&single, &native_of(&entry.fid)).await {
                                Ok(link) => probe.pass(
                                    "share_link",
                                    format!(
                                        "size={} headers={} url={}",
                                        link.size,
                                        link.headers.len(),
                                        truncate(&link.url, 70)
                                    ),
                                ),
                                Err(err) => probe.fail("share_link", err),
                            }
                        } else {
                            probe.skip("share_link", "分享里没有可直接下载的文件");
                        }
                        share_save_step(drive, &share_ref, &listing.entries, save_to, probe).await;
                    }
                    Err(err) => probe.fail("share_list", err),
                }
            }
            Err(err) => probe.fail("share_open", err),
        }
    }

    async fn probe_drive(
        label: &str,
        drive: &dyn DriveProvider,
        root: &str,
        aliyun: Option<&AliyunProvider>,
    ) -> Probe {
        let caps = drive.capabilities();
        let mut probe = Probe::new(label, caps);

        let base = format!("yunyu-probe-pf-{}", std::process::id());
        let mv_dir_name = format!("{base}-mv");
        let file_name = format!("{base}.txt");
        let renamed_file = format!("{base}-renamed.txt");
        let copy_name = format!("{base}-copy.txt");

        println!("\n##### {label} #####");

        // ---------------------------------------------------------- auth ---
        let auth = drive.auth_state().await;
        probe.add(
            "auth_state",
            if auth.authenticated {
                Status::Pass
            } else {
                Status::Fail
            },
            format!(
                "authenticated={} source={} masked={} nickname={:?} avatar={}",
                auth.authenticated,
                auth.source,
                truncate(auth.masked.as_deref().unwrap_or("—"), 60),
                auth.nickname,
                if auth.avatar.is_some() { "有" } else { "空" }
            ),
        );
        if !auth.authenticated {
            probe.skip("全部动作", "未登录，无法继续");
            probe.report();
            return probe;
        }

        match drive.account().await {
            Ok(Some(info)) => probe.add(
                "account",
                Status::Pass,
                format!(
                    "nickname={:?} avatar={}",
                    info.nickname,
                    if info.avatar.is_some() { "有" } else { "空" }
                ),
            ),
            Ok(None) => probe.add("account", Status::Skipped, "None（未缓存）"),
            Err(err) => probe.add("account", Status::Fail, err.to_string()),
        }
        match drive.profile().await {
            Ok(Some(info)) => probe.pass(
                "profile",
                format!(
                    "nickname={:?} avatar={}",
                    info.nickname,
                    if info.avatar.is_some() { "有" } else { "空" }
                ),
            ),
            Ok(None) => probe.skip("profile", "None（联网后仍没有昵称/头像）"),
            Err(err) => probe.fail("profile", err),
        }
        if probe.halted() {
            probe.report();
            return probe;
        }

        // --------------------------------------------------------- quota ---
        match drive.quota().await {
            Ok(Some(quota)) => {
                let odd = quota.total == 0 || quota.used > quota.total;
                probe.add(
                    "quota",
                    if odd { Status::Fail } else { Status::Pass },
                    format!(
                        "used={} total={}（{} / {}）",
                        quota.used,
                        quota.total,
                        human(quota.used),
                        human(quota.total)
                    ),
                );
            }
            Ok(None) => probe.add("quota", Status::Skipped, "None（该盘没有返回容量）"),
            Err(err) => probe.fail("quota", err),
        }
        if probe.halted() {
            probe.report();
            return probe;
        }

        // ------------------------------------------------------ list root ---
        match drive.list(root).await {
            Ok(listing) => {
                let files: Vec<&EntryDto> = listing.entries.iter().filter(|e| !e.is_dir).collect();
                let dirs = listing.entries.len() - files.len();
                let no_time = listing
                    .entries
                    .iter()
                    .filter(|e| e.updated_at == 0)
                    .count();
                let zero_size = files.iter().filter(|e| e.size == 0).count();
                let prefix = format!("{label}:");
                let bare_fid = listing
                    .entries
                    .iter()
                    .filter(|e| !e.fid.starts_with(&prefix))
                    .count();
                probe.pass(
                    "list(根)",
                    format!(
                        "{} 条（目录 {} / 文件 {}）total={}；updated_at=0 的 {} 条；文件 size=0 的 {} 条；fid 未加前缀 {} 条",
                        listing.entries.len(),
                        dirs,
                        files.len(),
                        listing.total,
                        no_time,
                        zero_size,
                        bare_fid
                    ),
                );
                println!(
                    "    样例: {:?}",
                    listing
                        .entries
                        .iter()
                        .take(3)
                        .map(|e| format!(
                            "{} dir={} size={} at={}",
                            e.name, e.is_dir, e.size, e.updated_at
                        ))
                        .collect::<Vec<_>>()
                );
            }
            Err(err) => {
                probe.fail("list(根)", err);
                probe.report();
                return probe;
            }
        }

        // ---------------------------------------------------- make_dir -----
        let mut sandbox = match drive.make_dir(root, &base).await {
            Ok(id) => {
                probe.pass("make_dir", format!("{base} -> {id}"));
                native_of(&id)
            }
            Err(err) => {
                probe.fail("make_dir", err);
                probe.report();
                return probe;
            }
        };
        match find_settled(drive, root, &base, 6).await {
            Ok(Some((fid, entry))) => {
                sandbox = fid;
                probe.add(
                    "list 确认新目录",
                    if entry.is_dir { Status::Pass } else { Status::Fail },
                    format!("fid={} is_dir={}", entry.fid, entry.is_dir),
                );
            }
            Ok(None) => probe.add(
                "list 确认新目录",
                Status::Fail,
                "make_dir 返回了 id，但根列表里没有这个目录",
            ),
            Err(err) => probe.fail("list 确认新目录", err),
        }
        match drive.list(&sandbox).await {
            Ok(listing) => probe.add(
                "list(新目录)",
                if listing.entries.is_empty() {
                    Status::Pass
                } else {
                    Status::Fail
                },
                format!("{} 条（期望 0）", listing.entries.len()),
            ),
            Err(err) => probe.fail("list(新目录)", err),
        }

        let mv_dir = match drive.make_dir(root, &mv_dir_name).await {
            Ok(id) => {
                probe.pass("make_dir(第二目录)", format!("{mv_dir_name} -> {id}"));
                native_of(&id)
            }
            Err(err) => {
                probe.fail("make_dir(第二目录)", err);
                String::new()
            }
        };

        // ------------------------------------------------------- upload ----
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let local = std::env::temp_dir().join(&file_name);
        std::fs::write(&local, &payload).expect("写入临时文件");
        let _temp = TempFile(local.clone());
        let counter = Arc::new(CounterSink::new(Arc::new(AtomicU64::new(0))));
        let sink: Arc<dyn UploadSink> = counter.clone();
        let uploaded = drive
            .upload(
                &local,
                &sandbox,
                &file_name,
                sink,
                CancellationToken::new(),
            )
            .await;
        match uploaded {
            Ok(id) => probe.pass(
                "upload",
                format!(
                    "4096 字节 -> id={:?} phase={} done={}",
                    id,
                    counter.current_phase(),
                    counter.done.load(std::sync::atomic::Ordering::Relaxed)
                ),
            ),
            Err(err) => probe.fail("upload", err),
        }
        if probe.halted() {
            probe.report();
            return probe;
        }

        let mut current_file = file_name.clone();
        let mut file_fid = match find_settled(drive, &sandbox, &current_file, 6).await {
            Ok(Some((fid, entry))) => {
                let size_ok = entry.size == payload.len() as u64;
                probe.add(
                    "list 确认上传",
                    if size_ok { Status::Pass } else { Status::Fail },
                    format!(
                        "{} 字节（期望 {}）updated_at={} is_dir={}",
                        entry.size,
                        payload.len(),
                        entry.updated_at,
                        entry.is_dir
                    ),
                );
                Some(fid)
            }
            Ok(None) => {
                probe.add(
                    "list 确认上传",
                    Status::Fail,
                    format!("对话目录里找不到 {current_file}"),
                );
                None
            }
            Err(err) => {
                probe.fail("list(沙箱)", err);
                None
            }
        };

        // ----------------------------------------------------- rename ------
        match drive.rename(&sandbox, &format!("{base}-renameddir")).await {
            Ok(()) => {
                match find_settled(drive, root, &format!("{base}-renameddir"), 6).await {
                    Ok(Some((fid, _))) => {
                        sandbox = fid;
                        probe.pass("rename(目录)", format!("{base} -> {base}-renameddir"));
                    }
                    Ok(None) => probe.add(
                        "rename(目录)",
                        Status::Fail,
                        "返回成功但列表里没有新名字",
                    ),
                    Err(err) => probe.fail("rename(目录) 确认", err),
                }
                // Put the name back so later steps keep a stable sandbox name.
                if drive.rename(&sandbox, &base).await.is_ok() {
                    if let Ok(Some((fid, _))) = find_settled(drive, root, &base, 6).await {
                        sandbox = fid;
                    }
                }
            }
            Err(err) => probe.fail("rename(目录)", err),
        }

        if let Some(fid) = file_fid.clone() {
            match drive.rename(&fid, &renamed_file).await {
                Ok(()) => match find_settled(drive, &sandbox, &renamed_file, 6).await {
                    Ok(Some((new_fid, entry))) => {
                        file_fid = Some(new_fid);
                        current_file = renamed_file.clone();
                        probe.pass(
                            "rename(文件)",
                            format!("-> {renamed_file}（size={}）", entry.size),
                        );
                    }
                    Ok(None) => probe.add(
                        "rename(文件)",
                        Status::Fail,
                        "返回成功但新名字不在目录里",
                    ),
                    Err(err) => probe.fail("rename(文件) 确认", err),
                },
                Err(err) => probe.fail("rename(文件)", err),
            }
        } else {
            probe.skip("rename(文件)", "没有可用的文件 id");
        }

        // ---------------------------------------------------- download -----
        if let Some(fid) = file_fid.clone() {
            match drive.open_download(&fid, 0).await {
                Ok(handle) => {
                    let size = handle.size;
                    match read_first(handle, 1024).await {
                        Ok(bytes) => probe.add(
                            "open_download(0)",
                            if bytes == payload[..bytes.len()] {
                                Status::Pass
                            } else {
                                Status::Fail
                            },
                            format!(
                                "首 {} 字节，内容与上传一致={}，handle.size={}",
                                bytes.len(),
                                bytes == payload[..bytes.len()],
                                size
                            ),
                        ),
                        Err(err) => probe.fail("open_download(0)", err),
                    }
                }
                Err(err) => probe.fail("open_download(0)", err),
            }
            match drive.open_download(&fid, 100).await {
                Ok(handle) => match read_first(handle, 64).await {
                    Ok(bytes) => {
                        let expect = &payload[100..100 + bytes.len()];
                        probe.add(
                            "open_download(100)",
                            if bytes == expect {
                                Status::Pass
                            } else {
                                Status::Fail
                            },
                            format!(
                                "偏移 100 首块 {} 字节，内容一致={}",
                                bytes.len(),
                                bytes == expect
                            ),
                        );
                    }
                    Err(err) => probe.fail("open_download(100)", err),
                },
                Err(err) => probe.fail("open_download(100)", err),
            }
            probe.add(
                "supports_ranges",
                Status::Skipped,
                format!("{}", drive.supports_ranges()),
            );
        } else {
            probe.skip("open_download", "上传失败，没有可下载的文件");
        }
        if probe.halted() {
            probe.report();
            return probe;
        }

        // -------------------------------------------------- copy / move ----
        let mut copy_fid: Option<String> = None;
        if caps.copy {
            if let (Some(fid), false) = (file_fid.clone(), mv_dir.is_empty()) {
                match drive.transfer_entries(&[fid], &mv_dir, true).await {
                    Ok(count) => match find_settled(drive, &mv_dir, &current_file, 6).await {
                        Ok(Some((new_fid, entry))) => {
                            copy_fid = Some(new_fid);
                            probe.pass(
                                "copy",
                                format!(
                                    "返回 {count}；目标目录出现 {}（{} 字节）",
                                    entry.name, entry.size
                                ),
                            );
                        }
                        Ok(None) => probe.add(
                            "copy",
                            Status::Fail,
                            format!("返回 {count}，但目标目录里没有 {}", current_file),
                        ),
                        Err(err) => probe.fail("copy 确认", err),
                    },
                    Err(err) => probe.fail("copy", err),
                }
            } else {
                probe.skip("copy", "没有可复制的文件或目标目录");
            }
        } else {
            probe.missing("copy", "capabilities.copy=false");
        }

        // Rename the copy so the move test cannot collide with `current_file`.
        if let Some(fid) = copy_fid.clone() {
            if drive.rename(&fid, &copy_name).await.is_ok() {
                if let Ok(Some((new_fid, _))) = find_settled(drive, &mv_dir, &copy_name, 6).await {
                    copy_fid = Some(new_fid);
                }
            }
        }

        if caps.move_files {
            match copy_fid.clone() {
                Some(fid) if !sandbox.is_empty() => {
                    match drive.transfer_entries(&[fid], &sandbox, false).await {
                        Ok(count) => {
                            let target = find_settled(drive, &sandbox, &copy_name, 6)
                                .await
                                .ok()
                                .flatten();
                            let source = find_settled(drive, &mv_dir, &copy_name, 4)
                                .await
                                .ok()
                                .flatten();
                            let from_gone = match source {
                                None => true,
                                Some(_) => {
                                    // 天翼 applies the move lazily: the entry
                                    // keeps showing in the source for a while
                                    // even though the destination has it.
                                    let tries =
                                        if drive.id() == ProviderId::Cloud189 { 25 } else { 8 };
                                    gone_settled(drive, &mv_dir, &copy_name, tries).await
                                }
                            };
                            let to_present = target.is_some();
                            if let Some((new_fid, _)) = target {
                                copy_fid = Some(new_fid);
                            }
                            probe.add(
                                "move",
                                if from_gone && to_present {
                                    Status::Pass
                                } else {
                                    Status::Fail
                                },
                                format!(
                                    "返回 {count}；源目录已空={from_gone} 目标目录已出现={to_present}"
                                ),
                            );
                        }
                        Err(err) => probe.fail("move", err),
                    }
                }
                _ => probe.skip("move", "没有可移动的副本"),
            }
        } else {
            probe.missing("move", "capabilities.move=false");
        }

        // ------------------------------------------------------- search ----
        if caps.search {
            // Indexing lags behind writes on every drive here (夸克 measured at
            // ~2 s), so a single query right after an upload under-reports.
            let mut hits = Vec::new();
            let mut failure = None;
            for attempt in 0..3u32 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                match drive.search(root, &copy_name, 20).await {
                    Ok(found) => {
                        let hit = found.iter().any(|h| h.entry.name == copy_name);
                        hits = found;
                        if hit {
                            failure = None;
                            break;
                        }
                    }
                    Err(err) => {
                        failure = Some(err);
                    }
                }
            }
            match failure {
                Some(err) => probe.fail("search", err),
                None => {
                    let hit = hits.iter().any(|h| h.entry.name == copy_name);
                    probe.add(
                        "search",
                        if hit { Status::Pass } else { Status::Fail },
                        format!("{} 条命中，包含沙箱文件={hit}", hits.len()),
                    );
                }
            }
        } else {
            probe.missing("search", "capabilities.search=false");
        }

        // -------------------------------------------------------- shares ---
        let mut share_open_done = false;
        if caps.share_create {
            let code = crate::share::random_code(4);
            let title = format!("{base}-share");
            // Share the file when there is one: a folder share hides the child
            // behind `share_list`, so `share_link` would go untested.
            let shared: Vec<String> = match file_fid.clone() {
                Some(fid) => vec![fid],
                None => vec![sandbox.clone()],
            };
            match drive.create_share(&shared, &title, 1, &code).await {
                Ok(created) => {
                    probe.pass(
                        "create_share",
                        format!(
                            "url={} passcode={:?} share_id={} expired_at={}",
                            created.url, created.passcode, created.share_id, created.expired_at
                        ),
                    );
                    if caps.share_open {
                        // The trait takes the drive-side id, not the browser
                        // address: `commands` runs every link through
                        // `share::parse_link` first, so the probe must too.
                        let target = crate::share::parse_link(&created.url);
                        let passcode = if created.passcode.is_empty() {
                            code.clone()
                        } else {
                            created.passcode.clone()
                        };
                        probe.add(
                            "parse_link",
                            if target.is_ok() { Status::Pass } else { Status::Fail },
                            format!("{:?}", target.as_ref().map(|t| (&t.id, &t.passcode))),
                        );
                        let target = target.unwrap_or(crate::share::ShareTarget {
                            provider: ProviderId::Quark,
                            id: created.share_id.clone(),
                            passcode: passcode.clone(),
                        });
                        let pass = if target.passcode.is_empty() {
                            passcode.clone()
                        } else {
                            target.passcode.clone()
                        };
                        share_roundtrip(drive, label, &target.id, &pass, Some(&sandbox), &mut probe).await;
                        share_open_done = true;
                    } else {
                        probe.missing("share_open", "capabilities.share_open=false");
                    }
                    match drive
                        .revoke_share(&created.share_id, &[sandbox.clone()])
                        .await
                    {
                        Ok(()) => probe.pass("revoke_share", "已取消"),
                        Err(err) => probe.fail("revoke_share", err),
                    }
                }
                Err(err) => probe.fail("create_share", err),
            }
        } else {
            probe.missing("create_share", "capabilities.share_create=false");
        }

        // A drive that cannot create a share on this account may still open
        // one: fall back to a share the account already owns so `share_open`
        // is measured rather than assumed.
        if caps.share_open && !share_open_done && !probe.halted() {
            match drive.share_records().await {
                Ok(records) if !records.is_empty() => {
                    let record = &records[0];
                    probe.add(
                        "share_records",
                        Status::Pass,
                        format!(
                            "{} 条，取 url={} passcode={:?}",
                            records.len(),
                            truncate(&record.url, 60),
                            record.passcode
                        ),
                    );
                    let target = crate::share::parse_link(&record.url);
                    let (id, pass) = match &target {
                        Ok(t) => (
                            t.id.clone(),
                            if t.passcode.is_empty() {
                                record.passcode.clone()
                            } else {
                                t.passcode.clone()
                            },
                        ),
                        Err(_) => (record.share_id.clone(), record.passcode.clone()),
                    };
                    share_roundtrip(drive, label, &id, &pass, Some(&sandbox), &mut probe).await;
                }
                Ok(_) => probe.skip("share_open", "该盘没有可用的既有分享"),
                Err(err) => probe.skip("share_open", format!("既无可建分享，也读不到分享列表：{err}")),
            }
        }
        if probe.halted() {
            probe.report();
            return probe;
        }

        // ------------------------------------------------------- recycle ---
        // `delete` is checked on every drive: even without a bin the entry has
        // to disappear from the listing.
        let deleted = copy_fid.clone().or_else(|| file_fid.clone());
        if let Some(fid) = deleted.clone() {
            match drive.delete(&[fid]).await {
                Ok(()) => {
                    let gone = find_settled(drive, &sandbox, &copy_name, 5)
                        .await
                        .ok()
                        .flatten()
                        .is_none();
                    probe.add(
                        "delete",
                        if gone { Status::Pass } else { Status::Fail },
                        format!("已删除 {copy_name}；列表里已消失={gone}"),
                    );
                }
                Err(err) => probe.fail("delete", err),
            }
        } else {
            probe.skip("delete", "没有可删除的探针文件");
        }

        if caps.recycle {
            match deleted {
                Some(_) => {
                    match bin_mine(drive, &base, 5).await {
                        Ok((rows, mine)) => {
                            let mine: Vec<RecycleEntry> = mine;
                            probe.add(
                                "recycle_list",
                                if mine.is_empty() {
                                    Status::Fail
                                } else {
                                    Status::Pass
                                },
                                format!(
                                    "{} 条，本探针 {} 条：{}",
                                    rows,
                                    mine.len(),
                                    mine.iter()
                                        .map(|r| format!("{}({}B)", r.name, r.size))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                ),
                            );
                            if !mine.is_empty() {
                                match drive.recycle_restore(&mine).await {
                                    Ok(count) => {
                                        let back = find_settled(drive, &sandbox, &copy_name, 6)
                                            .await
                                            .ok()
                                            .flatten()
                                            .is_some();
                                        probe.add(
                                            "recycle_restore",
                                            if back { Status::Pass } else { Status::Fail },
                                            format!("恢复 {count} 条，list 里回来={back}"),
                                        );
                                    }
                                    Err(err) => probe.fail("recycle_restore", err),
                                }
                                // Trash it again, then purge.
                                let again = find_settled(drive, &sandbox, &copy_name, 4)
                                    .await
                                    .ok()
                                    .flatten()
                                    .map(|(fid, _)| fid)
                                    .or_else(|| copy_fid.clone());
                                if let Some(fid) = again {
                                    if let Err(err) = drive.delete(&[fid]).await {
                                        probe.fail("delete(二次)", err);
                                    }
                                    let mine2 = bin_mine(drive, &base, 4)
                                        .await
                                        .map(|(_, mine)| mine)
                                        .unwrap_or_default();
                                    match drive.recycle_purge(&mine2).await {
                                        Ok(count) => {
                                            probe.pass("recycle_purge", format!("{count} 条"))
                                        }
                                        Err(err) if err.to_string().contains("暂不支持") => {
                                            probe.missing("recycle_purge", err.to_string())
                                        }
                                        Err(err) => probe.fail("recycle_purge", err),
                                    }
                                }
                            }
                        }
                        Err(err) => probe.fail("recycle_list", err),
                    }
                }
                None => probe.skip("delete", "没有可删除的探针文件"),
            }
        } else {
            probe.missing("recycle_list", "capabilities.recycle=false");
            probe.missing("recycle_restore", "capabilities.recycle=false");
            probe.missing("recycle_purge", "capabilities.recycle=false");
        }

        // ------------------------------------------------------- cleanup ---
        // Everything we created lives under `base*`; delete the contents first
        // so the folders themselves can go.
        if let Ok(listing) = drive.list(&sandbox).await {
            let fids: Vec<String> = listing
                .entries
                .iter()
                .filter(|e| e.name.starts_with(&base))
                .map(|e| native_of(&e.fid))
                .collect();
            if !fids.is_empty() {
                let _ = drive.delete(&fids).await;
            }
        }
        if !mv_dir.is_empty() {
            if let Ok(listing) = drive.list(&mv_dir).await {
                let fids: Vec<String> = listing
                    .entries
                    .iter()
                    .map(|e| native_of(&e.fid))
                    .collect();
                if !fids.is_empty() {
                    let _ = drive.delete(&fids).await;
                }
            }
        }
        let mut doomed = vec![sandbox.clone()];
        if !mv_dir.is_empty() {
            doomed.push(mv_dir.clone());
        }
        let _ = drive.delete(&doomed).await;

        // Sweep stale `yunyu-probe-pf-*` leftovers from earlier probe runs too:
        // a run that died before its cleanup leaves them behind, and the
        // namespace is ours alone so no user data can match.
        let sweep = |listing: &DirListing| {
            listing
                .entries
                .iter()
                .filter(|e| e.name.starts_with("yunyu-probe-pf-"))
                .map(|e| native_of(&e.fid))
                .collect::<Vec<_>>()
        };
        for _ in 0..3 {
            let stale = match drive.list(root).await {
                Ok(listing) => sweep(&listing),
                Err(_) => break,
            };
            if stale.is_empty() {
                break;
            }
            let _ = drive.delete(&stale).await;
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        }

        let mut root_clean: Vec<String> = Vec::new();
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            }
            match drive.list(root).await {
                Ok(listing) => {
                    root_clean = listing
                        .entries
                        .iter()
                        .filter(|e| e.name.starts_with(&base))
                        .map(|e| e.name.clone())
                        .collect();
                    if root_clean.is_empty() {
                        break;
                    }
                }
                Err(err) => {
                    probe.fail("清理(根目录)", err);
                    break;
                }
            }
        }
        probe.add(
            "清理(根目录)",
            if root_clean.is_empty() {
                Status::Pass
            } else {
                Status::Fail
            },
            if root_clean.is_empty() {
                format!("{base}* 已全部移除")
            } else {
                format!("仍残留：{}", root_clean.join(", "))
            },
        );
        scrub_bin(drive, &base, aliyun, &mut probe, "回收站").await;

        probe.report();
        probe
    }

    // --------------------------------------------------------------- tests ---

    fn quark_drive() -> QuarkProvider {
        let dir = config_dir();
        let cookie = std::fs::read_to_string(dir.join("cookie.txt"))
            .expect("cookie.txt")
            .trim()
            .to_string();
        let client = libquarkpan::QuarkPan::builder()
            .cookie(cookie.clone())
            .api_base_url("https://drive.quark.cn")
            .prepare()
            .expect("quark client");
        QuarkProvider::new(
            client,
            cookie,
            dir.join("cookie.txt").to_string_lossy().to_string(),
        )
    }

    #[tokio::test]
    #[ignore]
    async fn probe_quark() {
        let drive = quark_drive();
        probe_drive("quark", &drive, "0", None).await;
    }

    #[tokio::test]
    #[ignore]
    async fn probe_yun139() {
        let drive = Yun139Provider::load(&config_dir()).expect("yun139");
        probe_drive("yun139", &drive, "0", None).await;
    }

    #[tokio::test]
    #[ignore]
    async fn probe_aliyun() {
        let drive = AliyunProvider::load(&config_dir()).expect("aliyun");
        probe_drive("aliyun", &drive, "0", Some(&drive)).await;
    }

    /// 阿里云盘 returns bare 401/403s that the provider maps to 「登录已过期」;
    /// the real body is only in the app log. This drives the same probe with
    /// the logger pointed at a temp dir so the original text can be quoted.
    #[tokio::test]
    #[ignore]
    async fn probe_aliyun_with_log() {
        let dir = std::env::temp_dir().join(format!("yunyu-probe-log-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        crate::logging::init(&dir);
        let drive = AliyunProvider::load(&config_dir()).expect("aliyun");
        probe_drive("aliyun", &drive, "0", Some(&drive)).await;
        if let Ok(text) = std::fs::read_to_string(dir.join("app.log")) {
            let lines: Vec<&str> = text
                .lines()
                .filter(|line| line.contains("aliyun:"))
                .collect();
            if !lines.is_empty() {
                eprintln!("    ── 诊断日志（aliyun: 行，尾部）──");
                for line in lines.iter().rev().take(8).rev() {
                    eprintln!("    {line}");
                }
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn probe_pan123() {
        let drive = Pan123Provider::load(&config_dir()).expect("pan123");
        probe_drive("pan123", &drive, "0", None).await;
    }

    #[tokio::test]
    #[ignore]
    async fn probe_baidu() {
        let drive = BaiduProvider::load(&config_dir()).expect("baidu");
        probe_drive("baidu", &drive, "0", None).await;
    }

    #[tokio::test]
    #[ignore]
    async fn probe_cloud189() {
        let drive = Cloud189Provider::load(&config_dir()).expect("cloud189");
        probe_drive("cloud189", &drive, "0", None).await;
    }

    #[tokio::test]
    #[ignore]
    async fn probe_pan115() {
        let drive = Pan115Provider::load(&config_dir()).expect("pan115");
        probe_drive("pan115", &drive, "0", None).await;
    }
}

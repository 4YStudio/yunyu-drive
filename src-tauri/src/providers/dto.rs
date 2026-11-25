//! Payload types shared by every drive and the frontend.
//!
//! These lived in `commands.rs` while Quark was the only backend; they are
//! provider-neutral, so they now sit next to the trait that produces them.

use serde::{Deserialize, Serialize};

/// One file or folder, in the shape the UI renders.
///
/// `fid` is always the **qualified** id (`"quark:abc123"`), so a stale entry
/// from another drive can never be mistaken for a local one.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryDto {
    pub fid: String,
    pub name: String,
    pub pdir_fid: String,
    pub size: u64,
    pub is_dir: bool,
    pub format_type: String,
    pub updated_at: u64,
    pub created_at: u64,
    pub ext: String,
    /// Which drive this entry belongs to, for the badge in mixed views.
    pub provider: String,
    /// Preview image url, when the drive reports one. The frontend inlines it
    /// before rendering (the CSP blocks remote hosts), so this stays `None`
    /// for drives that need a second request per file.
    pub thumb: Option<String>,
}

impl EntryDto {
    /// Lower-cased extension, used for the icon and the "kind" label.
    pub fn ext_of(name: &str) -> String {
        std::path::Path::new(name)
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_lowercase())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirListing {
    /// Qualified id of the folder that was listed.
    pub pdir_fid: String,
    pub entries: Vec<EntryDto>,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathNode {
    /// Qualified id; `"<provider>:0"` is the root.
    pub fid: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub entry: EntryDto,
    pub path: Vec<PathNode>,
}

/// One item sitting in a drive's recycle bin.
///
/// The drives disagree on what identifies a trashed item: 夸克 keeps a
/// `record_id` alongside the file id, 阿里 hands back a `file_id` that only
/// exists inside the bin, 123 keeps the original id. `token` carries whatever
/// the drive needs besides the id to restore or purge the row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecycleEntry {
    /// Qualified id (`"<provider>:<native>"`), like every other entry.
    pub fid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// When the drive says it was thrown away (ms); 0 when it does not.
    pub deleted_at: u64,
    /// Drive-specific handle the restore/purge calls need.
    pub token: String,
    pub provider: String,
}

/// Free/total space, when the drive exposes it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaInfo {
    pub used: u64,
    pub total: u64,
}

/// Monthly download allowance a drive meters separately from storage.
///
/// 123 云盘 is the only drive with one: free accounts get a monthly download
/// quota, and once it runs out the drive refuses to hand over file bodies until
/// the quota resets or the account is upgraded. `left` is what is still
/// available this period, in bytes.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrafficInfo {
    /// Remaining download allowance for the account's own files.
    pub left: u64,
    /// Remaining allowance for files pulled out of share links.
    pub share: u64,
    /// Remaining allowance for direct links.
    pub direct: u64,
}

/// A single file to download; the UI sends these from a selection.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    /// Qualified id.
    pub fid: String,
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub is_dir: bool,
    /// Share entries carry a per-file token required to save/download them.
    #[serde(default)]
    pub token: Option<String>,
}

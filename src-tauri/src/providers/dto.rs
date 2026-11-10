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

/// Free/total space, when the drive exposes it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaInfo {
    pub used: u64,
    pub total: u64,
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

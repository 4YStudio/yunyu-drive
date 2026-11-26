//! Where the bytes come from.
//!
//! The engine does not care whether a file sits in the user's own drive or
//! inside someone else's share link: both are "mint a url, send a `Range`
//! header, stream the body". The two paths differ only in how the url and the
//! headers are produced, so that is the only thing this trait asks for.
//!
//! Keeping the distinction out of the engine is what lets share downloads get
//! the same multi-connection treatment as ordinary ones — the single-stream
//! path they used before was a separate code path that could not be segmented
//! at all.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::error::AppError;
use crate::providers::{DownloadHandle, DriveProvider, RangeResult};
use crate::transfer::ShareRef;

/// One ranged fetch, however the url is obtained.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// Opens `[start, end)` of the file. `end` is exclusive; `None` reads to
    /// the end.
    async fn open(&self, start: u64, end: Option<u64>) -> RangeResult;

    /// A short name for logs.
    fn label(&self) -> &str;

    /// Identity of the file this source serves, used to decide whether a
    /// leftover resume plan belongs to it.
    ///
    /// Length is not enough. Sidecars live next to the target `.part`, the
    /// target name is made unique by appending " (1)" when a name is taken,
    /// and two different files of the same length downloaded to the same name
    /// would otherwise share a plan — resuming one file's offsets onto the
    /// other produces a file of exactly the right length with the wrong
    /// contents. A fingerprint is the only thing that can tell them apart.
    fn fingerprint(&self) -> String;

    /// Releases anything the source had to create to become downloadable.
    ///
    /// Aliyun has no direct share download: it copies the file into the user's
    /// own drive and hands out a url for that copy. The engine calls this once
    /// the bytes are on disk, otherwise every share download would leave a
    /// stray file behind in the user's drive.
    async fn cleanup(&self) {}
}

/// A file in the user's own drive.
pub struct ProviderSource {
    pub drive: Arc<dyn DriveProvider>,
    pub fid: String,
    pub label: String,
}

#[async_trait::async_trait]
impl Source for ProviderSource {
    async fn open(&self, start: u64, end: Option<u64>) -> RangeResult {
        self.drive.open_download_range(&self.fid, start, end).await
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn fingerprint(&self) -> String {
        format!("{}|{}", self.drive.id().as_str(), self.fid)
    }
}

/// How long a share url is reused before it is minted again.
///
/// Short enough to stay well inside every drive's signature lifetime, long
/// enough that a few hundred segments do not each trigger a fresh call. The
/// `Expired` result is the real safety net: a url that dies early comes back
/// with 401/403/412 and the next attempt re-mints immediately.
const SHARE_LINK_TTL: Duration = Duration::from_secs(60);

/// A file inside someone's share.
///
/// The url is cached for [`SHARE_LINK_TTL`] instead of being minted per
/// segment. On Aliyun a mint is not free: the drive has no direct share
/// download, so `share_link` copies the file into the user's own drive first.
/// Minting per segment would copy it hundreds of times.
pub struct ShareSource {
    pub drive: Arc<dyn DriveProvider>,
    pub share: ShareRef,
    pub fid: String,
    pub label: String,
    /// The cached url and when it was obtained.
    cached: Mutex<Option<(crate::providers::ShareLink, Instant)>>,
    /// Every temporary copy the drive made to serve a url, including the ones
    /// belonging to urls that have since expired.
    ///
    /// A list rather than "whatever the live cache holds": `invalidate` throws
    /// the cached link away precisely when a url goes stale, and on a drive that
    /// serves a share file by copying it into the user's drive, the *next* mint
    /// copies it again. Reading the copy id off the live cache would therefore
    /// forget the expired one and leave it in the user's drive for good.
    copies: Mutex<Vec<String>>,
}

impl ShareSource {
    /// Returns a usable link, minting one when the cache is empty, stale, or
    /// explicitly rejected by `invalidate`.
    async fn link(&self) -> Result<crate::providers::ShareLink, RangeResult> {
        let mut cached = self.cached.lock().await;
        if let Some((link, at)) = cached.as_ref() {
            if at.elapsed() < SHARE_LINK_TTL {
                return Ok(link.clone());
            }
        }
        let link = match self.drive.share_link(&self.share, &self.fid).await {
            Ok(link) => link,
            Err(err) => return Err(RangeResult::from_error(err)),
        };
        if let Some(copy) = link.cleanup_fid.clone() {
            self.copies.lock().await.push(copy);
        }
        *cached = Some((link.clone(), Instant::now()));
        Ok(link)
    }

    /// Drops the cached url so the next attempt gets a fresh one.
    async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }
}

#[async_trait::async_trait]
impl Source for ShareSource {
    async fn open(&self, start: u64, end: Option<u64>) -> RangeResult {
        let link = match self.link().await {
            Ok(link) => link,
            Err(result) => return result,
        };
        let client = match crate::share::http() {
            Ok(client) => crate::net::Http::new(client, "download"),
            Err(err) => return RangeResult::Failed(err),
        };

        let mut request = client.get(&link.url).streaming();
        for (name, value) in &link.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        if end.is_some() || start > 0 {
            let range = match end {
                Some(end) if end > start => format!("bytes={start}-{}", end - 1),
                _ => format!("bytes={start}-"),
            };
            request = request.header(reqwest::header::RANGE, range);
        }

        let res = match request.send().await {
            Ok(res) => res,
            Err(err) => return RangeResult::Failed(AppError::new(format!("下载失败：{err}"))),
        };
        let status = res.status();
        // A signed url that has gone stale is worth re-minting; a 404 is not.
        if matches!(status.as_u16(), 401 | 403 | 410 | 412) {
            self.invalidate().await;
            return RangeResult::Expired;
        }
        if !status.is_success() {
            return RangeResult::Failed(AppError::new(format!(
                "下载失败（HTTP {}）",
                status.as_u16()
            )));
        }
        // A CDN that ignores `Range` answers 200 with the whole body.
        if start > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return RangeResult::RangeIgnored;
        }

        let served = res
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let stream = {
            use futures_util::StreamExt;
            res.bytes_stream()
                .map(|chunk| chunk.map_err(|err| AppError::new(format!("读取失败：{err}"))))
        };
        RangeResult::body(
            DownloadHandle {
                size: link.size,
                md5: None,
                stream: Box::new(stream),
            },
            start,
            served.as_deref(),
        )
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn fingerprint(&self) -> String {
        format!(
            "{}:{}|{}|{}",
            self.drive.id().as_str(),
            self.share.provider,
            self.share.pwd_id,
            self.fid
        )
    }

    async fn cleanup(&self) {
        let copies = {
            let mut held = self.copies.lock().await;
            std::mem::take(&mut *held)
        };
        for copy in copies {
            self.drive.share_cleanup(&copy).await;
        }
    }
}

/// Builds the source for a share download.
pub fn share_source(
    drive: Arc<dyn DriveProvider>,
    share: ShareRef,
    fid: String,
    label: String,
) -> Arc<dyn Source> {
    Arc::new(ShareSource {
        drive,
        share,
        fid,
        label,
        cached: Mutex::new(None),
        copies: Mutex::new(Vec::new()),
    })
}

/// Builds the source for an ordinary download.
pub fn provider_source(
    drive: Arc<dyn DriveProvider>,
    fid: String,
    label: String,
) -> Arc<dyn Source> {
    Arc::new(ProviderSource { drive, fid, label })
}

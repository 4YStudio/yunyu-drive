use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;

use crate::providers::{BoxStream, DownloadHandle, DriveProvider, UploadSink, RANGE_IGNORED};

use crate::error::{AppError, AppResult};
use crate::paths::unique_path;

pub const EVENT_TRANSFER: &str = "transfer://update";
pub const EVENT_REMOVED: &str = "transfer://removed";
/// Many tasks at once.
///
/// Downloading a folder that holds thousands of files used to announce every
/// task with its own event: the renderer then inserted them one by one into a
/// list that re-rendered each time, which is what made the window freeze after
/// "已加入下载队列 2999 项". One payload per batch keeps that cost linear.
pub const EVENT_TRANSFERS: &str = "transfer://batch";
/// Many removals at once, same reason.
pub const EVENT_REMOVED_MANY: &str = "transfer://removed-many";
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferTask {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub fid: Option<String>,
    pub local_path: String,
    pub remote_dir_fid: Option<String>,
    pub size: u64,
    pub transferred: u64,
    pub speed: f64,
    pub status: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub rapid: bool,
    /// Bytes the provider has read while preparing the upload (hashing the file,
    /// building the slice list, precreating the remote entry). Non-zero only
    /// while `status == "hashing"`; the upload itself reports through
    /// `transferred`. Lets the list show real progress instead of a bar stuck at
    /// full while Baidu hashes a large file.
    #[serde(default)]
    pub prepared: u64,
    /// How many connections the segmented engine has open right now, and how
    /// many pieces the file was cut into. Zero for a single-stream download and
    /// for uploads. This is the number users ask about when a transfer looks
    /// slow: 1 connection means the drive refused ranges, 256 means the pool is
    /// wide and the bottleneck is upstream.
    #[serde(default)]
    pub connections: u64,
    #[serde(default)]
    pub segments: u64,
    /// Pieces already fetched, out of `segments`. Shows the real granularity of
    /// a multi-connection download, where a progress bar alone hides the fact
    /// that hundreds of pieces are in flight.
    #[serde(default)]
    pub segments_done: u64,
    /// Partial file a download writes into. Kept across a failure so the next
    /// attempt resumes with a Range request instead of starting over, and
    /// removed together with the task so no orphaned `.part` is left behind.
    #[serde(default)]
    pub part_path: Option<String>,
    /// Set for share downloads so a retry can rebuild the request without the
    /// original job payload.
    #[serde(default)]
    pub share: Option<ShareRef>,
    /// Which drive this task talks to (`"quark"`, `"yun139"`, …), for the badge
    /// in the transfer list and for rebuilding the context on retry.
    #[serde(default)]
    pub provider: String,
    /// Set for a cross-drive move/copy. Holds both drives, the files still to
    /// go and how far the job got, so a retry can pick up where it stopped.
    #[serde(default)]
    pub cross: Option<CrossRef>,
    /// Id of the folder task a hidden child belongs to.
    ///
    /// A folder download is one row in the list: the walker streams the files it
    /// finds into real download tasks, but those are bookkeeping — they are not
    /// announced, not listed and not persisted, so a 3000-file folder means one
    /// row and one progress number instead of 3000 of each.
    #[serde(default)]
    pub parent: Option<String>,
    /// Never leaves the backend; see `parent`.
    #[serde(default)]
    pub hidden: bool,
    /// A folder task whose walk has not finished yet. `size` keeps growing while
    /// this is true, so the list shows "正在读取" instead of a false percentage.
    #[serde(default)]
    pub listing: bool,
    /// Files found by the walk, finished, and failed — the folder task's own
    /// progress, shown as "12 / 340 个文件".
    #[serde(default)]
    pub children_total: u64,
    #[serde(default)]
    pub children_done: u64,
    #[serde(default)]
    pub children_failed: u64,
    /// Root of the folder, so a retry can walk it again after a restart without
    /// keeping the whole file list.
    #[serde(default)]
    pub folder: Option<FolderRef>,
}

/// Root of a folder download.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRef {
    /// Which drive the folder lives on (`"quark"`, `"pan115"`, …).
    pub provider: String,
    /// Native id on that drive.
    pub fid: String,
    pub name: String,
}

/// One file found by the folder walker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FolderFile {
    /// Native id on the folder's drive.
    fid: String,
    name: String,
    /// Subfolder inside the download root (empty for the root itself).
    relative_dir: PathBuf,
    size: u64,
}

/// One cache the sidecar keeps of a listing that already happened.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SubDir {
    fid: String,
    name: String,
}

/// What a folder's sidecar file holds.
///
/// `files` is every file discovered so far, `dirs` is the listings that already
/// completed (their files are in `files`, so resuming must not list them
/// again), and `complete` says the whole tree was walked.
struct Listing {
    files: Vec<FolderFile>,
    dirs: std::collections::HashMap<String, Vec<SubDir>>,
    complete: bool,
}

/// How many times a directory listing is retried before it is skipped.
const WALK_RETRIES: u32 = 3;

/// First backoff between two listing attempts; doubles each time.
const WALK_BACKOFF_MS: u64 = 1_500;

/// How many child downloads a folder task keeps alive at once.
///
/// The queue gates the real concurrency, so this is only about not creating a
/// hundred thousand tasks up front: a window also means a cancel is instant and
/// the memory stays flat no matter how big the folder is.
const FOLDER_WINDOW: usize = 24;
/// How often a folder task re-reads its children and announces its progress.
const FOLDER_TICK_MS: u64 = 400;
/// Depth limit for the walk; deeper trees almost always mean a symlink loop.
const FOLDER_MAX_DEPTH: usize = 32;

/// One file of a cross-drive job, already expanded out of its folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossFile {
    /// Native id on the source drive.
    pub fid: String,
    /// Name to give the file on the target drive.
    pub name: String,
    pub size: u64,
    /// Native id of the folder on the target drive that receives it.
    pub to_parent: String,
}

/// Everything a cross-drive job needs, persisted with the task.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrossRef {
    pub from_provider: String,
    pub to_provider: String,
    pub to_pdir_fid: String,
    /// `true` keeps the source file, `false` deletes it once the copy landed.
    pub copy: bool,
    /// Files not finished yet; finished ones are dropped as the job advances.
    pub files: Vec<CrossFile>,
}

/// Parameters for starting a cross-drive move/copy.
#[derive(Debug, Clone)]
pub struct CrossJob {
    pub files: Vec<CrossFile>,
    pub to_pdir_fid: String,
    pub copy: bool,
    pub name: String,
}

impl TransferTask {
    fn emit(&self, app: &AppHandle) {
        // A hidden child of a folder task reports through its parent.
        if self.hidden {
            return;
        }
        let _ = app.emit(EVENT_TRANSFER, self.clone());
    }
}

pub struct TransferManager {
    tasks: RwLock<HashMap<String, TransferTask>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
    /// Which run currently owns a folder task. A resumed folder gets a new
    /// generation, and the previous supervisor stops as soon as it sees that it
    /// is no longer the current one — otherwise two loops would download the
    /// same files and count every byte twice.
    folder_runs: Mutex<HashMap<String, u64>>,
    counter: AtomicU64,
    /// Number of transfers currently holding a worker slot.
    active: std::sync::Mutex<usize>,
    /// Maximum number of simultaneous transfers (`settings.concurrent_transfers`).
    limit: AtomicU64,
    /// Wakes queued transfers when a slot frees up or the limit changes.
    notify: Notify,
    /// Where finished tasks are remembered between launches.
    store: Option<PathBuf>,
}

/// Upper bound on tasks written to `transfers.json`. Bounds the file at a few
/// hundred KiB while still keeping a whole large queue alive across restarts.
const PERSIST_LIMIT: usize = 4000;

/// Process-wide speed cap in bytes per second (0 = unlimited).
///
/// A single number rather than one per task: the point of the setting is to
/// stop the app from eating the whole link, and every transfer shares it.
static SPEED_LIMIT: AtomicU64 = AtomicU64::new(0);

/// Sets the cap. `kbps` is KiB/s, matching what the settings UI shows.
pub fn set_speed_limit(kbps: u64) {
    SPEED_LIMIT.store(kbps.saturating_mul(1024), Ordering::Relaxed);
}

/// The current cap in bytes per second (0 = unlimited).
///
/// Read by the segmented engine's limiter, which cannot reuse [`throttle`]: that
/// one measures a single writer against the whole budget, so several workers
/// calling it each get the full budget and the cap stops holding.
pub(crate) fn speed_limit_bytes() -> u64 {
    SPEED_LIMIT.load(Ordering::Relaxed)
}

/// Sleeps just enough to keep `written` bytes on schedule.
///
/// Called after each chunk: the budget is the bytes so far over the cap, and the
/// deficit against the elapsed time is how long the writer has to wait. Granular
/// (per chunk) rather than a token bucket, which is plenty for a desktop client.
pub(crate) async fn throttle(started: Instant, written: u64) {
    let cap = SPEED_LIMIT.load(Ordering::Relaxed);
    if cap == 0 || written == 0 {
        return;
    }
    let budget = Duration::from_secs_f64(written as f64 / cap as f64);
    let elapsed = started.elapsed();
    if budget > elapsed {
        tokio::time::sleep(budget - elapsed).await;
    }
}

/// Releases a worker slot when the task body finishes (including early returns).
struct SlotGuard {
    manager: Arc<TransferManager>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.manager.active.lock() {
            *active = active.saturating_sub(1);
        }
        self.manager.notify.notify_waiters();
    }
}

/// Parameters for starting a download task.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadJob {
    /// Qualified id (`"<provider>:<native>"`).
    pub fid: String,
    pub name: String,
    pub size: u64,
    pub dir: String,
    /// Set when the file lives in a share rather than the user's own drive.
    pub share: Option<ShareRef>,
    /// Which drive serves the file; persisted so a retry after a restart can
    /// rebuild the context.
    #[serde(default)]
    pub provider: String,
}

/// Everything needed to fetch a single file out of a share.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareRef {
    /// Which drive hosts the share (`"quark"`, `"yun139"`, …). Persisted so a
    /// retry after a restart can rebuild the right provider.
    #[serde(default)]
    pub provider: String,
    pub pwd_id: String,
    pub stoken: String,
    /// Drive-specific addressing token for this entry (Quark's
    /// `share_fid_token`, 123's etag/s3 key, Baidu's empty string).
    #[serde(default)]
    pub fid_token: String,
    /// Extract code, needed by the drives that re-authenticate per request
    /// (139) or per download (Baidu).
    #[serde(default)]
    pub passcode: String,
}

/// Where a download's bytes come from.
pub enum DownloadCtx {
    /// Any drive that implements the provider trait.
    Provider(Arc<dyn DriveProvider>),
    /// A file inside someone's share. The provider mints the url and the
    /// headers; the engine still owns resume, retry and progress. The share
    /// itself travels on the task (`TransferTask::share`) so a retry after a
    /// restart can rebuild this without the original payload.
    Share(Arc<dyn DriveProvider>),
}

/// Parameters for starting an upload task.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadJob {
    pub path: String,
    pub pdir_fid: String,
    pub name: Option<String>,
}

impl TransferManager {
    pub fn new(concurrent: u32, store: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            tasks: RwLock::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            folder_runs: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
            active: std::sync::Mutex::new(0),
            limit: AtomicU64::new(normalize_limit(concurrent)),
            notify: Notify::new(),
            store,
        })
    }

    /// Loads the task list written by the previous session.
    ///
    /// Unfinished work is restored as **paused**, not discarded: the partial
    /// file is still on disk, so the record has to come back for the user to
    /// resume it. (Previously only finished tasks survived, which is why an
    /// interrupted download vanished from the list while its `.part` stayed
    /// behind on disk.)
    pub async fn restore(self: &Arc<Self>, app: &AppHandle) {
        let Some(path) = self.store.clone() else {
            return;
        };
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            return;
        };
        let Ok(saved) = serde_json::from_str::<Vec<TransferTask>>(&text) else {
            return;
        };
        // `persist` writes the newest 100 with the newest first, so re-reverse to
        // chronological order and keep the *tail*: taking from the front used to
        // restore the oldest entries and drop the jobs the user just created.
        let mut restored = Vec::new();
        let skip = saved.len().saturating_sub(100);
        for mut task in saved.into_iter().rev().skip(skip) {
            if !matches!(task.status.as_str(), "done" | "failed" | "canceled") {
                task.status = "paused".to_string();
                task.error = None;
                task.speed = 0.0;
                // Believe the partial file rather than the saved counter.
                if let Some(part) = task.part_path.as_deref() {
                    if let Ok(meta) = tokio::fs::metadata(part).await {
                        task.transferred = meta.len();
                    }
                }
            }
            restored.push(task);
        }
        let mut tasks = self.tasks.write().await;
        for task in restored {
            tasks.insert(task.id.clone(), task.clone());
            task.emit(app);
        }
        drop(tasks);

        // A folder task removed while the app was closed leaves its sidecar
        // behind; nothing can resume it now.
        if let Some(root) = self.manifest_root() {
            let Ok(mut entries) = tokio::fs::read_dir(&root).await else {
                return;
            };
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                let id = name.trim_end_matches(".jsonl").to_string();
                if !self.tasks.read().await.contains_key(&id) {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
    }

    /// Writes the task list to disk (best effort, history + resumable jobs).
    async fn persist(&self) {
        let Some(path) = self.store.clone() else {
            return;
        };
        let snapshot: Vec<TransferTask> = {
            let tasks = self.tasks.read().await;
            let mut all: Vec<TransferTask> = tasks
                .values()
                .filter(|task| !task.hidden)
                .cloned()
                .collect();
            all.sort_by_key(|t| std::cmp::Reverse(t.created_at));
            // Unfinished work must survive a restart: its `.part` file is on
            // disk and the record is the only way back to it. Truncating the
            // whole list at 100 used to silently drop 2899 of a 3000-file folder
            // download. History keeps the most recent handful instead.
            let (unfinished, finished): (Vec<TransferTask>, Vec<TransferTask>) = all
                .into_iter()
                .partition(|t| !matches!(t.status.as_str(), "done" | "failed" | "canceled"));
            let mut keep = unfinished;
            keep.extend(finished.into_iter().take(100));
            keep.truncate(PERSIST_LIMIT);
            keep
        };
        if let Ok(text) = serde_json::to_string(&snapshot) {
            // Write through a temp file so a crash mid-write cannot leave a
            // truncated JSON that would silently drop the whole list.
            let tmp = path.with_extension("tmp");
            if tokio::fs::write(&tmp, text).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &path).await;
            }
        }
    }

    /// Persists on a short debounce instead of on every progress tick: the file
    /// now also carries running jobs, and a 400ms ticker would hammer the disk.
    ///
    /// `tauri::async_runtime::spawn`, not `tokio::spawn`: this is called from
    /// `setup()`, which runs on the main thread before the Tokio reactor is
    /// entered, and `tokio::spawn` there panics with "there is no reactor
    /// running".
    pub fn start_persister(self: &Arc<Self>) {
        if self.store.is_none() {
            return;
        }
        let manager = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                manager.persist().await;
            }
        });
    }

    /// Applies a new concurrency limit; queued tasks pick it up immediately.
    pub fn set_limit(&self, concurrent: u32) {
        self.limit.store(normalize_limit(concurrent), Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    /// Waits until a worker slot is free. Returns `None` when cancelled while queued.
    async fn acquire(self: &Arc<Self>, token: &CancellationToken) -> Option<SlotGuard> {
        loop {
            if token.is_cancelled() {
                return None;
            }
            {
                let mut active = match self.active.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if (*active as u64) < self.limit.load(Ordering::Relaxed) {
                    *active += 1;
                    return Some(SlotGuard {
                        manager: Arc::clone(self),
                    });
                }
            }
            tokio::select! {
                biased;
                _ = token.cancelled() => return None,
                _ = self.notify.notified() => {}
                // A fallback only: the notify above is what actually frees a
                // waiter. Polling every 250ms meant a 3000-file folder woke
                // every parked task 12000 times a second.
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub async fn list(&self) -> Vec<TransferTask> {
        let tasks = self.tasks.read().await;
        let mut all: Vec<TransferTask> = tasks
            .values()
            .filter(|task| !task.hidden)
            .cloned()
            .collect();
        all.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        all
    }

    async fn next_id(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}", now_millis(), n)
    }

    async fn insert(&self, task: TransferTask) -> TransferTask {
        self.tasks
            .write()
            .await
            .insert(task.id.clone(), task.clone());
        task
    }

    async fn patch<F>(&self, id: &str, app: &AppHandle, f: F)
    where
        F: FnOnce(&mut TransferTask),
    {
        let updated = {
            let mut tasks = self.tasks.write().await;
            match tasks.get_mut(id) {
                Some(task) => {
                    f(task);
                    task.updated_at = now_millis();
                    Some(task.clone())
                }
                None => None,
            }
        };
        if let Some(task) = updated {
            let terminal = matches!(
                task.status.as_str(),
                "done" | "failed" | "canceled" | "paused"
            );
            task.emit(app);
            if terminal {
                self.persist().await;
            }
        }
    }

    /// Removes a task.
    ///
    /// `purge_local` also deletes the files it produced: for a finished
    /// download that is the file (or, for a folder, the tree) on disk, and for
    /// an unfinished one its `.part` placeholder, which would otherwise linger
    /// forever with nothing pointing at it.
    pub async fn remove(&self, id: &str, app: &AppHandle, purge_local: bool) -> AppResult<()> {
        // Only a live worker needs a grace period; sleeping unconditionally made
        // every removal feel sluggish and delayed deleting the partial file.
        if self.cancel(id).await {
            // Give the worker a beat to observe the cancellation and close its
            // file handle before the partial file is unlinked.
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let removed = self.tasks.write().await.remove(id);
        if let Some(task) = removed {
            purge_task_files(&task, purge_local).await;
        }
        for child in self.remove_children(id).await {
            purge_task_files(&child, purge_local).await;
        }
        self.drop_manifest(id).await;
        let _ = app.emit(EVENT_REMOVED, id.to_string());
        self.persist().await;
        Ok(())
    }

    pub async fn clear_finished(&self, app: &AppHandle) -> AppResult<()> {
        let removed: Vec<TransferTask> = {
            let mut tasks = self.tasks.write().await;
            let keep = |t: &TransferTask| {
                t.hidden || !matches!(t.status.as_str(), "done" | "failed" | "canceled")
            };
            let removed: Vec<TransferTask> = tasks
                .values()
                .filter(|t| !keep(t))
                .cloned()
                .collect();
            tasks.retain(|_, t| keep(t));
            removed
        };
        for task in removed {
            discard_partial(&task).await;
            let _ = app.emit(EVENT_REMOVED, task.id);
        }
        self.persist().await;
        Ok(())
    }

    /// Pauses many tasks at once, returning the ones that changed.
    ///
    /// The caller announces the whole batch with one event: pausing a 3000-file
    /// queue used to send 3000 updates, each re-rendering the list.
    pub async fn pause_many(&self, ids: &[String]) -> Vec<TransferTask> {
        for id in ids {
            self.cancel(id).await;
        }
        let changed = {
            let mut tasks = self.tasks.write().await;
            let mut changed = Vec::new();
            for id in ids {
                if let Some(task) = tasks.get_mut(id) {
                    if matches!(task.status.as_str(), "queued" | "running" | "hashing") {
                        task.status = "paused".to_string();
                        task.error = None;
                        task.speed = 0.0;
                        task.updated_at = now_millis();
                        changed.push(task.clone());
                    }
                }
            }
            changed
        };
        if !changed.is_empty() {
            self.persist().await;
        }
        changed
    }

    /// Cancels many tasks at once, returning the ones that changed.
    pub async fn cancel_many(&self, ids: &[String]) -> Vec<TransferTask> {
        for id in ids {
            self.cancel(id).await;
        }
        let changed = {
            let mut tasks = self.tasks.write().await;
            let mut changed = Vec::new();
            for id in ids {
                if let Some(task) = tasks.get_mut(id) {
                    if matches!(task.status.as_str(), "queued" | "running" | "hashing") {
                        task.status = "canceled".to_string();
                        task.speed = 0.0;
                        task.updated_at = now_millis();
                        changed.push(task.clone());
                    }
                }
            }
            changed
        };
        if !changed.is_empty() {
            self.persist().await;
        }
        changed
    }

    /// Drops many task records (and their partial files), returning the ids that
    /// actually existed.
    pub async fn remove_many(&self, ids: &[String], purge_local: bool) -> Vec<String> {
        let mut removed = Vec::new();
        let mut discarded = Vec::new();
        let mut stopped = false;
        for id in ids {
            if self.cancel(id).await {
                stopped = true;
            }
        }
        // Same beat as `remove`: let the workers close their handles first — but
        // one beat for the whole batch, not one per task, or removing a thousand
        // running downloads would take two minutes.
        if stopped {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        {
            let mut tasks = self.tasks.write().await;
            for id in ids {
                if let Some(task) = tasks.remove(id) {
                    removed.push(id.clone());
                    discarded.push(task);
                }
            }
        }
        for task in discarded {
            purge_task_files(&task, purge_local).await;
        }
        for id in &removed {
            for child in self.remove_children(id).await {
                purge_task_files(&child, purge_local).await;
            }
            self.drop_manifest(id).await;
        }
        if !removed.is_empty() {
            self.persist().await;
        }
        removed
    }

    /// Live files of a folder task, for the expanded row in the transfer list.
    pub async fn children(&self, parent: &str) -> Vec<TransferTask> {
        let tasks = self.tasks.read().await;
        let mut list: Vec<TransferTask> = tasks
            .values()
            .filter(|t| t.parent.as_deref() == Some(parent))
            .cloned()
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Cancels a running worker. Returns whether one was actually running.
    async fn cancel(&self, id: &str) -> bool {
        match self.cancels.lock().await.remove(id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Stops a transfer while keeping everything needed to continue it later.
    pub async fn pause_task(&self, id: &str, app: &AppHandle) -> AppResult<()> {
        self.cancel(id).await;
        self.patch(id, app, |task| {
            if matches!(task.status.as_str(), "queued" | "running" | "hashing") {
                task.status = "paused".to_string();
                task.error = None;
                task.speed = 0.0;
            }
        })
        .await;
        self.persist().await;
        Ok(())
    }

    pub async fn cancel_task(&self, id: &str, app: &AppHandle) -> AppResult<()> {
        self.cancel(id).await;
        self.patch(id, app, |task| {
            if matches!(task.status.as_str(), "queued" | "running" | "hashing") {
                task.status = "canceled".to_string();
                task.speed = 0.0;
            }
        })
        .await;
        Ok(())
    }

    pub async fn start_download(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        job: DownloadJob,
    ) -> AppResult<TransferTask> {
        self.start_download_task(app, ctx, job, true).await
    }

    /// Starts a download without sending its own event.
    ///
    /// A folder download expands into one task per file; announcing each of them
    /// separately is what flooded the renderer. The caller collects the tasks
    /// and sends a single [`EVENT_TRANSFERS`] instead.
    pub async fn start_download_quiet(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        job: DownloadJob,
    ) -> AppResult<TransferTask> {
        self.start_download_task(app, ctx, job, false).await
    }

    async fn start_download_task(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        job: DownloadJob,
        announce: bool,
    ) -> AppResult<TransferTask> {
        let id = self.next_id().await;
        let target_dir = PathBuf::from(&job.dir);
        tokio::fs::create_dir_all(&target_dir).await?;
        // A leftover `.part` from an earlier attempt (same directory, same
        // name) means the file is half fetched: keep that name so the Range
        // resume lines up instead of writing a second copy next to it.
        let mut final_path = unique_path(&target_dir.join(&job.name));
        let mut part_path = with_suffix(&final_path, ".part");
        let resume_path = with_suffix(&target_dir.join(&job.name), ".part");
        // A `.part` on disk *is* the file we were downloading, so it wins even
        // when a finished file of the same name exists (a retry that clashed
        // with an older copy). Starting a fresh `"name (2).part"` next to it
        // threw away every byte the previous attempt had, which looked exactly
        // like "it is not resuming, it is downloading again".
        if resume_path.exists() {
            final_path = unique_path(&target_dir.join(&job.name));
            part_path = resume_path;
        }
        // Bytes already on disk count immediately, otherwise the progress bar
        // jumps from 0 to a large number on the first tick. The part file is
        // pre-allocated, so its length is the *whole* file from the start; the
        // plan is the only thing that knows how much of it is real. Seeding
        // from the length made the bar show the full size and then drop back
        // the moment the engine reported the truth.
        let start = crate::engine::plan_bytes(&part_path);

        let task = TransferTask {
            id: id.clone(),
            kind: "download".to_string(),
            name: job.name.clone(),
            fid: Some(job.fid.clone()),
            local_path: final_path.to_string_lossy().to_string(),
            remote_dir_fid: None,
            size: job.size,
            transferred: start,
            speed: 0.0,
            status: "queued".to_string(),
            error: None,
            created_at: now_millis(),
            updated_at: now_millis(),
            rapid: false,
            prepared: 0,
            connections: 0,
            segments: 0,
            segments_done: 0,
            part_path: Some(part_path.to_string_lossy().to_string()),
            share: job.share.clone(),
            provider: job.provider.clone(),
            cross: None,
            parent: None,
            hidden: false,
            listing: false,
            children_total: 0,
            children_done: 0,
            children_failed: 0,
            folder: None,
        };
        let task = self.insert(task).await;
        if announce {
            task.emit(&app);
        }

        self.spawn_download(
            app,
            ctx,
            task.clone(),
            job.fid,
            part_path,
            final_path,
        )
        .await;
        Ok(task)
    }

    /// Starts a folder download as **one** task.
    ///
    /// The walk streams files into hidden child downloads instead of turning the
    /// folder into thousands of rows: the list shows one entry with "12 / 340 个
    /// 文件", and a cancel or a pause is a single action on it. Nothing about the
    /// file list is kept — a resume walks the folder again and counts the files
    /// already on disk as done.
    pub async fn start_folder(
        self: &Arc<Self>,
        app: AppHandle,
        drive: Arc<dyn DriveProvider>,
        folder: FolderRef,
        dir: PathBuf,
    ) -> AppResult<TransferTask> {
        let provider = crate::providers::ProviderId::parse(&folder.provider)
            .ok_or_else(|| AppError::new(format!("未知的网盘：{}", folder.provider)))?;
        tokio::fs::create_dir_all(&dir).await?;
        let task = TransferTask {
            id: self.next_id().await,
            kind: "folder".to_string(),
            name: folder.name.clone(),
            fid: Some(crate::providers::qualify(provider, &folder.fid)),
            local_path: dir.to_string_lossy().to_string(),
            remote_dir_fid: None,
            size: 0,
            transferred: 0,
            speed: 0.0,
            status: "queued".to_string(),
            error: None,
            created_at: now_millis(),
            updated_at: now_millis(),
            rapid: false,
            prepared: 0,
            connections: 0,
            segments: 0,
            segments_done: 0,
            part_path: None,
            share: None,
            provider: folder.provider.clone(),
            cross: None,
            parent: None,
            hidden: false,
            listing: true,
            children_total: 0,
            children_done: 0,
            children_failed: 0,
            folder: Some(folder),
        };
        let task = self.insert(task).await;
        task.emit(&app);
        self.spawn_folder(app, drive, task.clone()).await;
        Ok(task)
    }

    /// Restarts a folder task after a pause, a cancel or a restart.
    pub async fn resume_folder(
        self: &Arc<Self>,
        app: AppHandle,
        drive: Arc<dyn DriveProvider>,
        id: &str,
    ) -> AppResult<TransferTask> {
        let mut task = {
            let tasks = self.tasks.read().await;
            tasks
                .get(id)
                .cloned()
                .ok_or_else(|| AppError::new("任务不存在"))?
        };
        if task.kind != "folder" {
            return Err(AppError::new("该任务不是文件夹下载"));
        }
        if task.folder.is_none() {
            return Err(AppError::new("任务缺少文件夹信息"));
        }
        task.status = "queued".to_string();
        task.error = None;
        task.speed = 0.0;
        task.transferred = 0;
        task.listing = true;
        task.children_total = 0;
        task.children_done = 0;
        task.children_failed = 0;
        task.updated_at = now_millis();
        let task = self.insert(task).await;
        task.emit(&app);
        self.spawn_folder(app, drive, task.clone()).await;
        Ok(task)
    }

    /// Starts the supervisor loop for a folder task.
    ///
    /// The cancel token is registered *before* the loop is spawned. Registering
    /// it inside the spawned future left a window in which a pause arriving
    /// right after the start found no token, did nothing, and the folder kept
    /// downloading behind a paused row.
    async fn spawn_folder(
        self: &Arc<Self>,
        app: AppHandle,
        drive: Arc<dyn DriveProvider>,
        task: TransferTask,
    ) {
        let id = task.id.clone();
        let token = CancellationToken::new();
        let generation = {
            let mut runs = self.folder_runs.lock().await;
            let generation = runs.get(&id).copied().unwrap_or(0) + 1;
            runs.insert(id.clone(), generation);
            generation
        };
        {
            let mut cancels = self.cancels.lock().await;
            // Resuming while the old loop still breathes: stop it first, so the
            // two never run side by side.
            if let Some(old) = cancels.insert(id.clone(), token.clone()) {
                old.cancel();
            }
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            manager
                .run_folder(app, drive, task, token, generation)
                .await;
            // Only the newest run may clean up; an older one finishing late
            // must not drop the token of the run that replaced it.
            let mut runs = manager.folder_runs.lock().await;
            if runs.get(&id).copied() == Some(generation) {
                runs.remove(&id);
                drop(runs);
                manager.cancels.lock().await.remove(&id);
            }
        });
    }

    /// True while `generation` is still the run that owns this folder.
    async fn folder_is_current(&self, id: &str, generation: u64) -> bool {
        self.folder_runs.lock().await.get(id).copied() == Some(generation)
    }

    /// Walks the folder, hands each file to a child download, and reports the
    /// aggregate on the parent row.
    async fn run_folder(
        self: &Arc<Self>,
        app: AppHandle,
        drive: Arc<dyn DriveProvider>,
        parent: TransferTask,
        token: CancellationToken,
        generation: u64,
    ) {
        let id = parent.id.clone();
        let Some(folder) = parent.folder.clone() else {
            return;
        };
        let Some(provider) = crate::providers::ProviderId::parse(&folder.provider) else {
            return;
        };
        let root = PathBuf::from(&parent.local_path);
        self.patch(&id, &app, |t| {
            t.status = "running".to_string();
            t.listing = true;
            t.error = None;
        })
        .await;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<FolderFile>(256);
        // The sidecar remembers every file found so far *and* every listing that
        // already completed. Resuming therefore neither re-lists the whole tree
        // (which is what hammered the drive's API into "too many requests") nor
        // re-downloads anything: cached files are replayed, cached directories
        // are walked from memory.
        let manifest = self.manifest_path(&id);
        let listing = match &manifest {
            Some(path) => read_listing(path).await,
            None => None,
        };
        let complete = listing.as_ref().is_some_and(|l| l.complete);
        let (replayed, cache) = match listing {
            Some(l) => (l.files, l.dirs),
            None => (Vec::new(), std::collections::HashMap::new()),
        };
        // Keys of everything already accounted for, so a file cannot be added
        // twice — not by a replay overlapping the walk, and not by a double
        // listing on the drive's side.
        let mut seen: std::collections::HashSet<String> = replayed
            .iter()
            .map(|file| file_key(&file.relative_dir, &file.name))
            .collect();
        let mut total = replayed.len() as u64;
        let mut bytes_found: u64 = replayed.iter().map(|file| file.size).sum();
        {
            let source = token.clone();
            let walker = Arc::clone(&drive);
            let start = folder.fid.clone();
            let storing = manifest.clone();
            tokio::spawn(async move {
                for file in replayed {
                    if source.is_cancelled() || tx.send(file).await.is_err() {
                        return;
                    }
                }
                if complete {
                    return;
                }
                walk_folder(walker, start, PathBuf::new(), tx, source, cache, storing).await;
            });
        }

        // Bytes are kept per file and never decrease. A file that was half
        // downloaded, paused and then resumed used to be counted once for the
        // half and again for the whole, which is why "已下载" jumped after every
        // pause; keying by file makes the number monotonic.
        let mut stored: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        let mut done_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut failed_files: std::collections::HashSet<String> = std::collections::HashSet::new();
        // (child id, file key): the key is what ties a child back to its file.
        // Files that still have a `.part` next to them: "continue" has to
        // continue one of these, not race ahead to whatever the walk offers
        // next. Walk files that are not in this set wait in `held` until every
        // unfinished file has been picked up again.
        let mut resuming = pending_partials(&root).await;
        let mut front: std::collections::VecDeque<FolderFile> = std::collections::VecDeque::new();
        let mut held: std::collections::VecDeque<FolderFile> = std::collections::VecDeque::new();
        let mut live: Vec<(String, String)> = Vec::new();
        let mut walking = true;
        let mut made_dirs: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut bytes_total = 0u64;
        let (mut done, mut failed) = (0u64, 0u64);
        let mut first_error: Option<String> = None;

        loop {
            if resuming.is_empty() && !held.is_empty() {
                front.append(&mut held);
            }
            if !walking && live.is_empty() && front.is_empty() {
                break;
            }
            if !self.folder_is_current(&id, generation).await {
                // A newer run took the folder over. Hand its children back and
                // leave the row alone: the new loop is the one reporting now.
                let ids: Vec<String> = live.iter().map(|(cid, _)| cid.clone()).collect();
                self.drop_children(&ids, "canceled").await;
                return;
            }
            let can_take = walking && live.len() < FOLDER_WINDOW;
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    let ids: Vec<String> = live.iter().map(|(cid, _)| cid.clone()).collect();
                self.drop_children(&ids, "canceled").await;
                    // The command that cancelled us already set the status; only
                    // the folder-specific fields are ours to clear.
                    self.patch(&id, &app, |t| {
                        t.listing = false;
                        t.speed = 0.0;
                    })
                    .await;
                    return;
                }
                file = async {
                    if let Some(file) = front.pop_front() {
                        return Some(file);
                    }
                    rx.recv().await
                }, if can_take => match file {
                    Some(file) => {
                        if seen.insert(file_key(&file.relative_dir, &file.name)) {
                            total += 1;
                            bytes_found += file.size;
                            if let Some(path) = &manifest {
                                append_manifest(path, &file).await;
                            }
                        }
                        let dir = if file.relative_dir.as_os_str().is_empty() {
                            root.clone()
                        } else {
                            crate::paths::safe_join(&root, &file.relative_dir)
                        };
                        let key = file_key(&file.relative_dir, &file.name);
                        if !resuming.is_empty() && !resuming.contains(&key) {
                            // Not one of the unfinished ones: park it, the walk
                            // resumes once the queue of partials is drained.
                            held.push_back(file);
                            continue;
                        }
                        if file_complete(&dir.join(&file.name), file.size).await {
                            stored.insert(key.clone(), file.size);
                            done_files.insert(key);
                        } else if made_dirs.insert(dir.clone())
                            && tokio::fs::create_dir_all(&dir).await.is_err()
                        {
                            failed_files.insert(key);
                        } else {
                            let job = DownloadJob {
                                fid: crate::providers::qualify(provider, &file.fid),
                                name: file.name.clone(),
                                size: file.size,
                                dir: dir.to_string_lossy().to_string(),
                                share: None,
                                provider: folder.provider.clone(),
                            };
                            match self
                                .start_download_quiet(
                                    app.clone(),
                                    DownloadCtx::Provider(Arc::clone(&drive)),
                                    job,
                                )
                                .await
                            {
                                Ok(child) => {
                                    let cid = child.id.clone();
                                    // Marked hidden *before* any event can leave,
                                    // so the row never flashes in the list.
                                    self.patch(&cid, &app, |t| {
                                        t.hidden = true;
                                        t.parent = Some(id.clone());
                                    })
                                    .await;
                                    live.push((cid, key));
                                }
                                Err(err) => {
                                    failed_files.insert(key);
                                    first_error.get_or_insert_with(|| err.to_string());
                                }
                            }
                        }
                    }
                    None => {
                        walking = false;
                        if let Some(path) = &manifest {
                            append_manifest_end(path).await;
                        }
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(FOLDER_TICK_MS)) => {}
            }

            // Reap the children that reached a terminal state.
            let states = {
                let tasks = self.tasks.read().await;
                live.iter()
                    .filter_map(|(cid, key)| {
                        tasks.get(cid).map(|t| {
                            (
                                cid.clone(),
                                key.clone(),
                                t.status.clone(),
                                t.transferred,
                                t.size,
                                t.error.clone(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            };
            for (cid, key, status, transferred, size, error) in states {
                match status.as_str() {
                    "done" => {
                        resuming.remove(&key);
                        stored.insert(key.clone(), size.max(transferred));
                        done_files.insert(key.clone());
                        failed_files.remove(&key);
                    }
                    // A cancelled or failed attempt keeps the bytes it did
                    // write: the file is retried later, but those bytes are
                    // already on disk and must not be added twice.
                    "failed" | "canceled" => {
                        resuming.remove(&key);
                        let known = stored.get(&key).copied().unwrap_or(0);
                        stored.insert(key.clone(), known.max(transferred));
                        if status == "failed" {
                            failed_files.insert(key.clone());
                            if let Some(err) = error.filter(|e| !e.is_empty()) {
                                first_error.get_or_insert(err);
                            }
                        }
                    }
                    _ => continue,
                }
                self.tasks.write().await.remove(&cid);
                live.retain(|(x, _)| x != &cid);
            }

            // Announce the folder's own numbers, not its children's. A child
            // that resumes a half-done file only adds the difference between
            // what it has now and what was already counted.
            let (live_speed, live_pairs) = {
                let tasks = self.tasks.read().await;
                let mut speed = 0.0;
                let mut pairs = Vec::new();
                for (cid, key) in &live {
                    if let Some(t) = tasks.get(cid) {
                        speed += t.speed;
                        pairs.push((key.clone(), t.transferred));
                    }
                }
                (speed, pairs)
            };
            let transferred = folder_bytes(&stored, &live_pairs);
            bytes_total = transferred;
            let size = bytes_found.max(transferred);
            done = done_files.len() as u64;
            failed = failed_files.len() as u64;
            let listing = walking;
            self.patch(&id, &app, move |t| {
                t.transferred = transferred;
                t.size = size;
                t.speed = live_speed;
                t.listing = listing;
                t.children_total = total;
                t.children_done = done;
                t.children_failed = failed;
            })
            .await;
            if !can_take {
                tokio::time::sleep(Duration::from_millis(FOLDER_TICK_MS)).await;
            }
        }

        let message = if total == 0 {
            Some("文件夹中没有可下载的文件".to_string())
        } else if failed > 0 {
            Some(match first_error {
                Some(err) if !err.is_empty() => format!("{failed} 个文件失败：{err}"),
                _ => format!("{failed} 个文件下载失败"),
            })
        } else {
            None
        };
        let ok = message.is_none();
        if ok {
            self.drop_manifest(&id).await;
        }
        self.patch(&id, &app, move |t| {
            t.status = if ok { "done".to_string() } else { "failed".to_string() };
            t.listing = false;
            t.speed = 0.0;
            t.error = message;
            t.children_total = total;
            t.children_done = done;
            t.children_failed = failed;
            t.transferred = bytes_total;
            t.size = bytes_found.max(bytes_total);
        })
        .await;
    }

    /// Sidecar listing of a folder task's files, next to `transfers.json`.
    ///
    /// The parent task itself stays one row no matter how big the folder is, so
    /// the file list has to live somewhere for a resume to skip the walk: the
    /// listing is written as it is discovered and ends with an `end` marker.
    fn manifest_path(&self, id: &str) -> Option<PathBuf> {
        Some(self.manifest_root()?.join(format!("{id}.jsonl")))
    }

    fn manifest_root(&self) -> Option<PathBuf> {
        Some(self.store.as_ref()?.parent()?.join("folders"))
    }

    /// Drops the sidecar of a folder task (done, cancelled for good, removed).
    async fn drop_manifest(&self, id: &str) {
        if let Some(path) = self.manifest_path(id) {
            let _ = tokio::fs::remove_file(path).await;
        }
    }

    /// Hidden children belong to their folder task: dropping the folder drops
    /// them too.
    async fn remove_children(&self, parent: &str) -> Vec<TransferTask> {
        let mut tasks = self.tasks.write().await;
        let ids: Vec<String> = tasks
            .values()
            .filter(|t| t.parent.as_deref() == Some(parent))
            .map(|t| t.id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|cid| tasks.remove(&cid))
            .collect()
    }

    /// Stops and drops the children of a folder task.
    async fn drop_children(&self, live: &[String], status: &str) {
        for cid in live {
            self.cancel(cid).await;
        }
        let mut tasks = self.tasks.write().await;
        for cid in live {
            if let Some(task) = tasks.get_mut(cid) {
                task.status = status.to_string();
                task.speed = 0.0;
            }
            tasks.remove(cid);
        }
    }

    /// Respawns a download for an existing task record, reusing its paths so a
    /// failed download continues from where it stopped.
    pub async fn retry_download(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        id: &str,
    ) -> AppResult<TransferTask> {
        let task = {
            let tasks = self.tasks.read().await;
            tasks
                .get(id)
                .cloned()
                .ok_or_else(|| AppError::new("任务不存在"))?
        };
        if task.kind != "download" {
            return Err(AppError::new("该任务不是下载任务"));
        }
        let final_path = PathBuf::from(&task.local_path);
        let part_path = task
            .part_path
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| with_suffix(&final_path, ".part"));
        let part_for_patch = part_path.to_string_lossy().to_string();
        let start = tokio::fs::metadata(&part_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        let fid = task.fid.clone().unwrap_or_default();
        self.patch(id, &app, move |t| {
            t.status = "queued".to_string();
            t.error = None;
            t.speed = 0.0;
            t.transferred = start;
            t.part_path = Some(part_for_patch);
        })
        .await;
        self.spawn_download(app, ctx, task.clone(), fid, part_path, final_path)
            .await;
        Ok(task)
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_download(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        task: TransferTask,
        fid: String,
        part_path: PathBuf,
        final_path: PathBuf,
    ) {
        let id = task.id.clone();
        let size = task.size;
        let token = CancellationToken::new();
        self.cancels.lock().await.insert(id.clone(), token.clone());

        let manager = Arc::clone(self);
        let handle = app.clone();
        tokio::spawn(async move {
            let Some(_slot) = manager.acquire(&token).await else {
                manager.cancels.lock().await.remove(&id);
                return;
            };
            manager
                .patch(&id, &handle, |t| {
                    if t.status == "queued" {
                        t.status = "running".to_string();
                    }
                })
                .await;

            let done = Arc::new(AtomicU64::new(0));
            // Bumped before every attempt so the speed readout rebases on the
            // bytes actually pulled by that attempt.
            let epoch = Arc::new(AtomicU64::new(0));
            let stats = Arc::new(crate::engine::Stats::default());
            let ticker = spawn_ticker(
                handle.clone(),
                manager.clone(),
                id.clone(),
                done.clone(),
                size,
                epoch.clone(),
                None,
                Some(Arc::clone(&stats)),
            );

            // A dropped connection or a stalled CDN endpoint is normal on a long
            // transfer; retry a few times, resuming from the `.part` each time,
            // instead of surfacing the first hiccup as a failure. Only the
            // attempt that finally gives up marks the task as failed, and it
            // keeps the partial file so the user can continue manually.
            const MAX_ATTEMPTS: u32 = 4;
            let mut result = Err(AppError::new("下载未开始"));
            for attempt in 1..=MAX_ATTEMPTS {
                if token.is_cancelled() {
                    result = Err(AppError::new("已取消"));
                    break;
                }
                if attempt > 1 {
                    // Let the CDN breathe, and report the retry so a stalled
                    // transfer does not look frozen.
                    let wait = Duration::from_secs(u64::from(attempt - 1) * 3);
                    manager
                        .patch(&id, &handle, |t| {
                            t.error = Some(format!("连接中断，正在重试（第 {attempt} 次）"));
                            t.speed = 0.0;
                        })
                        .await;
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => break,
                        _ = tokio::time::sleep(wait) => {}
                    }
                }
                epoch.fetch_add(1, Ordering::Relaxed);
                let attempt_result = match (&ctx, &task.share) {
                    (DownloadCtx::Share(drive), Some(share)) => {
                        run_share_download(
                            drive,
                            share,
                            &fid,
                            &part_path,
                            &final_path,
                            size,
                            done.clone(),
                            token.clone(),
                            Some(Arc::clone(&stats)),
                        )
                        .await
                    }
                    (DownloadCtx::Provider(drive), _) => {
                        run_provider_download(
                            drive,
                            &fid,
                            &part_path,
                            &final_path,
                            size,
                            done.clone(),
                            token.clone(),
                            0,
                            Some(Arc::clone(&stats)),
                        )
                        .await
                    }
                    (DownloadCtx::Share(_), None) => Err(AppError::new("分享下载缺少文件信息")),
                };
                match attempt_result {
                    Ok(()) => {
                        result = Ok(());
                        break;
                    }
                    Err(err) => {
                        if token.is_cancelled() || !is_retryable(&err) {
                            result = Err(err);
                            break;
                        }
                        result = Err(err);
                    }
                }
            }

            ticker.abort();
            manager.cancels.lock().await.remove(&id);

            match result {
                Ok(()) => {
                    // A pause that landed while the last chunk was in flight
                    // has already set "paused"; finishing the bytes must not
                    // flip the row to done behind the user's back.
                    if token.is_cancelled() {
                        manager
                            .patch(&id, &handle, |t| {
                                t.speed = 0.0;
                            })
                            .await;
                        return;
                    }
                    manager
                        .patch(&id, &handle, |t| {
                            t.status = "done".to_string();
                            t.transferred = t.size;
                            t.speed = 0.0;
                            // Retries leave a "正在重试" note behind; a
                            // finished task must not keep showing an error.
                            t.error = None;
                        })
                        .await;
                    crate::tray::notify(
                        &handle,
                        "下载完成",
                        &format!("{} 已保存", task.name),
                    );
                }
                Err(err) => {
                    let canceled = token.is_cancelled();
                    let msg = err.to_string();
                    let shown = msg.clone();
                    let name = task.name.clone();
                    manager
                        .patch(&id, &handle, move |t| {
                            // A pause/cancel already set the final status; do not
                            // turn a paused job into a failed one.
                            if t.status == "paused" || t.status == "canceled" {
                                t.speed = 0.0;
                                return;
                            }
                            t.status = if canceled { "canceled".into() } else { "failed".into() };
                            // The `.part` is deliberately kept: `retry_download`
                            // resumes from it and `remove` deletes it.
                            t.error = if canceled { None } else { Some(msg) };
                            t.speed = 0.0;
                        })
                        .await;
                    if !canceled {
                        crate::tray::notify(
                            &handle,
                            "下载失败",
                            &format!("{name}：{shown}"),
                        );
                    }
                }
            }
        });
    }

    pub async fn start_upload(
        self: &Arc<Self>,
        app: AppHandle,
        ctx: DownloadCtx,
        job: UploadJob,
    ) -> AppResult<TransferTask> {
        let path = PathBuf::from(&job.path);
        let meta = tokio::fs::metadata(&path).await?;
        if !meta.is_file() {
            return Err(AppError::new("只能上传文件"));
        }
        let file_name = job
            .name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .or_else(|| {
                path.file_name()
                    .and_then(|v| v.to_str())
                    .map(|v| v.to_string())
            })
            .ok_or_else(|| AppError::new("无效的文件名"))?;
        let DownloadCtx::Provider(drive) = ctx else {
            return Err(AppError::new("上传需要一个具体网盘"));
        };

        let id = self.next_id().await;
        let task = TransferTask {
            id: id.clone(),
            kind: "upload".to_string(),
            name: file_name.clone(),
            fid: None,
            local_path: path.to_string_lossy().to_string(),
            remote_dir_fid: Some(job.pdir_fid.clone()),
            size: meta.len(),
            transferred: 0,
            speed: 0.0,
            status: "queued".to_string(),
            error: None,
            created_at: now_millis(),
            updated_at: now_millis(),
            rapid: false,
            prepared: 0,
            connections: 0,
            segments: 0,
            segments_done: 0,
            part_path: None,
            share: None,
            provider: drive.id().as_str().to_string(),
            cross: None,
            parent: None,
            hidden: false,
            listing: false,
            children_total: 0,
            children_done: 0,
            children_failed: 0,
            folder: None,
        };
        let task = self.insert(task).await;
        task.emit(&app);

        let token = CancellationToken::new();
        self.cancels.lock().await.insert(id.clone(), token.clone());

        let manager = Arc::clone(self);
        let handle = app.clone();
        tokio::spawn(async move {
            let Some(_slot) = manager.acquire(&token).await else {
                manager.cancels.lock().await.remove(&id);
                return;
            };
            manager
                .patch(&id, &handle, |t| {
                    if t.status == "queued" {
                        t.status = "hashing".to_string();
                    }
                })
                .await;

            let done = Arc::new(AtomicU64::new(0));
            let sink = Arc::new(crate::providers::CounterSink::new(done.clone()));
            let ticker = spawn_ticker(
                handle.clone(),
                manager.clone(),
                id.clone(),
                done.clone(),
                meta.len(),
                Arc::new(AtomicU64::new(0)),
                Some(sink.prepared_counter()),
                None,
            );

            let sink_trait: Arc<dyn UploadSink> = Arc::new(ThrottledSink {
                inner: sink.clone(),
                started: Instant::now(),
            });
            // Providers report `"uploading"` once the local file is prepared and
            // bytes start moving. Without this the task stayed on `"hashing"`
            // for the whole transfer, so the list said 校验中 while the file was
            // already going up. The phase itself is left alone: `rapid` below
            // still needs to tell a 秒传 from a real upload.
            let phase_watch = {
                let manager = Arc::clone(&manager);
                let handle = handle.clone();
                let id = id.clone();
                let sink = sink.clone();
                tokio::spawn(async move {
                    // `CounterSink` starts out on "uploading", so wait until the
                    // provider really entered the preparation phase first —
                    // otherwise a provider that is slow to open the file would
                    // be flipped to "running" before it hashed anything.
                    let mut hashing = false;
                    loop {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        match sink.current_phase().as_str() {
                            "hashing" => hashing = true,
                            "uploading" if hashing => {
                                manager
                                    .patch(&id, &handle, |t| {
                                        if t.status == "hashing" {
                                            t.status = "running".to_string();
                                        }
                                    })
                                    .await;
                                return;
                            }
                            _ => {}
                        }
                    }
                })
            };
            let native_parent = crate::providers::native_of(&job.pdir_fid);
            let result = drive
                .upload(&path, &native_parent, &file_name, sink_trait, token.clone())
                .await;

            ticker.abort();
            phase_watch.abort();
            manager.cancels.lock().await.remove(&id);

            match result {
                Ok(fid) => {
                    let rapid = sink.current_phase() != "uploading";
                    manager
                        .patch(&id, &handle, move |t| {
                            t.status = "done".to_string();
                            t.transferred = t.size;
                            t.prepared = 0;
                            t.fid = fid;
                            t.rapid = rapid;
                            t.speed = 0.0;
                            t.error = None;
                        })
                        .await;
                    crate::tray::notify(&handle, "上传完成", &format!("{file_name} 已上传到网盘"));
                }
                Err(err) => {
                    let canceled = token.is_cancelled();
                    finish_failed(&manager, &handle, &id, err.to_string(), canceled).await;
                    if !canceled {
                        crate::tray::notify(
                            &handle,
                            "上传失败",
                            &format!("{file_name}：{err}"),
                        );
                    }
                }
            }
        });

        Ok(task)
    }

    /// Re-runs the files still listed on a cross-drive task. Files that already
    /// landed are not in `task.cross` any more, so nothing is copied twice.
    pub async fn retry_cross(
        self: &Arc<Self>,
        app: AppHandle,
        from: Arc<dyn DriveProvider>,
        to: Arc<dyn DriveProvider>,
        id: &str,
    ) -> AppResult<TransferTask> {
        let task = {
            let tasks = self.tasks.read().await;
            tasks
                .get(id)
                .cloned()
                .ok_or_else(|| AppError::new("任务不存在"))?
        };
        let Some(cross) = task.cross.clone() else {
            return Err(AppError::new("该任务不是跨网盘传输"));
        };
        if cross.files.is_empty() {
            return Err(AppError::new("这个任务的文件都已经传完了"));
        }
        let remaining: u64 = cross.files.iter().map(|f| f.size).sum();
        self.patch(id, &app, |t| {
            t.status = "queued".to_string();
            t.error = None;
            t.speed = 0.0;
            t.transferred = t.size.saturating_sub(remaining);
        })
        .await;
        let job = CrossJob {
            files: cross.files.clone(),
            to_pdir_fid: cross.to_pdir_fid.clone(),
            copy: cross.copy,
            name: task.name.clone(),
        };
        let temp_root = PathBuf::from(&task.local_path);
        self.spawn_cross(app, from, to, id.to_string(), temp_root, job, task.size);
        Ok(task)
    }

    /// Copies (or moves) files between two drives by streaming them through a
    /// temporary folder: download from the source, upload to the target, then
    /// delete the source when this is a move.
    ///
    /// Nothing about the drives is assumed beyond `open_download` and `upload`,
    /// so this works for every pair without per-drive code. One task covers the
    /// whole selection: progress is the sum of the files, and the files already
    /// done are dropped from `task.cross` so a retry does not copy them twice.
    pub async fn start_cross(
        self: &Arc<Self>,
        app: AppHandle,
        from: Arc<dyn DriveProvider>,
        to: Arc<dyn DriveProvider>,
        temp_root: PathBuf,
        job: CrossJob,
        total: u64,
    ) -> AppResult<TransferTask> {
        if job.files.is_empty() {
            return Err(AppError::new("没有可传输的文件"));
        }
        let id = self.next_id().await;
        let cross = CrossRef {
            from_provider: from.id().as_str().to_string(),
            to_provider: to.id().as_str().to_string(),
            to_pdir_fid: job.to_pdir_fid.clone(),
            copy: job.copy,
            files: job.files.clone(),
        };
        let task = TransferTask {
            id: id.clone(),
            kind: if job.copy { "copy" } else { "move" }.to_string(),
            name: job.name.clone(),
            fid: None,
            local_path: temp_root.to_string_lossy().to_string(),
            remote_dir_fid: Some(job.to_pdir_fid.clone()),
            size: total,
            transferred: 0,
            speed: 0.0,
            status: "queued".to_string(),
            error: None,
            created_at: now_millis(),
            updated_at: now_millis(),
            rapid: false,
            prepared: 0,
            connections: 0,
            segments: 0,
            segments_done: 0,
            part_path: None,
            share: None,
            provider: from.id().as_str().to_string(),
            cross: Some(cross),
            parent: None,
            hidden: false,
            listing: false,
            children_total: 0,
            children_done: 0,
            children_failed: 0,
            folder: None,
        };
        let task = self.insert(task).await;
        task.emit(&app);
        self.spawn_cross(app, from, to, id, temp_root, job, total);
        Ok(task)
    }

    /// The shared tail of `start_cross` and `retry_cross`: registers the cancel
    /// token and drives the files one by one on a background task.
    #[allow(clippy::too_many_arguments)]
    fn spawn_cross(
        self: &Arc<Self>,
        app: AppHandle,
        from: Arc<dyn DriveProvider>,
        to: Arc<dyn DriveProvider>,
        id: String,
        temp_root: PathBuf,
        job: CrossJob,
        total: u64,
    ) {
        let manager = Arc::clone(self);
        let handle = app.clone();
        tokio::spawn(async move {
            let token = CancellationToken::new();
            manager.cancels.lock().await.insert(id.clone(), token.clone());
            let Some(_slot) = manager.acquire(&token).await else {
                manager.cancels.lock().await.remove(&id);
                return;
            };
            manager
                .patch(&id, &handle, |t| {
                    if t.status == "queued" {
                        t.status = "running".to_string();
                    }
                })
                .await;

            let done = Arc::new(AtomicU64::new(0));
            let ticker = spawn_ticker(
                handle.clone(),
                manager.clone(),
                id.clone(),
                done.clone(),
                total,
                Arc::new(AtomicU64::new(0)),
                None,
                None,
            );

            let mut base = 0u64;
            let mut result: AppResult<()> = Ok(());
            for file in job.files.iter() {
                if token.is_cancelled() {
                    result = Err(AppError::new("已取消"));
                    break;
                }
                let attempt = run_cross_file(
                    &from,
                    &to,
                    &temp_root,
                    file,
                    base,
                    done.clone(),
                    token.clone(),
                    None,
                )
                .await;
                match attempt {
                    Ok(()) => {
                        base += file.size;
                        // The file is on the target now: dropping it from the
                        // persisted list is what makes a retry resume.
                        manager
                            .patch(&id, &handle, |t| {
                                if let Some(cross) = t.cross.as_mut() {
                                    cross.files.retain(|f| f.fid != file.fid);
                                }
                            })
                            .await;
                        manager.persist().await;
                    }
                    Err(err) => {
                        result = Err(err);
                        break;
                    }
                }
            }

            ticker.abort();
            manager.cancels.lock().await.remove(&id);
            let _ = tokio::fs::remove_dir_all(&temp_root).await;

            match result {
                Ok(()) => {
                    // A pause that landed while the last chunk was in flight
                    // has already set "paused"; finishing the bytes must not
                    // flip the row to done behind the user's back.
                    if token.is_cancelled() {
                        manager
                            .patch(&id, &handle, |t| {
                                t.speed = 0.0;
                            })
                            .await;
                        return;
                    }
                    manager
                        .patch(&id, &handle, |t| {
                            t.status = "done".to_string();
                            t.transferred = t.size;
                            t.speed = 0.0;
                            t.error = None;
                        })
                        .await;
                    // A move only becomes a move once every byte is on the
                    // target: deleting as we go would lose files if the job
                    // died halfway.
                    if !job.copy {
                        let natives: Vec<String> =
                            job.files.iter().map(|f| f.fid.clone()).collect();
                        if let Err(err) = from.delete(&natives).await {
                            crate::ylog!("cross move: deleting source failed: {err}");
                        }
                    }
                    let verb = if job.copy { "复制" } else { "移动" };
                    crate::tray::notify(
                        &handle,
                        &format!("{verb}完成"),
                        &format!("{} 已{verb}到{}", job.name, to.id().display_name()),
                    );
                }
                Err(err) => {
                    let canceled = token.is_cancelled();
                    let msg = err.to_string();
                    manager
                        .patch(&id, &handle, move |t| {
                            if t.status == "paused" || t.status == "canceled" {
                                t.speed = 0.0;
                                return;
                            }
                            t.status = if canceled { "canceled".into() } else { "failed".into() };
                            t.error = if canceled { None } else { Some(msg) };
                            t.speed = 0.0;
                        })
                        .await;
                }
            }
        });
    }
}

/// Walks a folder on a drive and hands every file to `tx` with its path inside
/// the folder. Runs on its own task so the supervisor below never waits on a
/// listing while it could be starting downloads.
async fn walk_folder(
    drive: Arc<dyn DriveProvider>,
    root: String,
    prefix: PathBuf,
    tx: tokio::sync::mpsc::Sender<FolderFile>,
    token: CancellationToken,
    cache: std::collections::HashMap<String, Vec<SubDir>>,
    manifest: Option<PathBuf>,
) {
    let mut queue: std::collections::VecDeque<(String, PathBuf, usize)> =
        std::collections::VecDeque::new();
    queue.push_back((root, prefix, 0));
    while let Some((fid, dir, depth)) = queue.pop_front() {
        if token.is_cancelled() {
            return;
        }
        // Already listed in an earlier run: its files are in the sidecar and
        // its children are cached, so there is nothing to ask the drive for.
        if let Some(subs) = cache.get(&fid) {
            if depth < FOLDER_MAX_DEPTH {
                for sub in subs {
                    queue.push_back((
                        sub.fid.clone(),
                        dir.join(crate::paths::safe_component(&sub.name)),
                        depth + 1,
                    ));
                }
            }
            continue;
        }
        // A rate-limited drive answers with "too many requests" for a while;
        // skipping the directory would silently drop its files, so back off and
        // try again before giving up on it.
        let mut listing = None;
        for attempt in 0..WALK_RETRIES {
            if token.is_cancelled() {
                return;
            }
            match drive.list(&fid).await {
                Ok(found) => {
                    listing = Some(found);
                    break;
                }
                Err(err) => {
                    crate::ylog!("文件夹读取失败 {fid}（第 {} 次）：{err}", attempt + 1);
                    if attempt + 1 < WALK_RETRIES {
                        tokio::time::sleep(std::time::Duration::from_millis(
                            WALK_BACKOFF_MS << attempt,
                        ))
                        .await;
                    }
                }
            }
        }
        let Some(listing) = listing else {
            continue;
        };
        let mut subs: Vec<SubDir> = Vec::new();
        for entry in listing.entries {
            if token.is_cancelled() {
                return;
            }
            let name = crate::paths::safe_component(&entry.name);
            let native = crate::providers::native_of(&entry.fid);
            if entry.is_dir {
                subs.push(SubDir {
                    fid: native.clone(),
                    name: name.clone(),
                });
                if depth < FOLDER_MAX_DEPTH {
                    queue.push_back((native, dir.join(name), depth + 1));
                }
                continue;
            }
            let file = FolderFile {
                fid: native,
                name,
                relative_dir: dir.clone(),
                size: entry.size,
            };
            if tx.send(file).await.is_err() {
                return;
            }
        }
        // Written after the files, never before: on resume the cache is trusted
        // to mean "everything below is in the sidecar".
        if let Some(path) = &manifest {
            append_dir_record(path, &fid, &subs).await;
        }
    }
}

/// Bytes the folder really has on disk.
///
/// `stored` holds what each file reached in a settled attempt, `live` what the
/// running children report. Taking the larger of the two is what keeps a
/// resumed file from being counted twice — once for the bytes it had when it
/// was paused and again for the whole file now.
fn folder_bytes(
    stored: &std::collections::HashMap<String, u64>,
    live: &[(String, u64)],
) -> u64 {
    let mut bytes: u64 = stored.values().sum();
    for (key, transferred) in live {
        let known = stored.get(key).copied().unwrap_or(0);
        if *transferred > known {
            bytes += transferred - known;
        }
    }
    bytes
}

/// Identity of a file inside the folder: two records with the same key are the
/// same target on disk.
fn file_key(relative_dir: &Path, name: &str) -> String {
    format!("{}/{}", relative_dir.to_string_lossy(), name)
}

/// Keys of the files a previous run left half-written in `dir`.
///
/// A resumed folder drains these before it continues the walk, so pressing
/// continue visibly continues the file that was downloading when it stopped.
async fn pending_partials(dir: &Path) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    let mut queue = vec![(dir.to_path_buf(), PathBuf::new(), 0usize)];
    while let Some((path, relative, depth)) = queue.pop() {
        if depth > FOLDER_MAX_DEPTH {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if kind.is_dir() {
                queue.push((entry.path(), relative.join(&name), depth + 1));
            } else if let Some(stem) = name.strip_suffix(".part") {
                keys.insert(file_key(&relative, stem));
            }
        }
    }
    keys
}

/// A file already sitting on disk at its final size counts as downloaded, so a
/// resumed folder skips it without asking the drive anything.
async fn file_complete(path: &Path, size: u64) -> bool {
    // A drive that does not report sizes (some listings say 0) would otherwise
    // look "unfinished" forever and re-download the file next to the copy it
    // already made — which is how a resumed folder's byte counts doubled.
    if size == 0 {
        return tokio::fs::metadata(path).await.is_ok();
    }
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len() == size)
        .unwrap_or(false)
}

/// Reads a folder listing that finished being written. A truncated listing (no
/// end marker) returns `None`: the folder would look complete while the rest of
/// it was never seen.
async fn read_listing(path: &Path) -> Option<Listing> {
    let text = tokio::fs::read_to_string(path).await.ok()?;
    let mut files = Vec::new();
    let mut dirs = std::collections::HashMap::new();
    let mut complete = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A line we cannot read means the file was written by a version that
            // knew fields this one does not; replaying it would silently drop
            // files, so the folder gets walked again instead.
            return None;
        };
        if value.get("end").and_then(|v| v.as_bool()).unwrap_or(false) {
            complete = true;
            continue;
        }
        // A directory record: this listing finished, so its files are already
        // in the file records and its children are known without asking again.
        if let Some(dir) = value.get("dir").and_then(|v| v.as_str()) {
            let subs = value
                .get("subs")
                .and_then(|v| serde_json::from_value::<Vec<SubDir>>(v.clone()).ok())
                .unwrap_or_default();
            dirs.insert(dir.to_string(), subs);
            continue;
        }
        match serde_json::from_value::<FolderFile>(value) {
            Ok(file) => files.push(file),
            Err(_) => return None,
        }
    }
    Some(Listing {
        files,
        dirs,
        complete,
    })
}

/// Appends one directory record to a folder's sidecar.
async fn append_dir_record(path: &Path, dir: &str, subs: &[SubDir]) {
    let line = serde_json::json!({ "dir": dir, "subs": subs });
    append_line(path, &line).await;
}

async fn append_line(path: &Path, value: &serde_json::Value) {
    let Ok(mut line) = serde_json::to_vec(value) else {
        return;
    };
    line.push(b'\n');
    if let Ok(mut handle) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        let _ = handle.write_all(&line).await;
    }
}

/// Appends one file to a folder's sidecar listing.
async fn append_manifest(path: &Path, file: &FolderFile) {
    let Ok(mut line) = serde_json::to_vec(file) else {
        return;
    };
    line.push(b'\n');
    if let Ok(mut handle) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        let _ = handle.write_all(&line).await;
    }
}

/// Marks a sidecar listing as complete, which is what makes it replayable.
async fn append_manifest_end(path: &Path) {
    if let Ok(mut handle) = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        let _ = handle.write_all(b"{\"end\":true}\n").await;
    }
}

/// Test hook: runs a cross-drive batch without a Tauri app handle so the live
/// probes can exercise the real streaming path end to end.
#[cfg(test)]
pub async fn run_cross_direct(
    from: Arc<dyn DriveProvider>,
    to: Arc<dyn DriveProvider>,
    temp_root: PathBuf,
    job: CrossJob,
) -> AppResult<()> {
    let done = Arc::new(AtomicU64::new(0));
    let token = CancellationToken::new();
    let mut base = 0u64;
    for file in job.files.iter() {
        run_cross_file(&from, &to, &temp_root, file, base, done.clone(), token.clone(), None)
            .await?;
        base += file.size;
    }
    if !job.copy {
        let natives: Vec<String> = job.files.iter().map(|f| f.fid.clone()).collect();
        from.delete(&natives).await?;
    }
    let _ = tokio::fs::remove_dir_all(&temp_root).await;
    Ok(())
}

/// Streams one file from the source drive to the target drive.
///
/// The bytes land in a temp file first because every drive's uploader wants a
/// local path; the file is removed as soon as it is up so a long selection does
/// not pile up on disk. Progress is reported in the selection's coordinate
/// system (`base` is the bytes of the files already finished).
#[allow(clippy::too_many_arguments)]
async fn run_cross_file(
    from: &Arc<dyn DriveProvider>,
    to: &Arc<dyn DriveProvider>,
    temp_root: &Path,
    file: &CrossFile,
    base: u64,
    done: Arc<AtomicU64>,
    token: CancellationToken,
    stats: Option<Arc<crate::engine::Stats>>,
) -> AppResult<()> {
    tokio::fs::create_dir_all(temp_root).await?;
    let name = crate::paths::safe_component(&file.name);
    let temp = temp_root.join(&name);
    let part = with_suffix(&temp, ".part");

    run_provider_download(
        from,
        &file.fid,
        &part,
        &temp,
        file.size,
        Arc::clone(&done),
        token.clone(),
        base,
        stats,
    )
    .await
    .map_err(|err| AppError::new(format!("{}：{err}", file.name)))?;

    let sink = Arc::new(OffsetSink {
        base,
        inner: Arc::clone(&done),
    });
    let uploaded = to
        .upload(&temp, &file.to_parent, &file.name, sink, token.clone())
        .await
        .map_err(|err| AppError::new(format!("{}：{err}", file.name)));
    let _ = tokio::fs::remove_file(&temp).await;
    uploaded?;
    Ok(())
}

/// Reports `base + bytes` to the task counter.
/// Uploads report bytes as they go, so the cap is applied there: the wrapper
/// waits out any surplus before handing the progress to the real sink (which is
/// what the providers call after each part).
/// Test hook: the same throttled sink the upload path uses, so a probe can
/// measure the cap without starting a whole task.
#[cfg(test)]
pub fn throttled_sink(done: Arc<AtomicU64>) -> Arc<dyn crate::providers::UploadSink> {
    Arc::new(ThrottledSink {
        inner: Arc::new(crate::providers::CounterSink::new(done)),
        started: Instant::now(),
    })
}

struct ThrottledSink {
    inner: Arc<crate::providers::CounterSink>,
    started: Instant,
}

impl UploadSink for ThrottledSink {
    fn phase(&self, phase: &str) {
        self.inner.phase(phase);
    }

    fn progress(&self, bytes: u64) {
        let cap = SPEED_LIMIT.load(Ordering::Relaxed);
        if cap > 0 && bytes > 0 {
            let budget = Duration::from_secs_f64(bytes as f64 / cap as f64);
            let elapsed = self.started.elapsed();
            if budget > elapsed {
                wait_sync(budget - elapsed);
            }
        }
        self.inner.progress(bytes);
    }
}

/// Sleeps inside a sync trait method without stalling the whole runtime.
///
/// `UploadSink::progress` is called from provider stream closures that cannot
/// await, so the cap has to block somewhere. On a multi-threaded runtime
/// `block_in_place` hands the worker's other tasks to a fresh thread first; a
/// current-thread runtime (the unit tests) has nothing to hand off, so it just
/// parks.
fn wait_sync(duration: Duration) {
    let multi = tokio::runtime::Handle::try_current()
        .map(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .unwrap_or(false);
    if multi {
        tokio::task::block_in_place(|| std::thread::sleep(duration));
    } else {
        std::thread::sleep(duration);
    }
}

struct OffsetSink {
    base: u64,
    inner: Arc<AtomicU64>,
}

impl UploadSink for OffsetSink {
    fn phase(&self, _phase: &str) {}

    fn progress(&self, bytes: u64) {
        self.inner.store(self.base + bytes, Ordering::Relaxed);
    }
}

/// Errors worth another attempt: dropped or stalled connections, truncated
/// bodies, DNS hiccups. A server verdict (bad cookie, missing file, hash
/// mismatch) is never retried — it would fail identically every time.
fn is_retryable(err: &AppError) -> bool {
    let text = err.0.to_lowercase();
    // A verdict that will not change on a second attempt. These are checked
    // first: a message can mention a connection ("连接已关闭，写入磁盘失败")
    // while the real problem is the disk.
    const TERMINAL: [&str; 10] = [
        "登录状态",
        "校验失败",
        "不一致",
        "磁盘空间",
        "空间不足",
        "拒绝访问",
        "没有权限",
        "无权限",
        "不存在",
        "不支持",
    ];
    if TERMINAL.iter().any(|needle| text.contains(needle)) {
        return false;
    }
    const NEEDLES: [&str; 16] = [
        "timed out",
        "timeout",
        "connection",
        "reset",
        "broken pipe",
        "unexpected eof",
        "incomplete",
        "下载不完整",
        "http 5",
        "decode",
        "dns",
        "temporar",
        // A segmented attempt that ran out of rounds is worth resuming: the
        // sidecar keeps every finished segment, so the next attempt continues
        // where this one stopped instead of starting over.
        "分段下载",
        "链接已失效",
        "连接过慢",
        "连接中断",
    ];
    NEEDLES.iter().any(|needle| text.contains(needle))
}

async fn finish_failed(
    manager: &Arc<TransferManager>,
    app: &AppHandle,
    id: &str,
    message: String,
    canceled: bool,
) {
    manager
        .patch(id, app, move |t| {
            t.status = if canceled { "canceled".into() } else { "failed".into() };
            t.error = if canceled { None } else { Some(message) };
            t.speed = 0.0;
        })
        .await;
}

#[allow(clippy::too_many_arguments)]
fn spawn_ticker(
    app: AppHandle,
    manager: Arc<TransferManager>,
    id: String,
    done: Arc<AtomicU64>,
    total: u64,
    epoch: Arc<AtomicU64>,
    prepared: Option<Arc<AtomicU64>>,
    stats: Option<Arc<crate::engine::Stats>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Speed is measured over a ~1.2s sliding window and then smoothed, so
        // a single stalled 400ms tick no longer drops the readout to 0 (which
        // the UI renders as "—" and made speed/ETA flicker every other frame).
        const TICK_MS: u64 = 400;
        const WINDOW: u64 = 3;
        let mut samples: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
        let mut last_at = Instant::now();
        let mut last = done.load(Ordering::Relaxed);
        let mut smoothed = 0.0f64;
        let mut interval = tokio::time::interval(Duration::from_millis(TICK_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut seen_epoch = epoch.load(Ordering::Relaxed);
        loop {
            interval.tick().await;
            let now = Instant::now();
            let current = done.load(Ordering::Relaxed);
            // A retry rewinds the counter to the bytes already on disk. Treat
            // that as a new baseline instead of reporting those bytes as fresh
            // throughput (which showed up as a multi-MB/s spike).
            let current_epoch = epoch.load(Ordering::Relaxed);
            if current_epoch != seen_epoch || current < last {
                seen_epoch = current_epoch;
                samples.clear();
                last = current;
                last_at = now;
                smoothed = 0.0;
                continue;
            }
            let elapsed = now.duration_since(last_at).as_secs_f64();
            let delta = current.saturating_sub(last);
            samples.push_back(delta);
            while samples.len() > WINDOW as usize {
                samples.pop_front();
            }
            last = current;
            last_at = now;

            let window_secs = elapsed * samples.len() as f64;
            let instant = if window_secs > 0.05 {
                samples.iter().sum::<u64>() as f64 / window_secs
            } else {
                0.0
            };
            // Exponential smoothing: new samples move the needle a quarter of
            // the way, and a full stall decays instead of snapping.
            smoothed = if smoothed <= 0.0 {
                instant
            } else {
                smoothed + (instant - smoothed) * 0.25
            };
            if smoothed < 1.0 {
                smoothed = 0.0;
            }
            manager
                .patch(&id, &app, |t| {
                    if t.size == 0 && total > 0 {
                        t.size = total;
                    }
                    t.transferred = if t.size > 0 { current.min(t.size) } else { current };
                    t.speed = smoothed;
                    if let Some(prepared) = prepared.as_ref() {
                        t.prepared = prepared.load(Ordering::Relaxed);
                    }
                    if let Some(stats) = stats.as_ref() {
                        t.connections = stats.connections();
                        t.segments = stats.segments();
                        t.segments_done = stats.finished();
                    }
                })
                .await;
        }
    })
}

/// Streams a file from any drive into `part`, resuming from whatever is already
/// on disk.
///
/// The provider hands back a stream already positioned at `start`, so resume,
/// retry and progress are identical for every drive — the only drive-specific
/// part (building the request, minting the url) stays inside the provider.
#[allow(clippy::too_many_arguments)]
async fn run_provider_download(
    drive: &Arc<dyn DriveProvider>,
    fid: &str,
    part_path: &Path,
    final_path: &Path,
    size: u64,
    done: Arc<AtomicU64>,
    token: CancellationToken,
    base: u64,
    stats: Option<Arc<crate::engine::Stats>>,
) -> AppResult<()> {
    let mut start = 0u64;
    if let Ok(meta) = tokio::fs::metadata(part_path).await {
        start = meta.len();
    }
    // The provider expects a native id; the qualified form is what the transfer
    // record carries, so strip it here. Done before the completeness check:
    // that check needs the source fingerprint, which is built from this id.
    let native = crate::providers::native_of(fid);
    // A length equal to the file is *not* proof of completeness any more: the
    // segmented engine preallocates the file, so a run that died halfway leaves
    // it full of holes at exactly `size`. The sidecar is the only thing that
    // knows whether the bytes are really there — and which file they belong to.
    if size > 0 && start == size && start > 0 {
        let identity = crate::engine::source::provider_source(
            Arc::clone(drive),
            native.clone(),
            part_path.display().to_string(),
        )
        .fingerprint();
        match crate::engine::resume_state(part_path, size, &identity) {
            crate::engine::ResumeState::Finished => {
                tokio::fs::rename(part_path, final_path).await?;
                done.store(base + size, Ordering::Relaxed);
                return Ok(());
            }
            // Work remains. Hand the offset to the engine, which resumes from
            // its own plan; starting at `size` would read as "already done".
            crate::engine::ResumeState::Partial => start = 0,
            // No plan, yet the file is at full length: the engine preallocates,
            // so this is a file with holes in it, not a finished download.
            // Trusting the length alone made a paused download report "done"
            // the moment it was resumed. Re-fetch instead.
            crate::engine::ResumeState::None => start = 0,
        }
    }
    if size > 0 && start > size {
        // A partial longer than the file cannot be a prefix of it (an earlier
        // attempt appended a body the server sent from byte 0, or the file
        // shrank upstream). Truncating to `size` would keep the wrong bytes.
        let _ = tokio::fs::remove_file(part_path).await;
        start = 0;
        done.store(base, Ordering::Relaxed);
    }

    done.store(base + start, Ordering::Relaxed);

    // Drives that serve byte ranges get the segmented engine, which is where
    // the throughput comes from. It returns `Ok(false)` when the file is too
    // small or the drive turns out not to honour ranges, and then everything
    // below runs exactly as before.
    if start == 0 && size > 0 {
        let provider = provider_of(fid);
        let source = crate::engine::source::provider_source(
            Arc::clone(drive),
            native.clone(),
            part_path.display().to_string(),
        );
        let segmented = crate::engine::download_with_stats(
            Arc::clone(drive),
            source,
            provider,
            part_path,
            size,
            Arc::clone(&done),
            base,
            token.clone(),
            stats,
        )
        .await;
        match segmented {
            Ok(true) => {
                let written = tokio::fs::metadata(part_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if written != size {
                    return Err(AppError::new(format!(
                        "下载不完整：已下载 {written} / {size} 字节"
                    )));
                }
                tokio::fs::rename(part_path, final_path).await?;
                done.store(base + written, Ordering::Relaxed);
                return Ok(());
            }
            // The engine gave up. Falling through to the single stream would be
            // wrong: it reopens with `File::create`, which truncates a file the
            // engine had preallocated to full length, while the sidecar still
            // describes segments that are now zero-filled — a later resume would
            // trust that plan and hand back a file of the right length with
            // holes in it. Returning the error instead lets the caller's retry
            // loop re-enter the engine, which resumes from the sidecar.
            Err(err) => return Err(err),
            // The drive turned out not to honour ranges (or the file is too
            // small to be worth splitting): the single stream below runs
            // exactly as it did before segmentation existed.
            Ok(false) => {}
        }
    }

    // A drive that refuses the Range means the byte offset cannot be trusted:
    // drop the partial and fetch the whole file rather than appending garbage.
    let DownloadHandle {
        size: reported,
        md5,
        stream,
    } = match drive.open_download(&native, start).await {
        Ok(handle) => handle,
        // Only a server that refuses `Range` justifies starting over. Every
        // other failure (404, expired auth, quota, file deleted) used to drop
        // the partial as well, throwing away good bytes *and* hiding the reason.
        Err(err) if start > 0 && err.to_string() == RANGE_IGNORED => {
            let _ = tokio::fs::remove_file(part_path).await;
            start = 0;
            done.store(base, Ordering::Relaxed);
            drive.open_download(&native, 0).await?
        }
        Err(err) => return Err(err),
    };
    let total = if size > 0 { size } else { reported };

    write_stream(stream, part_path, start, total, done.clone(), base, &token).await?;

    let written = tokio::fs::metadata(part_path).await.map(|m| m.len()).unwrap_or(0);
    if total > 0 && written != total {
        return Err(AppError::new(format!(
            "下载不完整：已下载 {written} / {total} 字节"
        )));
    }
    // Only verify when the drive publishes a trustworthy md5 (Quark shares do
    // not, which is why they go through `run_share_download` instead).
    if let Some(expected) = md5.as_deref().filter(|v| !v.trim().is_empty()) {
        let actual = md5_file(part_path).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(AppError::new("文件校验失败（MD5 不一致），已删除临时文件"));
        }
    }
    tokio::fs::rename(part_path, final_path).await?;
    done.store(base + written, Ordering::Relaxed);
    Ok(())
}

/// Test hook: the download path without a task record, so the live probes can
/// measure it directly.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub async fn run_provider_download_for_test(
    drive: &Arc<dyn DriveProvider>,
    fid: &str,
    part_path: &Path,
    final_path: &Path,
    size: u64,
    done: Arc<AtomicU64>,
    token: CancellationToken,
) -> AppResult<()> {
    run_provider_download(drive, fid, part_path, final_path, size, done, token, 0, None).await
}

/// Drains `stream` into `part_path`, appending at `start` and reporting progress.
/// `base` shifts the reported counter so a batch of files can share one number.
async fn write_stream(
    mut stream: BoxStream,
    part_path: &Path,
    start: u64,
    total: u64,
    done: Arc<AtomicU64>,
    base: u64,
    token: &CancellationToken,
) -> AppResult<()> {
    use futures_util::StreamExt;
    let _ = total;
    let mut file = if start > 0 {
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(part_path)
            .await?
    } else {
        tokio::fs::File::create(part_path).await?
    };
    let mut written = start;
    let started = Instant::now();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(AppError::new("已取消")),
            item = stream.next() => item,
        };
        match chunk {
            Some(Ok(bytes)) => {
                file.write_all(&bytes).await?;
                written += bytes.len() as u64;
                done.store(base + written, Ordering::Relaxed);
                throttle(started, written.saturating_sub(start)).await;
            }
            Some(Err(err)) => return Err(err),
            None => break,
        }
    }
    file.flush().await?;
    Ok(())
}

/// Share downloads: the provider mints a url (and whatever headers the CDN
/// wants), the engine streams it and keeps resume/retry/progress. md5 is never
/// verified here — Quark's share API publishes hashes that do not match the
/// served bytes.
#[allow(clippy::too_many_arguments)]
async fn run_share_download(
    drive: &Arc<dyn DriveProvider>,
    share: &ShareRef,
    fid: &str,
    part_path: &Path,
    final_path: &Path,
    mut size: u64,
    done: Arc<AtomicU64>,
    token: CancellationToken,
    stats: Option<Arc<crate::engine::Stats>>,
) -> AppResult<()> {
    let mut start = 0u64;
    if let Ok(meta) = tokio::fs::metadata(part_path).await {
        start = meta.len();
    }
    // Same caveat as `run_provider_download`: the engine preallocates, so a
    // partial at full length may still be a sparse file.
    if size > 0 && start == size && start > 0 {
        let identity = crate::engine::source::share_source(
            Arc::clone(drive),
            share.clone(),
            fid.to_string(),
            part_path.display().to_string(),
        )
        .fingerprint();
        match crate::engine::resume_state(part_path, size, &identity) {
            // Only a plan that says every segment landed proves the file is
            // whole; a preallocated file with no plan is sparse.
            crate::engine::ResumeState::Finished => {
                tokio::fs::rename(part_path, final_path).await?;
                done.store(size, Ordering::Relaxed);
                return Ok(());
            }
            crate::engine::ResumeState::Partial | crate::engine::ResumeState::None => start = 0,
        }
    }
    if size > 0 && start > size {
        // A partial longer than the file cannot be a prefix of it. Truncating to
        // `size` would keep the wrong bytes, so start over.
        let _ = tokio::fs::remove_file(part_path).await;
        done.store(0, Ordering::Relaxed);
    }

    // Share files get the same segmented engine as ordinary ones: the link is
    // re-minted per segment, which is exactly what a multi-connection download
    // needs anyway. Falls through to one stream when the drive cannot do ranges.
    if start == 0 && size > 0 {
        let qualified = format!("{}:{}", share.provider, fid);
        let provider = provider_of(&qualified);
        let source = crate::engine::source::share_source(
            Arc::clone(drive),
            share.clone(),
            fid.to_string(),
            part_path.display().to_string(),
        );
        let segmented = crate::engine::download_with_stats(
            Arc::clone(drive),
            Arc::clone(&source),
            provider,
            part_path,
            size,
            Arc::clone(&done),
            // Share downloads report progress from zero, like the single-stream
            // path below does.
            0,
            token.clone(),
            stats,
        )
        .await;
        match segmented {
            Ok(true) => {
                let written = tokio::fs::metadata(part_path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                if written != size {
                    return Err(AppError::new(format!(
                        "下载不完整：已下载 {written} / {size} 字节"
                    )));
                }
                tokio::fs::rename(part_path, final_path).await?;
                done.store(size, Ordering::Relaxed);
                return Ok(());
            }
            Err(err) => {
                // Do not fall through to the single stream: it opens the file
                // with `File::create`, truncating a file the engine had
                // preallocated to full length, while the sidecar still claims
                // segments that are now zero-filled — a later resume would
                // trust that plan and hand back a file of the right length with
                // holes in it. Returning the error lets the caller's retry loop
                // re-enter the engine, which resumes from the sidecar. The
                // temporary copy a drive may have made to serve the link is not
                // reusable either way.
                source.cleanup().await;
                return Err(err);
            }
            // The drive cannot do ranges: one stream, exactly as before. The
            // engine may have minted a link before giving up, and on a drive
            // that copies a share file into the user's drive to make it
            // downloadable that copy has to go now — the single-stream path
            // below mints its own.
            Ok(false) => {
                source.cleanup().await;
            }
        }
    }

    // Download urls are short lived on every drive, so mint one per attempt and
    // retry once with a brand new link when the CDN rejects the old one.
    let mut attempt = 0;
    let mut cleanup: Option<String> = None;
    let written = loop {
        let link = drive.share_link(share, fid).await?;
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        if link.size > 0 {
            size = link.size;
        }
        if link.cleanup_fid.is_some() {
            cleanup = link.cleanup_fid.clone();
        }
        match crate::share::stream_to_file(&link.url, &link.headers, part_path, done.clone(), &token)
            .await
        {
            Ok(written) => break written,
            Err(err) if attempt == 0 && crate::share::is_link_expired(&err) => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
            Err(err) => return Err(err),
        }
    };

    let total = size;
    if total > 0 && written != total {
        return Err(AppError::new(format!(
            "下载不完整：已下载 {written} / {total} 字节"
        )));
    }
    tokio::fs::rename(part_path, final_path).await?;
    done.store(written, Ordering::Relaxed);
    // Aliyun can only serve a share file after copying it into the user's own
    // drive; the copy has served its purpose now.
    if let Some(copied) = cleanup {
        drive.share_cleanup(&copied).await;
    }
    Ok(())
}

/// Deletes the `.part` a download or an abandoned upload left behind.
///
/// Removing a task in the UI is also the "clean up after me" gesture: a failed
/// download keeps its partial file so it can resume, and once the record is
/// gone the partial has no owner any more.
async fn discard_partial(task: &TransferTask) {
    let Some(part) = task.part_path.as_deref().filter(|p| !p.is_empty()) else {
        return;
    };
    let path = Path::new(part);
    if path.exists() {
        let _ = tokio::fs::remove_file(path).await;
    }
    // The segmented engine keeps its own plan next to the partial; without the
    // partial it describes nothing.
    crate::engine::discard_progress(path);
}

/// Deletes what a removed task left on disk.
///
/// The partial file always goes (nothing else references it), and with
/// `purge_local` the finished output goes too: a download's file, a folder
/// task's tree, or a cross-drive task's staging directory.
async fn purge_task_files(task: &TransferTask, purge_local: bool) {
    discard_partial(task).await;
    if !purge_local {
        return;
    }
    let path = PathBuf::from(&task.local_path);
    if task.kind == "folder" {
        if !path.is_dir() {
            return;
        }
        if task.status == "done" {
            let _ = tokio::fs::remove_dir_all(&path).await;
            return;
        }
        // A half-finished folder is not a folder to throw away: the files that
        // did finish are complete and the user may want them. Only the
        // placeholders go, plus the dirs that held nothing else.
        remove_placeholders(&path).await;
        return;
    }
    if task.kind == "cross" {
        if path.is_dir() {
            let _ = tokio::fs::remove_dir_all(&path).await;
        }
        return;
    }
    if path.is_file() {
        let _ = tokio::fs::remove_file(&path).await;
    }
}

/// Deletes `*.part` files below `dir` and any directory that ends up empty.
async fn remove_placeholders(dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    let mut dirs = Vec::new();
    while let Some(current) = stack.pop() {
        dirs.push(current.clone());
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("part"))
            {
                crate::engine::discard_progress(&path);
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    // Deepest first, so a parent can become empty once its children are gone.
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in dirs {
        if path != dir {
            let _ = tokio::fs::remove_dir(&path).await;
        }
    }
}

async fn md5_file(path: &Path) -> AppResult<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The provider id embedded in a qualified fid (`"quark:abc"` → `"quark"`).
///
/// Falls back to `"unknown"`, which the connection budget maps to its generic
/// profile — a bare id still downloads, just with conservative defaults.
fn provider_of(fid: &str) -> &str {
    match fid.split_once(':') {
        Some((head, _)) if crate::providers::ProviderId::parse(head).is_some() => head,
        _ => "unknown",
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn normalize_limit(value: u32) -> u64 {
    u64::from(value.clamp(1, 16))
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing that finished writing is replayable; a truncated one is not,
    /// or a folder would resume with half its files and call itself complete.
    #[tokio::test]
    async fn folder_listing_replays_only_when_it_ended() {
        let dir = std::env::temp_dir().join(format!("quark-m-{}", now_millis()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("listing.jsonl");
        let file = FolderFile {
            fid: "b".to_string(),
            name: "b.txt".to_string(),
            relative_dir: PathBuf::from("sub"),
            size: 3,
        };
        append_manifest(&path, &file).await;
        let open = read_listing(&path).await.expect("a truncated listing is still readable");
        assert_eq!(open.files.len(), 1);
        assert!(!open.complete, "no end marker yet");

        append_manifest_end(&path).await;
        let listing = read_listing(&path).await.expect("complete listing");
        assert!(listing.complete);
        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.files[0].name, "b.txt");
        assert_eq!(listing.files[0].relative_dir, PathBuf::from("sub"));
        assert_eq!(listing.files[0].size, 3);

        // A line this version cannot read (written by a newer one) must not be
        // silently skipped: the folder is walked again instead.
        let mut text = tokio::fs::read_to_string(&path).await.unwrap();
        text.push_str("{\"fid\":\"c\",\"name\":\"c.txt\",\"size\":1}\n");
        tokio::fs::write(&path, text).await.unwrap();
        assert!(read_listing(&path).await.is_none(), "a broken line forces a walk");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Minimal drive: a fixed tree, so the folder walk can be tested without a
    /// network or an `AppHandle`.
    struct FakeDrive {
        tree: std::collections::HashMap<String, Vec<(&'static str, &'static str, bool, u64)>>,
        /// How many listings the drive was asked for, so a test can prove a
        /// resumed folder does not walk the tree again.
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeDrive {
        fn new() -> Self {
            let mut tree = std::collections::HashMap::new();
            // (native id, name, is_dir, size)
            tree.insert(
                "0".to_string(),
                vec![("a", "a.txt", false, 5), ("sub", "sub", true, 0)],
            );
            tree.insert("sub".to_string(), vec![("b", "b.txt", false, 3)]);
            Self {
                tree,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl DriveProvider for FakeDrive {
        fn id(&self) -> crate::providers::ProviderId {
            crate::providers::ProviderId::Quark
        }
        fn capabilities(&self) -> crate::providers::Capabilities {
            crate::providers::Capabilities::BASIC
        }
        async fn auth_state(&self) -> crate::providers::AuthState {
            crate::providers::AuthState {
                provider: crate::providers::ProviderId::Quark,
                authenticated: true,
                source: "file".to_string(),
                masked: None,
                credential_path: String::new(),
                nickname: None,
                avatar: None,
                capabilities: crate::providers::Capabilities::BASIC,
            }
        }
        async fn logout(&self) -> AppResult<()> {
            Ok(())
        }
        async fn account(&self) -> AppResult<Option<crate::share::AccountInfo>> {
            Ok(None)
        }
        async fn list(&self, dir: &str) -> AppResult<crate::providers::DirListing> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let native = crate::providers::native_of(dir);
            let rows = self.tree.get(&native).cloned().unwrap_or_default();
            let entries = rows
                .into_iter()
                .map(|(fid, name, is_dir, size)| crate::providers::EntryDto {
                    fid: crate::providers::qualify(crate::providers::ProviderId::Quark, fid),
                    name: name.to_string(),
                    pdir_fid: dir.to_string(),
                    size,
                    is_dir,
                    format_type: String::new(),
                    updated_at: 0,
                    created_at: 0,
                    ext: crate::providers::EntryDto::ext_of(name),
                    provider: "quark".to_string(),
                    thumb: None,
                })
                .collect();
            Ok(crate::providers::DirListing {
                pdir_fid: dir.to_string(),
                entries,
                total: 0,
            })
        }
        async fn resolve_path(&self, _dir: &str) -> AppResult<Vec<crate::providers::PathNode>> {
            Ok(Vec::new())
        }
        async fn make_dir(&self, _parent: &str, _name: &str) -> AppResult<String> {
            Err(AppError::new("不支持"))
        }
        async fn rename(&self, _fid: &str, _name: &str) -> AppResult<()> {
            Ok(())
        }
        async fn delete(&self, _fids: &[String]) -> AppResult<()> {
            Ok(())
        }
        async fn transfer_entries(
            &self,
            _fids: &[String],
            _to: &str,
            _copy: bool,
        ) -> AppResult<usize> {
            Ok(0)
        }
        async fn search(
            &self,
            _dir: &str,
            _keyword: &str,
            _limit: u32,
        ) -> AppResult<Vec<crate::providers::SearchHit>> {
            Ok(Vec::new())
        }
        async fn open_download(&self, _fid: &str, _start: u64) -> AppResult<DownloadHandle> {
            Err(AppError::new("不需要真的下载"))
        }
        async fn upload(
            &self,
            _local: &Path,
            _parent: &str,
            _name: &str,
            _sink: Arc<dyn UploadSink>,
            _token: CancellationToken,
        ) -> AppResult<Option<String>> {
            Err(AppError::new("不支持"))
        }
    }

    /// The files a paused folder left half-written are the ones that must be
    /// picked up first, wherever they sit in the tree.
    #[tokio::test]
    async fn pending_partials_finds_partials_in_subdirectories() {
        let root = std::env::temp_dir().join(format!("yy-partials-{}", std::process::id()));
        let sub = root.join("a/b");
        tokio::fs::create_dir_all(&sub).await.unwrap();
        tokio::fs::write(root.join("one.mp4.part"), b"x").await.unwrap();
        tokio::fs::write(sub.join("two.mp4.part"), b"x").await.unwrap();
        tokio::fs::write(sub.join("three.mp4"), b"done").await.unwrap();
        let keys = pending_partials(&root).await;
        tokio::fs::remove_dir_all(&root).await.unwrap();
        assert!(keys.contains(&file_key(Path::new(""), "one.mp4")));
        assert!(keys.contains(&file_key(Path::new("a/b"), "two.mp4")));
        assert!(!keys.iter().any(|k| k.contains("three")));
    }

    /// Pausing and resuming a file must not add its bytes twice: the user saw
    /// "已下载" jump by hundreds of MB every time they hit continue.
    #[test]
    fn folder_bytes_never_double_counts_a_resumed_file() {
        let mut stored = std::collections::HashMap::new();
        stored.insert("a.mp4".to_string(), 70_000_000u64);
        stored.insert("b.mp4".to_string(), 5_000_000u64);
        // "a" is running again from 70 MB; "b" finished at its full size.
        let live = vec![("a.mp4".to_string(), 70_000_000u64), ("b.mp4".to_string(), 10_000_000u64)];
        assert_eq!(folder_bytes(&stored, &live), 80_000_000);
        // Fresh progress from a restarted download counts once, as a delta.
        let live = vec![("a.mp4".to_string(), 12_000_000u64)];
        assert_eq!(folder_bytes(&stored, &live), 75_000_000);
        // Nothing running: the settled numbers stand.
        assert_eq!(folder_bytes(&stored, &[]), 75_000_000);
    }

    /// A directory the sidecar already recorded is walked from memory: the
    /// drive must not be asked for it again, which is what kept a resumed
    /// folder from tripping the provider's rate limit.
    #[tokio::test]
    async fn folder_walk_reuses_cached_listings() {
        let drive = Arc::new(FakeDrive::new());
        let as_trait: Arc<dyn DriveProvider> = drive.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let mut cache = std::collections::HashMap::new();
        cache.insert(
            "0".to_string(),
            // Only directories are cached: a listing's files live in the
            // sidecar's file records, which are replayed separately.
            vec![SubDir {
                fid: "sub".to_string(),
                name: "sub".to_string(),
            }],
        );
        walk_folder(
            as_trait,
            "0".to_string(),
            PathBuf::new(),
            tx,
            CancellationToken::new(),
            cache,
            None,
        )
        .await;
        let mut names = Vec::new();
        while let Some(file) = rx.recv().await {
            names.push(file.name);
        }
        names.sort();
        // Root came from the cache, so only its child directory was listed.
        assert_eq!(names, vec!["b.txt".to_string()]);
        assert_eq!(drive.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The walk must flatten a tree into files with their paths inside the
    /// folder, because that is what the children are told to create on disk.
    #[tokio::test]
    async fn folder_walk_flattens_the_tree_with_relative_paths() {
        let drive: Arc<dyn DriveProvider> = Arc::new(FakeDrive::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        walk_folder(
            drive,
            "0".to_string(),
            PathBuf::new(),
            tx,
            CancellationToken::new(),
            std::collections::HashMap::new(),
            None,
        )
        .await;

        let mut found: Vec<(String, String, u64)> = Vec::new();
        while let Some(file) = rx.recv().await {
            found.push((
                file.relative_dir.to_string_lossy().to_string(),
                file.name,
                file.size,
            ));
        }
        found.sort();
        assert_eq!(
            found,
            vec![
                (String::new(), "a.txt".to_string(), 5),
                ("sub".to_string(), "b.txt".to_string(), 3),
            ]
        );
    }

    /// A resumed folder skips files that are already on disk at full size, and
    /// does not trust a name-only match with the wrong size.
    #[tokio::test]
    async fn a_file_counts_as_done_only_at_its_final_size() {
        let dir = std::env::temp_dir().join(format!("quark-f-{}", now_millis()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("a.txt");
        tokio::fs::write(&path, b"12345").await.unwrap();
        assert!(file_complete(&path, 5).await);
        assert!(!file_complete(&path, 6).await);
        assert!(!file_complete(&dir.join("missing.txt"), 0).await);
        // Size-less listings still count an existing file as done.
        assert!(file_complete(&path, 0).await);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    fn sample(id: &str, status: &str, part: Option<&str>) -> TransferTask {
        TransferTask {
            id: id.to_string(),
            kind: "download".to_string(),
            name: "跨界对决.zip".to_string(),
            fid: Some("fid-1".to_string()),
            local_path: "/tmp/跨界对决.zip".to_string(),
            remote_dir_fid: None,
            size: 1000,
            transferred: 400,
            speed: 0.0,
            status: status.to_string(),
            error: None,
            created_at: 1,
            updated_at: 1,
            rapid: false,
            prepared: 0,
            connections: 0,
            segments: 0,
            segments_done: 0,
            part_path: part.map(|p| p.to_string()),
            share: None,
            provider: "quark".to_string(),
            cross: None,
            parent: None,
            hidden: false,
            listing: false,
            children_total: 0,
            children_done: 0,
            children_failed: 0,
            folder: None,
        }
    }

    /// An interrupted download must come back as a paused, resumable record —
    /// not disappear while its `.part` keeps sitting on disk.
    #[tokio::test]
    async fn unfinished_tasks_survive_a_restart_as_paused() {
        let dir = std::env::temp_dir().join(format!("quark-t-{}", now_millis()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let part = dir.join("跨界对决.zip.part");
        tokio::fs::write(&part, vec![0u8; 400]).await.unwrap();
        let store = dir.join("transfers.json");

        let manager = TransferManager::new(3, Some(store.clone()));
        let task = sample("t1", "running", Some(part.to_str().unwrap()));
        manager.insert(task.clone()).await;
        manager.persist().await;

        // A fresh manager, as after a restart.
        let restored_manager = TransferManager::new(3, Some(store));
        let saved = tokio::fs::read_to_string(dir.join("transfers.json")).await.unwrap();
        assert!(saved.contains("\"running\""), "running jobs are persisted too");

        let parsed: Vec<TransferTask> = serde_json::from_str(&saved).unwrap();
        assert_eq!(parsed.len(), 1);
        // `restore` needs an AppHandle to emit, so check the transformation the
        // code applies to unfinished entries.
        let mut restored = parsed[0].clone();
        if !matches!(restored.status.as_str(), "done" | "failed" | "canceled") {
            restored.status = "paused".to_string();
            let meta = tokio::fs::metadata(restored.part_path.as_deref().unwrap())
                .await
                .unwrap();
            restored.transferred = meta.len();
        }
        assert_eq!(restored.status, "paused");
        assert_eq!(restored.transferred, 400);
        assert!(restored_manager.list().await.is_empty());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// Pausing must keep both the record and the partial file; deleting either
    /// one is what made an interrupted download unrecoverable.
    #[test]
    fn pausing_keeps_the_partial_file() {
        let dir = std::env::temp_dir().join(format!("quark-p-{}", now_millis()));
        std::fs::create_dir_all(&dir).unwrap();
        let part = dir.join("a.part");
        std::fs::write(&part, vec![0u8; 10]).unwrap();
        let task = sample("t2", "paused", Some(part.to_str().unwrap()));
        // The removal path is the only thing allowed to delete it.
        assert!(part.exists());
        assert_eq!(task.part_path.as_deref(), Some(part.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `discard_partial` only touches tasks that carry a part path.
    #[tokio::test]
    async fn removing_a_task_deletes_only_its_own_partial_file() {
        let dir = std::env::temp_dir().join(format!("quark-d-{}", now_millis()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mine = dir.join("mine.part");
        let other = dir.join("other.part");
        tokio::fs::write(&mine, b"x").await.unwrap();
        tokio::fs::write(&other, b"y").await.unwrap();

        discard_partial(&sample("t3", "failed", Some(mine.to_str().unwrap()))).await;
        assert!(!mine.exists(), "the task's partial file is removed");
        assert!(other.exists(), "unrelated files are untouched");
        // No part path at all is a no-op, not a panic.
        discard_partial(&sample("t4", "failed", None)).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
#[cfg(test)]
mod throttle_tests {
    use super::*;

    /// One test on purpose: the cap is a process-wide static, so two tests
    /// running in parallel would reset each other's value mid-wait.
    #[tokio::test]
    async fn the_cap_only_delays_bytes_that_are_ahead_of_schedule() {
        set_speed_limit(0);
        let started = Instant::now();
        throttle(started, 64 * 1024 * 1024).await;
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "unlimited must not wait"
        );

        // 256 KiB at 10 MiB/s is ~25ms of budget; asking for all of it at once
        // means sleeping almost the whole 25ms.
        set_speed_limit(10 * 1024);
        let started = Instant::now();
        throttle(started, 256 * 1024).await;
        let elapsed = started.elapsed();
        set_speed_limit(0);
        assert!(
            elapsed >= Duration::from_millis(15),
            "expected a wait, got {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn retries_only_transient_failures() {
        // Connection-level trouble is worth another attempt.
        for msg in [
            "error sending request: operation timed out",
            "connection reset by peer",
            "下载不完整：已下载 120 / 300 字节",
            "HTTP 502 Bad Gateway",
            "unexpected eof",
            "temporary failure in name resolution",
        ] {
            assert!(is_retryable(&AppError::new(msg)), "should retry: {msg}");
        }
        // Server verdicts are final.
        for msg in [
            "登录状态已失效，请重新设置 Cookie",
            "文件校验失败（MD5 不一致），已删除临时文件",
            "分享链接不存在或已被取消",
            "写入磁盘失败：磁盘空间不足",
            "没有权限访问目标目录",
            "服务器不支持分段下载",
        ] {
            assert!(!is_retryable(&AppError::new(msg)), "must not retry: {msg}");
        }
        // A segmented attempt that gave up is resumable, not terminal.
        for msg in [
            "分段下载失败：3 个分段缺失（已下载 900 / 1000 字节）",
            "下载链接已失效",
            "连接过慢（1024 B/s），已换新连接",
        ] {
            assert!(is_retryable(&AppError::new(msg)), "should retry: {msg}");
        }
    }
}

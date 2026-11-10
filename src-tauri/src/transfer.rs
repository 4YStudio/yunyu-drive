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
}

impl TransferTask {
    fn emit(&self, app: &AppHandle) {
        let _ = app.emit(EVENT_TRANSFER, self.clone());
    }
}

pub struct TransferManager {
    tasks: RwLock<HashMap<String, TransferTask>>,
    cancels: Mutex<HashMap<String, CancellationToken>>,
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
        if restored.is_empty() {
            return;
        }
        let mut tasks = self.tasks.write().await;
        for task in restored {
            tasks.insert(task.id.clone(), task.clone());
            task.emit(app);
        }
    }

    /// Writes the task list to disk (best effort, history + resumable jobs).
    async fn persist(&self) {
        let Some(path) = self.store.clone() else {
            return;
        };
        let snapshot: Vec<TransferTask> = {
            let tasks = self.tasks.read().await;
            let mut all: Vec<TransferTask> = tasks.values().cloned().collect();
            all.sort_by_key(|t| std::cmp::Reverse(t.created_at));
            all.truncate(100);
            all
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
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
    }

    pub async fn list(&self) -> Vec<TransferTask> {
        let tasks = self.tasks.read().await;
        let mut all: Vec<TransferTask> = tasks.values().cloned().collect();
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

    pub async fn remove(&self, id: &str, app: &AppHandle) -> AppResult<()> {
        // Only a live worker needs a grace period; sleeping unconditionally made
        // every removal feel sluggish and delayed deleting the partial file.
        if self.cancel(id).await {
            // Give the worker a beat to observe the cancellation and close its
            // file handle before the partial file is unlinked.
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let removed = self.tasks.write().await.remove(id);
        if let Some(task) = removed {
            discard_partial(&task).await;
        }
        let _ = app.emit(EVENT_REMOVED, id.to_string());
        self.persist().await;
        Ok(())
    }

    pub async fn clear_finished(&self, app: &AppHandle) -> AppResult<()> {
        let removed: Vec<TransferTask> = {
            let mut tasks = self.tasks.write().await;
            let keep = |t: &TransferTask| !matches!(t.status.as_str(), "done" | "failed" | "canceled");
            let removed: Vec<TransferTask> =
                tasks.values().filter(|t| !keep(t)).cloned().collect();
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
        let id = self.next_id().await;
        let target_dir = PathBuf::from(&job.dir);
        tokio::fs::create_dir_all(&target_dir).await?;
        // A leftover `.part` from an earlier attempt (same directory, same
        // name) means the file is half fetched: keep that name so the Range
        // resume lines up instead of writing a second copy next to it.
        let mut final_path = unique_path(&target_dir.join(&job.name));
        let mut part_path = with_suffix(&final_path, ".part");
        let resume_path = with_suffix(&target_dir.join(&job.name), ".part");
        if !final_path.exists() && resume_path.exists() {
            final_path = target_dir.join(&job.name);
            part_path = resume_path;
        }
        // Bytes already on disk count immediately, otherwise the progress bar
        // jumps from 0 to a large number on the first tick.
        let start = tokio::fs::metadata(&part_path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);

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
            part_path: Some(part_path.to_string_lossy().to_string()),
            share: job.share.clone(),
            provider: job.provider.clone(),
        };
        let task = self.insert(task).await;
        task.emit(&app);

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
            let ticker = spawn_ticker(
                handle.clone(),
                manager.clone(),
                id.clone(),
                done.clone(),
                size,
                epoch.clone(),
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
            part_path: None,
            share: None,
            provider: drive.id().as_str().to_string(),
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
            let ticker = spawn_ticker(
                handle.clone(),
                manager.clone(),
                id.clone(),
                done.clone(),
                meta.len(),
                Arc::new(AtomicU64::new(0)),
            );

            // The drive hashes and uploads in whatever way it needs (rapid
            // upload, chunks, presigned urls) and reports through the sink; the
            // manager only translates `phase` into the task status.
            let sink = Arc::new(crate::providers::CounterSink::new(done.clone()));
            let sink_trait: Arc<dyn UploadSink> = sink.clone();
            let native_parent = crate::providers::native_of(&job.pdir_fid);
            let result = drive
                .upload(&path, &native_parent, &file_name, sink_trait, token.clone())
                .await;

            ticker.abort();
            manager.cancels.lock().await.remove(&id);

            match result {
                Ok(fid) => {
                    let rapid = sink.phase() != "uploading";
                    manager
                        .patch(&id, &handle, move |t| {
                            t.status = "done".to_string();
                            t.transferred = t.size;
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
}

/// Errors worth another attempt: dropped or stalled connections, truncated
/// bodies, DNS hiccups. A server verdict (bad cookie, missing file, hash
/// mismatch) is never retried — it would fail identically every time.
fn is_retryable(err: &AppError) -> bool {
    let text = err.0.to_lowercase();
    if text.contains("登录状态") || text.contains("校验失败") || text.contains("不存在") {
        return false;
    }
    const NEEDLES: [&str; 12] = [
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

fn spawn_ticker(
    app: AppHandle,
    manager: Arc<TransferManager>,
    id: String,
    done: Arc<AtomicU64>,
    total: u64,
    epoch: Arc<AtomicU64>,
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
) -> AppResult<()> {
    let mut start = 0u64;
    if let Ok(meta) = tokio::fs::metadata(part_path).await {
        start = meta.len();
    }
    if size > 0 && start == size && start > 0 {
        // The partial already holds the whole file — finish it.
        tokio::fs::rename(part_path, final_path).await?;
        done.store(size, Ordering::Relaxed);
        return Ok(());
    }
    if size > 0 && start > size {
        // A partial longer than the file cannot be a prefix of it (an earlier
        // attempt appended a body the server sent from byte 0, or the file
        // shrank upstream). Truncating to `size` would keep the wrong bytes.
        let _ = tokio::fs::remove_file(part_path).await;
        start = 0;
        done.store(0, Ordering::Relaxed);
    }

    // The provider expects a native id; the qualified form is what the transfer
    // record carries, so strip it here.
    let native = crate::providers::native_of(fid);
    done.store(start, Ordering::Relaxed);

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
            done.store(0, Ordering::Relaxed);
            drive.open_download(&native, 0).await?
        }
        Err(err) => return Err(err),
    };
    let total = if size > 0 { size } else { reported };

    write_stream(stream, part_path, start, total, done.clone(), &token).await?;

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
    done.store(written, Ordering::Relaxed);
    Ok(())
}

/// Drains `stream` into `part_path`, appending at `start` and reporting progress.
async fn write_stream(
    mut stream: BoxStream,
    part_path: &Path,
    start: u64,
    total: u64,
    done: Arc<AtomicU64>,
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
                done.store(written, Ordering::Relaxed);
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
) -> AppResult<()> {
    let mut start = 0u64;
    if let Ok(meta) = tokio::fs::metadata(part_path).await {
        start = meta.len();
    }
    if size > 0 && start == size && start > 0 {
        // The partial already holds the whole file — finish it.
        tokio::fs::rename(part_path, final_path).await?;
        done.store(size, Ordering::Relaxed);
        return Ok(());
    }
    if size > 0 && start > size {
        // A partial longer than the file cannot be a prefix of it. Truncating to
        // `size` would keep the wrong bytes, so start over.
        let _ = tokio::fs::remove_file(part_path).await;
        done.store(0, Ordering::Relaxed);
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
            part_path: part.map(|p| p.to_string()),
            share: None,
            provider: "quark".to_string(),
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
        ] {
            assert!(!is_retryable(&AppError::new(msg)), "must not retry: {msg}");
        }
    }
}

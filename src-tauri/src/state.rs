use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::RwLock;

use libquarkpan::QuarkPan;

use crate::config::{AppPaths, Settings};
use crate::error::{AppError, AppResult};
use crate::providers::{
    AliyunProvider, BaiduProvider, DriveProvider, Pan123Provider, ProviderId, Providers,
    QuarkProvider, Yun139Provider,
};
use crate::transfer::TransferManager;

/// Set by the tray's "quit" item (and by SIGTERM handling) so the window
/// close handler knows this close is a real exit and not "hide to tray".
static EXITING: AtomicBool = AtomicBool::new(false);

pub fn request_exit() {
    EXITING.store(true, Ordering::SeqCst);
}

pub fn is_exiting() -> bool {
    EXITING.load(Ordering::SeqCst)
}

/// Application-wide state shared by all commands.
pub struct AppState {
    pub paths: AppPaths,
    pub cookie: RwLock<Option<String>>,
    pub client: RwLock<Option<QuarkPan>>,
    pub settings: RwLock<Settings>,
    pub transfers: Arc<TransferManager>,
    /// Nickname + avatar of the signed-in account, filled lazily.
    pub account: RwLock<Option<crate::share::AccountInfo>>,
    /// A `quark-drive://` link that arrived before the UI could listen.
    pub pending_link: RwLock<Option<String>>,
    /// Mirrors of the two settings the tray and the notification code need.
    ///
    /// Those callbacks run on the main thread AND inside tokio tasks, where
    /// `blocking_read()` on the settings lock panics ("Cannot block the current
    /// thread from within a runtime") and takes the whole app down. Atomics are
    /// read from both worlds safely.
    pub close_to_tray: AtomicBool,
    pub notifications: AtomicBool,
    /// Every drive, built lazily the first time one is used.
    ///
    /// `RwLock<Option<...>>` rather than a plain field because construction is
    /// async (each drive may refresh a token) and must not run during `setup`.
    pub providers: RwLock<Option<Arc<Providers>>>,
    /// In-flight 阿里云盘 QR login, kept between `generate` and `poll`.
    pub aliyun_qr: RwLock<Option<Arc<crate::providers::aliyun_qr::AliyunQrSession>>>,
}

impl AppState {
    /// `pending` is the share link that arrived on the command line, if any. It
    /// is filled at construction on purpose: taking the write lock for it later
    /// would mean `blocking_write()` on a runtime thread, which panics.
    pub fn with_pending_link(paths: AppPaths, pending: Option<String>) -> AppResult<Arc<Self>> {
        paths.ensure()?;
        let settings = crate::config::load_settings(&paths);
        let cookie = crate::config::read_cookie(&paths);
        let transfers = TransferManager::new(
            settings.concurrent_transfers,
            Some(paths.transfers_file()),
        );
        Ok(Arc::new(Self {
            paths,
            close_to_tray: AtomicBool::new(settings.close_to_tray),
            notifications: AtomicBool::new(settings.notifications),
            cookie: RwLock::new(cookie),
            client: RwLock::new(None),
            providers: RwLock::new(None),
            aliyun_qr: RwLock::new(None),
            settings: RwLock::new(settings),
            transfers,
            account: RwLock::new(None),
            pending_link: RwLock::new(pending),
        }))
    }

    pub async fn set_cookie(&self, cookie: String) -> AppResult<()> {
        let cookie = cookie.trim().to_string();
        if cookie.is_empty() {
            return Err(AppError::new("Cookie 不能为空"));
        }
        crate::config::write_cookie(&self.paths, &cookie)?;
        *self.cookie.write().await = Some(cookie);
        *self.client.write().await = None;
        self.invalidate_providers().await;
        Ok(())
    }

    /// The cookie string currently in use (env var or file).
    pub async fn cookie_string(&self) -> AppResult<String> {
        self.cookie
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::new("尚未登录：请先设置夸克 Cookie"))
    }

    /// Persists a cookie that was refreshed in place (e.g. a new `__puus`)
    /// without touching anything else, then rebuilds the api client.
    pub async fn persist_cookie(&self, cookie: String) -> AppResult<()> {
        let cookie = cookie.trim().to_string();
        if cookie.is_empty() {
            return Err(AppError::new("Cookie 不能为空"));
        }
        crate::config::write_cookie(&self.paths, &cookie)?;
        *self.cookie.write().await = Some(cookie);
        *self.client.write().await = None;
        self.invalidate_providers().await;
        Ok(())
    }

    pub async fn clear_cookie(&self) -> AppResult<()> {
        crate::config::clear_cookie(&self.paths)?;
        *self.cookie.write().await = None;
        *self.client.write().await = None;
        self.invalidate_providers().await;
        Ok(())
    }

    /// Builds (once) and returns every drive.
    ///
    /// Drives that cannot be constructed because they are not logged in are
    /// still registered, so the UI can show them as "未登录" instead of hiding
    /// them.
    pub async fn providers(&self) -> AppResult<Arc<Providers>> {
        if let Some(list) = self.providers.read().await.clone() {
            return Ok(list);
        }
        let mut list: Vec<Arc<dyn DriveProvider>> = Vec::new();

        // Quark: needs a cookie before it can talk to anything.
        if let Some(cookie) = self.cookie.read().await.clone() {
            if !cookie.trim().is_empty() {
                let base = self.settings.read().await.api_base_url.clone();
                let client = QuarkPan::builder()
                    .cookie(cookie.clone())
                    .api_base_url(if base.trim().is_empty() {
                        "https://drive.quark.cn".to_string()
                    } else {
                        base
                    })
                    .prepare()?;
                list.push(Arc::new(QuarkProvider::new(
                    client,
                    cookie,
                    self.paths.cookie_file().to_string_lossy().to_string(),
                )));
            }
        }

        // 139: needs its own token file; a missing one just means "not logged in".
        match Yun139Provider::load(&self.paths.config_dir) {
            Ok(drive) => list.push(Arc::new(drive)),
            Err(_) => { /* not logged in; the switcher shows a login hint */ }
        }

        // 123 云盘: account/password token stored in its own file.
        if let Ok(drive) = Pan123Provider::load(&self.paths.config_dir) {
            list.push(Arc::new(drive));
        }

        // 阿里云盘: OAuth token pair in its own file.
        if let Ok(drive) = AliyunProvider::load(&self.paths.config_dir) {
            list.push(Arc::new(drive));
        }

        // 百度网盘: a web session cookie lifted from the login webview.
        if let Ok(drive) = BaiduProvider::load(&self.paths.config_dir) {
            list.push(Arc::new(drive));
        }

        let providers = Arc::new(Providers::new(list));
        *self.providers.write().await = Some(Arc::clone(&providers));
        Ok(providers)
    }

    /// Resolves a qualified entry id (`"quark:abc"`) to its drive.
    pub async fn drive(&self, entry: &str) -> AppResult<(Arc<dyn DriveProvider>, String)> {
        self.providers().await?.resolve(entry)
    }

    /// Resolves just the drive that owns `provider`.
    pub async fn drive_of(&self, provider: ProviderId) -> AppResult<Arc<dyn DriveProvider>> {
        self.providers().await?.get(provider)
    }

    /// Drops the cached registry so the next call rebuilds it (after a login).
    pub async fn invalidate_providers(&self) {
        *self.providers.write().await = None;
    }

    pub async fn settings(&self) -> Settings {
        self.settings.read().await.clone()
    }

    /// True when closing the window should hide to the tray. Safe from any
    /// thread — the window handler calls it while the runtime may be active.
    pub fn close_to_tray(&self) -> bool {
        self.close_to_tray.load(Ordering::Relaxed)
    }

    /// True when desktop notifications are enabled. Safe from any thread.
    pub fn notifications_enabled(&self) -> bool {
        self.notifications.load(Ordering::Relaxed)
    }

    pub async fn update_settings(&self, next: Settings) -> AppResult<Settings> {
        {
            let mut guard = self.settings.write().await;
            *guard = next.clone();
        }
        self.close_to_tray.store(next.close_to_tray, Ordering::Relaxed);
        self.notifications
            .store(next.notifications, Ordering::Relaxed);
        crate::config::save_settings(&self.paths, &next)?;
        // The concurrency limit lives in the transfer manager so queued jobs
        // can pick it up without a restart.
        self.transfers.set_limit(next.concurrent_transfers);
        Ok(next)
    }
}

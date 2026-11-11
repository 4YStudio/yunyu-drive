//! IPC commands.
//!
//! Everything here is provider-agnostic: an entry id arrives as
//! `"<provider>:<native>"`, gets resolved to a [`DriveProvider`] and the call is
//! forwarded. The only drive-specific pieces left in this file are the Quark
//! cookie lives in `state.rs` and the share endpoints in `share.rs`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::config::{self, Settings};
use crate::error::{AppError, AppResult};
use crate::providers::aliyun_qr::QrPollResult;
use crate::providers::{
    native_of, qualify, AuthState, Capabilities, DirListing, DownloadRequest, DriveProvider,
    EntryDto, PathNode, ProviderId, QuotaInfo, SearchHit,
};
use crate::state::AppState;
use crate::transfer::{DownloadJob, TransferTask, UploadJob};

// ---------------------------------------------------------------------------
// auth
// ---------------------------------------------------------------------------

/// Login state of every drive at once, for the switcher in the sidebar.
///
/// Deliberately cheap: it only reports what the providers already have cached.
/// Nicknames and avatars are fetched by [`provider_profiles`], which the UI asks
/// for once it is on screen — downloading four avatars must not delay the first
/// paint of the file list.
#[tauri::command]
pub async fn auth_states(state: State<'_, Arc<AppState>>) -> AppResult<Vec<AuthState>> {
    let providers = state.providers().await?;
    let mut out = Vec::new();
    for id in ProviderId::all() {
        match providers.get(id) {
            Ok(drive) => out.push(drive.auth_state().await),
            Err(_) => {
                // Not logged in (or not implemented yet): still report it so the
                // UI can offer a login button instead of hiding the drive.
                out.push(AuthState {
                    provider: id,
                    authenticated: false,
                    source: "none".to_string(),
                    masked: None,
                    credential_path: state.paths.config_dir.to_string_lossy().to_string(),
                    nickname: None,
                    avatar: None,
                    capabilities: Capabilities::BASIC,
                });
            }
        }
    }
    Ok(out)
}

/// One drive's display profile, as far as the sidebar is concerned.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileInfo {
    pub provider: ProviderId,
    pub nickname: Option<String>,
    /// Inlined `data:` URL, or `None` when the drive has no avatar.
    pub avatar: Option<String>,
}

/// Nickname + avatar for every signed-in drive.
///
/// Each drive answers at most once per process (the result is cached in the
/// provider), so this is a single fan-out of network calls after startup rather
/// than a cost paid on every render. Failures are logged and skipped — a drive
/// without a profile just keeps the nickname it already reported.
#[tauri::command]
pub async fn provider_profiles(state: State<'_, Arc<AppState>>) -> AppResult<Vec<ProfileInfo>> {
    let providers = state.providers().await?;
    let mut out = Vec::new();
    for id in ProviderId::all() {
        let Ok(drive) = providers.get(id) else { continue };
        if !drive.auth_state().await.authenticated {
            continue;
        }
        match drive.profile().await {
            Ok(Some(info)) => out.push(ProfileInfo {
                provider: id,
                nickname: Some(info.nickname).filter(|n| !n.is_empty()),
                avatar: info.avatar,
            }),
            Ok(None) => {}
            Err(err) => crate::ylog!("{id} profile failed: {err}"),
        }
    }
    Ok(out)
}

/// Single-drive status query the older UI rows still use (the per-drive status
/// lives in `auth_states`).
#[tauri::command]
pub async fn auth_status(state: State<'_, Arc<AppState>>) -> AppResult<AuthStatus> {
    let cookie = state.cookie.read().await.clone();
    let cookie_path = state.paths.cookie_file().to_string_lossy().to_string();
    let source = if std::env::var("QUARK_COOKIE")
        .ok()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        "env".to_string()
    } else if state.paths.cookie_file().exists() {
        "file".to_string()
    } else {
        "none".to_string()
    };
    Ok(AuthStatus {
        authenticated: cookie.is_some(),
        source,
        masked: cookie.as_deref().map(config::mask_cookie),
        cookie_path,
    })
}

#[tauri::command]
pub async fn set_cookie(state: State<'_, Arc<AppState>>, cookie: String) -> AppResult<AuthStatus> {
    state.set_cookie(cookie).await?;
    // Prime `__puus` right away: download urls reject requests without a
    // current one, and the cookie copied out of a browser often lacks it.
    crate::share::refresh_cookie(&state).await;
    auth_status(state).await
}

#[tauri::command]
pub async fn clear_cookie(state: State<'_, Arc<AppState>>) -> AppResult<AuthStatus> {
    state.clear_cookie().await?;
    auth_status(state).await
}

/// Verifies the stored cookie by listing the root folder.
#[tauri::command]
pub async fn validate_cookie(state: State<'_, Arc<AppState>>) -> AppResult<u32> {
    let drive = state.drive_of(ProviderId::Quark).await?;
    let listing = drive.list("0").await?;
    Ok(listing.total)
}

/// Logs in a drive that uses a pasted credential (139 today).
///
/// Quark keeps using the cookie commands above; drives that need a QR code or a
/// password get their own command later.
#[tauri::command]
pub async fn provider_login(
    state: State<'_, Arc<AppState>>,
    provider: String,
    credential: String,
) -> AppResult<AuthState> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    crate::ylog!("login: provider={:?} credential_len={}", id, credential.len());
    match id {
        ProviderId::Yun139 => {
            let config =
                crate::providers::Yun139Provider::save_token(&state.paths.config_dir, &credential)
                    .await?;
            state.invalidate_providers().await;
            let drive = crate::providers::Yun139Provider::new(
                config,
                state
                    .paths
                    .config_dir
                    .join("yun139.toml")
                    .to_string_lossy()
                    .to_string(),
            );
            Ok(drive.auth_state().await)
        }
        ProviderId::Pan123 => {
            // `provider_login` carries one opaque string; 123 needs two, packed
            // as "user\u{1f}password" so the IPC surface stays uniform.
            let (user, pass) = credential
                .split_once('\u{1f}')
                .ok_or_else(|| AppError::new("123 云盘需要账号和密码"))?;
            let drive = crate::providers::Pan123Provider::login(
                &state.paths.config_dir,
                user,
                pass,
            )
            .await?;
            state.invalidate_providers().await;
            let mut auth = drive.auth_state().await;
            // Prove the token actually works before claiming success: a login
            // that returns a token the API rejects would otherwise look fine
            // and then fail at the first listing.
            match drive.list("0").await {
                Ok(listing) => crate::ylog!(
                    "login: pan123 root listing ok, {} entries",
                    listing.entries.len()
                ),
                Err(err) => {
                    crate::ylog!("login: pan123 root listing failed: {err}");
                    auth.authenticated = false;
                }
            }
            Ok(auth)
        }
        ProviderId::Aliyun => {
            // The QR flow stores the pair itself; here we accept a pasted
            // refresh_token so the drive is usable without the webview.
            let creds = crate::providers::aliyun::AliyunCredentials {
                refresh_token: credential.trim().to_string(),
                ..Default::default()
            };
            let drive =
                crate::providers::AliyunProvider::save_tokens(&state.paths.config_dir, creds)?;
            state.invalidate_providers().await;
            let auth = drive.auth_state().await;
            // Prove the token works — and register the signing device — before
            // reporting success, otherwise the first listing fails mysteriously.
            if let Err(err) = drive.prime().await {
                crate::ylog!("login: aliyun prime failed: {err}");
                return Err(err);
            }
            crate::ylog!("login: aliyun prime ok");
            Ok(auth)
        }
        ProviderId::Baidu => {
            // The webview harvests a cookie; a pasted cookie works the same
            // way, so both paths land here.
            let drive = crate::providers::BaiduProvider::save_cookie(
                &state.paths.config_dir,
                &credential,
            )
            .await?;
            state.invalidate_providers().await;
            let auth = drive.auth_state().await;
            // Prove the session works before claiming success, otherwise the
            // first listing is what reports the failure.
            match drive.list("0").await {
                Ok(listing) => crate::ylog!(
                    "login: baidu root listing ok, {} entries",
                    listing.entries.len()
                ),
                Err(err) => {
                    crate::ylog!("login: baidu root listing failed: {err}");
                    return Err(err);
                }
            }
            Ok(auth)
        }
        _ => Err(AppError::new(format!(
            "{} 还没有实现登录",
            id.display_name()
        ))),
    }
}

/// Starts a 阿里云盘 QR login and returns the code to render.
///
/// The rest of the flow is `/aliyun_qr_poll`; splitting it this way lets the UI
/// show the QR immediately and then poll once a second.
#[tauri::command]
pub async fn aliyun_qr_start(
    state: State<'_, Arc<AppState>>,
) -> AppResult<crate::providers::aliyun_qr::QrCode> {
    let session = Arc::new(crate::providers::aliyun_qr::AliyunQrSession::new()?);
    let qr = session.generate().await?;
    *state.aliyun_qr.write().await = Some(Arc::clone(&session));
    Ok(qr)
}

/// Polls with the QR descriptor the frontend holds.
#[tauri::command]
pub async fn aliyun_qr_poll_with(
    state: State<'_, Arc<AppState>>,
    t: i64,
    ck: String,
    content: String,
) -> AppResult<QrPollResult> {
    let session = state
        .aliyun_qr
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::new("没有进行中的扫码，请重新获取二维码"))?;

    // A confirmed scan invalidates the QR code, so polling passport again only
    // yields EXPIRED. Once the payload is in hand, finish the token exchange
    // instead — and keep it, so a failed exchange can be retried.
    if let Some(biz) = session.confirmed_payload().await {
        return finish_aliyun_login(&state, &session, &biz).await;
    }

    let qr = crate::providers::aliyun_qr::QrCode::from_parts(content, t, ck);
    let (scan, biz_ext) = session.poll(&qr).await?;
    let result = QrPollResult::from_state(&scan);
    match scan {
        crate::providers::aliyun_qr::ScanState::Confirmed => {
            let biz = biz_ext.ok_or_else(|| AppError::new("扫码已确认但没有返回凭据"))?;
            session.remember_confirmed(&biz).await;
            finish_aliyun_login(&state, &session, &biz).await
        }
        crate::providers::aliyun_qr::ScanState::Expired
        | crate::providers::aliyun_qr::ScanState::Failed(_) => {
            *state.aliyun_qr.write().await = None;
            Ok(result)
        }
        _ => Ok(result),
    }
}

/// Exchanges a confirmed `bizExt` for tokens and primes the provider.
async fn finish_aliyun_login(
    state: &State<'_, Arc<AppState>>,
    session: &crate::providers::aliyun_qr::AliyunQrSession,
    biz: &str,
) -> AppResult<QrPollResult> {
    let creds = match session.exchange(biz).await {
        Ok(creds) => creds,
        Err(err) => {
            // The QR code is already spent. Report the real reason and keep the
            // payload so the next poll retries instead of showing "已过期".
            crate::ylog!("aliyun qr: exchange failed: {err}");
            let mut result = QrPollResult::from_state(
                &crate::providers::aliyun_qr::ScanState::Confirmed,
            );
            result.message = Some(format!("换取登录凭据失败：{err}"));
            return Ok(result);
        }
    };
    let drive = crate::providers::AliyunProvider::save_tokens(&state.paths.config_dir, creds)?;
    state.invalidate_providers().await;
    *state.aliyun_qr.write().await = None;

    let mut result = QrPollResult::from_state(&crate::providers::aliyun_qr::ScanState::Confirmed);
    // Prime the account profile and the signing device while the user is still
    // looking at the QR code, so a failure is reported here.
    match drive.prime().await {
        Ok(()) => {
            result.authenticated = true;
            result.message = Some("登录成功".to_string());
        }
        Err(err) => {
            crate::ylog!("aliyun qr: prime failed: {err}");
            result.authenticated = false;
            result.message = Some(format!("登录失败：{err}"));
        }
    }
    Ok(result)
}

/// Forgets a drive's stored credential.
#[tauri::command]
pub async fn provider_logout(
    state: State<'_, Arc<AppState>>,
    provider: String,
) -> AppResult<()> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    if id == ProviderId::Quark {
        state.clear_cookie().await?;
        return Ok(());
    }
    let drive = state.drive_of(id).await?;
    drive.logout().await?;
    state.invalidate_providers().await;
    Ok(())
}

pub struct AuthStatus {
    pub authenticated: bool,
    pub source: String,
    pub masked: Option<String>,
    pub cookie_path: String,
}

impl Serialize for AuthStatus {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("AuthStatus", 4)?;
        st.serialize_field("authenticated", &self.authenticated)?;
        st.serialize_field("source", &self.source)?;
        st.serialize_field("masked", &self.masked)?;
        st.serialize_field("cookiePath", &self.cookie_path)?;
        st.end()
    }
}

// ---------------------------------------------------------------------------
// listing
// ---------------------------------------------------------------------------

/// Lists a folder. `pdir_fid` is a qualified id (`"<provider>:<native>"`).
#[tauri::command]
pub async fn list_dir(state: State<'_, Arc<AppState>>, pdir_fid: String) -> AppResult<DirListing> {
    let (drive, native) = state.drive(&pdir_fid).await?;
    drive.list(&native).await
}

#[tauri::command]
pub async fn resolve_path(
    state: State<'_, Arc<AppState>>,
    pdir_fid: String,
) -> AppResult<Vec<PathNode>> {
    let (drive, native) = state.drive(&pdir_fid).await?;
    drive.resolve_path(&native).await
}

/// Breadth-first name search starting at `pdir_fid`, limited by `limit`.
#[tauri::command]
pub async fn search(
    state: State<'_, Arc<AppState>>,
    pdir_fid: String,
    keyword: String,
    limit: Option<u32>,
) -> AppResult<Vec<SearchHit>> {
    let (drive, native) = state.drive(&pdir_fid).await?;
    drive.search(&native, &keyword, limit.unwrap_or(120)).await
}

// ---------------------------------------------------------------------------
// mutations
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn make_dir(
    state: State<'_, Arc<AppState>>,
    pdir_fid: String,
    name: String,
) -> AppResult<String> {
    let (drive, native) = state.drive(&pdir_fid).await?;
    drive.make_dir(&native, &name).await
}

#[tauri::command]
pub async fn rename_entry(
    state: State<'_, Arc<AppState>>,
    fid: String,
    name: String,
) -> AppResult<()> {
    let (drive, native) = state.drive(&fid).await?;
    drive.rename(&native, &name).await
}

/// Moves or copies entries into `to_pdir_fid` (both qualified).
///
/// Same drive: one call, the drive does the work server-side. Different drive:
/// there is no server-side primitive, so the files are streamed down to a temp
/// directory and uploaded one by one — that runs as a transfer task because it
/// takes as long as a normal download plus an upload.
#[tauri::command]
pub async fn transfer_entries(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    fids: Vec<String>,
    to_pdir_fid: String,
    copy: bool,
) -> AppResult<usize> {
    if fids.is_empty() {
        return Err(AppError::new("未选择任何文件"));
    }
    let target = crate::providers::EntryId::parse(&to_pdir_fid)?;
    let mut native_fids = Vec::with_capacity(fids.len());
    let mut drive: Option<Arc<dyn DriveProvider>> = None;
    let mut foreign = false;
    for raw in &fids {
        let id = crate::providers::EntryId::parse(raw)?;
        if id.provider != target.provider {
            foreign = true;
        }
        drive = Some(state.drive_of(id.provider).await?);
        native_fids.push(id.native);
    }
    let drive = drive.ok_or_else(|| AppError::new("未选择任何文件"))?;
    if !foreign {
        return drive
            .transfer_entries(&native_fids, &target.native, copy)
            .await;
    }
    // A mixed selection keeps the per-file drive: `start_cross_drive` resolves
    // each source itself, so the loop above only needed the first one.
    start_cross_drive(&state, &app, &fids, &target, copy).await
}

/// Expands the selection, recreates the folder tree on the target drive and
/// hands the whole batch to one transfer task.
async fn start_cross_drive(
    state: &Arc<AppState>,
    app: &AppHandle,
    fids: &[String],
    target: &crate::providers::EntryId,
    copy: bool,
) -> AppResult<usize> {
    let to = state.drive_of(target.provider).await?;
    let mut files: Vec<crate::transfer::CrossFile> = Vec::new();
    let mut dirs: HashMap<(String, String), String> = HashMap::new();
    let mut first_name = String::new();
    for raw in fids {
        let id = crate::providers::EntryId::parse(raw)?;
        let from = state.drive_of(id.provider).await?;
        let listing = from.list(&id.native).await?;
        let entry = listing
            .entries
            .into_iter()
            .find(|e| native_of(&e.fid) == id.native)
            .ok_or_else(|| AppError::new("找不到要传输的文件"))?;
        if first_name.is_empty() {
            first_name = entry.name.clone();
        }
        if !entry.is_dir {
            files.push(crate::transfer::CrossFile {
                fid: id.native.clone(),
                name: crate::paths::safe_component(&entry.name),
                size: entry.size,
                to_parent: target.native.clone(),
            });
            continue;
        }
        // Folders: rebuild the tree below the target and remember each folder
        // id so siblings do not list the same parent over and over.
        let root = cross_dir(&to, &mut dirs, &target.native, &entry.name).await?;
        let walked = collect_remote_files(&from, &id.native).await?;
        for file in walked {
            let mut parent = root.clone();
            if let Some(dir) = file.relative.parent().filter(|p| !p.as_os_str().is_empty()) {
                for part in dir.components() {
                    let name = part.as_os_str().to_string_lossy().to_string();
                    parent = cross_dir(&to, &mut dirs, &parent, &name).await?;
                }
            }
            files.push(crate::transfer::CrossFile {
                fid: file.fid,
                name: file.name,
                size: file.size,
                to_parent: parent,
            });
        }
    }
    if files.is_empty() {
        return Err(AppError::new("选中的内容里没有可传输的文件"));
    }
    let count = files.len();
    let total: u64 = files.iter().map(|f| f.size).sum();
    let job = crate::transfer::CrossJob {
        files,
        to_pdir_fid: target.native.clone(),
        copy,
        name: first_name,
    };
    let temp = state.paths.cache_dir.join("cross");
    let from = state.drive_of(
        crate::providers::EntryId::parse(&fids[0])?.provider,
    )
    .await?;
    let task = state
        .transfers
        .start_cross(app.clone(), from, to, temp, job, total)
        .await?;
    crate::ylog!(
        "cross {} {} file(s) -> {} ({})",
        if copy { "copy" } else { "move" },
        count,
        target.provider.as_str(),
        task.id
    );
    Ok(count)
}

/// Finds or creates `name` below `parent` on the target drive.
async fn cross_dir(
    to: &Arc<dyn DriveProvider>,
    seen: &mut HashMap<(String, String), String>,
    parent: &str,
    name: &str,
) -> AppResult<String> {
    if let Some(hit) = seen.get(&(parent.to_string(), name.to_string())) {
        return Ok(hit.clone());
    }
    let listing = to.list(parent).await?;
    let found = listing
        .entries
        .into_iter()
        .find(|e| e.is_dir && e.name == name)
        .map(|e| native_of(&e.fid));
    let native = match found {
        Some(native) => native,
        None => native_of(&to.make_dir(parent, name).await?),
    };
    seen.insert((parent.to_string(), name.to_string()), native.clone());
    Ok(native)
}

#[tauri::command]
pub async fn delete_entries(state: State<'_, Arc<AppState>>, fids: Vec<String>) -> AppResult<()> {
    if fids.is_empty() {
        return Err(AppError::new("未选择任何文件"));
    }
    // Group by drive so one call serves a mixed selection.
    let mut by_drive: HashMap<ProviderId, Vec<String>> = HashMap::new();
    for raw in &fids {
        let id = crate::providers::EntryId::parse(raw)?;
        by_drive.entry(id.provider).or_default().push(id.native);
    }
    for (provider, natives) in by_drive {
        let drive = state.drive_of(provider).await?;
        drive.delete(&natives).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// transfers
// ---------------------------------------------------------------------------

/// One file discovered while walking a remote folder tree.
struct RemoteFile {
    relative: PathBuf,
    fid: String,
    name: String,
    size: u64,
}

/// Walks `dir` depth-first and collects every file below it.
async fn collect_remote_files(
    drive: &Arc<dyn DriveProvider>,
    dir: &str,
) -> AppResult<Vec<RemoteFile>> {
    const MAX_FILES: usize = 5000;
    const MAX_DEPTH: usize = 12;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut queue: VecDeque<(String, PathBuf, usize)> = VecDeque::new();
    queue.push_back((dir.to_string(), PathBuf::new(), 0));
    while let Some((fid, prefix, depth)) = queue.pop_front() {
        let listing = drive.list(&fid).await?;
        for entry in listing.entries {
            let native = native_of(&entry.fid);
            // Remote names are untrusted: a name like `../../x` must not be able
            // to place the file outside the chosen download folder.
            let name = crate::paths::safe_component(&entry.name);
            if entry.is_dir {
                if depth < MAX_DEPTH {
                    queue.push_back((native, prefix.join(name), depth + 1));
                } else {
                    // Reported instead of silently dropping the subtree.
                    skipped += 1;
                }
                continue;
            }
            out.push(RemoteFile {
                relative: prefix.join(&name),
                fid: native,
                name,
                size: entry.size,
            });
            if out.len() >= MAX_FILES {
                return Err(AppError::new(
                    "文件夹内文件过多（超过 5000 个），请分次下载",
                ));
            }
        }
    }
    if skipped > 0 && out.is_empty() {
        return Err(AppError::new(format!(
            "有 {skipped} 个目录超过 {MAX_DEPTH} 层，未找到可下载的文件"
        )));
    }
    Ok(out)
}

#[tauri::command]
pub async fn download_entries(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    items: Vec<DownloadRequest>,
    dir: Option<String>,
) -> AppResult<Vec<TransferTask>> {
    if items.is_empty() {
        return Err(AppError::new("未选择任何文件"));
    }
    let settings = state.settings().await;
    let target = dir
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(settings.download_dir);
    if target.trim().is_empty() {
        return Err(AppError::new("请先在设置中选择下载目录"));
    }
    let mut tasks = Vec::new();
    for item in items {
        let id = crate::providers::EntryId::parse(&item.fid)?;
        let drive = state.drive_of(id.provider).await?;
        if !item.is_dir {
            let job = DownloadJob {
                fid: item.fid.clone(),
                name: item.name,
                size: item.size,
                dir: target.clone(),
                share: None,
                provider: id.provider.as_str().to_string(),
            };
            tasks.push(
                state
                    .transfers
                    .start_download(app.clone(), arc_ctx(drive), job)
                    .await?,
            );
            continue;
        }

        // Folders are flattened into one task per file so progress, cancel and
        // resume keep working per file; the folder name becomes the local root.
        let root = crate::paths::unique_path(
            &PathBuf::from(&target).join(crate::paths::safe_component(&item.name)),
        );
        tokio::fs::create_dir_all(&root).await?;
        let files = collect_remote_files(&drive, &id.native).await?;
        if files.is_empty() {
            return Err(AppError::new(format!("「{}」中没有可下载的文件", item.name)));
        }
        for file in files {
            let dir = match file.relative.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    crate::paths::safe_join(&root, parent)
                }
                _ => root.clone(),
            };
            tokio::fs::create_dir_all(&dir).await?;
            let job = DownloadJob {
                fid: qualify(id.provider, &file.fid),
                name: file.name,
                size: file.size,
                dir: dir.to_string_lossy().to_string(),
                share: None,
                provider: id.provider.as_str().to_string(),
            };
            tasks.push(
                state
                    .transfers
                    .start_download(app.clone(), arc_ctx(Arc::clone(&drive)), job)
                    .await?,
            );
        }
    }
    if tasks.is_empty() {
        return Err(AppError::new("没有可下载的内容"));
    }
    Ok(tasks)
}

fn arc_ctx(drive: Arc<dyn DriveProvider>) -> crate::transfer::DownloadCtx {
    crate::transfer::DownloadCtx::Provider(drive)
}

/// Finds (or creates) the remote folder `name` below `parent`.
async fn ensure_remote_dir(
    drive: &Arc<dyn DriveProvider>,
    cache: &mut HashMap<String, Vec<EntryDto>>,
    parent: &str,
    name: &str,
) -> AppResult<String> {
    if !cache.contains_key(parent) {
        let listing = drive.list(parent).await?;
        cache.insert(parent.to_string(), listing.entries);
    }
    if let Some(entry) = cache
        .get(parent)
        .and_then(|list| list.iter().find(|e| e.is_dir && e.name == name))
    {
        return Ok(native_of(&entry.fid));
    }
    let qualified = drive.make_dir(parent, name).await?;
    let native = native_of(&qualified);
    if let Some(list) = cache.get_mut(parent) {
        list.push(EntryDto {
            fid: qualified,
            name: name.to_string(),
            pdir_fid: String::new(),
            size: 0,
            is_dir: true,
            format_type: "folder".to_string(),
            updated_at: 0,
            created_at: 0,
            ext: String::new(),
            provider: drive.id().as_str().to_string(),
            thumb: None,
        });
    }
    Ok(native)
}

/// Walks a local directory and returns (relative path, absolute path) pairs.
async fn collect_local_files(root: &Path) -> AppResult<Vec<(PathBuf, PathBuf)>> {
    const MAX_FILES: usize = 5000;
    let mut out = Vec::new();
    let mut queue: VecDeque<(PathBuf, PathBuf)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), PathBuf::new()));
    while let Some((dir, prefix)) = queue.pop_front() {
        let mut reader = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = reader.next_entry().await? {
            let name = entry.file_name();
            let name_text = name.to_string_lossy().to_string();
            if name_text.starts_with('.') {
                continue;
            }
            let path = entry.path();
            let relative = prefix.join(&name);
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                queue.push_back((path, relative));
            } else if meta.is_file() {
                out.push((relative, path));
                if out.len() >= MAX_FILES {
                    return Err(AppError::new(
                        "文件夹内文件过多（超过 5000 个），请分次上传",
                    ));
                }
            }
        }
    }
    Ok(out)
}

#[tauri::command]
pub async fn upload_files(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    paths: Vec<String>,
    pdir_fid: String,
) -> AppResult<Vec<TransferTask>> {
    if paths.is_empty() {
        return Err(AppError::new("未选择任何文件"));
    }
    let id = crate::providers::EntryId::parse(&pdir_fid)?;
    let drive = state.drive_of(id.provider).await?;
    let root_fid = id.native;
    let settings = state.settings().await;

    let mut dir_cache: HashMap<String, Vec<EntryDto>> = HashMap::new();
    let mut used: HashMap<String, Vec<String>> = HashMap::new();
    let existing = drive.list(&root_fid).await?;
    used.insert(
        root_fid.clone(),
        existing.entries.iter().map(|e| e.name.clone()).collect(),
    );
    dir_cache.insert(root_fid.clone(), existing.entries);

    let mut tasks = Vec::new();
    let mut skipped = 0usize;
    let mut sources: Vec<(String, String, String)> = Vec::new();

    for raw in paths {
        let path = PathBuf::from(&raw);
        let meta = tokio::fs::metadata(&path).await?;
        if meta.is_dir() {
            // Folders become a real remote folder tree; every file inside is a
            // separate task so progress and cancel stay per file.
            let root_name = path
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("folder")
                .to_string();
            let folder_fid =
                ensure_remote_dir(&drive, &mut dir_cache, &root_fid, &root_name).await?;
            let files = collect_local_files(&path).await?;
            if files.is_empty() {
                return Err(AppError::new(format!("「{root_name}」是空文件夹")));
            }
            for (relative, absolute) in files {
                let mut parent = folder_fid.clone();
                if let Some(rel_parent) = relative.parent() {
                    for segment in rel_parent.components() {
                        let piece = segment.as_os_str().to_string_lossy().to_string();
                        if piece.is_empty() {
                            continue;
                        }
                        parent =
                            ensure_remote_dir(&drive, &mut dir_cache, &parent, &piece).await?;
                    }
                }
                let file_name = absolute
                    .file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("file")
                    .to_string();
                sources.push((absolute.to_string_lossy().to_string(), parent, file_name));
            }
            continue;
        }

        let base_name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("file")
            .to_string();
        sources.push((raw, root_fid.clone(), base_name));
    }

    for (raw, target_fid, base_name) in sources {
        if !used.contains_key(&target_fid) {
            let listing = drive.list(&target_fid).await?;
            used.insert(
                target_fid.clone(),
                listing.entries.into_iter().map(|e| e.name).collect(),
            );
        }
        let names = used.get_mut(&target_fid).expect("populated above");
        let Some(name) = resolve_conflict(&base_name, names, &settings) else {
            // "skip" policy: a same-named entry already exists in the folder.
            skipped += 1;
            continue;
        };
        names.push(name.clone());
        let job = UploadJob {
            path: raw,
            pdir_fid: qualify(id.provider, &target_fid),
            name: Some(name),
        };
        tasks.push(
            state
                .transfers
                .start_upload(app.clone(), arc_ctx(Arc::clone(&drive)), job)
                .await?,
        );
    }

    if tasks.is_empty() && skipped > 0 {
        return Err(AppError::new(format!(
            "已跳过 {skipped} 个同名文件（上传重名处理设置为「跳过」）"
        )));
    }
    if tasks.is_empty() {
        return Err(AppError::new("没有可上传的内容"));
    }
    Ok(tasks)
}

fn resolve_conflict(name: &str, used: &[String], settings: &Settings) -> Option<String> {
    let taken = |candidate: &str| used.iter().any(|n| n == candidate);
    if !taken(name) {
        return Some(name.to_string());
    }
    match settings.upload_conflict.as_str() {
        "skip" => None,
        // "overwrite" still needs a unique cloud name, so we rename: none of the
        // drives we talk to offers an in-place overwrite primitive.
        _ => {
            for index in 2..10_000u32 {
                let candidate = crate::paths::renamed_for_conflict(name, index);
                if !taken(&candidate) {
                    return Some(candidate);
                }
            }
            Some(name.to_string())
        }
    }
}

#[tauri::command]
pub async fn transfers_list(state: State<'_, Arc<AppState>>) -> AppResult<Vec<TransferTask>> {
    Ok(state.transfers.list().await)
}

#[tauri::command]
pub async fn transfer_cancel(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
) -> AppResult<()> {
    state.transfers.cancel_task(&id, &app).await
}

/// Stops a transfer but keeps its record (and its `.part`) so it can resume.
#[tauri::command]
pub async fn transfer_pause(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
) -> AppResult<()> {
    state.transfers.pause_task(&id, &app).await
}

#[tauri::command]
pub async fn transfer_remove(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
) -> AppResult<()> {
    state.transfers.remove(&id, &app).await
}

#[tauri::command]
pub async fn transfer_clear(state: State<'_, Arc<AppState>>, app: AppHandle) -> AppResult<()> {
    state.transfers.clear_finished(&app).await
}

/// Restarts a failed (or cancelled) download, resuming from its `.part`.
#[tauri::command]
pub async fn transfer_retry(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
) -> AppResult<TransferTask> {
    let task = {
        let list = state.transfers.list().await;
        list.into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| AppError::new("任务不存在"))?
    };
    if task.kind == "copy" || task.kind == "move" {
        // Cross-drive jobs remember which files are still missing, so a retry
        // only re-runs those.
        let cross = task
            .cross
            .clone()
            .ok_or_else(|| AppError::new("任务缺少传输信息"))?;
        let from = state
            .drive_of(
                ProviderId::parse(&cross.from_provider)
                    .ok_or_else(|| AppError::new(format!("未知的网盘：{}", cross.from_provider)))?,
            )
            .await?;
        let to = state
            .drive_of(
                ProviderId::parse(&cross.to_provider)
                    .ok_or_else(|| AppError::new(format!("未知的网盘：{}", cross.to_provider)))?,
            )
            .await?;
        return state.transfers.retry_cross(app, from, to, &id).await;
    }
    if task.kind != "download" {
        return Err(AppError::new("上传任务无法续传，请重新上传"));
    }
    let ctx = match &task.share {
        Some(share) => {
            // A share download needs the provider that hosts the share, not the
            // one the file id belongs to. The provider is persisted with the
            // task so a retry after a restart still knows where to go.
            let provider = if share.provider.trim().is_empty() {
                ProviderId::Quark
            } else {
                ProviderId::parse(&share.provider)
                    .ok_or_else(|| AppError::new(format!("未知的网盘：{}", share.provider)))?
            };
            let drive = state.drive_of(provider).await?;
            crate::transfer::DownloadCtx::Share(drive)
        }
        None => {
            let drive = match &task.fid {
                Some(fid) => state.drive(fid).await?.0,
                None => return Err(AppError::new("任务缺少文件信息")),
            };
            crate::transfer::DownloadCtx::Provider(drive)
        }
    };
    state.transfers.retry_download(app, ctx, &id).await
}

// ---------------------------------------------------------------------------
// settings / misc
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn settings_get(state: State<'_, Arc<AppState>>) -> AppResult<Settings> {
    Ok(state.settings().await)
}

#[tauri::command]
pub async fn settings_set(
    state: State<'_, Arc<AppState>>,
    settings: Settings,
) -> AppResult<Settings> {
    state.update_settings(settings).await
}

/// Capacity of one drive, when it exposes it.
#[tauri::command]
pub async fn provider_quota(
    state: State<'_, Arc<AppState>>,
    provider: String,
) -> AppResult<Option<QuotaInfo>> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive.quota().await
}

/// Inlines a preview image so the renderer can show it.
///
/// The app CSP only allows `data:`/`asset:` images, so a remote thumbnail has
/// to be fetched here and handed over as a data URL. Thumbnail hosts are
/// pre-signed and short-lived, which is why this takes the url rather than an
/// entry id: the listing already carries a fresh one.
#[tauri::command]
pub async fn inline_image(url: String) -> AppResult<Option<String>> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Ok(None);
    }
    let client = crate::share::http()?;
    Ok(crate::share::fetch_image(&client, &url, None).await)
}

/// Lists a drive's recycle bin.
#[tauri::command]
pub async fn recycle_list(
    state: State<'_, Arc<AppState>>,
    provider: String,
    offset: Option<u32>,
    limit: Option<u32>,
) -> AppResult<Vec<crate::providers::RecycleEntry>> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive
        .recycle_list(offset.unwrap_or(0), limit.unwrap_or(100))
        .await
}

/// Restores entries from the recycle bin.
#[tauri::command]
pub async fn recycle_restore(
    state: State<'_, Arc<AppState>>,
    provider: String,
    entries: Vec<crate::providers::RecycleEntry>,
) -> AppResult<u32> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive.recycle_restore(&entries).await
}

/// Deletes recycle-bin entries for good, where the drive allows it.
#[tauri::command]
pub async fn recycle_purge(
    state: State<'_, Arc<AppState>>,
    provider: String,
    entries: Vec<crate::providers::RecycleEntry>,
) -> AppResult<u32> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive.recycle_purge(&entries).await
}

/// Empties the whole bin (aliyun has no per-item purge).
#[tauri::command]
pub async fn recycle_clear(
    state: State<'_, Arc<AppState>>,
    provider: String,
) -> AppResult<()> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive.recycle_clear().await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub version: String,
    pub config_dir: String,
    pub cookie_path: String,
    pub download_dir: String,
    pub platform: String,
    /// Where the app writes its log; shown in settings so a user can send it.
    pub log_path: Option<String>,
}

/// Quits the application even when "close to tray" would normally hide it.
#[tauri::command]
pub fn quit_app(app: AppHandle) {
    crate::state::request_exit();
    app.exit(0);
}

#[tauri::command]
pub async fn app_info(state: State<'_, Arc<AppState>>) -> AppResult<AppInfo> {
    let settings = state.settings().await;
    Ok(AppInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        config_dir: state.paths.config_dir.to_string_lossy().to_string(),
        cookie_path: state.paths.cookie_file().to_string_lossy().to_string(),
        download_dir: settings.download_dir,
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        log_path: crate::logging::path().map(|p| p.to_string_lossy().to_string()),
    })
}

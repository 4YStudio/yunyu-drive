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
use tauri::{AppHandle, Emitter, State};

use crate::config::{self, Settings};
use crate::error::{AppError, AppResult};
use crate::providers::aliyun_qr::QrPollResult;
use crate::providers::{
    native_of, qualify, AuthState, Capabilities, DirListing, DownloadRequest, DriveProvider,
    EntryDto, PathNode, ProviderId, QuotaInfo, SearchHit, TrafficInfo,
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
        ProviderId::Pan115 => {
            let dir = state.paths.config_dir.clone();
            let drive = crate::providers::Pan115Provider::save_cookie(&dir, &credential)?;
            state.invalidate_providers().await;
            let mut auth = drive.auth_state().await;
            if drive.list("0").await.is_err() {
                auth.authenticated = false;
            }
            Ok(auth)
        }
        ProviderId::Cloud189 => {
            // The QR flow writes the file itself; here we accept a pasted
            // `session_key\u{1f}session_secret` pair (what the web client
            // keeps) so the drive is usable without the webview.
            let (key, secret) = credential
                .trim()
                .split_once('\u{1f}')
                .ok_or_else(|| {
                    AppError::new("天翼云盘需要 sessionKey 与 sessionSecret（用扫码登录更简单）")
                })?;
            let creds = crate::providers::cloud189::Cloud189Credentials {
                session_key: key.trim().to_string(),
                session_secret: secret.trim().to_string(),
                ..Default::default()
            };
            let drive = crate::providers::Cloud189Provider::save_session(
                &state.paths.config_dir,
                creds,
            )?;
            state.invalidate_providers().await;
            let mut auth = drive.auth_state().await;
            // Prove the session signs correctly before claiming success.
            match drive.list("0").await {
                Ok(listing) => crate::ylog!(
                    "login: cloud189 root listing ok, {} entries",
                    listing.entries.len()
                ),
                Err(err) => {
                    crate::ylog!("login: cloud189 root listing failed: {err}");
                    auth.authenticated = false;
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

/// Starts a 天翼云盘 QR login and returns the code to render.
#[tauri::command]
pub async fn cloud189_qr_start(
    state: State<'_, Arc<AppState>>,
) -> AppResult<crate::providers::cloud189_qr::QrCode> {
    let session = Arc::new(crate::providers::cloud189_qr::Cloud189QrSession::new()?);
    let qr = session.generate().await?;
    *state.cloud189_qr.write().await = Some(Arc::clone(&session));
    Ok(qr)
}

/// Polls the 天翼云盘 QR login.
#[tauri::command]
pub async fn cloud189_qr_poll(
    state: State<'_, Arc<AppState>>,
) -> AppResult<crate::providers::cloud189_qr::QrPollResult> {
    let session = state
        .cloud189_qr
        .read()
        .await
        .clone()
        .ok_or_else(|| AppError::new("没有进行中的扫码，请重新获取二维码"))?;

    // A confirmed scan spends the code, so finish the exchange instead of
    // polling again — and keep the payload so a failure is retryable.
    if let Some(redirect) = session.confirmed_payload().await {
        return finish_cloud189_login(&state, &session, &redirect).await;
    }

    let qr = crate::providers::cloud189_qr::QrCode::from_parts(
        String::new(),
        chrono::Utc::now().timestamp_millis(),
        String::new(),
    );
    let (scan, redirect) = session.poll(&qr).await?;
    let mut result = crate::providers::cloud189_qr::QrPollResult::from_state(&scan);
    match scan {
        crate::providers::cloud189_qr::ScanState::Confirmed => {
            let redirect = redirect
                .ok_or_else(|| AppError::new("扫码已确认但没有返回跳转地址"))?;
            session.remember_confirmed(&redirect).await;
            finish_cloud189_login(&state, &session, &redirect).await
        }
        crate::providers::cloud189_qr::ScanState::Expired
        | crate::providers::cloud189_qr::ScanState::Failed(_) => {
            *state.cloud189_qr.write().await = None;
            result.authenticated = false;
            Ok(result)
        }
        _ => Ok(result),
    }
}

/// Exchanges a confirmed `redirectUrl` for the signed session and primes it.
async fn finish_cloud189_login(
    state: &State<'_, Arc<AppState>>,
    session: &crate::providers::cloud189_qr::Cloud189QrSession,
    redirect: &str,
) -> AppResult<crate::providers::cloud189_qr::QrPollResult> {
    let creds = match session.exchange(redirect).await {
        Ok(creds) => creds,
        Err(err) => {
            // The code is spent; report the real reason and keep the payload so
            // the next poll retries instead of showing "已过期".
            crate::ylog!("cloud189 qr: exchange failed: {err}");
            let mut result = crate::providers::cloud189_qr::QrPollResult::from_state(
                &crate::providers::cloud189_qr::ScanState::Confirmed,
            );
            result.message = Some(format!("换取登录凭据失败：{err}"));
            return Ok(result);
        }
    };
    let drive = crate::providers::Cloud189Provider::save_session(&state.paths.config_dir, creds)?;
    state.invalidate_providers().await;
    *state.cloud189_qr.write().await = None;

    let mut result = crate::providers::cloud189_qr::QrPollResult::from_state(
        &crate::providers::cloud189_qr::ScanState::Confirmed,
    );
    // Listing the root proves the signature works while the user is still
    // looking at the QR code, so a failure surfaces here instead of later.
    match drive.list("0").await {
        Ok(listing) => {
            crate::ylog!(
                "cloud189 qr: login ok, {} entries at root",
                listing.entries.len()
            );
            result.authenticated = true;
            result.message = Some("登录成功".to_string());
        }
        Err(err) => {
            crate::ylog!("cloud189 qr: prime failed: {err}");
            result.authenticated = false;
            result.message = Some(format!("登录失败：{err}"));
        }
    }
    Ok(result)
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
        let walked = collect_remote_files(&from, &id.native, |_| {}).await?;
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
    mut progress: impl FnMut(usize),
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
    progress(out.len());
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
                parent: None,
            };
            tasks.push(
                state
                    .transfers
                    .start_download(app.clone(), arc_ctx(drive), job)
                    .await?,
            );
            continue;
        }

        // A folder becomes **one** task. Its files are downloaded by hidden
        // children inside the transfer manager, so the list stays readable and
        // pause / cancel / resume act on the folder as a whole.
        let root = crate::paths::unique_path(
            &PathBuf::from(&target).join(crate::paths::safe_component(&item.name)),
        );
        let folder = crate::transfer::FolderRef {
            provider: id.provider.as_str().to_string(),
            fid: id.native.clone(),
            name: item.name.clone(),
        };
        tasks.push(
            state
                .transfers
                .start_folder(app.clone(), drive, folder, root)
                .await?,
        );
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
#[cfg_attr(not(test), allow(dead_code))]
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
            // A folder upload is one row in the transfer list, not one row per
            // file: the manager walks the tree, creates the remote folders and
            // keeps a few uploads in flight. Sending 3000 files as 3000 tasks is
            // what used to freeze the window.
            let folder = crate::transfer::UploadDirRef {
                provider: id.provider.as_str().to_string(),
                pdir_fid: folder_fid,
                path: raw.clone(),
                name: root_name,
                conflict: settings.upload_conflict.clone(),
            };
            let task = state
                .transfers
                .start_upload_folder(app.clone(), Arc::clone(&drive), folder)
                .await?;
            tasks.push(task);
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
            parent: None,
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
    purge_local: Option<bool>,
) -> AppResult<()> {
    state
        .transfers
        .remove(&id, &app, purge_local.unwrap_or(false))
        .await
}

/// The files a folder task is working on right now, for the expanded row.
#[tauri::command]
pub async fn transfer_children(
    state: State<'_, Arc<AppState>>,
    id: String,
) -> AppResult<Vec<TransferTask>> {
    Ok(state.transfers.children(&id).await)
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
    resume_task(state.inner(), app, &id).await
}

/// Batch pause / cancel / remove / resume for a multi-selection.
///
/// Pausing a 3000-file queue one command at a time meant 3000 IPC round trips
/// and 3000 events; this does the work in one pass and announces the result
/// once.
#[tauri::command]
pub async fn transfers_bulk(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    ids: Vec<String>,
    action: String,
    purge_local: Option<bool>,
) -> AppResult<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    match action.as_str() {
        "pause" => {
            let changed = state.transfers.pause_many(&ids).await;
            if !changed.is_empty() {
                let _ = app.emit(crate::transfer::EVENT_TRANSFERS, &changed);
            }
            Ok(changed.len())
        }
        "cancel" => {
            let changed = state.transfers.cancel_many(&ids).await;
            if !changed.is_empty() {
                let _ = app.emit(crate::transfer::EVENT_TRANSFERS, &changed);
            }
            Ok(changed.len())
        }
        "remove" => {
            let gone = state
                .transfers
                .remove_many(&ids, purge_local.unwrap_or(false))
                .await;
            if !gone.is_empty() {
                let _ = app.emit(crate::transfer::EVENT_REMOVED_MANY, &gone);
            }
            Ok(gone.len())
        }
        "resume" => {
            let mut started = 0;
            let mut first: Option<String> = None;
            for id in &ids {
                match resume_task(state.inner(), app.clone(), id).await {
                    Ok(_) => started += 1,
                    Err(err) => {
                        if first.is_none() {
                            first = Some(err.to_string());
                        }
                    }
                }
            }
            if started == 0 {
                return Err(AppError::new(
                    first.unwrap_or_else(|| "没有可继续的任务".to_string()),
                ));
            }
            Ok(started)
        }
        other => Err(AppError::new(format!("未知的批量操作：{other}"))),
    }
}

/// Pins or unpins one job ("置顶"): it sorts above its peers and takes the next
/// free transfer slot even when the concurrency limit is already reached.
/// Per-task speed cap in KiB/s (`None` = follow the global setting).
#[tauri::command]
pub async fn transfer_limit(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
    kbps: Option<u32>,
) -> AppResult<TransferTask> {
    state.transfers.set_task_limit(&app, &id, kbps).await
}

#[tauri::command]
pub async fn transfer_pin(
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
    id: String,
    pinned: bool,
) -> AppResult<TransferTask> {
    state.transfers.set_pinned(&app, &id, pinned).await
}

/// The single-task retry body, shared by `transfer_retry`, `transfers_bulk` and
/// the auto-resume supervisor.
pub(crate) async fn resume_task(
    state: &Arc<AppState>,
    app: AppHandle,
    id: &str,
) -> AppResult<TransferTask> {
    let id = id.to_string();
    let task = state
        .transfers
        .snapshot(&id)
        .await
        .ok_or_else(|| AppError::new("任务不存在"))?;
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
    if task.kind == "folder" {
        let folder = task
            .folder
            .clone()
            .ok_or_else(|| AppError::new("任务缺少文件夹信息"))?;
        let provider = ProviderId::parse(&folder.provider)
            .ok_or_else(|| AppError::new(format!("未知的网盘：{}", folder.provider)))?;
        let drive = state.drive_of(provider).await?;
        return state.transfers.resume_folder(app, drive, &id).await;
    }
    if task.kind == "upload" {
        // Folder uploads resume. A single file cannot: the drive has to be sent
        // every byte again, so the retry becomes a brand-new task and the failed
        // row is dropped (its file on disk is untouched either way).
        let Some(dir) = task.upload_dir.clone() else {
            let provider = ProviderId::parse(&task.provider)
                .ok_or_else(|| AppError::new(format!("未知的网盘：{}", task.provider)))?;
            let drive = state.drive_of(provider).await?;
            let job = crate::transfer::UploadJob {
                path: task.local_path.clone(),
                pdir_fid: task.remote_dir_fid.clone().unwrap_or_default(),
                name: Some(task.name.clone()),
                parent: task.parent.clone(),
            };
            let fresh = state
                .transfers
                .start_upload(app.clone(), crate::transfer::DownloadCtx::Provider(drive), job)
                .await?;
            let _ = state.transfers.remove(&id, &app, false).await;
            return Ok(fresh);
        };
        let provider = ProviderId::parse(&dir.provider)
            .ok_or_else(|| AppError::new(format!("未知的网盘：{}", dir.provider)))?;
        let drive = state.drive_of(provider).await?;
        return state.transfers.resume_upload_folder(app, drive, &id).await;
    }
    if task.kind != "download" {
        return Err(AppError::new("这个任务无法续传，请重新开始"));
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

/// Monthly download allowance of one drive, when it meters one.
#[tauri::command]
pub async fn provider_traffic(
    state: State<'_, Arc<AppState>>,
    provider: String,
) -> AppResult<Option<TrafficInfo>> {
    let id = ProviderId::parse(&provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    let drive = state.drive_of(id).await?;
    drive.traffic().await
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

/// Frame-accurate UI probe, debug builds only.
///
/// The drive-switcher popup and the view swap are driven through the Web
/// Animations API because WebKitGTK steps a CSS `transition` badly, but there is
/// no way to see what the compositor actually painted from inside the webview.
/// This command lets `src/lib/probe.ts` hand each animation sample (opacity,
/// transform, layer state, time since the first sample) to the native side,
/// which writes it to `app.log` — one line per sample, so a flash or a stall
/// shows up as a gap in the timestamps instead of a mystery.
///
/// Compiled out of release builds on purpose: it must never ship.
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn debug_probe(tag: String, sample: String) -> AppResult<()> {
    crate::ylog!("probe {tag} {sample}");
    Ok(())
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
        version: crate::version::VERSION.to_string(),
        config_dir: state.paths.config_dir.to_string_lossy().to_string(),
        cookie_path: state.paths.cookie_file().to_string_lossy().to_string(),
        download_dir: settings.download_dir,
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        log_path: crate::logging::path().map(|p| p.to_string_lossy().to_string()),
    })
}

// ---------------------------------------------------------------------------
// live upload probe
// ---------------------------------------------------------------------------
//
// Diagnostic only: every test here is `#[ignore]` and talks to a real account.
// It creates exactly one remote folder `yunyu-probe-up-<pid>` per drive, works
// inside it, and deletes it at the end. Nothing outside that folder is touched.
//
// Run one drive at a time, e.g.:
//   cargo test --lib quark_live_upload_probe -- --ignored --nocapture
//
// The command layer itself needs a `tauri::AppHandle` (not constructible in a
// unit test), so these tests drive `DriveProvider::upload` directly and reuse the
// very helpers `upload_files` uses (`collect_local_files`, `ensure_remote_dir`,
// `resolve_conflict`), plus the same `CancellationToken` that pause/cancel use.
/// Keeps the queue moving without the user watching it.
///
/// Two situations need this: a task whose drive hiccuped (a timeout, a dropped
/// connection, a rate limit) waits out a backoff and starts again by itself, and
/// a task that was still running when the app closed continues right after the
/// next start. Both look identical in the task list — "queued" with no worker
/// behind it — so one loop covers them.
pub(crate) async fn supervise_transfers(state: Arc<AppState>, app: AppHandle) {
    // Let the first moments of startup (windows, providers, restore) settle.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    loop {
        for id in state.transfers.due_retries().await {
            match resume_task(&state, app.clone(), &id).await {
                Ok(task) => crate::ylog!("自动继续 {}（{}）", task.name, task.id),
                Err(err) => {
                    // The task stays queued and would be picked again every
                    // second; push its deadline out instead and give up for good
                    // once the attempts are used up.
                    let retry = state.transfers.defer_retry(&id, &app, &err.to_string()).await;
                    crate::ylog!("自动继续 {} 失败（{retry}）：{err}", id);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    }
}

#[cfg(test)]
mod live_upload_probe {
    use super::*;
    use crate::providers::UploadSink;
    use futures_util::StreamExt;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};
    use tokio_util::sync::CancellationToken;

    /// Native root id accepted by every provider (`cloud189` maps "0" -> "-11").
    const ROOT: &str = "0";
    const BIG: usize = 10 * 1024 * 1024;
    const CANCEL_SIZE: usize = 50 * 1024 * 1024;

    fn config_dir() -> PathBuf {
        crate::config::AppPaths::resolve()
            .expect("无法定位配置目录")
            .config_dir
    }

    fn probe_name() -> String {
        format!("yunyu-probe-up-{}", std::process::id())
    }

    // ------------------------------------------------------------- providers ---

    fn quark_drive() -> Option<Arc<dyn DriveProvider>> {
        let paths = crate::config::AppPaths::resolve().ok()?;
        let cookie = crate::config::read_cookie(&paths)?;
        let client = libquarkpan::QuarkPan::builder()
            .cookie(cookie.clone())
            .api_base_url("https://drive.quark.cn".to_string())
            .prepare()
            .ok()?;
        Some(Arc::new(crate::providers::quark::QuarkProvider::new(
            client,
            cookie,
            paths.cookie_file().to_string_lossy().to_string(),
        )))
    }

    macro_rules! loader {
        ($name:ident, $ty:path) => {
            fn $name() -> Option<Arc<dyn DriveProvider>> {
                <$ty>::load(&config_dir())
                    .ok()
                    .map(|p| Arc::new(p) as Arc<dyn DriveProvider>)
            }
        };
    }

    loader!(yun139_drive, crate::providers::yun139::Yun139Provider);
    loader!(aliyun_drive, crate::providers::aliyun::AliyunProvider);
    loader!(pan115_drive, crate::providers::pan115::Pan115Provider);
    loader!(pan123_drive, crate::providers::pan123::Pan123Provider);
    loader!(baidu_drive, crate::providers::baidu::BaiduProvider);
    loader!(cloud189_drive, crate::providers::cloud189::Cloud189Provider);

    // --------------------------------------------------------------- helpers ---

    /// Deterministic pseudo-random bytes: unique per run, incompressible enough
    /// that no drive answers with a rapid upload by accident.
    fn payload(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    struct Row {
        item: String,
        ok: bool,
        detail: String,
    }

    /// Records what the provider reports while it uploads.
    struct Recorder {
        bytes: AtomicU64,
        phases: std::sync::Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                bytes: AtomicU64::new(0),
                phases: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn phases(&self) -> Vec<String> {
            self.phases.lock().map(|v| v.clone()).unwrap_or_default()
        }

        fn bytes(&self) -> u64 {
            self.bytes.load(Ordering::Relaxed)
        }
    }

    impl UploadSink for Recorder {
        fn phase(&self, phase: &str) {
            if let Ok(mut list) = self.phases.lock() {
                if list.last().map(String::as_str) != Some(phase) {
                    list.push(phase.to_string());
                }
            }
        }

        fn progress(&self, bytes: u64) {
            self.bytes.fetch_max(bytes, Ordering::Relaxed);
        }
    }

    async fn download_all(drive: &Arc<dyn DriveProvider>, fid: &str) -> AppResult<Vec<u8>> {
        let mut handle = drive.open_download(fid, 0).await?;
        let mut out = Vec::with_capacity(handle.size as usize);
        while let Some(chunk) = handle.stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }

    /// Deletes everything below `dir` (files first, then folders bottom-up).
    async fn purge_dir(drive: &Arc<dyn DriveProvider>, dir: &str, depth: usize) -> AppResult<usize> {
        if depth > 8 {
            return Ok(0);
        }
        let entries = drive.list(dir).await?.entries;
        let mut removed = 0usize;
        for entry in entries.iter().filter(|e| !e.is_dir) {
            drive.delete(&[native_of(&entry.fid)]).await?;
            removed += 1;
        }
        for entry in entries.iter().filter(|e| e.is_dir) {
            removed += Box::pin(purge_dir(drive, &native_of(&entry.fid), depth + 1)).await?;
            drive.delete(&[native_of(&entry.fid)]).await?;
            removed += 1;
        }
        Ok(removed)
    }

    /// `relative path -> (native id, size)` for a remote tree.
    async fn remote_tree(
        drive: &Arc<dyn DriveProvider>,
        dir: &str,
        prefix: &str,
        out: &mut BTreeMap<String, (String, u64)>,
        depth: usize,
    ) -> AppResult<()> {
        if depth > 8 {
            return Ok(());
        }
        for entry in drive.list(dir).await?.entries {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            if entry.is_dir {
                Box::pin(remote_tree(
                    drive,
                    &native_of(&entry.fid),
                    &path,
                    out,
                    depth + 1,
                ))
                .await?;
            } else {
                out.insert(path, (native_of(&entry.fid), entry.size));
            }
        }
        Ok(())
    }

    struct Uploaded {
        fid: Option<String>,
        phases: Vec<String>,
        bytes: u64,
        ms: u128,
    }

    struct Probe {
        label: &'static str,
        drive: Arc<dyn DriveProvider>,
        /// Native id of `yunyu-probe-up-<pid>`.
        remote: String,
        local: PathBuf,
        rows: Vec<Row>,
        started: Instant,
    }

    impl Probe {
        async fn new(label: &'static str, drive: Arc<dyn DriveProvider>) -> AppResult<Self> {
            let remote_name = probe_name();
            // Leftovers from an aborted earlier run with the same pid are ours.
            for entry in drive
                .list(ROOT)
                .await?
                .entries
                .iter()
                .filter(|e| e.is_dir && e.name == remote_name)
            {
                let native = native_of(&entry.fid);
                let _ = purge_dir(&drive, &native, 0).await;
                let _ = drive.delete(&[native]).await;
            }
            let qualified = drive.make_dir(ROOT, &remote_name).await?;
            let local = std::env::temp_dir().join(format!("{remote_name}-local"));
            tokio::fs::create_dir_all(&local).await?;
            Ok(Self {
                label,
                drive,
                remote: native_of(&qualified),
                local,
                rows: Vec::new(),
                started: Instant::now(),
            })
        }

        fn record(&mut self, item: impl Into<String>, result: AppResult<String>) {
            let item = item.into();
            match result {
                Ok(detail) => self.rows.push(Row {
                    item,
                    ok: true,
                    detail,
                }),
                Err(err) => self.rows.push(Row {
                    item,
                    ok: false,
                    detail: err.to_string(),
                }),
            }
        }

        async fn upload(&self, local: &Path, parent: &str, name: &str) -> AppResult<Uploaded> {
            let sink = Recorder::new();
            let started = Instant::now();
            let fid = self
                .drive
                .upload(local, parent, name, sink.clone(), CancellationToken::new())
                .await?;
            Ok(Uploaded {
                fid,
                phases: sink.phases(),
                bytes: sink.bytes(),
                ms: started.elapsed().as_millis(),
            })
        }

        async fn entries(&self, parent: &str) -> AppResult<Vec<EntryDto>> {
            Ok(self.drive.list(parent).await?.entries)
        }

        /// Listing size + full byte comparison, the only trustworthy check that
        /// what the drive stored is what we meant to store. Some drives serve a
        /// stale listing right after an upload, so the entry is polled for up to
        /// 30 s and the observed visibility delay is reported.
        async fn read_back(&self, dir: &str, name: &str, expected: &[u8]) -> AppResult<String> {
            let started = Instant::now();
            let mut waited = 0u128;
            let hit = loop {
                let entries = self.entries(dir).await?;
                if let Some(hit) = entries.iter().find(|e| e.name == name) {
                    break hit.clone();
                }
                if started.elapsed() > Duration::from_secs(30) {
                    return Err(AppError::new(format!(
                        "列表里没有「{name}」（轮询 30 秒仍未出现）"
                    )));
                }
                waited = started.elapsed().as_millis();
                tokio::time::sleep(Duration::from_secs(2)).await;
            };
            if hit.size != expected.len() as u64 {
                return Err(AppError::new(format!(
                    "列表 size={} 与本地 {} 不一致",
                    hit.size,
                    expected.len()
                )));
            }
            let bytes = download_all(&self.drive, &native_of(&hit.fid)).await?;
            if bytes != expected {
                return Err(AppError::new(format!(
                    "下载内容不一致：下载 {} 字节，本地 {} 字节",
                    bytes.len(),
                    expected.len()
                )));
            }
            Ok(format!(
                "size={} 一致，逐字节下载一致（{} 字节）{}，fid={}",
                hit.size,
                bytes.len(),
                if waited > 0 {
                    format!("，列表延迟约 {waited}ms 才可见")
                } else {
                    String::new()
                },
                hit.fid
            ))
        }
    }

    /// The remote tree `upload_files` builds for one local folder: one remote
    /// dir per relative directory, `resolve_conflict` per file.
    async fn upload_tree(
        probe: &Probe,
        tree: &Path,
        settings: &Settings,
    ) -> AppResult<(usize, usize, String, Vec<u64>)> {
        let files = super::collect_local_files(tree).await?;
        let mut cache: HashMap<String, Vec<EntryDto>> = HashMap::new();
        let mut used: HashMap<String, Vec<String>> = HashMap::new();
        let root_name = tree
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("tree")
            .to_string();
        let folder =
            super::ensure_remote_dir(&probe.drive, &mut cache, &probe.remote, &root_name).await?;
        let mut uploaded = 0usize;
        let mut skipped = 0usize;
        let mut millis = Vec::new();
        for (relative, absolute) in files {
            let mut parent = folder.clone();
            if let Some(rel_parent) = relative.parent() {
                for segment in rel_parent.components() {
                    let piece = segment.as_os_str().to_string_lossy().to_string();
                    if piece.is_empty() {
                        continue;
                    }
                    parent =
                        super::ensure_remote_dir(&probe.drive, &mut cache, &parent, &piece).await?;
                }
            }
            let file_name = absolute
                .file_name()
                .and_then(|v| v.to_str())
                .unwrap_or("file")
                .to_string();
            if !used.contains_key(&parent) {
                let listing = probe.drive.list(&parent).await?;
                used.insert(
                    parent.clone(),
                    listing.entries.iter().map(|e| e.name.clone()).collect(),
                );
            }
            let names = used.get_mut(&parent).expect("populated above");
            let Some(name) = super::resolve_conflict(&file_name, names, settings) else {
                skipped += 1;
                continue;
            };
            names.push(name.clone());
            let started = Instant::now();
            probe
                .drive
                .upload(&absolute, &parent, &name, Recorder::new(), CancellationToken::new())
                .await?;
            millis.push(started.elapsed().as_millis() as u64);
            uploaded += 1;
        }
        Ok((uploaded, skipped, folder, millis))
    }

    /// 3 levels, 30 files: empty file, 10 MiB file, Chinese/emoji names.
    async fn build_tree(root: &Path, seed: u64) -> AppResult<()> {
        let level2 = root.join("中文 目录").join("level2");
        tokio::fs::create_dir_all(&level2).await?;
        tokio::fs::write(root.join("a.txt"), payload(1024, seed ^ 0x11)).await?;
        tokio::fs::write(root.join("empty.bin"), b"").await?;
        tokio::fs::write(
            root.join("中文 目录").join("b.txt"),
            payload(2048, seed ^ 0x22),
        )
        .await?;
        tokio::fs::write(
            root.join("中文 目录").join("😀表情.txt"),
            payload(1024, seed ^ 0x33),
        )
        .await?;
        for index in 0..26u64 {
            let path = level2.join(format!("f{index:02}.bin"));
            let bytes = match index {
                20 => payload(BIG, seed ^ index),
                25 => Vec::new(),
                _ => payload(1024 + index as usize * 37, seed ^ index),
            };
            tokio::fs::write(&path, bytes).await?;
        }
        Ok(())
    }

    fn report(probe: &Probe) -> String {
        let mut text = format!(
            "== {} 上传 live 探针 ==\n远端临时目录：{}/{}\n用时：{}ms\n",
            probe.label,
            ROOT,
            probe_name(),
            probe.started.elapsed().as_millis()
        );
        for row in &probe.rows {
            text.push_str(&format!(
                "{} | {} | {}\n",
                if row.ok { "PASS" } else { "FAIL" },
                row.item,
                row.detail.replace('\n', " ")
            ));
        }
        let failed = probe.rows.iter().filter(|r| !r.ok).count();
        text.push_str(&format!(
            "合计 {} 项，失败 {}\n",
            probe.rows.len(),
            failed
        ));
        text
    }

    async fn run(mut probe: Probe) -> (usize, String) {
        let remote = probe.remote.clone();
        let local = probe.local.clone();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5eed);
        let settings = crate::config::load_settings(&crate::config::AppPaths::resolve().unwrap());

        // ---- a1. 0 字节 ----
        let zero = payload(0, seed);
        let path = local.join("zero.bin");
        let result = async {
            tokio::fs::write(&path, &zero).await?;
            let up = probe.upload(&path, &remote, "zero.bin").await?;
            let detail = probe.read_back(&remote, "zero.bin", &zero).await?;
            Ok(format!(
                "{detail}；phases={:?}；{}ms；fid={:?}",
                up.phases, up.ms, up.fid
            ))
        }
        .await;
        probe.record("单文件：0 字节", result);

        // ---- a2. 1 MiB ----
        let one_mib = payload(1024 * 1024, seed ^ 0x1001);
        let path = local.join("onemib.bin");
        let result = async {
            tokio::fs::write(&path, &one_mib).await?;
            let up = probe.upload(&path, &remote, "onemib.bin").await?;
            let detail = probe.read_back(&remote, "onemib.bin", &one_mib).await?;
            Ok(format!(
                "{detail}；phases={:?}；{}ms；sink 记录 {} 字节",
                up.phases, up.ms, up.bytes
            ))
        }
        .await;
        probe.record("单文件：1 MiB", result);

        // ---- a3. 中文 + emoji 名 ----
        let odd_name = "中文 测试😀 #1.bin";
        let odd = payload(128 * 1024, seed ^ 0x2002);
        let path = local.join("odd.bin");
        let result = async {
            tokio::fs::write(&path, &odd).await?;
            let up = probe.upload(&path, &remote, odd_name).await?;
            let detail = probe.read_back(&remote, odd_name, &odd).await?;
            Ok(format!("{detail}；phases={:?}；{}ms", up.phases, up.ms))
        }
        .await;
        probe.record("单文件：中文/emoji 文件名", result);

        // ---- a4. 重名 ----
        let dup = payload(64 * 1024, seed ^ 0x3003);
        let dup_other = payload(64 * 1024, seed ^ 0x4004);
        let path = local.join("dup.bin");
        let result = async {
            tokio::fs::write(&path, &dup).await?;
            probe.upload(&path, &remote, "dup.bin").await?;
            let names: Vec<String> = probe
                .entries(&remote)
                .await?
                .iter()
                .map(|e| e.name.clone())
                .collect();
            let chosen = super::resolve_conflict("dup.bin", &names, &settings)
                .ok_or_else(|| AppError::new("重名策略为「跳过」，命令层会静默跳过"))?;
            let up = probe.upload(&path, &remote, &chosen).await?;
            let verified = probe.read_back(&remote, &chosen, &dup).await?;
            let renamed_phases = format!("{:?}", up.phases);
            // Same name again, bypassing the policy: does the drive overwrite,
            // keep two entries, or reject?
            tokio::fs::write(&path, &dup_other).await?;
            let raw = probe.upload(&path, &remote, "dup.bin").await?;
            let entries = probe.entries(&remote).await?;
            let same: Vec<&EntryDto> = entries.iter().filter(|e| e.name == "dup.bin").collect();
            let after = if same.len() == 1 {
                let bytes = download_all(&probe.drive, &native_of(&same[0].fid)).await?;
                if bytes == dup_other {
                    "只有 1 条，内容被覆盖"
                } else if bytes == dup {
                    "只有 1 条，内容仍是第一次"
                } else {
                    "只有 1 条，内容既非旧也非新"
                }
            } else {
                "出现多条同名条目"
            };
            Ok(format!(
                "重名策略={}，命令层选名「{chosen}」（phases={renamed_phases}）；{verified}；直接同名再传：{after}（共 {} 条 dup.bin）；phases={:?}；{}ms",
                settings.upload_conflict,
                same.len(),
                raw.phases,
                raw.ms
            ))
        }
        .await;
        probe.record("单文件：重名/不覆盖", result);

        // ---- b. rename + 再 list ----
        let result = async {
            let entries = probe.entries(&remote).await?;
            let hit = entries
                .iter()
                .find(|e| e.name == "onemib.bin")
                .ok_or_else(|| AppError::new("找不到 onemib.bin"))?;
            let fid = native_of(&hit.fid);
            let renamed = format!("renamed-{}.bin", std::process::id());
            probe.drive.rename(&fid, &renamed).await?;
            let entries = probe.entries(&remote).await?;
            if entries.iter().any(|e| e.name == "onemib.bin") {
                return Err(AppError::new("改名后旧名仍在列表里"));
            }
            let detail = probe.read_back(&remote, &renamed, &one_mib).await?;
            Ok(format!("→{renamed}；{detail}"))
        }
        .await;
        probe.record("改名 + 再 list + 内容核对", result);

        // ---- c. 文件夹上传 ----
        let tree = local.join("probe-tree");
        let mut tree_folder = String::new();
        let result = async {
            build_tree(&tree, seed).await?;
            let (uploaded, skipped, folder, millis) =
                upload_tree(&probe, &tree, &settings).await?;
            let files = super::collect_local_files(&tree).await?;
            let mut expected: BTreeMap<String, u64> = BTreeMap::new();
            for (relative, absolute) in &files {
                expected.insert(
                    format!("probe-tree/{}", relative.to_string_lossy().replace('\\', "/")),
                    tokio::fs::metadata(absolute).await?.len(),
                );
            }
            let mut actual: BTreeMap<String, (String, u64)> = BTreeMap::new();
            remote_tree(&probe.drive, &folder, "probe-tree", &mut actual, 0).await?;
            let missing: Vec<&String> = expected.keys().filter(|k| !actual.contains_key(*k)).collect();
            let extra: Vec<&String> = actual.keys().filter(|k| !expected.contains_key(*k)).collect();
            let wrong: Vec<String> = expected
                .iter()
                .filter_map(|(k, size)| match actual.get(k) {
                    Some((_, got)) if got != size => Some(format!("{k}: {got}≠{size}")),
                    _ => None,
                })
                .collect();
            if !missing.is_empty() || !extra.is_empty() || !wrong.is_empty() {
                return Err(AppError::new(format!(
                    "远端 {} 个文件 / 本地 {} 个：缺 {:?}，多 {:?}，大小不符 {:?}",
                    actual.len(),
                    expected.len(),
                    missing,
                    extra,
                    wrong
                )));
            }
            // Spot-check the bytes of a shallow and a deep file.
            for relative in ["a.txt", "中文 目录/level2/f00.bin"] {
                let key = format!("probe-tree/{relative}");
                let (fid, _) = actual.get(&key).expect("checked above");
                let want = tokio::fs::read(tree.join(relative)).await?;
                let got = download_all(&probe.drive, fid).await?;
                if got != want {
                    return Err(AppError::new(format!("抽查 {relative} 内容不一致")));
                }
            }
            tree_folder = folder.clone();
            let slowest = millis.iter().max().copied().unwrap_or(0);
            let total: u64 = millis.iter().sum();
            Ok(format!(
                "层数 3，本地 30 个文件（含空文件与 {} MiB 大文件，中文/emoji 目录名）；ensure_remote_dir 建目录 + 逐文件上传：上传 {uploaded}，跳过 {skipped}，远端 {}/{} 一致，抽查 2 个文件逐字节一致；串行总耗时 {}ms，单文件最慢 {}ms",
                BIG / 1024 / 1024,
                actual.len(),
                expected.len(),
                total,
                slowest
            ))
        }
        .await;
        probe.record("文件夹上传（3 层 / 30 文件）", result);

        // ---- c2. 重复执行同一个文件夹 ----
        if !tree_folder.is_empty() {
            let result = async {
                let (uploaded, skipped, folder, _) = upload_tree(&probe, &tree, &settings).await?;
                let mut actual: BTreeMap<String, (String, u64)> = BTreeMap::new();
                remote_tree(&probe.drive, &folder, "probe-tree", &mut actual, 0).await?;
                let copies = actual.keys().filter(|k| k.contains(" (2)")).count();
                Ok(format!(
                    "重复上传同一目录：新上传 {uploaded}，跳过 {skipped}，远端共 {} 个文件，其中带「 (2)」的副本 {copies} 个（策略 {}）",
                    actual.len(),
                    settings.upload_conflict
                ))
            }
            .await;
            probe.record("文件夹重复上传（是否产生副本）", result);
        }

        // ---- d. 50 MiB 上传中取消 ----
        let big_path = local.join("cancel-me.bin");
        let cancel_payload = payload(CANCEL_SIZE, seed ^ 0x7777);
        let result = async {
            tokio::fs::write(&big_path, &cancel_payload).await?;
            let drive = probe.drive.clone();
            let parent = remote.clone();
            let path = big_path.clone();
            let token = CancellationToken::new();
            let child = token.clone();
            let task = tokio::spawn(async move {
                drive
                    .upload(&path, &parent, "cancel-me.bin", Recorder::new(), child)
                    .await
            });
            tokio::time::sleep(Duration::from_secs(5)).await;
            if task.is_finished() {
                let outcome = match task.await {
                    Ok(Ok(_)) => "5 秒内已传完，未能测试中途取消".to_string(),
                    Ok(Err(err)) => format!("5 秒内已失败：{err}"),
                    Err(err) => format!("5 秒内任务崩溃：{err}"),
                };
                return Ok(outcome);
            }
            token.cancel();
            let outcome = match tokio::time::timeout(Duration::from_secs(120), task).await {
                Ok(Ok(Ok(fid))) => format!("取消后上传仍返回成功（fid={fid:?}）"),
                Ok(Ok(Err(err))) => format!("取消后返回错误：{err}"),
                Ok(Err(err)) => format!("任务崩溃：{err}"),
                Err(_) => "取消后 120 秒仍未返回".to_string(),
            };
            tokio::time::sleep(Duration::from_secs(3)).await;
            let left: Vec<String> = probe
                .entries(&remote)
                .await?
                .into_iter()
                .filter(|e| e.name.contains("cancel"))
                .map(|e| format!("{} size={}", e.name, e.size))
                .collect();
            Ok(format!(
                "{outcome}；3 秒后远端残留：{left:?}"
            ))
        }
        .await;
        probe.record("50 MiB 上传中取消（5 秒后）", result);

        // ---- d2. 取消后重新完整上传 ----
        let result = async {
            let up = probe
                .upload(&big_path, &remote, "cancel-ok.bin")
                .await?;
            let detail = probe
                .read_back(&remote, "cancel-ok.bin", &cancel_payload)
                .await?;
            Ok(format!("{detail}；{}ms；phases={:?}", up.ms, up.phases))
        }
        .await;
        probe.record("50 MiB 完整上传 + 内容核对", result);

        // ---- e. 秒传 / 极速上传 ----
        let rapid = payload(1024 * 1024, seed ^ 0x9999);
        let path = local.join("rapid.bin");
        let result = async {
            tokio::fs::write(&path, &rapid).await?;
            let first = probe.upload(&path, &remote, "rapid.bin").await?;
            let second = probe.upload(&path, &remote, "rapid.bin").await?;
            let entries = probe.entries(&remote).await?;
            let hits: Vec<&EntryDto> = entries.iter().filter(|e| e.name.starts_with("rapid")).collect();
            let sizes: Vec<u64> = hits.iter().map(|e| e.size).collect();
            Ok(format!(
                "首传 phases={:?} {}ms；二传 phases={:?} {}ms sink 记录 {} 字节（首传 {}）；列表名={:?} 大小={:?}",
                first.phases,
                first.ms,
                second.phases,
                second.ms,
                second.bytes,
                first.bytes,
                hits.iter().map(|e| e.name.clone()).collect::<Vec<_>>(),
                sizes
            ))
        }
        .await;
        probe.record("秒传/同内容二次上传", result);

        // ---- 清理 ----
        let result = async {
            let removed = purge_dir(&probe.drive, &remote, 0).await?;
            probe.drive.delete(std::slice::from_ref(&remote)).await?;
            let left = probe
                .entries(ROOT)
                .await?
                .into_iter()
                .filter(|e| e.name == probe_name())
                .count();
            let _ = tokio::fs::remove_dir_all(&local).await;
            if left != 0 {
                return Err(AppError::new(format!(
                    "远端仍残留 {left} 个同名目录"
                )));
            }
            Ok(format!("删除远端 {removed} 项 + 临时目录；根目录已无 {}", probe_name()))
        }
        .await;
        probe.record("清理", result);

        let failed = probe.rows.iter().filter(|r| !r.ok).count();
        (failed, report(&probe))
    }

    async fn run_drive(label: &'static str, drive: Option<Arc<dyn DriveProvider>>) {
        let Some(drive) = drive else {
            println!("SKIP {label}: 没有可用凭据");
            return;
        };
        let probe = match Probe::new(label, drive).await {
            Ok(probe) => probe,
            Err(err) => panic!("{label} 建立临时目录失败：{err}"),
        };
        let (failed, text) = run(probe).await;
        println!("{text}");
        let safe: String = label.chars().filter(|c| c.is_alphanumeric()).collect();
        let path = std::env::temp_dir().join(format!("yunyu-probe-report-{safe}.txt"));
        let _ = std::fs::write(&path, &text);
        println!("报告写入 {}", path.display());
        assert_eq!(failed, 0, "{label} 有 {failed} 项失败，详见上方输出");
    }

    #[tokio::test]
    #[ignore = "live: 需要真实夸克凭据"]
    async fn quark_live_upload_probe() {
        run_drive("夸克网盘", quark_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实移动云盘凭据"]
    async fn yun139_live_upload_probe() {
        run_drive("移动云盘139", yun139_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实阿里云盘凭据"]
    async fn aliyun_live_upload_probe() {
        run_drive("阿里云盘", aliyun_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实 115 凭据"]
    async fn pan115_live_upload_probe() {
        run_drive("115网盘", pan115_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实 123 凭据"]
    async fn pan123_live_upload_probe() {
        run_drive("123云盘", pan123_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实百度凭据"]
    async fn baidu_live_upload_probe() {
        run_drive("百度网盘", baidu_drive()).await;
    }

    #[tokio::test]
    #[ignore = "live: 需要真实天翼凭据"]
    async fn cloud189_live_upload_probe() {
        run_drive("天翼云盘189", cloud189_drive()).await;
    }
}

//! Share links, for every drive the app knows.
//!
//! The link is parsed here (each drive has its own shape), then the work is
//! handed to the drive itself: `DriveProvider::share_open` / `share_list` /
//! `share_save` / `share_link`. Keeping the per-drive protocol inside the
//! provider means the share screens need no credential handling of their own,
//! and a drive that cannot do something simply leaves the default
//! implementation in place.
//!
//! Quark-specific plumbing that the rest of the app also needs (cookie
//! freshness, the account profile) lives here because it predates the
//! multi-drive split and is still the only drive that needs it.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, REFERER, USER_AGENT};
use serde::Serialize;
use serde_json::Value;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::providers::ProviderId;
use crate::state::AppState;
use crate::transfer::{DownloadCtx, TransferTask};

pub const UA: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const QUARK_REFERER: &str = "https://pan.quark.cn/";
const QUARK_API: &str = "https://drive.quark.cn";
const QUARK_ACCOUNT_URL: &str = "https://pan.quark.cn/account/info?fr=pc&platform=pc";

// ---------------------------------------------------------------- plumbing ---

/// Shared client for the share endpoints.
///
/// No total `.timeout()` on purpose. That setting is a deadline for the whole
/// request — connect through the last body byte — and the same client streams
/// share downloads, so a large file used to die mid-flight with a timeout no
/// matter how healthy the connection was. `read_timeout` resets after every
/// successful read and therefore only trips when the transfer genuinely stalls.
pub fn http() -> AppResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))
}

/// Header map for a Quark API call.
pub fn quark_headers(cookie: &str) -> HeaderMap {
    let mut map = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(cookie) {
        map.insert(COOKIE, value);
    }
    map.insert(REFERER, HeaderValue::from_static(QUARK_REFERER));
    map.insert(USER_AGENT, HeaderValue::from_static(UA));
    map
}

/// Pulls `__puus` out of the `Set-Cookie` headers of a response.
pub fn puus_from(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(reqwest::header::SET_COOKIE).iter() {
        let raw = value.to_str().ok()?;
        if let Some(rest) = raw.strip_prefix("__puus=") {
            let token = rest.split(';').next()?.trim();
            if token.len() > 8 {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Replaces (or appends) one cookie pair inside a `a=b; c=d` header string.
pub fn merge_cookie(cookie: &str, name: &str, value: &str) -> String {
    let mut parts: Vec<String> = cookie
        .split(';')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .filter(|p| !p.starts_with(&format!("{name}=")))
        .collect();
    parts.push(format!("{name}={value}"));
    parts.join("; ")
}

/// Refreshes the stored Quark cookie with a fresh `__puus` and persists it.
///
/// Download URLs reject requests without a current `__puus`, and the cookie
/// saved by the login window frequently lacks it entirely.
pub async fn refresh_cookie(state: &AppState) -> Option<String> {
    let cookie = state.cookie_string().await.ok()?;
    let merged = ensure_puus(&cookie).await;
    if merged == cookie {
        return None;
    }
    let _ = state.persist_cookie(merged.clone()).await;
    Some(merged)
}

/// Returns the Quark cookie with a current `__puus` attached.
///
/// The download hosts (`dl-pc-*.quark.cn`) answer `412 Precondition Failed`
/// without it. `/1/clouddrive/config` always hands a new one out, while the
/// share download endpoint only sets `__sdid`, so this has to be an extra call.
pub async fn ensure_puus(cookie: &str) -> String {
    let Ok(client) = http() else {
        return cookie.to_string();
    };
    let Ok(res) = client
        .get(format!("{QUARK_API}/1/clouddrive/config?pr=ucpro&fr=pc"))
        .headers(quark_headers(cookie))
        .send()
        .await
    else {
        return cookie.to_string();
    };
    match puus_from(res.headers()) {
        Some(puus) => merge_cookie(cookie, "__puus", &puus),
        None => cookie.to_string(),
    }
}

/// True when the error means the download URL itself was rejected.
///
/// Every drive reports that case with this exact sentence so the transfer
/// engine can retry with a freshly minted link.
pub fn is_link_expired(err: &AppError) -> bool {
    err.0.contains("下载链接已失效")
}

/// Bytes of a URL-encoded string, decoded.
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ------------------------------------------------------------------- types ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub nickname: String,
    /// `data:` URL — the CSP does not allow remote images.
    pub avatar: Option<String>,
}

/// One row inside a share.
///
/// `token` is free-form: each drive stores whatever it needs to address the
/// entry again (Quark a `share_fid_token`, 123 an etag/s3 key, Baidu nothing
/// because the fs id is enough).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareEntry {
    pub fid: String,
    pub token: String,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub updated_at: i64,
}

/// An opened share link.
///
/// `stoken` is likewise drive-specific: the Quark stoken, the Baidu `sekey`,
/// the Aliyun `share_token`, the 123 share password, or nothing at all (139
/// re-authenticates with the link id and passcode).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareSession {
    /// `quark` / `yun139` / `aliyun` / `pan123` / `baidu`.
    pub provider: String,
    /// Share id as the drive spells it.
    pub pwd_id: String,
    pub passcode: String,
    pub stoken: String,
    pub title: String,
    pub owner: String,
    pub owner_avatar: Option<String>,
    pub file_num: u64,
    pub total_size: u64,
    pub expired_at: i64,
    pub first_fid: Option<String>,
    /// Whether "save to my drive" works for this drive.
    pub can_save: bool,
    /// Browser address of the share, for the "open in browser" button.
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareListing {
    pub pdir_fid: String,
    pub entries: Vec<ShareEntry>,
    pub total: u64,
}

/// A parsed share link.
#[derive(Debug, Clone)]
pub struct ShareTarget {
    pub provider: ProviderId,
    pub id: String,
    pub passcode: String,
}

// ------------------------------------------------------------- link parser ---

/// Accepts the several shapes a share link can arrive in.
pub fn parse_link(input: &str) -> AppResult<ShareTarget> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(AppError::new("请输入分享链接"));
    }
    let mut url = raw.to_string();
    // Our own link form; `deeplink::normalize` produces it from every other shape.
    let own_scheme = ["yunyu-drive://"]
        .into_iter()
        .find(|prefix| url.to_ascii_lowercase().starts_with(prefix));
    if let Some(scheme) = own_scheme {
        // <scheme>share?url=<encoded> or ?pwd_id=&passcode=
        let rest = url[scheme.len()..].to_string();
        let query = rest.split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
        let mut pwd_id = String::new();
        let mut passcode = String::new();
        let mut inner = String::new();
        for pair in query.split('&') {
            let Some((k, v)) = pair.split_once('=') else { continue };
            let value = percent_decode(v);
            match k {
                "url" | "link" => inner = value,
                "pwd_id" | "pwdid" => pwd_id = value,
                "passcode" | "pwd" => passcode = value,
                _ => {}
            }
        }
        if !inner.is_empty() {
            return parse_link(&inner);
        }
        if pwd_id.is_empty() {
            return Err(AppError::new("分享链接无效"));
        }
        return Ok(ShareTarget {
            provider: ProviderId::Quark,
            id: pwd_id,
            passcode,
        });
    }
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    let (before_hash, hash) = match url.split_once('#') {
        Some((head, tail)) => (head.to_string(), tail.to_string()),
        None => (url.clone(), String::new()),
    };
    let (path_part, query) = match before_hash.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (before_hash.clone(), String::new()),
    };
    let host = path_part
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let mut passcode = String::new();
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if k == "pwd" || k == "passcode" {
            passcode = percent_decode(v);
        }
    }

    // 139 puts the link id behind a fragment: `#/w/i/<id>`, and the passcode
    // sometimes rides in the fragment too (`#/w/i/<id>?pwd=xxxx`).
    if host.ends_with("139.com") {
        let tail = hash
            .split("/w/i/")
            .nth(1)
            .ok_or_else(|| AppError::new("没有从链接里找到分享 ID，请检查链接"))?;
        let id = tail
            .split(['/', '?'])
            .next()
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            return Err(AppError::new("没有从链接里找到分享 ID，请检查链接"));
        }
        if passcode.is_empty() {
            if let Some((_, frag_query)) = tail.split_once('?') {
                for pair in frag_query.split('&') {
                    let Some((k, v)) = pair.split_once('=') else { continue };
                    if k == "pwd" || k == "passcode" {
                        passcode = percent_decode(v);
                    }
                }
            }
        }
        return Ok(ShareTarget {
            provider: ProviderId::Yun139,
            id,
            passcode,
        });
    }

    if host.contains("123pan") || host.contains("123684") || host.contains("123865") {
        // `https://<uid>.share.123pan.cn/123pan/<key>?pwd=xxxx` or
        // `https://www.123pan.com/s/<key>`.
        let segments: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
        let id = segments
            .iter()
            .skip(1)
            .copied()
            .last()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::new("没有从链接里找到分享 ID，请检查链接"))?;
        return Ok(ShareTarget {
            provider: ProviderId::Pan123,
            id: id.to_string(),
            passcode,
        });
    }

    if host.contains("baidu.com") {
        // `https://pan.baidu.com/s/1Gb3CPF1uBTgEaH5BdXwbAQ` — the first
        // character of the last segment is a version marker, not part of the
        // surl.
        let segment = path_part
            .split("/s/")
            .nth(1)
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty() && !v.contains('/'))
            .ok_or_else(|| AppError::new("没有从链接里找到分享 ID，请检查链接"))?;
        let surl = segment
            .strip_prefix('1')
            .filter(|v| !v.is_empty())
            .unwrap_or(&segment)
            .to_string();
        return Ok(ShareTarget {
            provider: ProviderId::Baidu,
            id: surl,
            passcode,
        });
    }

    if host.contains("alipan.com") || host.contains("aliyundrive.com") {
        let id = path_part
            .split("/s/")
            .nth(1)
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty() && !v.contains('/'))
            .ok_or_else(|| AppError::new("没有从链接里找到分享 ID，请检查链接"))?;
        return Ok(ShareTarget {
            provider: ProviderId::Aliyun,
            id,
            passcode,
        });
    }

    if host.contains("quark.cn") {
        let pwd_id = path_part
            .split("/s/")
            .nth(1)
            .map(|v| v.trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty() && !v.contains('/'))
            .ok_or_else(|| AppError::new("没有从链接里找到分享 ID，请检查链接"))?;
        return Ok(ShareTarget {
            provider: ProviderId::Quark,
            id: pwd_id,
            passcode,
        });
    }

    Err(AppError::new("没有从链接里找到分享 ID，请检查链接"))
}

// ---------------------------------------------------------------- commands ---

/// Account nickname + avatar (Quark; downloaded and inlined as a data URL).
#[tauri::command]
pub async fn account_info(
    state: State<'_, Arc<AppState>>,
    force: Option<bool>,
) -> AppResult<Option<AccountInfo>> {
    if force != Some(true) {
        if let Some(cached) = state.account.read().await.clone() {
            return Ok(Some(cached));
        }
    }
    let cookie = state.cookie_string().await.map_err(|_| AppError::new("尚未登录"))?;
    let client = http()?;
    let res = client
        .get(QUARK_ACCOUNT_URL)
        .headers(quark_headers(&cookie))
        .send()
        .await?;
    let value: Value = res.json().await?;
    if value.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return Ok(None);
    }
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let nickname = data
        .get("nickname")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if nickname.is_empty() {
        return Ok(None);
    }
    let mut info = AccountInfo {
        nickname,
        avatar: None,
    };
    if let Some(url) = data.get("avatarUri").and_then(|v| v.as_str()) {
        info.avatar = fetch_avatar(&client, url).await;
    }
    *state.account.write().await = Some(info.clone());
    Ok(Some(info))
}

/// Downloads an avatar and inlines it as a `data:` URL — the app CSP does not
/// allow remote images.
pub async fn fetch_avatar(client: &reqwest::Client, url: &str) -> Option<String> {
    let url = decorate_avatar(url);
    let res = client.get(&url).send().await.ok()?;
    if !res.status().is_success() {
        return None;
    }
    let mime = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string())
        .filter(|v| v.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = res.bytes().await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    Some(format!("data:{mime};base64,{b64}"))
}

/// Quark's avatar URIs carry a trailing `;w,h` resize hint and a `;;` marker
/// before the object path; the CDN is happy with either scheme.
fn decorate_avatar(url: &str) -> String {
    let base = match url.rsplit_once(';') {
        Some((head, tail))
            if tail.len() < 12
                && tail.split(',').count() == 2
                && tail.split(',').all(|part| part.parse::<u32>().is_ok()) =>
        {
            head
        }
        _ => url,
    };
    base.replacen("http://", "https://", 1)
}

async fn drive_for(state: &Arc<AppState>, provider: &str) -> AppResult<Arc<dyn crate::providers::DriveProvider>> {
    let id = ProviderId::parse(provider)
        .ok_or_else(|| AppError::new(format!("未知的网盘：{provider}")))?;
    state.drive_of(id).await
}

/// Resolves a share link into a session (drive-specific).
#[tauri::command]
pub async fn share_open(state: State<'_, Arc<AppState>>, link: String) -> AppResult<ShareSession> {
    let target = parse_link(&link)?;
    let drive = state.drive_of(target.provider).await?;
    if !drive.capabilities().share_open {
        return Err(AppError::new(format!(
            "{}暂不支持打开分享链接",
            target.provider.display_name()
        )));
    }
    drive.share_open(&target.id, &target.passcode).await
}

/// Lists one folder inside a share.
#[tauri::command]
pub async fn share_list(
    state: State<'_, Arc<AppState>>,
    provider: Option<String>,
    pwd_id: String,
    stoken: String,
    pdir_fid: Option<String>,
    passcode: Option<String>,
) -> AppResult<ShareListing> {
    let provider = provider.unwrap_or_else(|| ProviderId::Quark.as_str().to_string());
    let drive = drive_for(&state, &provider).await?;
    let dir = pdir_fid.unwrap_or_else(|| "0".to_string());
    let share = crate::transfer::ShareRef {
        provider,
        pwd_id,
        stoken,
        fid_token: String::new(),
        passcode: passcode.unwrap_or_default(),
    };
    drive.share_list(&share, &dir).await
}

/// Saves share entries into the current user's drive.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn share_save(
    state: State<'_, Arc<AppState>>,
    provider: Option<String>,
    pwd_id: String,
    stoken: String,
    fids: Vec<String>,
    tokens: Vec<String>,
    to_pdir_fid: Option<String>,
    passcode: Option<String>,
) -> AppResult<u64> {
    if fids.is_empty() {
        return Err(AppError::new("请先选择要转存的内容"));
    }
    let provider = provider.unwrap_or_else(|| ProviderId::Quark.as_str().to_string());
    let drive = drive_for(&state, &provider).await?;
    let share = crate::transfer::ShareRef {
        provider,
        pwd_id,
        stoken,
        fid_token: String::new(),
        passcode: passcode.unwrap_or_default(),
    };
    let entries: Vec<ShareEntry> = fids
        .iter()
        .enumerate()
        .map(|(index, fid)| ShareEntry {
            fid: fid.clone(),
            token: tokens.get(index).cloned().unwrap_or_default(),
            name: String::new(),
            size: 0,
            is_dir: false,
            updated_at: 0,
        })
        .collect();
    drive.share_save(&share, &entries, to_pdir_fid.as_deref()).await
}

/// Downloads share entries: expands folders into their files and queues one
/// transfer per file, mirroring the personal-drive download behaviour.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn share_download(
    state: State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    provider: Option<String>,
    pwd_id: String,
    stoken: String,
    passcode: Option<String>,
    items: Vec<crate::providers::DownloadRequest>,
    dir: Option<String>,
) -> AppResult<Vec<TransferTask>> {
    if items.is_empty() {
        return Err(AppError::new("请先选择要下载的内容"));
    }
    let provider = provider.unwrap_or_else(|| ProviderId::Quark.as_str().to_string());
    let drive = drive_for(&state, &provider).await?;
    let settings = state.settings().await;
    let target = dir
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(settings.download_dir);
    if target.trim().is_empty() {
        return Err(AppError::new("请先在设置中选择下载目录"));
    }
    let base = crate::transfer::ShareRef {
        provider: provider.clone(),
        pwd_id,
        stoken,
        fid_token: String::new(),
        passcode: passcode.unwrap_or_default(),
    };

    let mut tasks = Vec::new();
    for item in items {
        if !item.is_dir {
            let share = crate::transfer::ShareRef {
                fid_token: item.token.clone().unwrap_or_default(),
                ..base.clone()
            };
            let job = crate::transfer::DownloadJob {
                fid: item.fid,
                name: item.name,
                size: item.size,
                dir: target.clone(),
                share: Some(share),
                provider: provider.clone(),
            };
            tasks.push(
                state
                    .transfers
                    .start_download(
                        app.clone(),
                        DownloadCtx::Share(drive.clone()),
                        job,
                    )
                    .await?,
            );
            continue;
        }

        let root = crate::paths::unique_path(
            &Path::new(&target).join(crate::paths::safe_component(&item.name)),
        );
        tokio::fs::create_dir_all(&root).await?;
        let files = drive.share_walk(&base, &item.fid).await?;
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
            let share = crate::transfer::ShareRef {
                fid_token: file.token,
                ..base.clone()
            };
            let job = crate::transfer::DownloadJob {
                fid: file.fid,
                name: file.name,
                size: file.size,
                dir: dir.to_string_lossy().to_string(),
                share: Some(share),
                provider: provider.clone(),
            };
            tasks.push(
                state
                    .transfers
                    .start_download(
                        app.clone(),
                        DownloadCtx::Share(drive.clone()),
                        job,
                    )
                    .await?,
            );
        }
    }
    if tasks.is_empty() {
        return Err(AppError::new("没有可下载的内容"));
    }
    Ok(tasks)
}

/// Fetches the first bytes of a share download link, to prove it is usable.
///
/// Returns a one-line description of the response; used by the live probes.
#[cfg(test)]
pub async fn probe_link(url: &str, headers: &[(String, String)]) -> AppResult<String> {
    let client = http()?;
    let mut request = client.get(url).header(reqwest::header::RANGE, "bytes=0-63");
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let res = request.send().await?;
    let status = res.status();
    let len = res.bytes().await.map(|b| b.len()).unwrap_or(0);
    Ok(format!("HTTP {} · {} bytes", status.as_u16(), len))
}

/// Fetches `url` with the headers the drive asked for, appending into `part`.
pub async fn stream_to_file(
    url: &str,
    headers: &[(String, String)],
    part: &Path,
    done: Arc<std::sync::atomic::AtomicU64>,
    token: &tokio_util::sync::CancellationToken,
) -> AppResult<u64> {
    use futures_util::StreamExt;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;

    let client = http()?;
    let start = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    let mut request = client.get(url);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if start > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
    }
    let res = request.send().await?;
    let status = res.status();
    if status == reqwest::StatusCode::FORBIDDEN
        || status == reqwest::StatusCode::PRECONDITION_FAILED
        || status == reqwest::StatusCode::UNAUTHORIZED
    {
        return Err(AppError::new("下载链接已失效（登录状态过期），请重新登录后再试"));
    }
    if !status.is_success() {
        return Err(AppError::new(format!("下载失败（HTTP {}）", status.as_u16())));
    }
    let append = start > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut file = if append {
        tokio::fs::OpenOptions::new().append(true).open(part).await?
    } else {
        tokio::fs::File::create(part).await?
    };
    let mut written = if append { start } else { 0 };
    done.store(written, Ordering::Relaxed);
    let mut stream = res.bytes_stream();
    loop {
        let next = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(AppError::new("已取消")),
            item = stream.next() => item,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        done.store(written, Ordering::Relaxed);
    }
    file.flush().await?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quark_share_links() {
        let target = parse_link("https://pan.quark.cn/s/af886c338574?pwd=6M1m").unwrap();
        assert_eq!(target.provider, ProviderId::Quark);
        assert_eq!(target.id, "af886c338574");
        assert_eq!(target.passcode, "6M1m");

        let target = parse_link("pan.quark.cn/s/abc123").unwrap();
        assert_eq!(target.id, "abc123");
        assert!(target.passcode.is_empty());

        let target = parse_link(
            "yunyu-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Fzzz%3Fpwd%3Dq1",
        )
        .unwrap();
        assert_eq!(target.id, "zzz");
        assert_eq!(target.passcode, "q1");

        assert!(parse_link("https://example.com/nothing").is_err());
    }

    #[test]
    fn parses_every_other_drive() {
        let target = parse_link("https://yun.139.com/shareweb/#/w/i/2yA8EKhoE66zy").unwrap();
        assert_eq!(target.provider, ProviderId::Yun139);
        assert_eq!(target.id, "2yA8EKhoE66zy");

        let target = parse_link("https://1818505815.share.123pan.cn/123pan/yYphjv-HgZXH?pwd=YfKu#").unwrap();
        assert_eq!(target.provider, ProviderId::Pan123);
        assert_eq!(target.id, "yYphjv-HgZXH");
        assert_eq!(target.passcode, "YfKu");

        let target = parse_link("https://www.alipan.com/s/iqj4mzirYtX").unwrap();
        assert_eq!(target.provider, ProviderId::Aliyun);
        assert_eq!(target.id, "iqj4mzirYtX");

        let target = parse_link("https://pan.baidu.com/s/1Gb3CPF1uBTgEaH5BdXwbAQ").unwrap();
        assert_eq!(target.provider, ProviderId::Baidu);
        assert_eq!(target.id, "Gb3CPF1uBTgEaH5BdXwbAQ");
    }

    #[test]
    fn merges_cookie_values() {
        let merged = merge_cookie("a=1; __puus=old; b=2", "__puus", "new");
        assert_eq!(merged, "a=1; b=2; __puus=new");
    }
}

//! 中国移动云盘 (139 / cloud139).
//!
//! Backed by the MIT-licensed `cloud139` crate, whose public API is *path*
//! addressed (`/父目录/文件.txt`) rather than id addressed. This adapter keeps a
//! `id -> path` map so the rest of the app can keep using opaque ids like every
//! other drive:
//!
//! * `"0"` is the root, mapped to `/`.
//! * Any other id is `<url-safe base64 of the path>`; it is decoded back to a
//!   path before every crate call. Encoding keeps ids opaque and free of `/`,
//!   which matters because they travel through JSON and appear in breadcrumbs.
//!
//! The map is filled lazily by [`Yun139Provider::list`] and refreshed on demand,
//! so a cold id (from a saved setting) still resolves: it is decoded, then
//! confirmed against the API.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use cloud139::client::{api as c139, auth as c139_auth, ClientError};
use cloud139::config::Config as C139Config;
use cloud139::services;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    qualify, AuthState, Capabilities, DirListing, DownloadHandle, DriveProvider, EntryDto,
    PathNode, ProviderId, QuotaInfo, SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};
use crate::error::{AppError, AppResult};
use crate::share::{AccountInfo, ShareCreated, ShareEntry, ShareListing, ShareRecord, ShareSession};

/// Marker id for the root folder.
const ROOT: &str = "0";

/// Where the sibling `cloud139` CLI keeps its config:
/// `~/.config/cloud139/cloud139rc.toml`.
fn cli_config_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    home.unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".config")
        .join("cloud139")
        .join("cloud139rc.toml")
}

pub struct Yun139Provider {
    config: RwLock<C139Config>,
    /// `fileId -> 绝对路径`, filled by listing and by `ensure_map`.
    paths: RwLock<HashMap<String, String>>,
    config_path: String,
    /// Profile (nickname + avatar) fetched from `user-njs`; `None` until asked.
    /// Kept separate from `config.account`, which is the login phone number.
    profile: RwLock<Option<AccountInfo>>,
    /// Set once a fetch ran, so a failure is not retried on every render.
    profile_tried: std::sync::atomic::AtomicBool,
    /// Numeric `userDomainId` from `user/getUser`. The share endpoints refuse
    /// every request without it (`200000401`), and `cloud139`'s config does not
    /// carry it, so it is fetched alongside the profile and cached here.
    domain_id: RwLock<Option<String>>,
}

impl Yun139Provider {
    /// Builds the adapter from a saved `cloud139rc.toml`.
    pub fn new(config: C139Config, config_path: String) -> Self {
        Self {
            config: RwLock::new(config),
            paths: RwLock::new(HashMap::new()),
            config_path,
            profile: RwLock::new(None),
            profile_tried: std::sync::atomic::AtomicBool::new(false),
            domain_id: RwLock::new(None),
        }
    }

    /// Reads the token from the app's own config directory, falling back to the
    /// path the sibling CLI uses so an existing `cloud139 login` keeps working.
    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let own = own_dir.join("yun139.toml");
        let text = if own.exists() {
            std::fs::read_to_string(&own)
                .map_err(|e| AppError::new(format!("读取 139 配置失败：{e}")))?
        } else {
            // The CLI stores its config globally; try that before giving up, so a
            // user who already ran `cloud139 login` does not have to sign in again.
            // Computed here rather than through `C139Config::config_path()`:
            // `lib.rs` points the crate's own save path at `yun139.toml`, so the
            // crate would answer with our file instead of the CLI's.
            let cli = cli_config_path();
            if !cli.exists() {
                return Err(AppError::new("尚未登录中国移动云盘"));
            }
            std::fs::read_to_string(&cli)
                .map_err(|e| AppError::new(format!("读取 139 配置失败：{e}")))?
        };
        let config: C139Config = toml::from_str(&text)
            .map_err(|e| AppError::new(format!("139 配置格式错误：{e}")))?;
        if config.authorization.trim().is_empty() {
            return Err(AppError::new("尚未登录中国移动云盘"));
        }
        Ok(Self::new(config, own.to_string_lossy().to_string()))
    }

    /// Saves the token into the app's config dir (mode 0600).
    /// Decodes and stores a 139 token, returning the config that was written.
    ///
    /// The account name is parsed out of the token because the drive API resolves
    /// the personal-cloud host by `accountName`: with an empty account every
    /// subsequent call fails, which is exactly what looked like "登录成功但连接失败".
    pub async fn save_token(own_dir: &Path, token: &str) -> AppResult<C139Config> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AppError::new("Token 不能为空"));
        }
        // The crate parses account / token info / expiry out of the bearer string.
        let config = match c139_auth::login(token, "personal_new", None).await {
            Ok(config) => config,
            // A web-harvested token can be shorter than the app's own; keep the
            // login usable by decoding the account ourselves.
            Err(_) => {
                let account = decode_account(token).ok_or_else(|| {
                    AppError::new("Token 格式不正确，应为 Basic 后面的那串 base64")
                })?;
                C139Config {
                    authorization: token.to_string(),
                    account,
                    storage_type: "personal_new".to_string(),
                    cloud_id: None,
                    custom_upload_part_size: 0,
                    report_real_size: true,
                    use_large_thumbnail: false,
                    personal_cloud_host: None,
                    refresh_token: None,
                    // Unknown expiry: say "far in the future" so the client does
                    // not try to refresh a token it cannot refresh.
                    token_expire_time: Some(chrono::Utc::now().timestamp_millis()
                        + 30 * 24 * 60 * 60 * 1000),
                    root_folder_id: None,
                    user_domain_id: None,
                }
            }
        };
        std::fs::create_dir_all(own_dir)
            .map_err(|e| AppError::new(format!("创建配置目录失败：{e}")))?;
        let path = own_dir.join("yun139.toml");
        let text = toml::to_string_pretty(&config)
            .map_err(|e| AppError::new(format!("序列化 139 配置失败：{e}")))?;
        std::fs::write(&path, text)
            .map_err(|e| AppError::new(format!("写入 139 配置失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        Ok(config)
    }

    async fn config(&self) -> C139Config {
        self.config.read().await.clone()
    }

    /// Encodes a path as an opaque id.
    fn encode(path: &str) -> String {
        if path == "/" || path.is_empty() {
            return ROOT.to_string();
        }
        let trimmed = path.trim_start_matches('/');
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(trimmed)
    }

    /// Decodes an id back to a path. Bare ids that are not our encoding fall
    /// through as literal paths, which keeps old saved values working.
    fn decode(id: &str) -> String {
        if id == ROOT || id.is_empty() {
            return "/".to_string();
        }
        match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(id) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) if !text.is_empty() => format!("/{}", text.trim_start_matches('/')),
                _ => id.to_string(),
            },
            Err(_) => id.to_string(),
        }
    }

    /// Resolves the path behind an id, creating a parent chain when the id came
    /// from saved state rather than a fresh listing.
    async fn path_of(&self, id: &str) -> String {
        // Tolerate a qualified id (`yun139:...`) as well as a bare one: the
        // command layer strips it, but tests and saved state occasionally do
        // not, and a qualified id decodes to garbage.
        let id = super::native_of(id);
        let id = id.as_str();
        if let Some(path) = self.paths.read().await.get(id) {
            return path.clone();
        }
        // No cache hit: the id is our own base64 of the path, so decoding is
        // the answer. The reverse map is not consulted here — it is keyed by
        // path, not by id.
        Self::decode(id)
    }

    /// Records `id -> path` for one entry.
    ///
    /// The id is `base64(path)` anyway, so a miss only costs a decode; this map
    /// exists so a listing does not have to re-derive paths for entries whose
    /// names contain characters we would encode differently.
    async fn remember(&self, id: &str, path: &str) {
        self.paths.write().await.insert(id.to_string(), path.to_string());
    }

    /// Rewrites the cache after the drive moved an entry somewhere else.
    ///
    /// Without this a rename or a move left the old path in the map, so the next
    /// download/rename/delete sent the drive a path that no longer exists
    /// (`File or directory not found`). Both the old and the new id are keyed by
    /// `base64(path)`, so the entry has to move between keys, not just values.
    async fn relocated(&self, old_id: &str, old_path: &str, new_path: &str) {
        let mut paths = self.paths.write().await;
        paths.remove(old_path);
        // Keep the stale id as an alias: the id *is* `base64(old path)`, so a
        // client that still holds it (a row the UI has not refreshed yet) would
        // otherwise decode straight back to the path that no longer exists.
        paths.insert(old_id.to_string(), new_path.to_string());
        paths.insert(Self::encode(new_path), new_path.to_string());
    }

    fn item_to_dto(&self, item: &cloud139::models::PersonalFileItem, dir: &str) -> EntryDto {
        let name = item.name.clone().unwrap_or_default();
        let is_dir = matches!(item.file_type.as_deref(), Some("folder") | Some("dir"))
            || item.file_type.as_deref() == Some("1");
        let modified = item
            .updated_at
            .as_deref()
            .or(item.update_date.as_deref())
            .or(item.last_modified.as_deref())
            .unwrap_or_default();
        let updated_at = super::parse_time(&serde_json::Value::String(modified.to_string()));
        let full = join_path(dir, &name);
        let native = Self::encode(&full);
        EntryDto {
            fid: qualify(ProviderId::Yun139, &native),
            pdir_fid: qualify(ProviderId::Yun139, &dir_id(dir)),
            ext: EntryDto::ext_of(&name),
            name,
            size: item.size.unwrap_or(0).max(0) as u64,
            is_dir,
            format_type: if is_dir { "folder".into() } else { "file".into() },
            updated_at,
            created_at: updated_at,
            provider: ProviderId::Yun139.as_str().to_string(),
            // 139 reports `thumbnailUrls: [{style, url}]`; the first entry is
            // the large one (`use_large_thumbnail` asks for it).
            thumb: item
                .thumbnail_urls
                .as_ref()
                .and_then(|list| list.iter().find_map(|t| t.url.clone()))
                .filter(|v| !v.is_empty()),
        }
    }
}

/// `dir_id` mirrors `Yun139Provider::encode` for use in `pdir_fid`.
fn dir_id(path: &str) -> String {
    Yun139Provider::encode(path)
}

/// Appends one entry name to a directory path, without doubling the slash.
fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn map_err(err: ClientError) -> AppError {
    AppError::new(err.to_string())
}

// ------------------------------------------------------- share plumbing ---
//
// The share domain is a different host and a different wire format: every
// request body is `base64(iv || AES-128-CBC(sorted JSON))` — a bare string, not
// a JSON object — and the response is either plain JSON (starts with `{`) or the
// same envelope encrypted. Three headers are mandatory; without them the server
// answers `9530 功能参数值非法` before it even looks at the body.

const SHARE_HOST: &str = "https://share-kd-njs.yun.139.com/yun-share";
const SHARE_AES_KEY: &[u8] = b"PVGDwmcvfs1uV3d1";
const SHARE_DEVICEINFO: &str = "||9|12.27.0|firefox|140.0|||linux unknown|1920X526|zh-CN|||";

impl Yun139Provider {
    /// One encrypted round trip to the share host.
    async fn share_post(&self, path: &str, body: &serde_json::Value) -> AppResult<serde_json::Value> {
        // Callers name the endpoint only; every one of them lives under this
        // prefix, and without it the host answers `404 Route Not Found`.
        let path = if path.starts_with("/richlifeApp/") {
            path.to_string()
        } else {
            format!("/richlifeApp/devapp{path}")
        };
        let config = self.config().await;
        // Key order matters for the ciphertext (not for the server, which parses
        // the decrypted JSON), and `cloud139` already sorts keys the same way.
        let plaintext = cloud139::utils::sort_json_value_to_string(body);
        let iv = random_iv();
        let cipher = cloud139::utils::aes_cbc_encrypt(
            plaintext.as_bytes(),
            SHARE_AES_KEY,
            &iv,
        )
        .map_err(|e| AppError::new(format!("139 分享请求加密失败：{e}")))?;
        let mut payload = iv.clone();
        payload.extend_from_slice(&cipher);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);
        let headers = yun_headers(&config.authorization);

        let res = crate::share::http()?
            .post(format!("{SHARE_HOST}{path}"))
            .headers(headers)
            .body(encoded)
            .send()
            .await?;
        let status = res.status();
        let raw = res.bytes().await?;
        let value = if raw.first() == Some(&b'{') {
            serde_json::from_slice::<serde_json::Value>(&raw).map_err(|_| {
                AppError::new(format!("139 分享接口返回异常（HTTP {}）", status.as_u16()))
            })?
        } else {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(&raw)
                .map_err(|_| {
                    AppError::new(format!("139 分享响应解码失败（HTTP {}）", status.as_u16()))
                })?;
            if decoded.len() < 32 {
                return Err(AppError::new("139 分享响应长度异常"));
            }
            let plain = cloud139::utils::aes_cbc_decrypt(
                &decoded[16..],
                SHARE_AES_KEY,
                &decoded[..16],
            )
            .map_err(|e| AppError::new(format!("139 分享响应解密失败：{e}")))?;
            serde_json::from_slice::<serde_json::Value>(&plain)
                .map_err(|_| AppError::new("139 分享响应不是合法 JSON"))?
        };
        let code = value
            .get("resultCode")
            .or_else(|| value.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if code != "0" && !code.is_empty() {
            let desc = value
                .get("desc")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let message = match code {
                "9188" => "提取码不正确".to_string(),
                "200000727" => "分享链接不存在或已被取消".to_string(),
                "200000401" => "139 登录已过期，请重新登录".to_string(),
                "200000400" => "分享参数不完整（缺少目录）".to_string(),
                "9530" => "139 拒绝了这次分享请求（请求头不完整）".to_string(),
                _ if !desc.is_empty() => format!("{desc}（code={code}）"),
                _ => format!("139 分享接口返回异常（code={code}）"),
            };
            return Err(AppError::new(message));
        }
        #[cfg(test)]
        if std::env::var("YUN139_DUMP").is_ok() {
            eprintln!("[139 raw] {value}");
        }
        Ok(value.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }

    /// Raw JSON (not the `data` envelope) of one share call, for probes.
    #[cfg(test)]
    pub async fn debug_share_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> serde_json::Value {
        match self.share_post(path, body).await {
            Ok(v) => v,
            Err(err) => serde_json::json!({ "error": err.to_string() }),
        }
    }

    #[cfg(test)]
    pub async fn debug_account(&self) -> String {
        self.config().await.account
    }

    /// `(name, fileId)` for every child of `dir`, for probes.
    ///
    /// The adapter addresses 139 by path, but the share API wants the opaque
    /// `fileId`/`catalogID`, so a probe needs this to reach it at all.
    #[cfg(test)]
    pub async fn debug_children(&self, dir: &str) -> Vec<(String, String)> {
        let config = self.config().await;
        let path = self.path_of(dir).await;
        let Ok(file_id) = resolve_file_id(&config, &path).await else {
            return Vec::new();
        };
        match c139::list_personal_files(&config, &file_id).await {
            Ok(items) => items
                .into_iter()
                .map(|i| {
                    (
                        i.name.unwrap_or_default(),
                        i.file_id.unwrap_or_default(),
                    )
                })
                .collect(),
            Err(err) => {
                println!("list failed: {err}");
                Vec::new()
            }
        }
    }

    /// Nickname + avatar from `user-njs` (the host the web app's
    /// `getUserAvatar` calls).
    ///
    /// Separate host *and* wire format from the share domain: a 32-byte AES key
    /// here (`UqEZkrjCKfa02pP6jntzFmkzOz86zHUC`) instead of 16, and the response
    /// is a bare base64 blob rather than a JSON string. `cloud139` cannot help —
    /// its helper is hard-wired to AES-128 — hence the local AES-256 below.
    async fn fetch_profile(&self) -> AppResult<AccountInfo> {
        const PROFILE_HOST: &str = "https://user-njs.yun.139.com";
        const PROFILE_KEY: &[u8] = b"UqEZkrjCKfa02pP6jntzFmkzOz86zHUC";
        let config = self.config().await;
        if config.authorization.trim().is_empty() {
            return Err(AppError::new("尚未登录 139 云盘"));
        }
        let iv = random_iv();
        let cipher = aes256_cbc_encrypt(b"{}", PROFILE_KEY, &iv)
            .map_err(|e| AppError::new(format!("139 资料请求加密失败：{e}")))?;
        let mut payload = iv.clone();
        payload.extend_from_slice(&cipher);
        let body = base64::engine::general_purpose::STANDARD.encode(&payload);
        let res = crate::share::http()?
            .post(format!("{PROFILE_HOST}/user/getUser"))
            .headers(yun_headers(&config.authorization))
            .body(body)
            .send()
            .await?;
        let status = res.status();
        let raw = res.bytes().await?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&raw)
            .map_err(|_| AppError::new(format!("139 资料响应解码失败（HTTP {}）", status.as_u16())))?;
        if decoded.len() < 32 {
            return Err(AppError::new("139 资料响应长度异常"));
        }
        let plain = aes256_cbc_decrypt(&decoded[16..], PROFILE_KEY, &decoded[..16])
            .map_err(|e| AppError::new(format!("139 资料响应解密失败：{e}")))?;
        let value: serde_json::Value = serde_json::from_slice(&plain)
            .map_err(|_| AppError::new("139 资料响应不是合法 JSON"))?;
        if value.get("success").and_then(|v| v.as_bool()) != Some(true) {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("139 资料接口返回失败");
            return Err(AppError::new(message.to_string()));
        }
        let info = value
            .pointer("/data/userProfileInfo")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        // Same response carries the numeric domain id the share host insists on;
        // stash it so `create_share` does not need a second round trip.
        let domain = value
            .pointer("/data/userDomainId")
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .filter(|v| !v.is_empty() && v != "null");
        if let Some(domain) = domain {
            *self.domain_id.write().await = Some(domain);
        }
        // `auditNickName` is the nickname that passed review (the one the web app
        // and the phone show); `userName` is the masked phone number.
        let nickname = text_of(&info, "auditNickName");
        let nickname = if nickname.is_empty() {
            text_of(&info, "userName")
        } else {
            nickname
        };
        let mut avatar = None;
        for key in ["bigAvatarUrl", "avatarUrl", "miniUserAvatarUrl"] {
            let url = text_of(&info, key);
            if url.is_empty() {
                continue;
            }
            avatar = crate::share::fetch_avatar(&crate::share::http()?, &url).await;
            if avatar.is_some() {
                break;
            }
        }
        if nickname.is_empty() && avatar.is_none() {
            return Err(AppError::new("139 资料接口没有返回昵称"));
        }
        Ok(AccountInfo { nickname, avatar })
    }

    /// The numeric `userDomainId` the share host requires.
    ///
    /// Every `share-kd-njs` call answers `200000401 IP鉴权失败、用户账号鉴权失败`
    /// without it, and no header or `commonAccountInfo` substitute works: the
    /// value belongs inside the request body as a plain number. It arrives with
    /// the profile response, so this piggybacks on `fetch_profile`.
    async fn user_domain_id(&self) -> AppResult<String> {
        if let Some(cached) = self.domain_id.read().await.clone() {
            return Ok(cached);
        }
        // A failed fetch leaves the cache empty; the caller gets the error and
        // the next attempt retries, which is what we want after a re-login.
        self.fetch_profile().await?;
        self.domain_id
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::new("139 没有返回账号标识，请重新登录"))
    }

    /// Splits a selection into the opaque ids `getOutLink` wants.
    ///
    /// 139 keeps files and folders in separate lists (`coIDLst` / `caIDLst`),
    /// and the ids are neither paths nor our encoded paths — they are the
    /// server's own `fileId`, so each one is resolved from its parent listing.
    async fn share_targets(&self, fids: &[String]) -> AppResult<(Vec<String>, Vec<String>)> {
        let config = self.config().await;
        let mut files = Vec::new();
        let mut folders = Vec::new();
        for fid in fids {
            let path = self.path_of(fid).await;
            let file_id = resolve_file_id(&config, &path).await?;
            if file_id.is_empty() || file_id == "/" {
                return Err(AppError::new("139 不能直接分享根目录"));
            }
            let name = title_of(&path);
            let parent = match path.trim_end_matches('/').rsplit_once('/') {
                Some((head, _)) if !head.is_empty() => head.to_string(),
                _ => "/".to_string(),
            };
            let parent_id = resolve_file_id(&config, &parent).await?;
            let parent_id = if parent_id.is_empty() { "/".to_string() } else { parent_id };
            let items = c139::list_personal_files(&config, &parent_id)
                .await
                .map_err(map_err)?;
            let is_dir = items
                .iter()
                .find(|item| item.name.as_deref() == Some(name.as_str()))
                .map(|item| {
                    matches!(item.file_type.as_deref(), Some("folder") | Some("dir"))
                        || item.file_type.as_deref() == Some("1")
                })
                .unwrap_or(false);
            if is_dir {
                folders.push(file_id);
            } else {
                files.push(file_id);
            }
        }
        Ok((files, folders))
    }

    /// `getOutLinkInfoV6` — the listing *and* the share metadata.
    async fn share_info(
        &self,
        link_id: &str,
        passwd: &str,
        parent: &str,
    ) -> AppResult<serde_json::Value> {
        let config = self.config().await;
        let body = serde_json::json!({
            "getOutLinkInfoReq": {
                "account": config.account,
                "linkID": link_id,
                "passwd": passwd,
                "pCaID": parent,
            }
        });
        self.share_post("/richlifeApp/devapp/IOutLink/getOutLinkInfoV6", &body)
            .await
    }

    /// Waits for a share-save batch task to finish.
    async fn poll_share_task(&self, account: &str, task: &str) -> AppResult<()> {
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let body = serde_json::json!({
                "queryBatchOprTaskDetailReq": { "taskID": task, "msisdn": account }
            });
            let Ok(data) = self
                .share_post("/IBatchOprTask/queryBatchOprTaskDetail", &body)
                .await
            else {
                continue;
            };
            let status = data
                .pointer("/batchOprTask/taskStatus")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if status == 2 {
                let result = data
                    .pointer("/batchOprTask/taskResultCode")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if result == 1 {
                    return Ok(());
                }
                return Err(AppError::new("139 转存失败，请稍后重试"));
            }
            if status < 0 {
                return Err(AppError::new("139 转存失败，请稍后重试"));
            }
        }
        Err(AppError::new("转存超时，请稍后在「我的文件」中查看"))
    }
}

fn text_of(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Last path segment, which is the file name for a 139 id.
fn title_of(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `ctTime`/`expireTime` are `%Y%m%d%H%M%S` local-time strings; an empty one
/// means "never", which the UI renders as 永久.
fn stamp(raw: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(raw, "%Y%m%d%H%M%S")
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

/// `udTime`/`ctTime` are `%Y%m%d%H%M%S` local-time strings.
fn ud_time(item: &serde_json::Value) -> i64 {
    for key in ["udTime", "ctTime"] {
        if let Some(raw) = item.get(key).and_then(|v| v.as_str()) {
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(raw, "%Y%m%d%H%M%S") {
                return dt.and_utc().timestamp_millis();
            }
        }
    }
    0
}

/// A fresh 16-byte IV from the OS.
fn random_iv() -> Vec<u8> {
    use std::io::Read as _;
    let mut iv = vec![0u8; 16];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut iv).is_ok() {
            return iv;
        }
    }
    // Fallback: hash the clock so the IV is at least not constant.
    use sha2::Digest as _;
    let seed = format!("{:?}", std::time::SystemTime::now());
    let digest = sha2::Sha256::digest(seed.as_bytes());
    iv.copy_from_slice(&digest[..16]);
    iv
}

/// Headers every `*.yun.139.com` host wants.
///
/// Three of them are load-bearing: without `Content-Type: application/json`,
/// `X-Deviceinfo` (client type `9`, version >= `12.27.0`) or `hcy-cool-flag`
/// the share host answers `9530 功能参数值非法` before reading the body.
fn yun_headers(authorization: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    let mut put = |key: &'static str, value: &str| {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(value) {
            headers.insert(key, v);
        }
    };
    put("Content-Type", "application/json;charset=UTF-8");
    put("X-Deviceinfo", SHARE_DEVICEINFO);
    put("hcy-cool-flag", "1");
    put("CMS-DEVICE", "default");
    put("x-m4c-caller", "PC");
    // The share host wants this on every call. It is not a flag we control: the
    // literal string `enabled` tells 139 to look the account up from the body's
    // `userDomainId`, and omitting it fails with `200000401`.
    put("x-yun-userDomainId", "enabled");
    put("X-Yun-Api-Version", "v1");
    put("Origin", "https://yun.139.com");
    put("Referer", "https://yun.139.com/");
    put(reqwest::header::USER_AGENT.as_str(), crate::share::UA);
    if !authorization.trim().is_empty() {
        // The stored value carries no scheme prefix (the crate adds `Basic `
        // itself), but these hosts want the whole header.
        let auth = authorization.trim();
        let value = if auth.to_ascii_lowercase().starts_with("basic ") {
            auth.to_string()
        } else {
            format!("Basic {auth}")
        };
        put("Authorization", &value);
    }
    headers
}

/// AES-256-CBC + PKCS7, the profile host's wire format.
fn aes256_cbc_encrypt(plain: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    let cipher = cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv).map_err(|e| e.to_string())?;
    Ok(cipher.encrypt_padded_vec_mut::<Pkcs7>(plain))
}

fn aes256_cbc_decrypt(cipher_text: &[u8], key: &[u8], iv: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    let cipher = cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv).map_err(|e| e.to_string())?;
    cipher
        .decrypt_padded_vec_mut::<Pkcs7>(cipher_text)
        .map_err(|e| e.to_string())
}

#[async_trait]
impl DriveProvider for Yun139Provider {
    fn id(&self) -> ProviderId {
        ProviderId::Yun139
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            share_open: true,
            // `IOutLink/getOutLink` on the share host. It needs a header
            // (`x-yun-userDomainId: enabled`) and a numeric `userDomainId` in the
            // body; without either it answers `200000401`, which looks exactly
            // like an expired login. See `docs/provider-login.md`.
            share_create: true,
            move_files: true,
            copy: true,
            search: true,
            rename: true,
            folders: true,
            recycle: false,
        }
    }

    async fn auth_state(&self) -> AuthState {
        let config = self.config().await;
        let profile = self.profile.read().await.clone();
        AuthState {
            provider: ProviderId::Yun139,
            authenticated: !config.authorization.trim().is_empty(),
            source: "file".to_string(),
            masked: Some(mask_token(&config.authorization)),
            credential_path: self.config_path.clone(),
            nickname: profile
                .as_ref()
                .map(|p| p.nickname.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| Some(config.account.clone()).filter(|a| !a.is_empty())),
            avatar: profile.and_then(|p| p.avatar),
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        let path = Path::new(&self.config_path);
        if path.exists() {
            std::fs::remove_file(path)
                .map_err(|e| AppError::new(format!("删除 139 凭据失败：{e}")))?;
        }
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let config = self.config().await;
        if config.account.is_empty() {
            return Ok(None);
        }
        // Cached profile first: `profile()` is the only thing that hits the
        // network, and it runs once per process.
        if let Some(cached) = self.profile.read().await.clone() {
            return Ok(Some(cached));
        }
        Ok(Some(AccountInfo {
            nickname: config.account.clone(),
            avatar: None,
        }))
    }

    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        if let Some(cached) = self.profile.read().await.clone() {
            return Ok(Some(cached));
        }
        if self
            .profile_tried
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return self.account().await;
        }
        match self.fetch_profile().await {
            Ok(info) => {
                *self.profile.write().await = Some(info.clone());
                Ok(Some(info))
            }
            Err(err) => {
                crate::ylog!("139 profile fetch failed: {err}");
                self.account().await
            }
        }
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let config = self.config().await;
        let path = self.path_of(dir).await;
        let file_id = resolve_file_id(&config, &path).await?;
        let items = c139::list_personal_files(&config, &file_id).await
            .map_err(map_err)?;
        let mut entries = Vec::with_capacity(items.len());
        for item in &items {
            let dto = self.item_to_dto(item, &path);
            // Cache `child id -> child path`. Recording the parent here made
            // every later operation on a child (download, rename, delete)
            // resolve to the *directory* that contained it.
            let child = join_path(&path, &dto.name);
            self.remember(&super::native_of(&dto.fid), &child)
                .await;
            entries.push(dto);
        }
        let total = entries.len() as u32;
        super::sort_entries(&mut entries);
        Ok(DirListing {
            // Echo the *encoded path*, not the raw argument: entries below report
            // `dir_id(path)` as their parent, and the two must agree so the UI can
            // match a listing against the folder it opened.
            pdir_fid: qualify(ProviderId::Yun139, &Self::encode(&path)),
            entries,
            total,
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        let path = self.path_of(dir).await;
        if path == "/" {
            return Ok(Vec::new());
        }
        // Walk the real listings instead of splitting the decoded path: after a
        // rename or a move the encoded path is stale, and inventing breadcrumbs
        // from it showed folders that no longer exist under that name. Walking
        // also stops at the first segment that is gone, so the trail never
        // claims more than the drive agrees with.
        let segments: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let mut nodes = Vec::new();
        let mut acc = String::new();
        let mut parent = Self::encode("/");
        for segment in segments {
            acc.push('/');
            acc.push_str(segment);
            let listing = self.list(&parent).await?;
            let Some(hit) = listing.entries.iter().find(|e| e.name == segment) else {
                break;
            };
            nodes.push(PathNode {
                fid: hit.fid.clone(),
                name: hit.name.clone(),
            });
            parent = Self::encode(&acc);
        }
        Ok(nodes)
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = super::check_name(name)?;
        let config = self.config().await;
        let parent_path = self.path_of(parent).await;
        let full = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };
        services::mkdir_service::mkdir(&config, &full, true)
            .await
            .map_err(map_err)?;
        self.remember(&Self::encode(&full), &full).await;
        Ok(Self::encode(&full))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = super::check_name(name)?;
        let config = self.config().await;
        let path = self.path_of(fid).await;
        services::rename_service::rename(&config, &path, name, true)
            .await
            .map_err(map_err)?;
        let parent = path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        let parent = if parent.is_empty() { "/" } else { parent };
        self.relocated(fid, &path, &join_path(parent, name)).await;
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let config = self.config().await;
        // Delete one by one (the crate has no batch call) but keep going: aborting
        // on the first failure reported the whole selection as failed even though
        // earlier files were already gone.
        let mut failures: Vec<String> = Vec::new();
        for fid in fids {
            let path = self.path_of(fid).await;
            if let Err(e) = services::delete_service::delete(&config, &path, false).await {
                failures.push(format!("{path}：{e}"));
            }
        }
        if !failures.is_empty() {
            return Err(AppError::new(format!(
                "有 {} 项删除失败：{}",
                failures.len(),
                failures.join("；")
            )));
        }
        Ok(())
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let config = self.config().await;
        let target = self.path_of(to).await;
        let mut sources = Vec::with_capacity(fids.len());
        for fid in fids {
            sources.push(self.path_of(fid).await);
        }
        if copy {
            services::copy_service::cp(&config, &sources, &target, true)
                .await
                .map_err(map_err)?;
        } else {
            services::move_service::mv(&config, &sources, &target, true)
                .await
                .map_err(map_err)?;
            // The copies the drive made keep their own ids, but the originals
            // are gone from where they were: re-point their cache entries at the
            // destination so later operations do not chase a stale path.
            for (fid, source) in fids.iter().zip(sources.iter()) {
                let name = source.rsplit('/').next().unwrap_or(source);
                self.relocated(fid, source, &join_path(&target, name)).await;
            }
        }
        Ok(fids.len())
    }

    async fn search(&self, dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // 139 has no server-side search in the crate, so walk breadth-first the
        // same way the Quark adapter does.
        let mut hits = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((dir.to_string(), Vec::<PathNode>::new()));
        let mut visited = 0u32;
        while let Some((fid, path)) = queue.pop_front() {
            visited += 1;
            if visited > 80 || hits.len() >= limit as usize {
                break;
            }
            let Ok(listing) = self.list(&fid).await else {
                continue;
            };
            for entry in listing.entries {
                let child_path = {
                    let mut p = path.clone();
                    p.push(PathNode {
                        fid: entry.fid.clone(),
                        name: entry.name.clone(),
                    });
                    p
                };
                if entry.name.to_lowercase().contains(&needle) {
                    // `child_path` was already built above; the hit itself is the
                    // last node so the UI can show `path.slice(0, -1)` as its folder.
                    hits.push(SearchHit {
                        entry: entry.clone(),
                        path: child_path.clone(),
                    });
                    if hits.len() >= limit as usize {
                        break;
                    }
                }
                if entry.is_dir && path.len() < 8 {
                    queue.push_back((super::native_of(&entry.fid), child_path));
                }
            }
        }
        Ok(hits)
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        // The crate has no capacity call. The web client asks the orchestrator
        // for `diskSize`/`freeDiskSize` with the same signed headers every other
        // 139 call uses, so we do the same and stay silent on any failure: the
        // sidebar simply shows no bar rather than an invented number.
        let config = self.config().await;
        let url = "https://yun.139.com/orchestration/personalCloud/user/v1.0/getDiskInfo";
        // The web client's request interceptor adds `commonAccountInfo` to every
        // body and the vuex action itself passes `account`, so the wire body
        // carries both. The raw response nests the sizes one level deeper than
        // the store ends up exposing (`data.data`, normalised into `data.diskInfo`).
        let body = serde_json::json!({
            "account": config.account,
            "commonAccountInfo": { "account": config.account, "accountType": 1 }
        });
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let rand_str = cloud139::client::generate_rand_str(16);
        let sign = cloud139::utils::crypto::calc_sign(&body.to_string(), &ts, &rand_str);
        let Ok(headers) = cloud139::client::headers::build_signed_headers(
            &config.authorization,
            &ts,
            &rand_str,
            &sign,
            "1",
        ) else {
            return Ok(None);
        };
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let Ok(res) = client.post(url).headers(headers).json(&body).send().await else {
            return Ok(None);
        };
        let Ok(value) = res.json::<serde_json::Value>().await else {
            return Ok(None);
        };
        let total = value
            .pointer("/data/diskInfo/diskSize")
            .or_else(|| value.pointer("/data/data/diskSize"))
            .and_then(|v| v.as_u64());
        let free = value
            .pointer("/data/diskInfo/freeDiskSize")
            .or_else(|| value.pointer("/data/data/freeDiskSize"))
            .and_then(|v| v.as_u64());
        // `diskSize`/`freeDiskSize` are megabytes, not bytes: the web client
        // renders them through `c(1024 * e * 1024)`, i.e. it scales them to
        // bytes before formatting. Treating them as bytes showed "420 KB" for
        // a 420 GB drive.
        const MB: u64 = 1024 * 1024;
        match (free, total) {
            (Some(free), Some(total)) if total > 0 && free <= total => Ok(Some(QuotaInfo {
                used: (total - free).saturating_mul(MB),
                total: total.saturating_mul(MB),
            })),
            _ => Ok(None),
        }
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let config = self.config().await;
        let path = self.path_of(fid).await;
        let file_id = resolve_file_id(&config, &path).await?;
        let url = c139::get_personal_download_link(&config, &file_id)
            .await
            .map_err(map_err)?;
        if url.is_empty() {
            return Err(AppError::new("没有拿到下载地址"));
        }
        // 139's CDN honours Range, so resume works the same as everywhere else.
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let mut request = client.get(&url);
        if start > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let res = request
            .send()
            .await
            .map_err(|e| AppError::new(format!("下载失败：{e}")))?;
        if !res.status().is_success() {
            return Err(AppError::new(format!(
                "下载失败（HTTP {}）",
                res.status().as_u16()
            )));
        }
        // A CDN that ignores `Range` answers 200 with the whole body. Appending
        // that to an existing `.part` would corrupt the file, so refuse: the
        // caller drops the partial and starts over.
        if start > 0 && res.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(RANGE_IGNORED));
        }
        let size = res.content_length().unwrap_or(0);
        let stream = res.bytes_stream().map(|chunk| {
            chunk.map_err(|e| AppError::new(format!("读取失败：{e}")))
        });
        Ok(DownloadHandle {
            size,
            md5: None,
            stream: Box::new(stream),
        })
    }

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        let mut config = self.config().await;
        let parent_path = self.path_of(parent).await;
        let remote = if parent_path == "/" {
            format!("/{name}")
        } else {
            format!("{parent_path}/{name}")
        };
        let size = tokio::fs::metadata(local)
            .await
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?
            .len();

        // Hashing first, like every other adapter: the UI shows 校验中 and the
        // server needs the SHA-256 before it will hand out part URLs.
        sink.phase("hashing");
        let content_hash = sha256_file(local, sink.clone(), token.clone()).await?;

        let host = c139::get_personal_cloud_host(&mut config)
            .await
            .map_err(map_err)?;
        let parent_file_id = if parent_path == "/" || parent_path.is_empty() {
            "/".to_string()
        } else {
            c139::get_file_id_by_path(&config, &parent_path)
                .await
                .map_err(map_err)?
        };

        let part_size = services::upload_service::get_part_size(size as i64, 0) as u64;
        let part_count = size.div_ceil(part_size).max(1);

        // The crate's `upload()` hides all of this behind one call with no
        // progress callback, so we drive the four documented steps ourselves.
        let init: cloud139::models::PersonalUploadResp = c139::personal_api_request(
            &config,
            &format!("{host}/file/create"),
            serde_json::json!({
                "contentHash": content_hash,
                "contentHashAlgorithm": "SHA256",
                "contentType": content_type(local),
                "parallelUpload": false,
                "partInfos": part_infos(size, part_size, part_count),
                "size": size,
                "parentFileId": parent_file_id,
                "name": name,
                "type": "file",
                "fileRenameMode": "auto_rename",
            }),
            cloud139::domain::StorageType::PersonalNew,
        )
        .await
        .map_err(map_err)?;
        if !init.base.success {
            return Err(AppError::new(format!(
                "139 创建上传任务失败：{}",
                init.base.message.unwrap_or_default()
            )));
        }
        let Some(data) = init.data else {
            // No data at all means the server considers the file complete.
            sink.progress(size);
            return Ok(Some(Self::encode(&remote)));
        };
        let file_id = data.file_id.clone().unwrap_or_default();
        let upload_id = data.upload_id.clone().unwrap_or_default();
        let stored_name = data.file_name.clone().unwrap_or_else(|| name.to_string());
        let parts = data.part_infos.unwrap_or_default();
        crate::ylog!(
            "139 upload: file_id={file_id} parts={} rapid={:?}",
            parts.len(),
            data.rapid_upload
        );
        if parts.is_empty() || file_id.is_empty() || upload_id.is_empty() {
            // Rapid upload (content hash already on the server).
            sink.progress(size);
            return Ok(Some(Self::encode(&join_path(
                &parent_path,
                &stored_name,
            ))));
        }

        sink.phase("uploading");
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| AppError::new(format!("打开本地文件失败：{e}")))?;
        let mut sent = 0u64;
        for part in &parts {
            if token.is_cancelled() {
                return Err(AppError::new("已取消"));
            }
            let index = part.part_number as u64;
            let offset = index.saturating_sub(1) * part_size;
            let want = part_size.min(size.saturating_sub(offset));
            let chunk = read_chunk(&mut file, offset, want as usize).await?;
            if chunk.len() as u64 != want {
                return Err(AppError::new(format!(
                    "本地文件在第 {} 片时提前结束（已发送 {sent} / {size} 字节）",
                    index
                )));
            }
            let response = client
                .put(&part.upload_url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, chunk.len())
                .body(chunk)
                .send()
                .await
                .map_err(|e| AppError::new(format!("上传分片 {index} 失败：{e}")))?;
            if !response.status().is_success() {
                return Err(AppError::new(format!(
                    "上传分片 {index} 失败（HTTP {}）",
                    response.status().as_u16()
                )));
            }
            sent += want;
            sink.progress(sent);
        }
        if sent != size {
            return Err(AppError::new(format!(
                "上传不完整（{sent} / {size} 字节），已放弃提交"
            )));
        }

        let complete: serde_json::Value = c139::personal_api_request(
            &config,
            &format!("{host}/file/complete"),
            serde_json::json!({
                "contentHash": content_hash,
                "contentHashAlgorithm": "SHA256",
                "uploadId": upload_id,
                "fileId": file_id,
            }),
            cloud139::domain::StorageType::PersonalNew,
        )
        .await
        .map_err(map_err)?;
        if complete.get("success").and_then(|v| v.as_bool()) == Some(false) {
            return Err(AppError::new(format!(
                "139 提交上传失败：{}",
                complete
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("未知错误")
            )));
        }
        Ok(Some(Self::encode(&join_path(&parent_path, &stored_name))))
    }

    // ---------------------------------------------------------- shares ---

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let data = self.share_info(id, passcode, "root").await?;
        let owner = text_of(&data, "owner");
        let owner_avatar = None;
        let expired_at = data
            .get("expireTime")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y%m%d%H%M%S").ok())
            .map(|dt| dt.and_utc().timestamp_millis())
            .unwrap_or(0);
        let file_num = data
            .get("caLength")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            + data
                .get("coLength")
                .and_then(|v| v.as_str())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        Ok(ShareSession {
            provider: ProviderId::Yun139.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            // 139 needs no session token; the link id + passcode are enough.
            stoken: String::new(),
            title: text_of(&data, "lkName"),
            owner: if owner.is_empty() {
                text_of(&data, "nickName")
            } else {
                owner
            },
            owner_avatar,
            file_num,
            total_size: 0,
            expired_at,
            first_fid: None,
            can_save: true,
            web_url: Some(format!(
                "https://yun.139.com/shareweb/#/w/i/{id}"
            )),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<ShareListing> {
        let parent = if dir.is_empty() { "root" } else { dir };
        let data = self.share_info(&share.pwd_id, &share.passcode, parent).await?;
        let mut entries = Vec::new();
        if let Some(dirs) = data.get("caLst").and_then(|v| v.as_array()) {
            for item in dirs {
                entries.push(ShareEntry {
                    fid: text_of(item, "caID"),
                    token: String::new(),
                    name: text_of(item, "caName"),
                    size: 0,
                    is_dir: true,
                    updated_at: ud_time(item),
                });
            }
        }
        if let Some(files) = data.get("coLst").and_then(|v| v.as_array()) {
            for item in files {
                entries.push(ShareEntry {
                    fid: text_of(item, "coID"),
                    token: String::new(),
                    name: text_of(item, "coName"),
                    size: item.get("coSize").and_then(|v| v.as_u64()).unwrap_or(0),
                    is_dir: false,
                    updated_at: ud_time(item),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        let total = entries.len() as u64;
        Ok(ShareListing {
            pdir_fid: parent.to_string(),
            entries,
            total,
        })
    }

    async fn share_save(
        &self,
        share: &crate::transfer::ShareRef,
        entries: &[ShareEntry],
        to: Option<&str>,
    ) -> AppResult<u64> {
        if entries.is_empty() {
            return Err(AppError::new("请先选择要转存的内容"));
        }
        // 139 can only save whole folders/files by id; the destination is a
        // path, and "/" is its root.
        let target = match to {
            Some(id) if !id.is_empty() && id != ROOT => self.path_of(id).await,
            _ => "/".to_string(),
        };
        let config = self.config().await;
        let ids: Vec<String> = entries.iter().map(|e| e.fid.clone()).collect();
        let body = serde_json::json!({
            "createOuterLinkBatchOprTaskReq": {
                "msisdn": config.account,
                "ownerAccount": config.account,
                "taskType": 3,
                "actionType": 310,
                "linkID": share.pwd_id,
                "taskInfo": {
                    "linkID": share.pwd_id,
                    "needPassword": share.passcode,
                    "newCatalogID": target,
                    "contentInfoList": ids,
                }
            }
        });
        let value = self.share_post("/IBatchOprTask/createOuterLinkBatchOprTask", &body).await?;
        // `share_post` already unwrapped the `data` envelope.
        let task = value
            .pointer("/taskID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if task.is_empty() {
            return Ok(entries.len() as u64);
        }
        self.poll_share_task(&config.account, &task).await?;
        Ok(entries.len() as u64)
    }

    async fn share_walk(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<Vec<ShareFile>> {
        const MAX_FILES: usize = 5000;
        const MAX_DEPTH: usize = 12;
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((
            if dir.is_empty() { "root".to_string() } else { dir.to_string() },
            std::path::PathBuf::new(),
            0usize,
        ));
        while let Some((fid, prefix, depth)) = queue.pop_front() {
            if out.len() >= MAX_FILES {
                return Err(AppError::new(
                    "文件夹内文件过多（超过 5000 个），请分次下载",
                ));
            }
            let listing = self.share_list(share, &fid).await?;
            for entry in listing.entries {
                let name = crate::paths::safe_component(&entry.name);
                if entry.is_dir {
                    if depth + 1 > MAX_DEPTH {
                        continue;
                    }
                    let mut next = prefix.clone();
                    next.push(name);
                    queue.push_back((entry.fid, next, depth + 1));
                } else {
                    let mut relative = prefix.clone();
                    relative.push(name);
                    out.push(ShareFile {
                        relative,
                        fid: entry.fid,
                        token: String::new(),
                        name: entry.name,
                        size: entry.size,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn share_link(
        &self,
        share: &crate::transfer::ShareRef,
        fid: &str,
    ) -> AppResult<ShareLink> {
        let config = self.config().await;
        let body = serde_json::json!({
            "dlFromOutLinkReqV3": {
                "account": config.account,
                "linkID": share.pwd_id,
                "passwd": share.passcode,
                "coIDLst": { "item": [fid] },
            }
        });
        let value = self.share_post("/IOutLink/dlFromOutLinkV3", &body).await?;
        let url = value
            .pointer("/redrUrl")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| AppError::new("139 没有返回分享下载地址"))?;
        // The presigned url is valid for 15 minutes and is GET-only; the CDN
        // does not want the share headers echoed back.
        Ok(ShareLink {
            url: url.to_string(),
            headers: vec![("User-Agent".to_string(), crate::share::UA.to_string())],
            size: 0,
            cleanup_fid: None,
        })
    }

    /// `getOutLink` — 139's create-a-share call.
    ///
    /// Three things are load-bearing and none are discoverable from the
    /// response: the `x-yun-userDomainId: enabled` header, a numeric
    /// `userDomainId` inside the request body, and splitting the selection into
    /// `coIDLst` (files) and `caIDLst` (folders) by opaque id. Missing any one
    /// answers `200000401`, which reads like an expired login.
    async fn create_share(
        &self,
        fids: &[String],
        title: &str,
        days: u32,
        passcode: &str,
    ) -> AppResult<ShareCreated> {
        if fids.is_empty() {
            return Err(AppError::new("请先选择要分享的内容"));
        }
        let (files, folders) = self.share_targets(fids).await?;
        if files.len() + folders.len() > 50 {
            return Err(AppError::new("139 单次分享最多 50 个文件或文件夹"));
        }
        let domain = self.user_domain_id().await?;
        // `encrypt: 1` makes the service mint a 4-character passcode; `0`
        // publishes the link without one. 139 has no way to set our own code.
        let encrypted = passcode.chars().count() == 4;
        let name = if title.is_empty() {
            title_of(&self.path_of(&fids[0]).await)
        } else {
            title.to_string()
        };
        let mut req = serde_json::json!({
            "linkType": 0,
            "subLinkType": 0,
            "encrypt": if encrypted { 1 } else { 0 },
            "coIDLst": { "item": files },
            "caIDLst": { "item": folders },
            "pubType": 1,
            "dedicatedName": name,
            "periodUnit": 1,
            "viewerLst": {},
            "userDomainId": domain,
            "extInfo": { "isWatermark": 0, "shareChannel": "3001" },
        });
        // Omitting `period` is how 139 says "never expires"; `0` is rejected.
        if days > 0 {
            req["period"] = serde_json::json!(days);
        }
        let value = self
            .share_post("/IOutLink/getOutLink", &serde_json::json!({ "getOutLinkReq": req }))
            .await?;
        let first = value
            .pointer("/getOutLinkResSet/0")
            .cloned()
            .ok_or_else(|| AppError::new("139 没有返回分享链接，请稍后重试"))?;
        let url = text_of(&first, "linkUrl");
        if url.is_empty() {
            return Err(AppError::new("139 没有返回分享链接，请稍后重试"));
        }
        let expired_at = if days == 0 {
            0
        } else {
            chrono::Local::now().timestamp_millis() + i64::from(days) * 86_400_000
        };
        Ok(ShareCreated {
            url,
            passcode: text_of(&first, "passwd"),
            expired_at,
            share_id: text_of(&first, "linkID"),
        })
    }

    /// `getOutLinkLst` — the shares this account owns.
    async fn share_records(&self) -> AppResult<Vec<ShareRecord>> {
        let domain = self.user_domain_id().await?;
        let body = serde_json::json!({
            "getOutLinkLstReq": {
                "needSetAccount": true,
                "srt": 1,
                "srtDr": 0,
                "bNum": 1,
                "eNum": 200,
                "qryType": 1,
                "userDomainId": domain,
            }
        });
        let value = self.share_post("/IOutLink/getOutLinkLst", &body).await?;
        let rows = value
            .get("outLinks")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let now = chrono::Utc::now().timestamp_millis();
        Ok(rows
            .iter()
            .map(|row| {
                // `expireTime` is empty for a share that never expires.
                let expired_at = stamp(&text_of(row, "expireTime"));
                ShareRecord {
                    share_id: text_of(row, "linkID"),
                    url: text_of(row, "url"),
                    title: text_of(row, "lkName"),
                    passcode: text_of(row, "passwd"),
                    expired_at,
                    created_at: stamp(&text_of(row, "ctTime")),
                    // The list reports a size for single files only, and no
                    // count at all, so 1 is the honest number here.
                    file_num: 1,
                    size: row.get("tolSize").and_then(|v| v.as_i64()).unwrap_or(0),
                    visit_count: row.get("brwTimes").and_then(|v| v.as_i64()).unwrap_or(0),
                    save_count: row.get("svTimes").and_then(|v| v.as_i64()).unwrap_or(0),
                    download_count: row.get("dlTimes").and_then(|v| v.as_i64()).unwrap_or(0),
                    expired: row.get("isOutLinkInvalid").and_then(|v| v.as_i64()) == Some(1)
                        || (expired_at > 0 && expired_at < now),
                    fids: Vec::new(),
                }
            })
            .collect())
    }

    /// `delOutLink` — revoking goes by `linkID`; the file ids are not needed.
    async fn revoke_share(&self, share_id: &str, _fids: &[String]) -> AppResult<()> {
        if share_id.is_empty() {
            return Err(AppError::new("缺少分享 ID"));
        }
        let domain = self.user_domain_id().await?;
        // `linkIDs` must be `{"item": [...]}`; a plain array answers
        // `9530 功能参数值非法`.
        let body = serde_json::json!({
            "delOutLinkReq": {
                "linkIDs": { "item": [share_id] },
                "userDomainId": domain,
            }
        });
        self.share_post("/IOutLink/delOutLink", &body).await.map(|_| ())
    }
}

/// 139 addresses files by path, so byte-level ids are mapped back first.
/// Turns a path into the `fileId` the 139 API expects.
///
/// The API never accepts a path: `file/list` and `getDownloadUrl` both want an
/// opaque id (`/` for the root, `/myfaverapp` and friends deeper down). Passing
/// the path straight through listed the root correctly but returned an empty
/// folder for every subdirectory.
async fn resolve_file_id(config: &C139Config, path: &str) -> AppResult<String> {
    if path == "/" || path.is_empty() {
        return Ok("/".to_string());
    }
    c139::get_file_id_by_path(config, path).await.map_err(map_err)
}

/// Reads the account out of `base64("pc:<account>:<token info>")`.
fn decode_account(token: &str) -> Option<String> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let mut parts = text.splitn(3, ':');
    let _scheme = parts.next()?;
    let account = parts.next()?;
    if account.trim().is_empty() {
        return None;
    }
    Some(account.to_string())
}

/// The `partInfos` array the create call wants: one entry per part, with the
/// byte offset the server hashes from. The reference client only sends the
/// first hundred, which is all a 10 GiB file at 100 MiB needs.
fn part_infos(size: u64, part_size: u64, part_count: u64) -> Vec<serde_json::Value> {
    (0..part_count.min(100))
        .map(|i| {
            let offset = i * part_size;
            serde_json::json!({
                "partNumber": (i + 1) as i32,
                "partSize": part_size.min(size.saturating_sub(offset)),
                "parallelHashCtx": { "partOffset": offset },
            })
        })
        .collect()
}

/// SHA-256 of the whole file, reported through `sink` so the UI can show
/// 校验中 progress instead of a frozen bar.
async fn sha256_file(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| AppError::new(format!("打开本地文件失败：{e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut hashed = 0u64;
    loop {
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        let read = file
            .read(&mut buf)
            .await
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        hashed += read as u64;
        sink.progress(hashed);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Reads exactly `want` bytes at `offset`.
async fn read_chunk(file: &mut tokio::fs::File, offset: u64, want: usize) -> AppResult<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| AppError::new(format!("定位本地文件失败：{e}")))?;
    let mut buf = vec![0u8; want];
    let mut filled = 0usize;
    while filled < want {
        let read = file
            .read(&mut buf[filled..])
            .await
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Content type for the create call, mirroring what the reference client sends.
fn content_type(path: &Path) -> String {
    let ext = path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let mime = match ext.as_str() {
        "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// Shows only the head and tail of a bearer token.
fn mask_token(token: &str) -> String {
    let token = token.trim();
    let chars: Vec<char> = token.chars().collect();
    // Slice by chars, not bytes: a base64 header is ASCII today, but a byte
    // slice on a multi-byte value panics.
    if chars.len() <= 12 {
        return "…".to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

use futures_util::StreamExt;


#[cfg(test)]
mod live_tests {
    use super::*;

    /// Live probe against whatever 139 account this machine is signed into.
    ///
    /// `cargo test --lib yun139_live -- --ignored --nocapture`
    ///
    /// Guards the two bugs that made 139 look half-broken: subfolders listing
    /// empty (path passed where a fileId was needed) and a capacity in MB read
    /// as bytes.
    #[tokio::test]
    #[ignore]
    async fn yun139_live_listing_and_quota() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");

        let root = drive.list(ROOT).await.expect("root listing");
        println!("root: {} entries", root.entries.len());
        assert!(!root.entries.is_empty(), "root listing came back empty");

        let folder = root
            .entries
            .iter()
            .find(|e| e.is_dir)
            .expect("at least one folder");
        let sub = drive
            .list(&super::super::native_of(&folder.fid))
            .await
            .expect("subfolder listing");
        println!("subfolder {} -> {} entries", folder.name, sub.entries.len());
        for e in sub.entries.iter().take(5) {
            println!("  {} dir={} size={}", e.name, e.is_dir, e.size);
        }
        assert!(
            !sub.entries.is_empty(),
            "subfolder {} listed empty — fileId resolution is broken",
            folder.name
        );

        // Breadcrumbs must come from real listings: the folder we just opened
        // has to appear as the last node with its own id.
        let crumbs = drive
            .resolve_path(&super::super::native_of(&folder.fid))
            .await
            .expect("resolve_path");
        println!(
            "crumbs: {:?}",
            crumbs.iter().map(|n| n.name.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(crumbs.last().map(|n| n.name.as_str()), Some(folder.name.as_str()));
        assert_eq!(crumbs.last().map(|n| n.fid.as_str()), Some(folder.fid.as_str()));

        let quota = drive.quota().await.expect("quota call").expect("quota");
        println!(
            "quota: used {} B / total {} B",
            quota.used, quota.total
        );
        // A personal 139 drive is never smaller than 1 GiB; the MB/byte mixup
        // produced numbers in the hundreds of kilobytes.
        assert!(quota.total > 1024 * 1024 * 1024, "quota unit looks wrong");
        assert!(quota.used <= quota.total);
    }

    /// Exercises the rewritten upload path end to end: hashing phase, per-part
    /// progress, and the id it hands back must resolve to a real file.
    ///
    /// `cargo test --lib yun139_live_upload -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn yun139_live_upload_progress_and_download() {
        use crate::providers::{CounterSink, UploadSink};
        use futures_util::StreamExt;
        use std::sync::atomic::AtomicU64;

        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");

        // 3 MiB with a per-run nonce, so the server cannot rapid-upload it: the
        // point of this test is the chunked path, not the fast one.
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("yunyu-upload-{nonce}.bin");
        let local = std::env::temp_dir().join(&name);
        let payload: Vec<u8> = (0..3 * 1024 * 1024u32)
            .map(|i| (i.wrapping_add(nonce as u32) % 251) as u8)
            .collect();
        std::fs::write(&local, &payload).unwrap();

        let done = Arc::new(AtomicU64::new(0));
        let sink = Arc::new(CounterSink::new(done.clone()));
        let phases = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct Spy {
            inner: Arc<CounterSink>,
            phases: Arc<std::sync::Mutex<Vec<String>>>,
        }
        impl UploadSink for Spy {
            fn phase(&self, phase: &str) {
                UploadSink::phase(self.inner.as_ref(), phase);
                if let Ok(mut list) = self.phases.lock() {
                    list.push(phase.to_string());
                }
            }
            fn progress(&self, bytes: u64) {
                self.inner.progress(bytes);
            }
        }
        let spy: Arc<dyn UploadSink> = Arc::new(Spy {
            inner: sink.clone(),
            phases: phases.clone(),
        });

        let created = drive
            .upload(&local, ROOT, &name, spy, CancellationToken::new())
            .await
            .expect("upload");
        let seen = phases.lock().unwrap().clone();
        println!("phases: {seen:?} done={}", done.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(seen.first().map(String::as_str), Some("hashing"));
        // A unique payload always needs real parts, so the upload phase must
        // follow the hashing phase (and only then may the task count as done).
        assert!(seen.iter().any(|p| p == "uploading"), "phases: {seen:?}");
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), payload.len() as u64);

        let fid = created.expect("upload should report an id");
        let listing = drive.list(ROOT).await.expect("root listing");
        let hit = listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .expect("uploaded file should be listed");
        println!("listed {} size={} fid={}", hit.name, hit.size, hit.fid);
        assert_eq!(hit.size, payload.len() as u64);

        // Download the first chunk and a resumed slice.
        let native = super::super::native_of(&fid);
        let mut handle = drive.open_download(&native, 0).await.expect("download");
        let first = handle.stream.next().await.expect("first chunk").expect("bytes");
        println!("download: {} bytes, first={:?}", first.len(), &first[..4.min(first.len())]);
        assert_eq!(&first[..4], &payload[..4]);
        drop(handle);
        let mut resumed = drive.open_download(&native, 100).await.expect("resume");
        let chunk = resumed.stream.next().await.expect("chunk").expect("bytes");
        assert_eq!(&chunk[..8], &payload[100..108]);
        drop(resumed);

        // Rename and move both go through the path/id mapping, which is the part
        // that used to be wrong.
        let renamed = format!("{name}.renamed");
        drive.rename(&native, &renamed).await.expect("rename");
        let listing = drive.list(ROOT).await.expect("root listing");
        assert!(
            listing.entries.iter().any(|e| e.name == renamed),
            "rename did not show up in the listing"
        );
        let moved = listing
            .entries
            .iter()
            .find(|e| e.name == renamed)
            .expect("renamed entry")
            .fid
            .clone();
        if let Some(folder) = listing.entries.iter().find(|e| e.is_dir) {
            let target = super::super::native_of(&folder.fid);
            drive
                .transfer_entries(&[super::super::native_of(&moved)], &target, false)
                .await
                .expect("move");
            let inside = drive.list(&target).await.expect("target listing");
            assert!(
                inside.entries.iter().any(|e| e.name == renamed),
                "moved file should be inside {}",
                folder.name
            );
            println!("moved {renamed} into {}", folder.name);
            drive
                .delete(&[super::super::native_of(&moved)])
                .await
                .expect("delete");
        } else {
            drive
                .delete(&[super::super::native_of(&moved)])
                .await
                .expect("delete");
        }
        let _ = std::fs::remove_file(&local);
        println!("deleted {renamed}");
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// Reproduces the user's two failures: a download of a file that lives in a
    /// folder with more than 100 entries, and an upload into that same folder.
    #[tokio::test]
    #[ignore]
    async fn yun139_probe_large_folder_ops() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");
        let root = drive.list(ROOT).await.unwrap();
        let big = root
            .entries
            .iter()
            .find(|e| e.name == "服务器备份")
            .expect("服务器备份");
        let native = super::super::native_of(&big.fid);
        let listing = drive.list(&native).await.unwrap();
        println!("服务器备份 -> {} entries (total {})", listing.entries.len(), listing.total);
        let file = listing
            .entries
            .iter()
            .find(|e| e.name == "backup_2026-04-17T22-00-00.tar.gz")
            .expect("backup file");
        println!("target {} fid={} idx={}", file.name, file.fid,
            listing.entries.iter().position(|e| e.name == file.name).unwrap());
        match drive.open_download(&super::super::native_of(&file.fid), 0).await {
            Ok(_) => println!("download ok"),
            Err(err) => println!("download ERR: {err}"),
        }
        let chatgpt = listing.entries.iter().find(|e| e.name.starts_with("ChatGPT Image"));
        println!("chatgpt in 服务器备份: {:?}", chatgpt.map(|e| e.name.clone()));
    }

    /// Uploads the user's failing file into the folders that broke before.
    ///
    /// `cargo test --lib -- --ignored --nocapture yun139_probe_upload_dirs`
    #[tokio::test]
    #[ignore]
    async fn yun139_probe_upload_dirs() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");
        let local = std::path::Path::new(
            "/home/henvei/桌面/ChatGPT Image 2026年5月2日 09_23_48.png",
        );
        let root = drive.list(ROOT).await.unwrap();
        let mut targets = vec![(ROOT.to_string(), "/".to_string())];
        for name in ["服务器备份", "手机图片", "我的应用收藏", "AI空间"] {
            if let Some(e) = root.entries.iter().find(|x| x.name == name) {
                targets.push((super::super::native_of(&e.fid), format!("/{name}")));
            }
        }
        for (parent, label) in targets {
            let sink = std::sync::Arc::new(super::super::CounterSink::new(std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(0),
            )));
            let token = tokio_util::sync::CancellationToken::new();
            let name = "yunyu-probe-image.png".to_string();
            match drive.upload(local, &parent, &name, sink.clone(), token).await {
                Ok(id) => {
                    println!("{label}: ok id={id:?}");
                    if let Some(id) = id {
                        let native = super::super::native_of(&id);
                        match drive.open_download(&native, 0).await {
                            Ok(h) => println!("  download ok size={}", h.size),
                            Err(e) => println!("  download ERR {e}"),
                        }
                        let _ = drive.delete(&[native]).await;
                    }
                }
                Err(err) => println!("{label}: ERR {err}"),
            }
        }
    }

    /// Do the ids our own listing reports work as `parentFileId`?
    #[tokio::test]
    #[ignore]
    async fn yun139_probe_child_ids() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");
        let config = drive.config().await;
        let items = c139::list_personal_files(&config, "/").await.unwrap();
        for it in items.iter() {
            let fid = it.file_id.clone().unwrap_or_default();
            let sub = c139::list_personal_files(&config, &fid).await;
            println!(
                "{:<28} id={:<34} list={:?}",
                it.name.clone().unwrap_or_default(),
                fid,
                sub.as_ref().map(|v| v.len()).map_err(|e| e.to_string())
            );
        }
    }

    /// Compares what `get_file_id_by_path` sees with what our own listing sees.
    #[tokio::test]
    #[ignore]
    async fn yun139_probe_path_lookup() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");
        let config = drive.config().await;
        let root = drive.list(ROOT).await.unwrap();
        let big = root.entries.iter().find(|e| e.name == "服务器备份").unwrap();
        let folder_id = c139::get_file_id_by_path(&config, "/服务器备份").await.unwrap();
        println!("folder id = {folder_id}");
        let items = c139::list_personal_files(&config, &folder_id).await.unwrap();
        println!("crate listing {} items; first 3:", items.len());
        for it in items.iter().take(3) {
            println!("  {:?} id={:?}", it.name, it.file_id);
        }
        let names: Vec<&str> = items.iter().filter_map(|i| i.name.as_deref()).collect();
        println!("backup present in crate listing: {}", names.iter().any(|n| n.starts_with("backup_2026")));
        println!("big.fid = {}", big.fid);
        let native = super::super::native_of(&big.fid);
        println!("native = {native} decode = {}", Yun139Provider::decode(&native));
    }

    /// Counts entries per folder to test the "more than one page" hypothesis.
    #[tokio::test]
    #[ignore]
    async fn yun139_probe_big_folder() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Yun139Provider::load(&dir).expect("yun139.toml");
        let root = drive.list(ROOT).await.unwrap();
        for e in root.entries.iter().filter(|e| e.is_dir) {
            let sub = drive.list(&super::super::native_of(&e.fid)).await.unwrap();
            println!("{:40} {} entries", e.name, sub.entries.len());
        }
    }
}

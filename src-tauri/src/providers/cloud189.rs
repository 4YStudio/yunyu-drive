//! 天翼云盘 (189 Cloud) adapter.
//!
//! The drive speaks two API dialects and this adapter uses both:
//!
//! * `api.cloud.189.cn` — the signed one. Every call carries a `Date`, the
//!   `SessionKey`, a `Signature` (HMAC-SHA1 over a fixed four-part string) and
//!   the standard `clientType`/`version`/`channelId` suffix. This is where the
//!   file list, the folder operations, the batch tasks, the download links and
//!   the upload handshake live.
//! * `cloud.189.cn/v2` — the cookie-authenticated web endpoints, kept only for
//!   the bits the signed API does not expose.
//!
//! Signing: `signatureOfHmac` builds `SessionKey=..&Operate=..&RequestURI=..&Date=..`
//! and HMAC-SHA1s it with the session secret, uppercase hex. The query string is
//! not part of the digest, so arguments are sent as plain query values: the
//! upstream clients encrypt them into a `params` blob, but the server has
//! rejected every such blob with `sessionsignature is not match` since 2026-11.
//!
//! Root: the personal drive root is `-11`, but the app expects every drive to
//! answer on `0`, so `0` is translated on the way in and out.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, CONTENT_RANGE, LOCATION, SET_COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::{AppError, AppResult};
use crate::share::{
    AccountInfo, ShareCreated, ShareEntry, ShareListing, ShareRecord, ShareSession,
};

use super::{
    check_name, native_of, qualify, sort_entries, AuthState, Capabilities, DirListing,
    DownloadHandle, DriveProvider, EntryDto, PathNode, ProviderId, QuotaInfo, RangeResult,
    RecycleEntry, SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};

const API: &str = "https://api.cloud.189.cn";
const WEB: &str = "https://cloud.189.cn";

/// The app id whose login page still embeds `lt`/`paramId`/`reqId`.
pub(crate) const APP_ID: &str = "8025431004";
const CLIENT_TYPE: &str = "TELEPC";
const VERSION: &str = "7.2.4.0";
const CHANNEL_ID: &str = "web_cloud.189.cn";
/// Root of the personal drive; the app's `0` maps onto it.
const ROOT: &str = "-11";
const PAGE_SIZE: u32 = 1000;
/// The upload handshake uses its own client identity.
const UPLOAD_SUFFIX: &str = "clientType=TELEMAC&version=1.0.0&channelId=web_cloud.189.cn";
/// The `open/*` family signs with MD5 and wants this app key.
const OPEN_APP_KEY: &str = "601102120";
/// Paging for share listings; the endpoint caps it well below 1000.
const SHARE_PAGE_SIZE: u32 = 100;

/// Persisted session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cloud189Credentials {
    #[serde(default)]
    pub session_key: String,
    #[serde(default)]
    pub session_secret: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub nickname: String,
    /// Inlined `data:` avatar, or a remote URL until `profile()` runs.
    #[serde(default)]
    pub avatar: Option<String>,
    /// Cookie header for the `/v2/*` web endpoints (recycle bin only).
    /// Minted from `accessToken` through the SSO bridge, never typed by anyone.
    #[serde(default)]
    pub web_cookie: String,
}

impl Cloud189Credentials {
    fn usable(&self) -> bool {
        !self.session_key.trim().is_empty() && !self.session_secret.trim().is_empty()
    }
}

pub struct Cloud189Provider {
    creds: RwLock<Cloud189Credentials>,
    credential_path: String,
    client: OnceLock<reqwest::Client>,
    profile_tried: AtomicBool,
    /// `open/*` access token and its expiry (ms since the epoch).
    open_token: RwLock<Option<(String, i64)>>,
}

impl Cloud189Provider {
    fn new(creds: Cloud189Credentials, credential_path: String) -> Self {
        Self {
            creds: RwLock::new(creds),
            credential_path,
            client: OnceLock::new(),
            profile_tried: AtomicBool::new(false),
            open_token: RwLock::new(None),
        }
    }

    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let path = own_dir.join("cloud189.json");
        let text = std::fs::read_to_string(&path).map_err(|_| AppError::new("尚未登录天翼云盘"))?;
        let creds: Cloud189Credentials = serde_json::from_str(&text)
            .map_err(|e| AppError::new(format!("天翼云盘凭据已损坏：{e}")))?;
        if !creds.usable() {
            return Err(AppError::new("尚未登录天翼云盘"));
        }
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    /// Writes a freshly exchanged session and returns the ready provider.
    pub fn save_session(own_dir: &Path, creds: Cloud189Credentials) -> AppResult<Self> {
        if !creds.usable() {
            return Err(AppError::new("没有拿到天翼云盘的登录凭据"));
        }
        std::fs::create_dir_all(own_dir)
            .map_err(|e| AppError::new(format!("创建配置目录失败：{e}")))?;
        let path = own_dir.join("cloud189.json");
        let text = serde_json::to_string_pretty(&creds)?;
        std::fs::write(&path, text)
            .map_err(|e| AppError::new(format!("写入天翼云盘凭据失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        crate::ylog!(
            "cloud189 login: saved session for {}",
            if creds.nickname.is_empty() {
                "(unknown)"
            } else {
                &creds.nickname
            }
        );
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    fn http(&self) -> AppResult<reqwest::Client> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let _ = self.client.set(client.clone());
        Ok(client)
    }

    async fn persist(&self) {
        let creds = self.creds.read().await.clone();
        let text = match serde_json::to_string_pretty(&creds) {
            Ok(text) => text,
            Err(err) => {
                crate::ylog!("cloud189: persist failed: {err}");
                return;
            }
        };
        if let Err(err) = std::fs::write(&self.credential_path, text) {
            crate::ylog!("cloud189: persist write failed: {err}");
        }
    }

    async fn session(&self) -> (String, String) {
        let creds = self.creds.read().await;
        (creds.session_key.clone(), creds.session_secret.clone())
    }

    // ------------------------------------------------------------- calls ---

    /// One signed call. `params` (when given) is AES-encrypted and echoed in
    /// the query; the signature covers the encrypted form.
    async fn call(
        &self,
        method: &str,
        path: &str,
        params: Option<Vec<(String, String)>>,
    ) -> AppResult<Value> {
        self.call_with(method, path, params, &[]).await
    }

    async fn call_with(
        &self,
        method: &str,
        path: &str,
        params: Option<Vec<(String, String)>>,
        query: &[(&str, &str)],
    ) -> AppResult<Value> {
        let (key, secret) = self.session().await;
        if key.is_empty() || secret.is_empty() {
            return Err(AppError::new("天翼云盘登录状态已失效，请重新登录"));
        }
        let url = format!("{API}{path}");
        // Arguments ride on the query string rather than in an encrypted
        // `params` blob. The blob is what the upstream clients do, but the
        // server has answered `sessionsignature is not match` for every shape
        // of it since 2026-11, while the same request signed without `params`
        // and carrying the arguments as plain query values succeeds.
        let arguments: Vec<(String, String)> = params
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .collect();
        let date = http_date();
        let signature = sign_hmac(&secret, &key, method, &url, &date);

        let client = self.http()?;
        let mut request = client
            .request(
                method
                    .parse()
                    .map_err(|_| AppError::new(format!("不支持的请求方法：{method}")))?,
                &url,
            )
            .query(&client_suffix())
            // Without this the server answers XML, and every field lookup below
            // would silently read `0`. It has to ride on the query string of
            // every call, signed or not.
            .query(&[("returnType", "JSON")])
            .header("Date", &date)
            .header("SessionKey", &key)
            .header("Sign-Type", "1")
            .header("Signature", signature)
            .header("X-Request-ID", random_id());
        for (name, value) in query {
            request = request.query(&[(name, value)]);
        }
        for (name, value) in &arguments {
            request = request.query(&[(name.as_str(), value.as_str())]);
        }
        if method == "POST" {
            // 189 reads every POST argument from the query string, so an empty
            // form is enough to set the content type the server expects.
            request = request.form(&[("", "")][..0]);
        }

        let res = request
            .send()
            .await
            .map_err(|e| net_error(&format!("天翼云盘请求 {path}"), &e))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| net_error("读取天翼云盘响应", &e))?;
        if status >= 400 {
            crate::ylog!(
                "cloud189: {method} {path} HTTP {status} head={}",
                head(&body, 200)
            );
        }
        let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
        if let Some(err) = resp_error(&value, status) {
            return Err(AppError::new(format!("天翼云盘：{err}")));
        }
        Ok(value)
    }

    #[allow(dead_code)]
    async fn get(&self, path: &str) -> AppResult<Value> {
        self.call("GET", path, None).await
    }

    // ------------------------------------------------------------ shapes ---

    fn file_entry(&self, item: &Value, parent: &str) -> EntryDto {
        let id = id_of(item);
        let name = name_of(item);
        let size = item.get("size").and_then(Value::as_u64).unwrap_or(0);
        let updated = parse_189_time(item.get("lastOpTime"));
        let created = parse_189_time(item.get("createDate"));
        let thumb = item
            .get("icon")
            .and_then(|i| i.get("smallUrl"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty());
        EntryDto {
            fid: qualify(ProviderId::Cloud189, &id),
            name: name.clone(),
            pdir_fid: qualify(ProviderId::Cloud189, parent),
            size,
            is_dir: false,
            format_type: "file".to_string(),
            updated_at: updated,
            created_at: created.max(updated),
            ext: EntryDto::ext_of(&name),
            provider: ProviderId::Cloud189.as_str().to_string(),
            thumb,
        }
    }

    fn folder_entry(&self, item: &Value, parent: &str) -> EntryDto {
        let id = id_of(item);
        let name = name_of(item);
        let updated = parse_189_time(item.get("lastOpTime"));
        let created = parse_189_time(item.get("createDate"));
        EntryDto {
            fid: qualify(ProviderId::Cloud189, &id),
            name,
            pdir_fid: qualify(ProviderId::Cloud189, parent),
            size: 0,
            is_dir: true,
            format_type: "folder".to_string(),
            updated_at: updated,
            created_at: created.max(updated),
            ext: String::new(),
            provider: ProviderId::Cloud189.as_str().to_string(),
            thumb: None,
        }
    }

    /// One page of `listFiles.action`.
    async fn list_page(&self, folder: &str, page: u32) -> AppResult<(Vec<EntryDto>, u32)> {
        let value = self
            .call_with(
                "GET",
                "/listFiles.action",
                None,
                &[
                    ("folderId", folder),
                    ("fileType", "0"),
                    ("mediaAttr", "0"),
                    ("iconOption", "5"),
                    ("pageNum", &page.to_string()),
                    ("pageSize", &PAGE_SIZE.to_string()),
                    ("recursive", "0"),
                    ("orderBy", "filename"),
                    ("descending", "false"),
                ],
            )
            .await?;
        let listing = value
            .get("fileListAO")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let count = listing.get("count").and_then(Value::as_u64).unwrap_or(0) as u32;
        let mut entries = Vec::new();
        for item in listing
            .get("folderList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            entries.push(self.folder_entry(&item, folder));
        }
        for item in listing
            .get("fileList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            entries.push(self.file_entry(&item, folder));
        }
        Ok((entries, count))
    }

    /// Resolves a download link and follows the CDN redirect by hand.
    async fn download_url(&self, fid: &str) -> AppResult<String> {
        let native = native_of(fid);
        let value = self
            .call_with(
                "GET",
                "/getFileDownloadUrl.action",
                None,
                &[("fileId", native.as_str()), ("dt", "3"), ("flag", "1")],
            )
            .await?;
        let url = value
            .get("fileDownloadUrl")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .replace("&amp;", "&");
        if url.is_empty() {
            return Err(AppError::new("天翼云盘没有返回下载地址"));
        }
        let url = if let Some(rest) = url.strip_prefix("http://") {
            format!("https://{rest}")
        } else {
            url
        };
        // The signed URL answers with a 302 to a short-lived CDN link; the
        // reqwest default would follow it, but then the Range header is lost.
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let res = client
            .get(&url)
            .send()
            .await
            .map_err(|e| net_error("获取下载地址", &e))?;
        if let Some(location) = res.headers().get(reqwest::header::LOCATION) {
            let location = location
                .to_str()
                .map_err(|_| AppError::new("下载地址无法解析"))?;
            return Ok(location.to_string());
        }
        // No redirect: some responses hand back the bytes directly.
        Ok(url)
    }

    /// A batch task, waited to completion.
    async fn batch(&self, kind: &str, target: Option<&str>, infos: Vec<Value>) -> AppResult<()> {
        // Copying an entry onto the folder it already lives in is a no-op the
        // server answers with a parked conflict task that never resolves — the
        // conflict endpoints do not even know the task id afterwards. Refusing
        // it here is both faster and truthful.
        if kind == "COPY" {
            if let Some(target) = target.filter(|t| !t.is_empty()) {
                for info in &infos {
                    let fid = info
                        .get("fileId")
                        .and_then(|v| match v {
                            Value::String(s) => Some(s.clone()),
                            other => other.as_i64().map(|n| n.to_string()),
                        })
                        .unwrap_or_default();
                    if fid.is_empty() {
                        continue;
                    }
                    let parent = self
                        .call_with("GET", "/getFileInfo.action", None, &[("fileId", &fid)])
                        .await
                        .ok()
                        .and_then(|v| v.get("parentId").and_then(Value::as_i64))
                        .map(|n| n.to_string());
                    if parent.as_deref() == Some(target) {
                        let name = info
                            .get("fileName")
                            .and_then(Value::as_str)
                            .unwrap_or("该文件");
                        return Err(AppError::new(format!(
                            "「{name}」已经在目标文件夹里了，无需再复制"
                        )));
                    }
                }
            }
        }
        let mut form = vec![
            ("type".to_string(), kind.to_string()),
            ("taskInfos".to_string(), Value::Array(infos).to_string()),
        ];
        if let Some(target) = target.filter(|t| !t.is_empty()) {
            form.push(("targetFolderId".to_string(), target.to_string()));
        }
        let value = self
            .call("POST", "/batch/createBatchTask.action", Some(form))
            .await?;
        let task_id = value
            .get("taskId")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        if task_id.is_empty() {
            return Err(AppError::new("天翼云盘没有返回任务编号"));
        }
        let step = if kind == "MOVE" { 400 } else { 1000 };
        let mut failures = 0u32;
        for _ in 0..150 {
            tokio::time::sleep(Duration::from_millis(step)).await;
            let form = vec![
                ("type".to_string(), kind.to_string()),
                ("taskId".to_string(), task_id.clone()),
            ];
            let value = self
                .call("POST", "/batch/checkBatchTask.action", Some(form))
                .await?;
            match value.get("taskStatus").and_then(Value::as_i64) {
                Some(4) => {
                    // A finished task can still have failed subtasks, and the
                    // server reports that only in `errorCode` (a COPY onto an
                    // existing name answers `FileAlreadyExists` with
                    // `taskStatus: 4`). Silently returning `Ok` here would make
                    // the UI claim success while nothing moved.
                    let failed = value
                        .get("failedCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    if failed == 0 {
                        return Ok(());
                    }
                    let code = value
                        .get("errorCode")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let hint = match code {
                        "FileAlreadyExists" => "目标位置已有同名文件",
                        "UserDayFlowOverLimited" => "当日流量已用尽",
                        _ => "任务执行失败",
                    };
                    return Err(AppError::new(format!(
                        "天翼云盘{hint}（{code}，失败 {failed} 项）"
                    )));
                }
                Some(2) => {
                    // `taskStatus == 2` is not an error on its own: it means the
                    // server parked the task and is waiting for a conflict
                    // decision. Leaving it there keeps it stuck forever (a COPY
                    // into a folder that already holds the name never moves), so
                    // the conflicts are resolved here and the task is released.
                    failures += 1;
                    if failures > 3 {
                        return Err(AppError::new("天翼云盘冲突处理未生效，请稍后重试"));
                    }
                    self.resolve_conflicts(kind, &task_id).await?;
                    continue;
                }
                Some(_) => continue,
                None => continue,
            }
        }
        Err(AppError::new("天翼云盘任务超时，请稍后在网页版确认"))
    }

    /// Answers the server's conflict question with "keep both".
    ///
    /// `manageBatchTask.action` takes the task's own `taskInfos` array back with
    /// `dealWay` filled in (1 skip, 2 keep both, 3 overwrite).
    async fn resolve_conflicts(&self, kind: &str, task_id: &str) -> AppResult<()> {
        let form = vec![
            ("type".to_string(), kind.to_string()),
            ("taskId".to_string(), task_id.to_string()),
        ];
        let info = self
            .call("POST", "/batch/getConflictTaskInfo.action", Some(form))
            .await?;
        let mut infos = info
            .get("taskInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if infos.is_empty() {
            return Err(AppError::new("天翼云盘提示同名冲突，但没有返回冲突明细"));
        }
        for info in &mut infos {
            if let Some(map) = info.as_object_mut() {
                // Keep both, which is what the desktop app offers as "保留两者".
                map.insert("dealWay".to_string(), json!(2));
                map.insert("isConflict".to_string(), json!(1));
            }
        }
        let mut form = vec![
            ("type".to_string(), kind.to_string()),
            ("taskId".to_string(), task_id.to_string()),
            ("taskInfos".to_string(), Value::Array(infos).to_string()),
        ];
        if let Some(target) = info.get("targetFolderId").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            other => other.as_i64().map(|n| n.to_string()),
        }) {
            form.push(("targetFolderId".to_string(), target));
        }
        // The task is released as soon as this answers, so a task still stuck at
        // `taskStatus: 2` after a couple of seconds means the decision did not
        // take — reporting success then would hide a copy that never happened.
        self.call("POST", "/batch/manageBatchTask.action", Some(form))
            .await?;
        for _ in 0..6 {
            tokio::time::sleep(Duration::from_millis(700)).await;
            let form = vec![
                ("type".to_string(), kind.to_string()),
                ("taskId".to_string(), task_id.to_string()),
            ];
            let value = self
                .call("POST", "/batch/checkBatchTask.action", Some(form))
                .await?;
            match value.get("taskStatus").and_then(Value::as_i64) {
                Some(2) => continue,
                Some(_) => return Ok(()),
                None => continue,
            }
        }
        Err(AppError::new(
            "天翼云盘同名冲突处理未生效，请稍后在网页版确认",
        ))
    }

    /// Walks up from `fid` to the root, collecting names.
    async fn chain(&self, fid: &str) -> AppResult<Vec<PathNode>> {
        let mut nodes: Vec<PathNode> = Vec::new();
        let mut cursor = fid.to_string();
        for _ in 0..32 {
            if cursor.is_empty() || cursor == ROOT {
                break;
            }
            let value = self
                .call_with(
                    "GET",
                    "/getFolderInfo.action",
                    None,
                    &[("folderId", &cursor)],
                )
                .await?;
            let name = name_of(&value);
            if name.is_empty() {
                break;
            }
            nodes.push(PathNode {
                fid: qualify(ProviderId::Cloud189, &cursor),
                name,
            });
            let parent = value
                .get("parentId")
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            cursor = parent;
        }
        nodes.reverse();
        Ok(nodes)
    }

    /// Nickname and avatar URL, from the one endpoint that has them.
    ///
    /// The session response carries neither, and the web `/v2/getLoginedInfos`
    /// endpoint the old code used is gone (404 since at least 2026-11). The open
    /// API answers with `nickName` and a `headPortrait` preview URL instead.
    async fn profile_from_open(&self) -> Option<(String, Option<String>)> {
        let value = self
            .open_post("/open/user/getUserInfoExt.action", &[])
            .await
            .ok()?;
        let name = value
            .get("nickName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        let avatar = value
            .get("headPortrait")
            .or_else(|| value.get("headPortraitUrl2"))
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(str::to_string);
        if name.is_empty() {
            return None;
        }
        Some((name, avatar))
    }

    // ------------------------------------------------------- open API ---

    /// The `open/*` family signs with MD5 and a short-lived access token that
    /// is minted from the session key. The token in the credential file is the
    /// same value, but the server only honours a freshly minted one, so it is
    /// cached in memory for the life of the process.
    async fn open_token(&self) -> AppResult<String> {
        {
            let cached = self.open_token.read().await;
            if let Some((token, until)) = cached.as_ref() {
                let now = chrono::Utc::now().timestamp_millis();
                if !token.is_empty() && *until > now + 60_000 {
                    return Ok(token.clone());
                }
            }
        }
        let (key, _) = self.session().await;
        if key.is_empty() {
            return Err(AppError::new("天翼云盘登录状态已失效，请重新登录"));
        }
        let stamp = chrono::Utc::now().timestamp_millis().to_string();
        // Keys sort as `AppKey` < `Timestamp` < `sessionKey` in ASCII, which is
        // the order the digest expects.
        let plain = format!("AppKey={OPEN_APP_KEY}&Timestamp={stamp}&sessionKey={key}");
        let client = self.http()?;
        crate::ylog!("cloud189 open: minting access token (sessionKey len={})", key.len());
        let res = client
            .get(format!("{API}/open/oauth2/getAccessTokenBySsKey.action"))
            .query(&[("sessionKey", key.as_str())])
            .header("AppKey", OPEN_APP_KEY)
            .header("Timestamp", &stamp)
            .header("Signature", md5_hex(&plain))
            .header("Sign-Type", "1")
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| net_error("换取天翼云盘访问令牌", &e))?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        crate::ylog!(
            "cloud189 open: token HTTP {status} body_len={} head={}",
            body.len(),
            head(&body, 160)
        );
        let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
        if let Some(err) = open_error(&value, status) {
            return Err(AppError::new(format!("换取天翼云盘访问令牌失败：{err}")));
        }
        let token = text_of(&value, "accessToken");
        if token.is_empty() {
            return Err(AppError::new("换取天翼云盘访问令牌失败：响应里没有 accessToken"));
        }
        let now = chrono::Utc::now().timestamp_millis();
        let until = value
            .get("expiresIn")
            .and_then(Value::as_i64)
            .filter(|v| *v > now)
            .unwrap_or(now + 3_600_000);
        *self.open_token.write().await = Some((token.clone(), until));
        Ok(token)
    }

    /// One `open/*` POST. Arguments ride in the form body, which is also what
    /// the signature covers — the same call with the arguments on the query
    /// string answers `accessTokenSignature is not match`.
    async fn open_post(&self, path: &str, form: &[(&str, String)]) -> AppResult<Value> {
        match self.open_post_once(path, form).await {
            Ok(value) => Ok(value),
            // The cached access token is minted from the session key and can be
            // dropped server-side at any time (a second device signing in, a
            // token sweep). Retrying once with a fresh token turns that from a
            // dead end into a hiccup.
            Err(err) if token_stale(&err.0) => {
                crate::ylog!("cloud189 open: token rejected on {path}, re-minting");
                *self.open_token.write().await = None;
                self.open_post_once(path, form).await
            }
            Err(err) => Err(err),
        }
    }

    async fn open_post_once(&self, path: &str, form: &[(&str, String)]) -> AppResult<Value> {
        let token = self.open_token().await?;
        let stamp = chrono::Utc::now().timestamp_millis().to_string();
        let mut pairs: Vec<(String, String)> = vec![
            ("AccessToken".to_string(), token.clone()),
            ("Timestamp".to_string(), stamp.clone()),
        ];
        pairs.extend(form.iter().map(|(k, v)| (k.to_string(), v.clone())));
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let plain = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let client = self.http()?;
        let res = client
            .post(format!("{API}{path}"))
            .header("AccessToken", &token)
            .header("Timestamp", &stamp)
            .header("Signature", md5_hex(&plain))
            .header("Sign-Type", "1")
            .header("Accept", "application/json;charset=UTF-8")
            .header("X-Request-ID", random_id())
            .form(&form.to_vec())
            .send()
            .await
            .map_err(|e| net_error(&format!("天翼云盘请求 {path}"), &e))?;
        let status = res.status().as_u16();
        let body = res.text().await.unwrap_or_default();
        // Every `open/*` failure so far has been diagnosable from the path and
        // the body alone; the form values are never logged because one of them
        // is the access token.
        if status >= 400 || !body.trim().is_empty() {
            crate::ylog!(
                "cloud189 open: POST {path} HTTP {status} body_len={} head={}",
                body.len(),
                head(&body, 200)
            );
        }
        if body.trim().is_empty() {
            return Ok(json!({}));
        }
        let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
        if let Some(err) = open_error(&value, status) {
            return Err(AppError::new(format!("天翼云盘：{err}")));
        }
        Ok(value)
    }

    /// Metadata of a share, addressed by the code in the link.
    /// A cookie header for the `/v2/*` web endpoints, minted from the client
    /// token.
    ///
    /// The two APIs are separate universes: everything else in this driver signs
    /// requests with `sessionSecret`, but `/v2/*` (which is the only place the
    /// recycle bin exists) authenticates with a browser cookie. Rather than ask
    /// the user to log in a second time, `open/ssoLoginByAccessToken` hands back
    /// a URL that sets that cookie when fetched — so one QR scan covers both.
    async fn web_cookie(&self, force: bool) -> AppResult<String> {
        if !force {
            let cached = self.creds.read().await.web_cookie.clone();
            if !cached.trim().is_empty() {
                return Ok(cached);
            }
        }
        let token = self.open_token().await?;
        let stamp = chrono::Utc::now().timestamp_millis().to_string();
        let form = [
            ("appId", APP_ID.to_string()),
            ("redirectUrl", format!("{WEB}/web/main")),
        ];
        let mut pairs = [
            ("AccessToken".to_string(), token.clone()),
            ("Timestamp".to_string(), stamp.clone()),
            ("appId".to_string(), APP_ID.to_string()),
            ("redirectUrl".to_string(), format!("{WEB}/web/main")),
        ];
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let plain = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let res = self
            .http()?
            .post(format!("{API}/open/ssoLoginByAccessToken.action"))
            .header("AccessToken", &token)
            .header("Timestamp", &stamp)
            .header("Signature", md5_hex(&plain))
            .header("Sign-Type", "1")
            .header("Accept", "application/json;charset=UTF-8")
            .header("X-Request-ID", random_id())
            .form(&form[..])
            .send()
            .await
            .map_err(|e| net_error("换取天翼云盘网页会话", &e))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| net_error("读取天翼云盘网页会话", &e))?;
        let start = body.trim().to_string();
        if !start.starts_with("http") {
            crate::ylog!(
                "cloud189 web: sso HTTP {status} head={}",
                head(&body, 200)
            );
            return Err(AppError::new(format!(
                "换取天翼云盘网页会话失败（HTTP {status}）"
            )));
        }
        // The bridge answers with a URL, not JSON; fetching it (through however
        // many hops it takes) is what actually sets COOKIE_LOGIN_USER.
        let mut jar: std::collections::BTreeMap<String, String> = Default::default();
        let mut next = start;
        for _ in 0..5 {
            let res = self
                .http()?
                .get(&next)
                .header("User-Agent", crate::share::UA)
                .header("Cookie", cookie_header(&jar))
                .send()
                .await
                .map_err(|e| net_error("跟随天翼云盘网页登录", &e))?;
            collect_cookies(&mut jar, res.headers());
            let redirect = res
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match redirect {
                Some(loc) if res.status().is_redirection() => {
                    next = if loc.starts_with("http") {
                        loc
                    } else {
                        format!("{WEB}{loc}")
                    };
                }
                _ => break,
            }
        }
        let header = cookie_header(&jar);
        if !header.contains("COOKIE_LOGIN_USER") {
            return Err(AppError::new("天翼云盘网页会话没有返回登录凭据，请重新登录"));
        }
        self.creds.write().await.web_cookie = header.clone();
        self.persist().await;
        crate::ylog!("cloud189 web: session ready ({} cookies)", jar.len());
        Ok(header)
    }

    /// One call against the web API (`cloud.189.cn`, cookie auth), retried once
    /// with a freshly minted cookie when the old one is refused.
    async fn web_request(
        &self,
        post: bool,
        path: &str,
        query: &[(&str, String)],
        form: &[(&str, String)],
    ) -> AppResult<Value> {
        let (session_key, _) = self.session().await;
        for attempt in 0..2 {
            let cookie = self.web_cookie(attempt > 0).await?;
            let method = if post {
                reqwest::Method::POST
            } else {
                reqwest::Method::GET
            };
            let mut req = self
                .http()?
                .request(method, format!("{WEB}{path}"))
                .header("User-Agent", crate::share::UA)
                .header("SessionKey", &session_key)
                .header("Cookie", &cookie)
                .header("Accept", "application/json;charset=UTF-8")
                .header("Referer", format!("{WEB}/web/main/recycle"))
                .header("Origin", WEB)
                .header("sign-type", "1")
                .query(&[("returnType", "JSON")]);
            for (k, v) in query {
                req = req.query(&[(*k, v.as_str())]);
            }
            if post {
                req = req.form(&form.to_vec());
            }
            let res = req
                .send()
                .await
                .map_err(|e| net_error("访问天翼云盘回收站", &e))?;
            let status = res.status().as_u16();
            let body = res
                .text()
                .await
                .map_err(|e| net_error("读取天翼云盘回收站", &e))?;
            if body.trim().is_empty() {
                return Ok(json!({}));
            }
            let value: Value =
                serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
            match resp_error(&value, status) {
                Some(err)
                    if attempt == 0
                        && (err.contains("InvalidSessionKey")
                            || err.contains("cookieUserSession")) =>
                {
                    crate::ylog!("cloud189 web: cookie rejected, minting again");
                    continue;
                }
                Some(err) => return Err(AppError::new(format!("天翼云盘：{err}"))),
                None => return Ok(value),
            }
        }
        Err(AppError::new("天翼云盘网页会话已失效，请重新登录"))
    }

    /// A recycle-bin batch task: create it, then wait for it.
    ///
    /// Captured from the web client (2026-11): restoring, deleting one entry and
    /// emptying the bin are all this one endpoint with a different `type` —
    /// `RESTORE`, `CLEAR_RECYCLE`, `EMPTY_RECYCLE` — and `checkBatchTask` polls
    /// with the same `type` string.
    async fn web_batch(&self, kind: &str, infos: Vec<Value>) -> AppResult<()> {
        let value = self
            .web_request(
                true,
                "/api/open/batch/createBatchTask.action",
                &[],
                &[
                    ("type", kind.to_string()),
                    ("taskInfos", Value::Array(infos).to_string()),
                    ("targetFolderId", String::new()),
                ],
            )
            .await?;
        let task = num_text(value.get("taskId"));
        if task.is_empty() {
            crate::ylog!(
                "cloud189 recycle: no task id for {kind} head={}",
                head(&value.to_string(), 200)
            );
            return Err(AppError::new("天翼云盘没有返回任务号，请稍后重试"));
        }
        for _ in 0..60 {
            let state = self
                .web_request(
                    true,
                    "/api/open/batch/checkBatchTask.action",
                    &[],
                    &[("taskId", task.clone()), ("type", kind.to_string())],
                )
                .await?;
            if state.get("taskStatus").and_then(Value::as_i64).unwrap_or(0) == 4 {
                let failed = state.get("failedCount").and_then(Value::as_i64).unwrap_or(0);
                if failed > 0 {
                    return Err(AppError::new(format!("有 {failed} 项没有处理成功")));
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        Err(AppError::new("天翼云盘回收站任务超时，请稍后刷新确认"))
    }

    async fn share_meta(&self, code: &str) -> AppResult<Value> {
        self.open_post(
            "/open/share/getShareInfoByCodeV2.action",
            &[("shareCode", code.to_string())],
        )
        .await
    }

    /// Exchanges the link code (+ access code) for the numeric share id.
    async fn share_id_of(&self, code: &str, passcode: &str) -> AppResult<String> {
        let value = self
            .open_post(
                "/open/share/checkAccessCode.action",
                &[
                    ("shareCode", code.to_string()),
                    ("accessCode", passcode.to_string()),
                    ("uuid", random_id().replace('-', "")),
                ],
            )
            .await?;
        let id = num_text(value.get("shareId"));
        if id.is_empty() {
            return Err(AppError::new(if passcode.is_empty() {
                "这个分享需要提取码"
            } else {
                "提取码不正确，请重新输入"
            }));
        }
        Ok(id)
    }

    /// One page of a share folder.
    async fn share_page(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<(Vec<ShareEntry>, u64)> {
        let meta = self.share_meta(&share.pwd_id).await?;
        let root = text_of(&meta, "fileId");
        if root.is_empty() {
            return Err(AppError::new("这个分享已失效或被取消"));
        }
        let native = native_of(dir);
        let at_root = native.is_empty() || native == "0" || native == "root" || native == root;
        let fid = if at_root { root.clone() } else { native };
        // A subfolder of a share is always a folder; the root is whatever the
        // share itself points at, which may be a single file.
        let is_folder = if at_root {
            meta.get("isFolder").and_then(Value::as_bool).unwrap_or(false)
        } else {
            true
        };
        let mode = num_text(meta.get("shareMode"));
        let share_id = if share.stoken.is_empty() {
            self.share_id_of(&share.pwd_id, &share.passcode).await?
        } else {
            share.stoken.clone()
        };
        let value = self
            .open_post(
                "/open/share/listShareDir.action",
                &[
                    ("shareId", share_id),
                    ("accessCode", share.passcode.clone()),
                    ("fileId", fid.clone()),
                    ("isFolder", is_folder.to_string()),
                    ("pageNum", "1".to_string()),
                    ("pageSize", SHARE_PAGE_SIZE.to_string()),
                    ("iconOption", "5".to_string()),
                    ("shareMode", if mode.is_empty() { "1".to_string() } else { mode }),
                    ("uuid", random_id().replace('-', "")),
                ],
            )
            .await?;
        let listing = value.get("fileListAO").cloned().unwrap_or_else(|| json!({}));
        let mut entries = Vec::new();
        for item in listing
            .get("folderList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            entries.push(self.share_entry(&item, true));
        }
        for item in listing
            .get("fileList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            entries.push(self.share_entry(&item, false));
        }
        let total = listing.get("count").and_then(Value::as_u64).unwrap_or(0);
        Ok((entries, total))
    }

    fn share_entry(&self, item: &Value, is_dir: bool) -> ShareEntry {
        ShareEntry {
            fid: qualify(ProviderId::Cloud189, &id_of(item)),
            // Share files are addressed by id alone; the share id rides in the
            // session, not in the entry.
            token: String::new(),
            name: name_of(item),
            size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
            is_dir,
            updated_at: parse_189_time(item.get("lastOpTime")).max(parse_189_time(
                item.get("createDate"),
            )) as i64,
        }
    }

    /// Follows the 302 the download gateway answers with, so callers end up on
    /// the CDN url that honours `Range`.
    async fn follow_redirect(&self, url: &str) -> AppResult<String> {
        let url = url.replace("&amp;", "&");
        let url = if let Some(rest) = url.strip_prefix("http://") {
            format!("https://{rest}")
        } else {
            url
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let res = client
            .get(&url)
            .send()
            .await
            .map_err(|e| net_error("获取下载地址", &e))?;
        if let Some(location) = res.headers().get(reqwest::header::LOCATION) {
            let location = location
                .to_str()
                .map_err(|_| AppError::new("下载地址无法解析"))?;
            return Ok(location.to_string());
        }
        Ok(url)
    }
}

#[async_trait::async_trait]
impl DriveProvider for Cloud189Provider {
    fn id(&self) -> ProviderId {
        ProviderId::Cloud189
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Browsing, saving and creating shares go through the `open/share`
            // family. The recycle bin lives on the `/v2/*` web endpoints, which
            // want a cookie session instead of the session-key signature; that
            // cookie is minted from the client token (see `web_cookie`), so the
            // user still only scans once.
            share_open: true,
            share_create: true,
            recycle: true,
            ..Capabilities::BASIC
        }
    }

    async fn auth_state(&self) -> AuthState {
        let creds = self.creds.read().await;
        let authenticated = creds.usable();
        AuthState {
            provider: ProviderId::Cloud189,
            authenticated,
            source: if authenticated { "file" } else { "none" }.to_string(),
            masked: if authenticated {
                Some(mask_token(&creds.session_key))
            } else {
                None
            },
            credential_path: self.credential_path.clone(),
            nickname: Some(creds.nickname.clone()).filter(|v| !v.is_empty()),
            avatar: creds.avatar.clone(),
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        let path = self.credential_path.clone();
        {
            let mut creds = self.creds.write().await;
            *creds = Cloud189Credentials::default();
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(AppError::new(format!("删除登录凭据失败：{err}"))),
        }
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let creds = self.creds.read().await;
        if creds.nickname.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccountInfo {
            nickname: creds.nickname.clone(),
            avatar: creds.avatar.clone(),
        }))
    }

    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        {
            let creds = self.creds.read().await;
            if !creds.nickname.is_empty() && creds.avatar.is_some() {
                return Ok(Some(AccountInfo {
                    nickname: creds.nickname.clone(),
                    avatar: creds.avatar.clone(),
                }));
            }
        }
        // One attempt per process: a failing profile call must not slow every
        // later request down.
        if self.profile_tried.swap(true, Ordering::SeqCst) {
            return self.account().await;
        }
        let Some((nickname, avatar)) = self.profile_from_open().await else {
            return self.account().await;
        };
        let inlined = match avatar {
            Some(url) => match self.http() {
                Ok(client) => crate::share::fetch_avatar(&client, &url).await,
                Err(_) => None,
            },
            None => None,
        };
        {
            let mut creds = self.creds.write().await;
            creds.nickname = nickname.clone();
            if inlined.is_some() {
                creds.avatar = inlined.clone();
            }
        }
        self.persist().await;
        Ok(Some(AccountInfo {
            nickname,
            avatar: inlined,
        }))
    }

    #[allow(unused_assignments)]
    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let folder = api_id(dir);
        let mut entries = Vec::new();
        let mut page = 1;
        let mut total = 0;
        loop {
            let (mut rows, count) = self.list_page(&folder, page).await?;
            total = count;
            let drained = rows.len();
            entries.append(&mut rows);
            if drained < PAGE_SIZE as usize || count == 0 || page >= 200 {
                break;
            }
            page += 1;
        }
        sort_entries(&mut entries);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Cloud189, &folder),
            entries,
            total,
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        self.chain(&api_id(dir)).await
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = check_name(name)?;
        let value = self
            .call_with(
                "POST",
                "/createFolder.action",
                None,
                &[
                    ("folderName", name),
                    ("relativePath", ""),
                    ("parentFolderId", api_id(parent).as_str()),
                ],
            )
            .await?;
        let native = id_of(&value);
        if native.is_empty() {
            return Err(AppError::new("天翼云盘没有返回新文件夹的 id"));
        }
        Ok(qualify(ProviderId::Cloud189, &native))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = check_name(name)?;
        let native = native_of(fid);
        // Files and folders live behind different endpoints, and asking the
        // wrong one answers 200 with no change at all — so the kind has to be
        // known before the call, not guessed from the response.
        let info = self
            .call_with("GET", "/getFileInfo.action", None, &[("fileId", &native)])
            .await
            .unwrap_or(Value::Null);
        let is_dir = info.get("mediaType").is_none();
        let value = if is_dir {
            self.call_with(
                "POST",
                "/renameFolder.action",
                None,
                &[("folderId", &native), ("destFolderName", name)],
            )
            .await?
        } else {
            self.call_with(
                "POST",
                "/renameFile.action",
                None,
                &[("fileId", &native), ("destFileName", name)],
            )
            .await?
        };
        if let Some(err) = resp_error(&value, 200) {
            if err.contains("FileAlreadyExists") {
                return Err(AppError::new(format!("已经有同名项目「{name}」了")));
            }
            return Err(AppError::new(format!("天翼云盘：{err}")));
        }
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Ok(());
        }
        let infos = self.task_infos(fids).await?;
        self.batch("DELETE", None, infos).await
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Ok(0);
        }
        let infos = self.task_infos(fids).await?;
        let count = infos.len();
        let kind = if copy { "COPY" } else { "MOVE" };
        let target = api_id(to);
        let target = if target.is_empty() {
            None
        } else {
            Some(target.as_str())
        };
        self.batch(kind, target, infos).await?;
        Ok(count)
    }

    async fn search(&self, dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // No server-side search, so walk breadth first and stop as soon as the
        // requested number of hits is in. Depth and visit limits keep a huge
        // account from turning a typo into a long scan.
        let mut hits = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let root = self.chain(&api_id(dir)).await.unwrap_or_default();
        queue.push_back(root);
        let mut visited = 0u32;
        while let Some(path) = queue.pop_front() {
            if visited > 80 || path.len() > 8 || hits.len() >= limit as usize {
                break;
            }
            let folder = path
                .last()
                .map(|node| node.fid.clone())
                .unwrap_or_else(|| qualify(ProviderId::Cloud189, api_id(dir).as_str()));
            let listing = match self.list(&folder).await {
                Ok(listing) => listing,
                Err(_) => continue,
            };
            visited += 1;
            for entry in listing.entries {
                let name = entry.name.to_lowercase();
                if name.contains(&needle) {
                    let mut hit_path = path.clone();
                    hits.push(SearchHit {
                        entry: entry.clone(),
                        path: hit_path.split_off(0),
                    });
                    if hits.len() >= limit as usize {
                        break;
                    }
                } else if entry.is_dir {
                    let mut child = path.clone();
                    if let Some(last) = child.last() {
                        child.push(PathNode {
                            fid: last.fid.clone(),
                            name: last.name.clone(),
                        });
                        child.pop();
                        child.pop();
                    }
                    child.push(PathNode {
                        fid: entry.fid.clone(),
                        name: entry.name.clone(),
                    });
                    queue.push_back(child);
                }
            }
        }
        Ok(hits)
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let url = self.download_url(fid).await?;
        let mut req = self.http()?.get(&url).header("User-Agent", crate::share::UA);
        if start > 0 {
            req = req.header("Range", format!("bytes={start}-"));
        }
        let res = req
            .send()
            .await
            .map_err(|e| net_error("下载文件", &e))?;
        let status = res.status();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            let code = status.as_u16();
            let body = res.text().await.unwrap_or_default();
            return Err(AppError::new(format!(
                "下载失败（HTTP {code}）：{}",
                head(&body, 120)
            )));
        }
        if start > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(RANGE_IGNORED));
        }
        let size = res.content_length().unwrap_or(0);
        let stream = {
            use futures_util::StreamExt;
            res.bytes_stream()
                .map(|chunk| chunk.map_err(|e| AppError::new(format!("读取失败：{e}"))))
        };
        Ok(DownloadHandle {
            size,
            md5: None,
            stream: Box::new(stream),
        })
    }

    fn supports_ranges(&self) -> bool {
        true
    }

    async fn open_download_range(
        &self,
        fid: &str,
        start: u64,
        end: Option<u64>,
    ) -> RangeResult {
        // A fresh link per segment, like the other drivers: the CDN load
        // balances on the URL, so reusing one pins every segment to one node.
        let url = match self.download_url(fid).await {
            Ok(url) => url,
            Err(err) => return RangeResult::from_error(err),
        };
        let range = match (start, end) {
            (0, None) => None,
            (0, Some(stop)) if stop > 0 => Some(format!("bytes=0-{}", stop - 1)),
            (0, Some(_)) => None,
            (from, Some(stop)) if stop > from => Some(format!("bytes={from}-{}", stop - 1)),
            (from, _) => Some(format!("bytes={from}-")),
        };
        let client = match self.http() {
            Ok(client) => client,
            Err(err) => return RangeResult::from_error(err),
        };
        let mut req = client.get(&url).header("User-Agent", crate::share::UA);
        if let Some(range) = range {
            req = req.header("Range", range);
        }
        let res = match req.send().await {
            Ok(res) => res,
            Err(err) => return RangeResult::Failed(net_error("下载文件", &err)),
        };
        let status = res.status().as_u16();
        if matches!(status, 401 | 403 | 410 | 412) {
            return RangeResult::Expired;
        }
        if start > 0 && status != 206 {
            return RangeResult::RangeIgnored;
        }
        let served = res
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let size = res.content_length().unwrap_or(0);
        let stream = {
            use futures_util::StreamExt;
            res.bytes_stream()
                .map(|chunk| chunk.map_err(|e| AppError::new(format!("读取失败：{e}"))))
        };
        RangeResult::body(
            DownloadHandle {
                size,
                md5: None,
                stream: Box::new(stream),
            },
            start,
            served.as_deref(),
        )
    }

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        let name = check_name(name)?;
        sink.phase("hashing");
        let md5 = md5_file(local, sink.clone(), token.clone()).await?;
        let size = std::fs::metadata(local)
            .map(|meta| meta.len())
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?;
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        sink.phase("uploading");
        let folder = api_id(parent);
        let create = self
            .call_with(
                "POST",
                "/createUploadFile.action",
                None,
                &[
                    ("parentFolderId", folder.as_str()),
                    ("fileName", name),
                    ("size", size.to_string().as_str()),
                    ("md5", md5.as_str()),
                    ("opertype", "1"),
                    ("flag", "1"),
                    ("resumePolicy", "1"),
                    ("isLog", "0"),
                ],
            )
            .await?;
        if let Some(err) = resp_error(&create, 200) {
            if err.contains("UserDayFlowOverLimited") {
                return Err(AppError::new("天翼云盘今日上传流量已用尽，请明天再试"));
            }
            if err.contains("InfoSecurityErrorCode") {
                return Err(AppError::new("天翼云盘拒绝了这个文件（内容审核未通过）"));
            }
            return Err(AppError::new(format!("创建上传任务失败：{err}")));
        }
        let upload_id = num_text(create.get("uploadFileId"));
        let upload_url = text_of(&create, "fileUploadUrl");
        let commit_url = text_of(&create, "fileCommitUrl");
        if upload_id.is_empty() || commit_url.is_empty() {
            return Err(AppError::new("创建上传任务失败：响应缺少任务信息"));
        }
        // A known md5 lets the server answer with a pointer to the file it
        // already has; nothing to upload in that case.
        let instant = create
            .get("fileDataExists")
            .map(|v| match v {
                Value::Number(n) => n.as_i64().unwrap_or(0) == 1,
                Value::Bool(flag) => *flag,
                Value::String(text) => text == "1",
                _ => false,
            })
            .unwrap_or(false);
        if !instant {
            let (session_key, _) = self.session().await;
            let raw = format!("bytes=0-{}", size.saturating_sub(1));
            let date = http_date();
            let signature = sign_hmac(
                &self.creds.read().await.session_secret,
                &session_key,
                "PUT",
                &upload_url,
                &date,
            );
            let file = tokio::fs::File::open(local)
                .await
                .map_err(|e| AppError::new(format!("打开本地文件失败：{e}")))?;
            let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
            let res = self
                .http()?
                .put(format!("{upload_url}?{UPLOAD_SUFFIX}"))
                .header("Date", &date)
                .header("SessionKey", &session_key)
                .header("Sign-Type", "1")
                .header("Signature", &signature)
                .header("Edrive-UploadFileId", &upload_id)
                .header("Content-Type", "application/octet-stream")
                .header("Edrive-UploadFileRange", raw)
                .header("ResumePolicy", "1")
                .body(body)
                .send()
                .await
                .map_err(|e| net_error("上传文件", &e))?;
            let status = res.status().as_u16();
            let text = res
                .text()
                .await
                .map_err(|e| net_error("读取上传响应", &e))?;
            if let Some(err) = xml_error(&text) {
                if !err.contains("UploadFileCompeletedError") {
                    return Err(AppError::new(format!("上传失败：{err}")));
                }
            } else if status >= 400 {
                return Err(AppError::new(format!("上传失败（HTTP {status}）")));
            }
            sink.progress(size);
        }
        // The commit endpoint wants the client suffix on the URL and its fields
        // in the body; with either missing it answers 200 and an empty body.
        let (session_key, _) = self.session().await;
        let secret = self.creds.read().await.session_secret.clone();
        let commit_full = format!("{commit_url}?{UPLOAD_SUFFIX}");
        let date = http_date();
        let signature = sign_hmac(&secret, &session_key, "POST", &commit_full, &date);
        let res = self
            .http()?
            .post(&commit_full)
            .header("Date", &date)
            .header("SessionKey", &session_key)
            .header("Sign-Type", "1")
            .header("Signature", &signature)
            .header("Accept", "application/json;charset=UTF-8")
            .form(&[
                ("uploadFileId", upload_id.clone()),
                ("opertype", "1".to_string()),
                ("isLog", "0".to_string()),
                ("ResumePolicy", "1".to_string()),
            ])
            .send()
            .await
            .map_err(|e| net_error("提交上传任务", &e))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| net_error("读取上传响应", &e))?;
        let mut native = if let Ok(value) = serde_json::from_str::<Value>(&body) {
            id_of(&value)
        } else {
            String::new()
        };
        if native.is_empty() {
            native = between(&body, "<id>", "</id>");
        }
        if native.is_empty() {
            crate::ylog!(
                "cloud189 upload: commit HTTP {status} head={}",
                head(&body, 200)
            );
        }
        if native.is_empty() {
            return Err(AppError::new("上传已完成但天翼云盘没有返回文件 id"));
        }
        self.refresh().await.ok();
        Ok(Some(qualify(ProviderId::Cloud189, &native)))
    }

    async fn refresh(&self) -> AppResult<()> {
        self.call_with("GET", "/getUserInfo.action", None, &[]).await?;
        Ok(())
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        let value = self
            .call_with("GET", "/portal/getUserSizeInfo.action", None, &[])
            .await?;
        let info = value.get("cloudCapacityInfo").cloned().unwrap_or(Value::Null);
        let used = info.get("usedSize").and_then(Value::as_u64);
        let total = info.get("totalSize").and_then(Value::as_u64);
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(QuotaInfo { used, total })),
            _ => Ok(None),
        }
    }

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let meta = self.share_meta(id).await?;
        let need = meta
            .get("needAccessCode")
            .map(|v| match v {
                Value::Number(n) => n.as_i64().unwrap_or(1) != 0,
                Value::Bool(flag) => *flag,
                other => other.as_str().map(|s| s == "1").unwrap_or(true),
            })
            .unwrap_or(true);
        if need && passcode.trim().is_empty() {
            return Err(AppError::new("这个分享需要提取码"));
        }
        let share_id = self.share_id_of(id, passcode).await?;
        let file_id = text_of(&meta, "fileId");
        let is_folder = meta
            .get("isFolder")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let owner = meta
            .pointer("/creator/nickName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let avatar = meta
            .pointer("/creator/iconURL")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        let inlined = match avatar {
            Some(url) => match self.http() {
                Ok(client) => crate::share::fetch_avatar(&client, &url).await,
                Err(_) => None,
            },
            None => None,
        };
        let file_num = meta
            .get("fileNum")
            .and_then(Value::as_u64)
            .unwrap_or(if is_folder { 0 } else { 1 });
        Ok(ShareSession {
            provider: ProviderId::Cloud189.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            stoken: share_id,
            title: text_of(&meta, "fileName"),
            owner,
            owner_avatar: inlined,
            file_num,
            total_size: meta.get("fileSize").and_then(Value::as_u64).unwrap_or(0),
            expired_at: expire_stamp(&meta),
            first_fid: Some(qualify(ProviderId::Cloud189, &file_id)),
            can_save: true,
            web_url: Some(format!("{WEB}/t/{id}")),
        })
    }

    async fn share_list(&self, share: &crate::transfer::ShareRef, dir: &str) -> AppResult<ShareListing> {
        let (entries, total) = self.share_page(share, dir).await?;
        Ok(ShareListing {
            pdir_fid: dir.to_string(),
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
            return Ok(0);
        }
        let infos = entries
            .iter()
            .map(|entry| {
                json!({
                    "fileId": native_of(&entry.fid),
                    "fileName": entry.name,
                    "isFolder": if entry.is_dir { 1 } else { 0 },
                })
            })
            .collect::<Vec<_>>();
        let count = infos.len() as u64;
        let value = self
            .open_post(
                "/open/batch/createBatchTask.action",
                &[
                    ("type", "SHARE_SAVE".to_string()),
                    ("taskInfos", Value::Array(infos).to_string()),
                    ("shareId", share.stoken.clone()),
                    ("targetFolderId", api_id(to.unwrap_or("0"))),
                ],
            )
            .await
            .map_err(|err| {
                if err.0.contains("ShareSaveTaskIsAlreadyExist") {
                    AppError::new("这个分享你已经转存过了，天翼云盘不允许重复转存")
                } else {
                    err
                }
            })?;
        let task = num_text(value.get("taskId"));
        if task.is_empty() {
            return Err(AppError::new("转存失败：没有拿到任务号"));
        }
        self.wait_open_task("SHARE_SAVE", &task).await?;
        Ok(count)
    }

    async fn share_walk(&self, share: &crate::transfer::ShareRef, dir: &str) -> AppResult<Vec<ShareFile>> {
        const MAX_FILES: usize = 5000;
        const MAX_DEPTH: usize = 12;
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<(String, std::path::PathBuf, usize)> =
            Default::default();
        queue.push_back((dir.to_string(), std::path::PathBuf::new(), 0));
        while let Some((folder, prefix, depth)) = queue.pop_front() {
            if out.len() >= MAX_FILES {
                break;
            }
            let entries = match self.share_page(share, &folder).await {
                Ok((rows, _)) => rows,
                Err(_) => continue,
            };
            for entry in entries {
                let mut path = prefix.clone();
                path.push(crate::paths::safe_component(&entry.name));
                if entry.is_dir {
                    if depth < MAX_DEPTH {
                        queue.push_back((entry.fid.clone(), path, depth + 1));
                    }
                } else {
                    out.push(ShareFile {
                        relative: path,
                        fid: entry.fid,
                        token: entry.token,
                        name: entry.name,
                        size: entry.size,
                    });
                    if out.len() >= MAX_FILES {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    async fn share_link(&self, share: &crate::transfer::ShareRef, fid: &str) -> AppResult<ShareLink> {
        let value = self
            .open_post(
                "/open/file/getFileDownloadUrl.action",
                &[
                    ("fileId", native_of(fid)),
                    ("dt", "1".to_string()),
                    ("shareId", share.stoken.clone()),
                ],
            )
            .await?;
        let url = text_of(&value, "fileDownloadUrl");
        if url.is_empty() {
            return Err(AppError::new("这个文件暂时拿不到下载地址"));
        }
        let url = self.follow_redirect(&url).await?;
        Ok(ShareLink {
            url,
            headers: vec![("User-Agent".to_string(), crate::share::UA.to_string())],
            size: 0,
            cleanup_fid: None,
        })
    }

    async fn create_share(
        &self,
        fids: &[String],
        _title: &str,
        days: u32,
        _passcode: &str,
    ) -> AppResult<ShareCreated> {
        if fids.is_empty() {
            return Err(AppError::new("请先选择要分享的内容"));
        }
        if fids.len() > 1 {
            // `createBatchShare` is the multi-select endpoint and it answers
            // `InvalidArgument` for every body shape we could derive from the
            // web client, so one entry at a time is the honest limit.
            return Err(AppError::new(
                "天翼云盘一次只能分享一个文件或文件夹，请逐个分享",
            ));
        }
        // 2099 is the "permanent" dial: anything but a day count is read as that.
        let expire = if days == 0 {
            "2099".to_string()
        } else {
            days.to_string()
        };
        let native = native_of(&fids[0]);
        crate::ylog!("cloud189 share: create fileId={native} expireTime={expire} fids=1");
        let value = self
            .open_post(
                "/open/share/createShareLink.action",
                &[
                    ("fileId", native),
                    ("expireTime", expire),
                    ("withAccessCode", "1".to_string()),
                ],
            )
            .await
            .map_err(|err| {
                if err.0.contains("Argument invalid") {
                    AppError::new("天翼云盘不能直接分享系统文件夹或该位置，请选择自己的文件/文件夹再试")
                } else {
                    err
                }
            })?;
        let first = value
            .get("shareLinkList")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .cloned()
            .unwrap_or(Value::Null);
        let url = {
            let primary = text_of(&first, "accessUrl");
            if primary.is_empty() {
                text_of(&first, "url")
            } else {
                primary
            }
        };
        if url.is_empty() {
            crate::ylog!(
                "cloud189 share: no link in response head={}",
                head(&value.to_string(), 300)
            );
            return Err(AppError::new("创建分享失败：没有拿到分享链接"));
        }
        crate::ylog!(
            "cloud189 share: created {url} code={}",
            text_of(&first, "accessCode")
        );
        Ok(ShareCreated {
            url,
            passcode: text_of(&first, "accessCode"),
            expired_at: if days == 0 {
                0
            } else {
                nanos() as i64 + days as i64 * 86_400_000
            },
            share_id: num_text(first.get("shareId")),
        })
    }

    async fn revoke_share(&self, share_id: &str, _fids: &[String]) -> AppResult<()> {
        self.open_post(
            "/open/share/cancelShareFile.action",
            &[("shareId", share_id.to_string())],
        )
        .await?;
        Ok(())
    }

    async fn share_records(&self) -> AppResult<Vec<ShareRecord>> {
        let value = self
            .open_post(
                "/open/share/listShareFiles.action",
                &[
                    ("pageNum", "1".to_string()),
                    ("pageSize", "100".to_string()),
                ],
            )
            .await?;
        let rows = value
            .get("fileList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let now = nanos() as i64;
        Ok(rows
            .into_iter()
            .map(|row| {
                let created = row
                    .get("shareDate")
                    .or_else(|| row.get("createDate"))
                    .map(|v| parse_189_time(Some(v)) as i64)
                    .unwrap_or_default();
                // `expireTime` is a *remaining day count* here, not a stamp;
                // `expireType == 0` marks a permanent link.
                let days = row.get("expireTime").and_then(Value::as_i64).unwrap_or(0);
                let kind = row.get("expireType").and_then(Value::as_i64).unwrap_or(0);
                let permanent = kind == 0 || days <= 0;
                let expired_at = if permanent {
                    0
                } else {
                    created + days * 86_400_000
                };
                ShareRecord {
                    share_id: num_text(row.get("shareId")),
                    url: text_of(&row, "accessURL"),
                    title: text_of(&row, "fileName"),
                    passcode: text_of(&row, "accessCode"),
                    expired_at,
                    created_at: created,
                    file_num: 1,
                    size: row.get("fileSize").and_then(Value::as_i64).unwrap_or(0),
                    // The endpoint reports none of these.
                    visit_count: 0,
                    save_count: 0,
                    download_count: 0,
                    expired: !permanent && expired_at > 0 && expired_at < now,
                    fids: vec![qualify(
                        ProviderId::Cloud189,
                        &num_text(row.get("fileId")),
                    )],
                }
            })
            .collect())
    }

    async fn recycle_list(&self, offset: u32, limit: u32) -> AppResult<Vec<RecycleEntry>> {
        let page_size = limit.clamp(1, 100);
        let mut page = offset / page_size + 1;
        let mut skip = offset % page_size;
        let mut out = Vec::new();
        while out.len() < limit as usize {
            let value = self
                .web_request(
                    false,
                    "/api/portal/listRecycleBinFilesV3.action",
                    &[
                        ("pageNum", page.to_string()),
                        ("pageSize", page_size.to_string()),
                        ("iconOption", "1".to_string()),
                        ("family", "false".to_string()),
                    ],
                    &[],
                )
                .await?;
            let rows = value
                .get("fileList")
                .or_else(|| value.get("fileListAO").and_then(|v| v.get("fileList")))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                break;
            }
            let total = rows.len();
            for item in rows.into_iter().skip(skip as usize) {
                let native = id_of(&item);
                if native.is_empty() {
                    continue;
                }
                out.push(RecycleEntry {
                    fid: qualify(ProviderId::Cloud189, &native),
                    name: name_of(&item),
                    is_dir: match item.get("isFolder").and_then(Value::as_bool) {
                        Some(flag) => flag,
                        None => item.get("isFolder").and_then(Value::as_i64).unwrap_or(0) == 1,
                    },
                    size: item.get("size").and_then(Value::as_u64).unwrap_or_default(),
                    deleted_at: item
                        .get("lastOpTime")
                        .or_else(|| item.get("createDate"))
                        .map(|v| parse_189_time(Some(v)))
                        .unwrap_or_default(),
                    token: native,
                    provider: ProviderId::Cloud189.as_str().to_string(),
                });
                if out.len() >= limit as usize {
                    break;
                }
            }
            skip = 0;
            if total < page_size as usize {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    async fn recycle_restore(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        if entries.is_empty() {
            return Ok(0);
        }
        let infos = entries
            .iter()
            .map(|entry| {
                json!({
                    "fileId": entry.token,
                    "fileName": entry.name,
                    "isFolder": if entry.is_dir { 1 } else { 0 },
                })
            })
            .collect::<Vec<_>>();
        let count = infos.len() as u32;
        self.web_batch("RESTORE", infos).await?;
        Ok(count)
    }

    async fn recycle_purge(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        if entries.is_empty() {
            return Ok(0);
        }
        let infos = entries
            .iter()
            .map(|entry| {
                json!({
                    "fileId": entry.token,
                    "fileName": entry.name,
                    "isFolder": if entry.is_dir { 1 } else { 0 },
                })
            })
            .collect::<Vec<_>>();
        let count = infos.len() as u32;
        self.web_batch("CLEAR_RECYCLE", infos).await?;
        Ok(count)
    }

    async fn recycle_clear(&self) -> AppResult<()> {
        self.web_batch("EMPTY_RECYCLE", Vec::new()).await
    }
}

impl Cloud189Provider {
    /// `taskInfos` needs each entry's name and kind, which the batch endpoints
    /// do not accept ids for.
    /// Polls an `open/batch` task to completion.
    async fn wait_open_task(&self, kind: &str, task: &str) -> AppResult<()> {
        for _ in 0..120 {
            let value = self
                .open_post(
                    "/open/batch/checkBatchTask.action",
                    &[
                        ("type", kind.to_string()),
                        ("taskId", task.to_string()),
                    ],
                )
                .await?;
            let state = value.get("taskStatus").and_then(Value::as_i64).unwrap_or(0);
            if state == 4 {
                let failed = value.get("failedCount").and_then(Value::as_i64).unwrap_or(0);
                if failed > 0 {
                    return Err(AppError::new(format!(
                        "转存失败：{failed} 个文件没有保存成功（可能超出容量或重名）"
                    )));
                }
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err(AppError::new("转存任务超时，请稍后在网页版确认"))
    }

    async fn task_infos(&self, fids: &[String]) -> AppResult<Vec<Value>> {
        let mut infos = Vec::new();
        for fid in fids {
            let native = native_of(fid);
            // `getFileInfo.action` answers for folders as well, and a folder is
            // the one that comes back without a `mediaType` key. That keeps this
            // to a single call per entry, and the name is the authoritative one
            // (the listing's name can be stale after a rename elsewhere).
            let value = self
                .call_with("GET", "/getFileInfo.action", None, &[("fileId", &native)])
                .await?;
            let is_folder = value.get("mediaType").is_none();
            infos.push(json!({
                "fileId": native,
                "fileName": name_of(&value),
                "isFolder": if is_folder { 1 } else { 0 },
            }));
        }
        Ok(infos)
    }
}

// ------------------------------------------------------------- helpers ---

/// Lowercase MD5 of a string, the digest the `open/*` family signs with.
fn cookie_header(jar: &std::collections::BTreeMap<String, String>) -> String {
    jar.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Keep whatever `Set-Cookie` names/values a hop handed back; the rest of the
/// attributes (path, expiry, flags) do not matter for a short replay.
fn collect_cookies(jar: &mut std::collections::BTreeMap<String, String>, headers: &HeaderMap) {
    for raw in headers.get_all(SET_COOKIE) {
        let Ok(text) = raw.to_str() else { continue };
        let pair = text.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        jar.insert(name.to_string(), value.to_string());
    }
}

fn md5_hex(text: &str) -> String {
    use md5::Digest as _;
    let mut hasher = md5::Md5::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Reads the `errorCode`/`code` family that the `open/*` family answers with.
/// A network-level failure, worded in Chinese. The raw `reqwest` text
/// ("error sending request for url …") reads like gibberish in the toast and
/// hides the one thing that matters: which call failed.
fn net_error(what: &str, err: &reqwest::Error) -> AppError {
    if err.is_timeout() {
        return AppError::new(format!("{what}超时，请检查网络后重试"));
    }
    if err.is_connect() {
        return AppError::new(format!("{what}失败：连不上服务器，请检查网络"));
    }
    AppError::new(format!("{what}失败：{err}"))
}

/// Does this error mean "the access token we sent is no longer valid"?
fn token_stale(text: &str) -> bool {
    text.contains("InvalidAccessToken") || text.contains("AccessToen not exits")
}

fn open_error(value: &Value, status: u16) -> Option<String> {
    for key in ["errorCode", "code"] {
        match value.get(key) {
            Some(Value::String(s)) if !s.is_empty() && s != "0" && s != "SUCCESS" => {
                let detail = ["errorMsg", "message", "msg", "res_message"]
                    .iter()
                    .filter_map(|k| value.get(*k).and_then(Value::as_str))
                    .find(|t| !t.is_empty() && *t != "成功")
                    .unwrap_or(s.as_str());
                return Some(if detail == s {
                    s.clone()
                } else {
                    format!("{detail}（{s}）")
                });
            }
            Some(Value::Number(n)) if n.as_i64().unwrap_or(0) != 0 => {
                return Some(n.to_string());
            }
            _ => {}
        }
    }
    resp_error(value, status)
}

/// Ids come back as strings on some endpoints and numbers on others.
fn num_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// Share expiry: `expireType: 1` means "`expireTime` days from now", `0` means
/// the share never expires.
fn expire_stamp(meta: &Value) -> i64 {
    let kind = meta.get("expireType").and_then(Value::as_i64).unwrap_or(0);
    let days = meta.get("expireTime").and_then(Value::as_i64).unwrap_or(0);
    if kind != 1 || days <= 0 {
        return 0;
    }
    let created = parse_189_time(meta.get("shareDate")) as i64;
    let created = if created > 0 {
        created
    } else {
        chrono::Utc::now().timestamp_millis()
    };
    created + days * 86_400_000
}

fn api_id(dir: &str) -> String {
    let native = native_of(dir);
    if native.is_empty() || native == "0" || native == "root" {
        ROOT.to_string()
    } else {
        native
    }
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn id_of(value: &Value) -> String {
    value
        .get("id")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn name_of(value: &Value) -> String {
    let name = value
        .get("name")
        .or_else(|| value.get("fileName"))
        .or_else(|| value.get("folderName"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The API escapes apostrophes as `\'`.
    name.replace("\\'", "'")
}

/// `clientType=TELEPC&version=..&channelId=..&rand=<a>_<b>`.
pub(crate) fn client_suffix() -> Vec<(&'static str, String)> {
    vec![
        ("clientType", CLIENT_TYPE.to_string()),
        ("version", VERSION.to_string()),
        ("channelId", CHANNEL_ID.to_string()),
        (
            "rand",
            format!("{}_{}", nanos() % 100_000, nanos() % 10_000_000_000),
        ),
    ]
}

/// `Date` header in HTTP format, always GMT.
fn http_date() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// `k=v&k=v`, keys sorted — the order is part of the signature.
/// HMAC-SHA1 over the fixed four-part string, hex-encoded uppercase.
fn sign_hmac(secret: &str, session_key: &str, method: &str, url: &str, date: &str) -> String {
    let plain = format!(
        "SessionKey={session_key}&Operate={method}&RequestURI={}&Date={date}",
        request_uri(url)
    );
    let mut mac = match <Hmac<Sha1> as Mac>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(plain.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// The path part of a URL, without the query.
fn request_uri(url: &str) -> String {
    let rest = match url.find("://") {
        Some(at) => &url[at + 3..],
        None => url,
    };
    match rest.find('/') {
        Some(at) => rest[at..]
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .to_string(),
        None => "/".to_string(),
    }
}

/// Reads the `res_code`/`errorCode` family of failures.
///
/// A code of `0` — as a number, or as the string `"0"` that a few endpoints
/// answer with — means success. The accompanying text is only used when it says
/// something: several failures come back as `res_message: "成功"` with the real
/// reason in `errorCode`, and surfacing that combination would report a failure
/// as "成功".
fn resp_error(value: &Value, status: u16) -> Option<String> {
    let code = ["res_code", "errorCode"].iter().find_map(|key| {
        match value.get(*key) {
            Some(Value::Number(n)) => {
                let n = n.as_i64().unwrap_or(0);
                (n != 0).then(|| n.to_string())
            }
            Some(Value::String(s)) if !s.is_empty() && s != "0" => Some(s.clone()),
            _ => None,
        }
    });
    let detail = ["errorMsg", "message", "res_message"]
        .iter()
        .find_map(|key| {
            value
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty() && *text != "成功")
        })
        .map(str::to_string);
    if let Some(code) = code {
        return Some(match detail {
            Some(text) => format!("{text}（{code}）"),
            None => code,
        });
    }
    if let Some(text) = value
        .get("error")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    if status >= 400 {
        return Some(detail.unwrap_or_else(|| format!("HTTP {status}")));
    }
    None
}

fn xml_error(body: &str) -> Option<String> {
    let code = between(body, "<code>", "</code>");
    let message = between(body, "<message>", "</message>");
    if code.is_empty() && message.is_empty() {
        return None;
    }
    Some(match (code.is_empty(), message.is_empty()) {
        (false, false) => format!("{message}（{code}）"),
        (false, true) => code,
        (true, false) => message,
        _ => return None,
    })
}

/// A short, single-line excerpt of a response body for the log. Line breaks
/// would split one diagnostic over several log entries.
fn head(text: &str, n: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= n {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(n).collect();
    format!("{cut}…")
}

fn between(hay: &str, start: &str, end: &str) -> String {
    let Some(at) = hay.find(start) else {
        return String::new();
    };
    let rest = &hay[at + start.len()..];
    let Some(stop) = rest.find(end) else {
        return String::new();
    };
    rest[..stop].trim().to_string()
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// A UUID-shaped request id. The drive only checks that it is present.
pub(crate) fn random_id() -> String {
    let seed = nanos();
    let mut x = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
    let mut hex = String::with_capacity(32);
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        hex.push_str(&format!("{x:016x}"));
    }
    let hex = &hex[..32];
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Timestamps arrive in a few shapes, none of them RFC 3339.
fn parse_189_time(value: Option<&Value>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    if let Some(number) = value.as_i64() {
        // Milliseconds when it looks like one, seconds otherwise.
        return if number > 100_000_000_000 {
            number as u64
        } else {
            (number as u64) * 1000
        };
    }
    let Some(text) = value.as_str() else {
        return 0;
    };
    let text = text
        .replace(['\u{202f}', '\u{00a0}'], " ")
        .trim()
        .to_string();
    if text.is_empty() {
        return 0;
    }
    let with_zone = if has_zone(&text) {
        normalize_zone(&text)
    } else {
        format!("{text} +0800")
    };
    for format in ["%Y-%m-%d %H:%M:%S %z", "%Y-%m-%d %H:%M:%S%.f %z"] {
        if let Ok(stamp) = chrono::DateTime::parse_from_str(&with_zone, format) {
            return stamp.timestamp_millis().max(0) as u64;
        }
    }
    // `Jan 2, 2006 3:04:05 PM -07` and the comma variant.
    for format in ["%b %e, %Y %l:%M:%S %p %z", "%b %e, %Y, %l:%M:%S %p %z"] {
        if let Ok(stamp) = chrono::DateTime::parse_from_str(&with_zone, format) {
            return stamp.timestamp_millis().max(0) as u64;
        }
    }
    super::parse_time(value)
}

fn has_zone(text: &str) -> bool {
    // `+08`, `+0800`, `-07`, or a trailing `Z`.
    let tail = text.rsplit(' ').next().unwrap_or_default();
    tail == "Z" || (tail.len() >= 3 && (tail.starts_with('+') || tail.starts_with('-')))
}

/// `%z` only accepts `+HHMM`/`+HH:MM`, while 189 writes `+08`.
fn normalize_zone(text: &str) -> String {
    let tail = text.rsplit(' ').next().unwrap_or_default();
    if tail.len() == 3 && (tail.starts_with('+') || tail.starts_with('-')) {
        return format!("{text}00");
    }
    text.to_string()
}

fn mask_token(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 12 {
        return "…".to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

async fn md5_file(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<String> {
    use md5::Digest as _;
    use tokio::io::AsyncReadExt;

    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = md5::Md5::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let n = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(AppError::new("已取消")),
            n = file.read(&mut buf) => n?,
        };
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        sink.progress(total);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uri_drops_the_host_and_query() {
        assert_eq!(
            request_uri("https://api.cloud.189.cn/listFiles.action?folderId=-11"),
            "/listFiles.action"
        );
        assert_eq!(
            request_uri("https://upload.cloud.189.cn/person/initMultiUpload?x=1"),
            "/person/initMultiUpload"
        );
        assert_eq!(request_uri("https://api.cloud.189.cn"), "/");
    }

    #[test]
    fn signature_is_uppercase_hex_and_stable() {
        let one = sign_hmac(
            "secret",
            "key",
            "GET",
            "https://api.cloud.189.cn/x.action?a=1",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        );
        let two = sign_hmac(
            "secret",
            "key",
            "GET",
            "https://api.cloud.189.cn/x.action?a=1",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        );
        assert_eq!(one, two);
        assert_eq!(one.len(), 40);
        assert_eq!(one, one.to_uppercase());
        // The query string is not part of the digest, so a different argument
        // list keeps the same signature — that is what lets the adapter send
        // its arguments as plain query values.
        let three = sign_hmac(
            "secret",
            "key",
            "GET",
            "https://api.cloud.189.cn/x.action?b=2",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        );
        assert_eq!(one, three);
        // The date and the method are.
        let four = sign_hmac(
            "secret",
            "key",
            "POST",
            "https://api.cloud.189.cn/x.action?a=1",
            "Mon, 01 Jan 2024 00:00:00 GMT",
        );
        assert_ne!(one, four);
    }

    #[test]
    fn http_date_has_the_gmt_shape() {
        let date = http_date();
        assert!(date.ends_with(" GMT"), "{date}");
        assert_eq!(date.len(), 29, "{date}");
    }

    #[test]
    fn random_id_is_uuid_shaped() {
        let id = random_id();
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(id.matches('-').count(), 4);
    }

    #[test]
    fn api_id_maps_root() {
        assert_eq!(api_id("0"), "-11");
        assert_eq!(api_id("cloud189:0"), "-11");
        assert_eq!(api_id(""), "-11");
        assert_eq!(api_id("cloud189:12345"), "12345");
        assert_eq!(api_id("12345"), "12345");
    }

    #[test]
    fn times_parse_in_every_shape() {
        let millis = parse_189_time(Some(&json!("2024-05-06 07:08:09")));
        assert!(millis > 1_700_000_000_000, "{millis}");
        let with_zone = parse_189_time(Some(&json!("2024-05-06 07:08:09 +08")));
        assert!(with_zone > 1_700_000_000_000, "{with_zone}");
        let seconds = parse_189_time(Some(&json!(1_700_000_000)));
        assert_eq!(seconds, 1_700_000_000_000);
        let millis_number = parse_189_time(Some(&json!(1_700_000_000_123i64)));
        assert_eq!(millis_number, 1_700_000_000_123);
        assert_eq!(parse_189_time(None), 0);
        assert_eq!(parse_189_time(Some(&json!(""))), 0);
    }

    #[test]
    fn names_unescape_apostrophes() {
        assert_eq!(name_of(&json!({"name": "it\\'s"})), "it's");
        assert_eq!(name_of(&json!({"fileName": "a.txt"})), "a.txt");
        assert_eq!(name_of(&json!({})), "");
    }

    #[test]
    fn errors_are_detected_in_every_shape() {
        assert!(resp_error(&json!({"res_code": 0}), 200).is_none());
        assert!(resp_error(&json!({}), 200).is_none());
        assert!(resp_error(&json!({"res_code": "FileAlreadyExists"}), 200).is_some());
        assert!(resp_error(
            &json!({"errorCode": "InvalidArgument", "errorMsg": "bad"}),
            400
        )
        .is_some());
        assert!(resp_error(&json!({"error": "boom"}), 200).is_some());
        assert!(resp_error(&json!({}), 500).is_some());
    }

    #[test]
    fn xml_errors_are_unpacked() {
        let body = "<error><code>UploadFileCompeletedError</code><message>done</message></error>";
        assert_eq!(
            xml_error(body).as_deref(),
            Some("done（UploadFileCompeletedError）")
        );
        assert!(xml_error("<file><id>1</id></file>").is_none());
    }

    #[test]
    fn mask_hides_the_middle() {
        assert_eq!(mask_token(""), "");
        assert_eq!(mask_token("short"), "…");
        let masked = mask_token("abcdefghijklmnop");
        assert!(masked.starts_with("abcdef"), "{masked}");
        assert!(masked.ends_with("mnop"), "{masked}");
    }


    /// End-to-end probe against the account this machine is signed into.
    ///
    /// `cargo test --lib cloud189_live -- --ignored --nocapture`
    ///
    /// Every step runs inside a throwaway folder, and the folder is deleted at
    /// the end, so a run leaves the drive exactly as it found it. It guards the
    /// four bugs found during the first live pass: missing `returnType=JSON`
    /// (listings came back as XML and every field read as 0), a folder detected
    /// through `getFolderInfo` (which answers for files too), a parked
    /// `taskStatus: 2` conflict task, and a `taskStatus: 4` task that still has
    /// `failedCount > 0`.
    #[tokio::test]
    #[ignore = "hits the live 189 account"]
    async fn cloud189_live_walk() {
        use crate::providers::{CounterSink, UploadSink};
        use futures_util::StreamExt;
        use std::sync::atomic::AtomicU64;

        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Cloud189Provider::load(&dir).expect("cloud189.json");

        let root = drive.list("0").await.expect("root listing");
        println!("root: {} entries, total {}", root.entries.len(), root.total);
        assert!(!root.entries.is_empty(), "root listing came back empty");

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let folder_name = format!("yunyu-cloud189-{stamp}");
        let folder = drive.make_dir("0", &folder_name).await.expect("make_dir");
        println!("made folder {folder_name} -> {folder}");

        let sink: Arc<dyn UploadSink> = Arc::new(CounterSink::new(Arc::new(AtomicU64::new(0))));
        let name = format!("probe-{stamp}.bin");
        let local = std::env::temp_dir().join(&name);
        let payload: Vec<u8> = (0..1_200_000u32)
            .map(|i| (i.wrapping_mul(31).wrapping_add(stamp as u32) % 251) as u8)
            .collect();
        std::fs::write(&local, &payload).unwrap();

        let created = drive
            .upload(&local, &folder, &name, Arc::clone(&sink), CancellationToken::new())
            .await
            .expect("upload");
        let created = created.expect("upload should report an id");
        println!("uploaded -> {created}");

        let listing = drive.list(&folder).await.expect("folder listing");
        let hit = listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .expect("uploaded file should be listed");
        assert_eq!(hit.size, payload.len() as u64);

        let mut handle = drive.open_download(&hit.fid, 0).await.expect("download");
        let first = handle.stream.next().await.expect("first chunk").expect("bytes");
        assert_eq!(&first[..8], &payload[..8]);
        drop(handle);
        let mut resumed = drive.open_download(&hit.fid, 1000).await.expect("resume");
        let chunk = resumed.stream.next().await.expect("chunk").expect("bytes");
        assert_eq!(&chunk[..8], &payload[1000..1008]);
        drop(resumed);

        let renamed = format!("{name}.renamed");
        drive.rename(&hit.fid, &renamed).await.expect("rename");
        let listing = drive.list(&folder).await.expect("listing");
        assert!(listing.entries.iter().any(|e| e.name == renamed));

        // Copy into the same folder: the target already holds the name, so this
        // is the parked-conflict path.
        let copied = drive
            .transfer_entries(std::slice::from_ref(&hit.fid), &folder, true)
            .await;
        println!("copy into the same folder -> {copied:?}");

        // Move it out to the root, then delete both copies.
        drive
            .transfer_entries(std::slice::from_ref(&hit.fid), "0", false)
            .await
            .expect("move");
        let root_now = drive.list("0").await.expect("root listing");
        assert!(root_now.entries.iter().any(|e| e.name == renamed));
        drive.delete(std::slice::from_ref(&hit.fid)).await.expect("delete file");
        drive.delete(std::slice::from_ref(&folder)).await.expect("delete folder");
        let _ = std::fs::remove_file(&local);

        let quota = drive.quota().await.expect("quota");
        println!("quota: {quota:?}");
        assert!(quota.is_some(), "capacity should be reported");
        println!("cleaned up {renamed} and {folder_name}");
    }

    /// Exercises the `open/share` family against the live account: open the
    /// sample link, page through it, mint a download link, then create and
    /// revoke a share of a scratch file.
    #[tokio::test]
    #[ignore = "hits the live 189 account"]
    async fn cloud189_live_share() {
        use crate::providers::{CounterSink, UploadSink};
        use crate::transfer::ShareRef;
        use std::sync::atomic::AtomicU64;

        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Cloud189Provider::load(&dir).expect("cloud189.json");

        // The link the user handed over: one pdf, access code 2sc1.
        let session = drive
            .share_open("YRry2aRzmUNr", "2sc1")
            .await
            .expect("share_open");
        println!(
            "share: title={} owner={} stoken={} size={}",
            session.title, session.owner, session.stoken, session.total_size
        );
        assert!(!session.stoken.is_empty(), "share id should be resolved");

        let share = ShareRef {
            provider: ProviderId::Cloud189.as_str().to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: session.stoken.clone(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };
        let listing = drive.share_list(&share, "0").await.expect("share_list");
        println!("share root: {} entries", listing.entries.len());
        assert!(!listing.entries.is_empty(), "share root should list a file");

        let files = drive.share_walk(&share, "0").await.expect("share_walk");
        println!("walked {} file(s)", files.len());
        let first = files.first().expect("at least one file");
        let link = drive
            .share_link(&share, &first.fid)
            .await
            .expect("share_link");
        println!("share link host: {}", link.url.split('/').nth(2).unwrap_or("?"));

        // A wrong code must be reported, not silently accepted.
        let wrong = drive.share_open("YRry2aRzmUNr", "0000").await;
        println!("wrong code -> {:?}", wrong.as_ref().err().map(|e| e.0.clone()));
        assert!(wrong.is_err(), "a wrong access code should fail");

        // Create + revoke a share of a scratch file we own.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("yunyu-share-{stamp}.txt");
        let local = std::env::temp_dir().join(&name);
        std::fs::write(&local, b"yunyu drive share probe\n").unwrap();
        let sink: Arc<dyn UploadSink> = Arc::new(CounterSink::new(Arc::new(AtomicU64::new(0))));
        let created = drive
            .upload(&local, "0", &name, Arc::clone(&sink), CancellationToken::new())
            .await
            .expect("upload")
            .expect("id");
        let made = drive
            .create_share(std::slice::from_ref(&created), &name, 7, "")
            .await
            .expect("create_share");
        println!("created share {} code {}", made.url, made.passcode);
        assert!(!made.url.is_empty());
        assert!(!made.share_id.is_empty());

        let records = drive.share_records().await.expect("share_records");
        println!("my shares: {}", records.len());
        assert!(records.iter().any(|r| r.share_id == made.share_id));

        // Open our own share through the visitor path, then save it back into a
        // scratch folder — that exercises `share_save` end to end.
        let code = made.url.rsplit('/').next().unwrap_or_default().to_string();
        let mine = drive
            .share_open(&code, &made.passcode)
            .await
            .expect("open own share");
        let mine_ref = ShareRef {
            provider: ProviderId::Cloud189.as_str().to_string(),
            pwd_id: mine.pwd_id.clone(),
            stoken: mine.stoken.clone(),
            fid_token: String::new(),
            passcode: mine.passcode.clone(),
        };
        let folder = drive
            .make_dir("0", &format!("yunyu-save-{stamp}"))
            .await
            .expect("make_dir");
        let saved = drive
            .share_save(
                &mine_ref,
                std::slice::from_ref(&session_entry(&mine, &name)),
                Some(&folder),
            )
            .await;
        match saved {
            Ok(count) => {
                println!("saved {count} entr(ies) into the scratch folder");
                let landed = drive.list(&folder).await.expect("folder listing");
                assert!(
                    landed.entries.iter().any(|e| e.name == name),
                    "the saved copy should show up in the target folder"
                );
            }
            // The account already saved this share once (earlier runs), and the
            // server answers with a duplicate-task code instead of saving again.
            Err(err) => {
                println!("share_save -> {}", err.0);
                assert!(err.0.contains("转存过"), "{}", err.0);
            }
        }
        drive.delete(&[folder]).await.expect("delete scratch folder");

        drive
            .revoke_share(&made.share_id, std::slice::from_ref(&created))
            .await
            .expect("revoke_share");
        let after = drive.share_records().await.expect("share_records");
        assert!(
            !after.iter().any(|r| r.share_id == made.share_id),
            "revoked share should be gone"
        );

        drive.delete(&[created]).await.expect("delete scratch file");
        let _ = std::fs::remove_file(&local);
        println!("share round trip ok");
    }

    /// A one-entry listing for the freshly made share.
    fn session_entry(
        session: &crate::share::ShareSession,
        name: &str,
    ) -> crate::share::ShareEntry {
        crate::share::ShareEntry {
            fid: session.first_fid.clone().unwrap_or_default(),
            token: String::new(),
            name: name.to_string(),
            size: 0,
            is_dir: false,
            updated_at: 0,
        }
    }

    /// The sidebar bubble needs a nickname *and* an inlined avatar; both come
    /// from `open/user/getUserInfoExt.action`, not from the session response.
    #[tokio::test]
    #[ignore = "hits the live 189 account"]
    async fn cloud189_live_profile() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Cloud189Provider::load(&dir).expect("cloud189.json");
        let info = drive.profile().await.expect("profile").expect("profile info");
        println!("profile: nickname={} avatar_len={}", info.nickname, info.avatar.as_ref().map(String::len).unwrap_or(0));
        assert!(!info.nickname.is_empty(), "nickname should be resolved");
        let avatar = info.avatar.expect("avatar should be inlined");
        assert!(avatar.starts_with("data:image/"), "avatar must be a data: URL, got {avatar:.40}");
    }

    #[test]
    fn zone_detection() {
        assert!(has_zone("2024-05-06 07:08:09 +08"));
        assert!(has_zone("2024-05-06 07:08:09 -07"));
        assert!(!has_zone("2024-05-06 07:08:09"));
    }
}

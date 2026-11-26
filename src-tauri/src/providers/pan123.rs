//! 123 云盘.
//!
//! Ported from the Go `123pan-cli` (GPL-3.0), which speaks the **Android**
//! client protocol — that is deliberate: the web protocol caps download traffic,
//! the Android one does not. Everything here is a straight reimplementation of
//! the requests documented in that repository; no Go code is linked in.
//!
//! Shape of the API:
//! * Login is account + password, and the response is a bearer token.
//! * File ids are plain int64, so they need no encoding (unlike 139's paths).
//! * Downloads: `/a/api/file/download_info` returns either a direct CDN url or a
//!   `web-pro2` proxy url whose `params` is base64 of the real url; the proxy
//!   needs `auto_redirect=0` and one manual redirect hop.
//! * Uploads are S3 multipart: `upload_request` → presigned part urls → PUT each
//!   part → `s3_complete_multipart_upload` → `upload_complete`.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    native_of, qualify, AuthState, Capabilities, DirListing, DownloadHandle, DriveProvider, EntryDto,
    PathNode, ProviderId, RecycleEntry, SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};
use crate::error::{AppError, AppResult};
use crate::share::{AccountInfo, ShareEntry, ShareListing, ShareSession};

const BASE: &str = "https://www.123pan.cn";
/// Share endpoints live on the web host, not the Android one.
const SHARE_BASE: &str = "https://yun.123pan.com";
/// 5 MiB is the smallest part S3 accepts for multipart uploads.
const PART_SIZE: usize = 5 * 1024 * 1024;
const ROOT: &str = "0";
/// Page size the Android protocol accepts for `/api/file/list/new`.
const PAGE_LIMIT: u32 = 100;

/// Persisted login state.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct Pan123Credentials {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub username: String,
    /// Stable per-install device fingerprint the API expects.
    #[serde(default)]
    pub loginuuid: String,
    #[serde(default)]
    pub os_version: String,
    #[serde(default)]
    pub device_type: String,
}

pub struct Pan123Provider {
    creds: RwLock<Pan123Credentials>,
    credential_path: String,
    /// Nickname + avatar from `/b/api/user/info`; `None` until fetched.
    profile: RwLock<Option<AccountInfo>>,
    /// Set once a fetch ran so a failure is not retried on every render.
    profile_tried: std::sync::atomic::AtomicBool,
}

impl Pan123Provider {
    pub fn new(creds: Pan123Credentials, credential_path: String) -> Self {
        Self {
            creds: RwLock::new(creds),
            credential_path,
            profile: RwLock::new(None),
            profile_tried: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Loads `<config>/pan123.json`, or reports "not logged in".
    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let path = own_dir.join("pan123.json");
        if !path.exists() {
            return Err(AppError::new("尚未登录 123 云盘"));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AppError::new(format!("读取 123 云盘凭据失败：{e}")))?;
        let creds: Pan123Credentials = serde_json::from_str(&text)
            .map_err(|e| AppError::new(format!("123 云盘凭据格式错误：{e}")))?;
        if creds.token.trim().is_empty() {
            return Err(AppError::new("尚未登录 123 云盘"));
        }
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    /// Account + password login. Returns the credentials that were saved.
    pub async fn login(own_dir: &Path, username: &str, password: &str) -> AppResult<Self> {
        let username = username.trim();
        if username.is_empty() || password.is_empty() {
            return Err(AppError::new("请填写 123 云盘的账号和密码"));
        }
        let creds = Pan123Credentials {
            token: String::new(),
            username: username.to_string(),
            loginuuid: random_hex(16),
            os_version: "Android_13".to_string(),
            device_type: "M2102K1C".to_string(),
        };
        let provider = Self::new(creds, String::new());
        let token = provider.sign_in(username, password).await?;
        {
            let mut guard = provider.creds.write().await;
            guard.token = token;
        }
        provider.persist(own_dir).await?;
        Ok(provider)
    }

    async fn persist(&self, own_dir: &Path) -> AppResult<()> {
        let creds = self.creds.read().await.clone();
        std::fs::create_dir_all(own_dir)
            .map_err(|e| AppError::new(format!("创建配置目录失败：{e}")))?;
        let path = own_dir.join("pan123.json");
        let text = serde_json::to_string_pretty(&creds)
            .map_err(|e| AppError::new(format!("序列化凭据失败：{e}")))?;
        std::fs::write(&path, text)
            .map_err(|e| AppError::new(format!("写入凭据失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        Ok(())
    }

    async fn sign_in(&self, username: &str, password: &str) -> AppResult<String> {
        let client = self.http()?;
        let creds = self.creds.read().await.clone();
        let body = json!({ "type": 1, "passport": username, "password": password });
        let res = client
            .post(format!("{BASE}/b/api/user/sign_in"))
            .headers(self.headers(&creds, false))
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::new(format!("登录请求失败：{e}")))?;
        let value: Value = res
            .json()
            .await
            .map_err(|e| AppError::new(format!("登录响应解析失败：{e}")))?;
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 200 {
            let msg = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("账号或密码错误");
            return Err(AppError::new(format!("登录失败：{msg}")));
        }
        let token = value
            .pointer("/data/token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::new("登录响应缺少 token"))?;
        Ok(format!("Bearer {token}"))
    }

    fn http(&self) -> AppResult<crate::net::Http> {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            // No total timeout: this client also streams downloads.
            .read_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))
            .map(|client| crate::net::Http::new(client, "123pan"))
    }

    /// Headers the Android client sends; the API rejects requests without them.
    fn headers(&self, creds: &Pan123Credentials, json_body: bool) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE, USER_AGENT};
        let mut map = HeaderMap::new();
        let put = |map: &mut HeaderMap, key: &'static str, value: &str| {
            if let Ok(v) = HeaderValue::from_str(value) {
                map.insert(key, v);
            }
        };
        put(&mut map, "platform", "android");
        put(&mut map, "devicename", "Xiaomi");
        put(&mut map, "app-version", "61");
        put(&mut map, "x-app-version", "2.4.0");
        put(
            &mut map,
            USER_AGENT.as_str(),
            &format!("123pan/v2.4.0({};Xiaomi)", creds.os_version),
        );
        put(&mut map, "osversion", &creds.os_version);
        put(&mut map, "devicetype", &creds.device_type);
        if !creds.loginuuid.is_empty() {
            put(&mut map, "loginuuid", &creds.loginuuid);
        }
        if !creds.token.is_empty() {
            put(&mut map, "authorization", &creds.token);
        }
        if json_body {
            put(&mut map, CONTENT_TYPE.as_str(), "application/json");
        }
        map
    }

    /// GET/POST helper that unwraps the `{code, message, data}` envelope.
    async fn call(&self, method: &str, url: &str, body: Option<Value>) -> AppResult<Value> {
        self.call_accepting(method, url, body, &[]).await
    }

    /// `call`, but the listed non-zero codes count as success.
    ///
    /// The permanent-delete endpoint answers `7301` ("已删除，系统释放空间需要
    /// 一段时间") on a request that *did* work, so it cannot go through the
    /// generic "any non-zero code is an error" path.
    async fn call_accepting(
        &self,
        method: &str,
        url: &str,
        body: Option<Value>,
        extra_ok: &[i64],
    ) -> AppResult<Value> {
        let client = self.http()?;
        let creds = self.creds.read().await.clone();
        let mut req = match method {
            "GET" => client.get(url),
            _ => client.post(url),
        };
        req = req.headers(self.headers(&creds, body.is_some()));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let res = req
            .send()
            .await
            .map_err(|e| AppError::new(format!("请求失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取响应失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let snippet: String = text.chars().take(160).collect();
            json!({ "code": -1, "message": format!("响应解析失败（HTTP {}）：{}", status.as_u16(), snippet) })
        });
        let code = value.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        // 5113/5114 mean the account has run out of download traffic. They used
        // to be waved through here, and since no caller handled them the empty
        // `data` then looked like an empty folder; report them like any error.
        if code == 5113 || code == 5114 {
            return Err(AppError::new(
                "123 云盘：本月下载流量已用完，请下个月再试（code=5113/5114）",
            ));
        }
        if code != 0 && code != 200 && !extra_ok.contains(&code) {
            let msg = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("接口返回异常");
            return Err(AppError::new(format!("123 云盘：{msg}（code={code}）")));
        }
        Ok(value)
    }

    /// One page of raw listing rows plus the server's total count.
    async fn list_raw(&self, parent: &str, page: u32) -> AppResult<(Vec<Value>, u32)> {
        let url = format!(
            "{BASE}/api/file/list/new?driveId=0&limit={PAGE_LIMIT}&next=0&orderBy=file_id&orderDirection=desc&parentFileId={parent}&trashed=false&SearchData=&Page={page}&OnlyLookAbnormalFile=0"
        );
        let value = self.call("GET", &url, None).await?;
        let total = value
            .pointer("/data/Total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let items = value
            .pointer("/data/InfoList")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok((items, total))
    }

    /// Every row of a folder, following pages until the total is reached.
    ///
    /// Lookups used to read page 1 only, so a file that sorted onto a later page
    /// was reported as missing (and, for renames, resolved to the wrong id).
    async fn all_rows(&self, parent: &str) -> AppResult<Vec<Value>> {
        let mut rows = Vec::new();
        let mut page = 1u32;
        loop {
            let (items, total) = self.list_raw(parent, page).await?;
            let got = items.len() as u32;
            rows.extend(items);
            let done = got == 0
                || (total > 0 && rows.len() as u32 >= total)
                || page >= 200
                || got < PAGE_LIMIT;
            if done {
                return Ok(rows);
            }
            page += 1;
        }
    }

    async fn list_page(&self, parent: &str, page: u32) -> AppResult<(Vec<EntryDto>, u32)> {
        let (items, total) = self.list_raw(parent, page).await?;
        let mut entries = Vec::with_capacity(items.len());
        for item in &items {
            // A row without an id used to become file 0, i.e. the root: deleting
            // or renaming it then hit the wrong target. Refuse instead.
            let entry = self.row_entry(item).ok_or_else(|| {
                AppError::new("123 云盘返回了没有 FileId 的文件条目")
            })?;
            entries.push(EntryDto {
                pdir_fid: qualify(ProviderId::Pan123, parent),
                ..entry
            });
        }
        Ok((entries, total))
    }

    /// One `/api/file/list/new` row as a UI entry, parent left unset.
    ///
    /// Search rows come from a tree-wide query, so the parent is filled in by
    /// the caller; the listing fills it from the folder it asked for.
    fn row_entry(&self, item: &Value) -> Option<EntryDto> {
        let id = item.get("FileId").and_then(|v| v.as_i64())?;
        let name = item
            .get("FileName")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let is_dir = item.get("Type").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
        let size = item.get("Size").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u64;
        // `UpdateAt`/`CreateAt` are ISO strings ("2026-11-09T15:12:31+08:00"),
        // not epoch seconds: reading them as integers rendered 1970.
        let updated = super::parse_time(item.get("UpdateAt").unwrap_or(&Value::Null));
        let created = super::parse_time(item.get("CreateAt").unwrap_or(&Value::Null));
        let parent = item.get("ParentFileId").and_then(|v| v.as_i64()).unwrap_or(0);
        Some(EntryDto {
            fid: qualify(ProviderId::Pan123, &id.to_string()),
            pdir_fid: qualify(ProviderId::Pan123, &parent.to_string()),
            ext: EntryDto::ext_of(&name),
            name,
            size,
            is_dir,
            format_type: if is_dir { "folder".into() } else { "file".into() },
            updated_at: updated,
            created_at: created.max(updated),
            provider: ProviderId::Pan123.as_str().to_string(),
            // `/b/api/file/list/new` carries a `Thumbnail` field, but the
            // Android protocol leaves it empty for every row we have seen.
            thumb: item
                .get("Thumbnail")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(|v| v.to_string()),
        })
    }

    /// Breadcrumb of a folder id, via `/b/api/file/get_path`.
    ///
    /// The endpoint answers the *ancestors* of a file (root first) and is a
    /// POST with a JSON body — the GET variant is not routed.
    async fn path_of_id(&self, id: i64) -> Vec<PathNode> {
        if id == 0 {
            return Vec::new();
        }
        let Ok(value) = self
            .call(
                "POST",
                &format!("{BASE}/b/api/file/get_path"),
                Some(json!({ "fileId": id, "driveId": 0 })),
            )
            .await
        else {
            return Vec::new();
        };
        value
            .get("data")
            .and_then(|v| v.as_array())
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        let fid = row.get("FileId").and_then(|v| v.as_i64())?;
                        Some(PathNode {
                            fid: qualify(ProviderId::Pan123, &fid.to_string()),
                            name: row
                                .get("FileName")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolves the real byte url for one file.
    ///
    /// The API answers with either a direct CDN url or a `web-pro2` proxy whose
    /// `params` is base64 of the real url; the proxy needs `auto_redirect=0` and
    /// exactly one manual redirect hop.
    async fn resolve_download_url(&self, fid: &str) -> AppResult<String> {
        // `download_info` needs the etag, the s3 key flag, the real name and the
        // size — all of which only the listing carries, so look the entry up
        // rather than making the caller thread them through.
        let entry = self.find_entry(fid).await?;
        let body = json!({
            "driveId": 0,
            "etag": entry.etag,
            "fileId": entry.id,
            "s3keyFlag": entry.s3_key_flag,
            "type": 0,
            "fileName": entry.name,
            "size": entry.size,
        });
        let value = self
            .call("POST", &format!("{BASE}/a/api/file/download_info"), Some(body))
            .await?;
        // `RedirectUrl` / `redirect_url` / `DownloadUrl`, in the order the Go
        // client prefers.
        let direct = value
            .pointer("/data/RedirectUrl")
            .and_then(|v| v.as_str())
            .or_else(|| value.pointer("/data/redirect_url").and_then(|v| v.as_str()))
            .filter(|u| !u.is_empty());
        if let Some(url) = direct {
            return Ok(url.to_string());
        }
        let download = value
            .pointer("/data/DownloadUrl")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| AppError::new("123 云盘没有返回下载地址"))?;
        // Both the plain CDN link and the (now dead) proxy form end up here:
        // `resolve_cdn_url` unwraps the proxy and follows the JSON hop.
        resolve_cdn_url(download).await
    }

    /// Finds `fid` and the folder it lives in, in one walk.
    ///
    /// The Android protocol has no "get one entry" call, so this breadth-first
    /// scan is the only way to learn a file's etag and parent. Copying needs
    /// both, and two separate walks doubled the round trips for nothing.
    async fn locate(&self, fid: &str) -> AppResult<(i64, Pan123Entry)> {
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(ROOT.to_string());
        let mut visited = 0;
        while let Some(parent) = queue.pop_front() {
            visited += 1;
            if visited > 200 {
                break;
            }
            // `?` on purpose: a transient failure used to be skipped with
            // `continue`, which surfaced as "找不到该文件" for a file that exists.
            let items = self.all_rows(&parent).await?;
            for item in &items {
                let Some(id) = item.get("FileId").and_then(|v| v.as_i64()) else {
                    continue;
                };
                let is_dir = item.get("Type").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
                if id.to_string() == fid {
                    let text = |key: &str| {
                        item.get(key)
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    return Ok((
                        parent.parse().unwrap_or(0),
                        Pan123Entry {
                            id,
                            etag: text("Etag"),
                            s3_key_flag: text("S3KeyFlag"),
                            name: text("FileName"),
                            size: item.get("Size").and_then(|v| v.as_i64()).unwrap_or(0).max(0)
                                as u64,
                        },
                    ));
                }
                if is_dir {
                    queue.push_back(id.to_string());
                }
            }
        }
        Err(AppError::new("在 123 云盘中找不到该文件"))
    }

    /// Fetches the listing entry for `fid` (needed for etag + s3key flag).
    async fn find_entry(&self, fid: &str) -> AppResult<Pan123Entry> {
        Ok(self.locate(fid).await?.1)
    }
}

/// The fields a download or a copy needs from one listing row.
struct Pan123Entry {
    id: i64,
    etag: String,
    s3_key_flag: String,
    name: String,
    size: u64,
}

/// Turns whatever `download_info` handed back into a url that serves bytes.
///
/// The `web-pro2.123952.com` proxy is gone — the host has no DNS record at all,
/// so asking it produced `error sending request for url (https://web-pro2…)`.
/// What the drive actually returns now is a CDN link that answers `HTTP 210`
/// with a tiny JSON body holding the real edge url:
///
/// ```json
/// {"data":{"redirect_url":"https://42-4-59-152-v3.pd1.cjjd19.com/…"},"code":0}
/// ```
///
/// So: unwrap a proxy url into its base64 `params` (that *is* the CDN url),
/// then follow the one hop, accepting a `Location` header, a JSON
/// `redirect_url`, or "this url already serves the file".
async fn resolve_cdn_url(raw: &str) -> AppResult<String> {
    let mut url = unproxy(raw);
    let client = crate::net::Http::new(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?,
        "123pan",
    );
    for _ in 0..3 {
        let res = client
            .get(&url)
            .streaming()
            // The edge host rejects requests without a referer.
            .header(reqwest::header::REFERER, "https://yun.123pan.com/")
            .header(reqwest::header::USER_AGENT, "Mozilla/5.0")
            // A range keeps this hop cheap; the real download re-requests anyway.
            .header(reqwest::header::RANGE, "bytes=0-1")
            .send()
            .await
            .map_err(|e| AppError::new(format!("解析下载地址失败：{e}")))?;
        let status = res.status();
        if let Some(loc) = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.starts_with("http"))
        {
            url = loc.to_string();
            continue;
        }
        // `210` is the drive's "here is the real link" status. `206`/`200` means
        // this url already serves the file, so stop.
        if status == reqwest::StatusCode::PARTIAL_CONTENT || status.is_success() {
            let content_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            if !content_type.contains("json") {
                return Ok(url);
            }
        }
        let body = res.text().await.unwrap_or_default();
        if let Some(next) = json_redirect(&body) {
            url = next;
            continue;
        }
        if let Some(next) = html_redirect(&body) {
            url = next;
            continue;
        }
        return Err(AppError::new(format!(
            "123 云盘没有返回可用的下载地址（HTTP {}）：{}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        )));
    }
    Ok(url)
}

/// Pulls `data.redirect_url` out of the CDN's JSON answer.
fn json_redirect(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let url = value
        .pointer("/data/redirect_url")
        .or_else(|| value.pointer("/data/RedirectUrl"))
        .or_else(|| value.pointer("/redirect_url"))
        .and_then(|v| v.as_str())?
        .to_string();
    url.starts_with("http").then_some(url)
}

/// Some answers embed the link in a tiny HTML page instead.
fn html_redirect(body: &str) -> Option<String> {
    let at = body.find("href=")? + "href=".len();
    let mut rest = body[at..].trim_start();
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    rest = &rest[quote.len_utf8()..];
    let end = rest.find(quote)?;
    let url = &rest[..end];
    url.starts_with("http").then(|| url.to_string())
}

/// Replaces a `web-pro*` proxy url with the CDN url hidden in its `params`.
///
/// The proxy form carries the real target as base64, so no network call is
/// needed — and the proxy host itself no longer resolves.
fn unproxy(raw: &str) -> String {
    if !raw.contains("web-pro") {
        return raw.to_string();
    }
    let Some(params) = query_param(raw, "params") else {
        return raw.to_string();
    };
    let Some(inner) = b64_decode(&params) else {
        return raw.to_string();
    };
    if !inner.starts_with("http") {
        return raw.to_string();
    }
    // `auto_redirect=0` is what makes the CDN answer JSON instead of a 302 to a
    // URL that needs a second round trip; keep it.
    inner
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix(&format!("{key}=")) {
            return Some(value.to_string());
        }
    }
    None
}




fn b64_decode(input: &str) -> Option<String> {
    for engine in [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ] as [&base64::engine::GeneralPurpose; 4]
    {
        if let Ok(bytes) = engine.decode(input) {
            if let Ok(text) = String::from_utf8(bytes) {
                return Some(text);
            }
        }
    }
    None
}

fn random_hex(bytes: usize) -> String {
    // No `rand` dependency: hash the clock plus a counter into a hex string.
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut out = String::with_capacity(bytes * 2);
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15);
    for _ in 0..bytes {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:02x}", (seed & 0xff) as u8));
    }
    out
}

#[async_trait]
impl DriveProvider for Pan123Provider {
    fn id(&self) -> ProviderId {
        ProviderId::Pan123
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            share_open: true,
            share_create: true,
            // Moving uses `mod_pid`; copying uses `copy/async`, which always
            // lands the copy next to the original, so a copy to another folder
            // is "copy in place, then move the new file".
            move_files: true,
            copy: true,
            search: true,
            rename: true,
            folders: true,
            recycle: true,
        }
    }

    async fn auth_state(&self) -> AuthState {
        let creds = self.creds.read().await;
        let profile = self.profile.read().await.clone();
        AuthState {
            provider: ProviderId::Pan123,
            authenticated: !creds.token.trim().is_empty(),
            source: "file".to_string(),
            masked: Some(creds.username.clone()),
            credential_path: self.credential_path.clone(),
            nickname: profile
                .as_ref()
                .map(|p| p.nickname.clone())
                .filter(|n| !n.is_empty())
                .or_else(|| Some(creds.username.clone()).filter(|u| !u.is_empty())),
            avatar: profile.and_then(|p| p.avatar),
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        {
            let mut creds = self.creds.write().await;
            creds.token.clear();
        }
        let path = Path::new(&self.credential_path);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let creds = self.creds.read().await;
        if creds.username.is_empty() {
            return Ok(None);
        }
        if let Some(cached) = self.profile.read().await.clone() {
            return Ok(Some(cached));
        }
        Ok(Some(AccountInfo {
            nickname: creds.username.clone(),
            avatar: None,
        }))
    }

    /// `/b/api/user/info` is the only place 123 reports the display name and
    /// avatar; the login token itself carries no profile.
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
                crate::ylog!("123 profile fetch failed: {err}");
                self.account().await
            }
        }
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        // `limit` is capped at 100 by the API, and a folder can hold more, so
        // page until the reported total is reached. Stopping at page 1 is how a
        // folder silently loses its tail.
        const PAGE_SIZE: u32 = 100;
        let mut entries: Vec<EntryDto> = Vec::new();
        let mut total = 0u32;
        let mut page = 1u32;
        loop {
            let (chunk, reported) = self.list_page(dir, page).await?;
            total = reported.max(total);
            let got = chunk.len() as u32;
            entries.extend(chunk);
            let done = got == 0
                || (total > 0 && entries.len() as u32 >= total)
                || page >= 200
                || got < PAGE_SIZE;
            if done {
                break;
            }
            page += 1;
        }
        let count = entries.len() as u32;
        super::sort_entries(&mut entries);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Pan123, dir),
            entries,
            total: total.max(count),
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        if dir == ROOT {
            return Ok(Vec::new());
        }
        // Breadth-first from the root, recording the chain that reaches `dir`.
        let mut queue: std::collections::VecDeque<(String, Vec<(String, String)>)> =
            std::collections::VecDeque::new();
        queue.push_back((ROOT.to_string(), Vec::new()));
        let mut visited = 0;
        while let Some((parent, path)) = queue.pop_front() {
            visited += 1;
            if visited > 200 {
                break;
            }
            let Ok(listing) = self.list(&parent).await else {
                continue;
            };
            for entry in listing.entries {
                let native = super::native_of(&entry.fid);
                if native == dir {
                    let mut chain: Vec<PathNode> = path
                        .iter()
                        .map(|(fid, name)| PathNode {
                            fid: qualify(ProviderId::Pan123, fid),
                            name: name.clone(),
                        })
                        .collect();
                    chain.push(PathNode {
                        fid: qualify(ProviderId::Pan123, &native),
                        name: entry.name,
                    });
                    return Ok(chain);
                }
                if entry.is_dir && path.len() < 10 {
                    let mut next = path.clone();
                    next.push((native.clone(), entry.name.clone()));
                    queue.push_back((native, next));
                }
            }
        }
        Ok(Vec::new())
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = super::check_name(name)?;
        let body = json!({
            "driveId": 0,
            "etag": "",
            "fileName": name,
            "parentFileId": numeric_id(parent)?,
            "size": 0,
            "type": 1,
            "duplicate": 1,
            "NotReuse": true,
            "event": "newCreateFolder",
            "operateType": 1,
        });
        let value = self
            .call("POST", &format!("{BASE}/a/api/file/upload_request"), Some(body))
            .await?;
        let id = value
            .pointer("/data/Info/FileId")
            .or_else(|| value.pointer("/data/FileId"))
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AppError::new("123 云盘没有返回新建文件夹的 ID"))?;
        Ok(qualify(ProviderId::Pan123, &id.to_string()))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = super::check_name(name)?;
        let body = json!({
            "driveId": 0,
            "fileId": numeric_id(fid)?,
            "fileName": name,
        });
        self.call("POST", &format!("{BASE}/a/api/file/rename"), Some(body))
            .await?;
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let mut list: Vec<Value> = Vec::with_capacity(fids.len());
        for fid in fids {
            // A bad id used to become 0 (the root), i.e. the wrong delete target.
            // `native_of` makes this tolerant of a qualified id as well: the
            // commands strip the prefix, but a probe (or a future caller) may
            // hand over `pan123:123` straight from a listing.
            list.push(json!({ "FileId": numeric_id(&super::native_of(fid))? }));
        }
        let body = json!({
            "driveId": 0,
            "fileTrashInfoList": list,
            "operation": true,
        });
        self.call("POST", &format!("{BASE}/a/api/file/trash"), Some(body))
            .await?;
        Ok(())
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        if copy {
            return self.copy_entries(fids, to).await;
        }
        let mut list: Vec<Value> = Vec::with_capacity(fids.len());
        for fid in fids {
            list.push(json!({ "FileId": numeric_id(fid)? }));
        }
        let body = json!({
            "fileIdList": list,
            "parentFileId": numeric_id(to)?,
        });
        self.call("POST", &format!("{BASE}/b/api/file/mod_pid"), Some(body))
            .await?;
        Ok(fids.len())
    }

    /// Server-side search through `/api/file/list/new` with `SearchData`.
    ///
    /// 123 has no dedicated search path (every `/search` variant is 404), but
    /// the listing endpoint accepts a keyword and returns the whole tree, so
    /// one request replaces the 80-folder BFS. Rows carry `ParentFileId`,
    /// which `/b/api/file/get_path` turns into a breadcrumb.
    async fn search(&self, _dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        let mut page = 1u32;
        while hits.len() < limit as usize && page <= 20 {
            let url = format!(
                "{BASE}/api/file/list/new?driveId=0&limit={PAGE_LIMIT}&next=0&orderBy=file_id&orderDirection=desc&parentFileId=0&trashed=false&SearchData={}&Page={page}&OnlyLookAbnormalFile=0",
                enc(needle)
            );
            let value = self.call("GET", &url, None).await?;
            let rows = value
                .pointer("/data/InfoList")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let count = rows.len();
            let total = value
                .pointer("/data/Total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            for row in rows {
                let Some(entry) = self.row_entry(&row) else {
                    continue;
                };
                let parent = row
                    .get("ParentFileId")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let mut path = self.path_of_id(parent).await;
                path.push(PathNode {
                    fid: entry.fid.clone(),
                    name: entry.name.clone(),
                });
                hits.push(SearchHit { entry, path });
                if hits.len() >= limit as usize {
                    break;
                }
            }
            if count < PAGE_LIMIT as usize
                || (total > 0 && u64::from(page) * u64::from(PAGE_LIMIT) >= total)
            {
                break;
            }
            page += 1;
        }
        Ok(hits)
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let url = self.resolve_download_url(fid).await?;
        let client = self.http()?;
        let mut request = client
            .get(&url)
            // The edge host rejects requests without a referer.
            .header(reqwest::header::REFERER, "https://yun.123pan.com/");
        if start > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let res = request
            .send()
            .await
            .map_err(|e| AppError::new(format!("下载失败：{e}")))?;
        if !res.status().is_success() && res.status() != reqwest::StatusCode::PARTIAL_CONTENT {
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

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        use tokio::io::AsyncReadExt;

        sink.phase("hashing");
        let (md5, size) = md5_file(local, sink.clone(), token.clone()).await?;
        sink.phase("uploading");

        // 1. Ask for an upload slot (rapid-upload short-circuits here).
        let body = json!({
            "driveId": 0,
            "etag": md5,
            "fileName": name,
            "parentFileId": numeric_id(parent)?,
            "size": size,
            "type": 0,
            "duplicate": 1,
        });
        let value = self
            .call("POST", &format!("{BASE}/b/api/file/upload_request"), Some(body))
            .await?;
        // The field is `FileId` (lower-case d); the older `FileID` spelling is
        // kept as a fallback because the reference client's Go struct is named
        // `FileID` and it is easy to read that as the wire name.
        // `Info.FileId` is the real file id; the sibling `FileId` is the upload
        // session id (a different number entirely). Rapid uploads only carry the
        // real one inside `Info`, so prefer it and keep the others as fallbacks.
        let new_file_id = value
            .pointer("/data/Info/FileId")
            .and_then(|v| v.as_i64())
            .or_else(|| value.pointer("/data/FileId").and_then(|v| v.as_i64()))
            .or_else(|| value.pointer("/data/FileID").and_then(|v| v.as_i64()));
        if value.pointer("/data/Reuse").and_then(|v| v.as_bool()) == Some(true) {
            sink.progress(size);
            // Without an id this used to return the root, so "open containing
            // folder" after a rapid upload jumped to the drive root.
            let id = new_file_id
                .ok_or_else(|| AppError::new("123 云盘秒传成功但没有返回文件 ID"))?;
            return Ok(Some(qualify(ProviderId::Pan123, &id.to_string())));
        }
        let bucket = text_at(&value, "/data/Bucket");
        let key = text_at(&value, "/data/Key");
        let upload_id = text_at(&value, "/data/UploadId");
        let file_id = match new_file_id {
            Some(id) => id,
            None => {
                // The drive answered `code: 0` without any id. Log the body so
                // the next report names the shape instead of guessing.
                crate::ylog!(
                    "123 upload: upload_request had no FileId for {name}: {}",
                    serde_json::to_string(&value).unwrap_or_default()
                );
                return Err(AppError::new(format!(
                    "123 云盘没有返回上传文件 ID（{}）",
                    text_at(&value, "/message")
                )));
            }
        };
        let storage_node = text_at(&value, "/data/StorageNode");
        if bucket.is_empty() || key.is_empty() || upload_id.is_empty() {
            return Err(AppError::new("123 云盘没有返回上传会话"));
        }

        // 3. Upload each 5 MiB part.
        let mut file = tokio::fs::File::open(local).await?;
        let mut part: u32 = 1;
        let mut sent: u64 = 0;
        loop {
            if token.is_cancelled() {
                return Err(AppError::new("已取消"));
            }
            let mut buf = vec![0u8; PART_SIZE];
            let mut filled = 0usize;
            while filled < PART_SIZE {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let link_body = json!({
                "bucket": bucket,
                "key": key,
                "partNumberEnd": part + 1,
                "partNumberStart": part,
                "uploadId": upload_id,
                "StorageNode": storage_node,
            });
            let link = self
                .call(
                    "POST",
                    &format!("{BASE}/b/api/file/s3_repare_upload_parts_batch"),
                    Some(link_body),
                )
                .await?;
            let part_url = link
                .pointer(&format!("/data/PresignedUrls/{}", part))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    link.pointer("/data/presignedUrls")
                        .and_then(|m| m.get(part.to_string()))
                        .and_then(|v| v.as_str())
                })
                .ok_or_else(|| AppError::new(format!("没有拿到分片 {part} 的上传地址")))?
                .to_string();
            let client = self.http()?;
            let res = client
                .put(&part_url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(buf[..filled].to_vec())
                .send()
                .await
                .map_err(|e| AppError::new(format!("上传分片 {part} 失败：{e}")))?;
            if !res.status().is_success() {
                return Err(AppError::new(format!(
                    "上传分片 {part} 失败（HTTP {}）",
                    res.status().as_u16()
                )));
            }
            sent += filled as u64;
            sink.progress(sent);
            part += 1;
        }
        drop(file);

        // Commit through `upload_complete/v2`. The older three-step dance
        // (`s3_list_upload_parts` → `s3_complete_multipart_upload` →
        // `upload_complete`) answers 200 with an empty body and the file never
        // appears in the listing; v2 takes the session plus the size and hands
        // back the real `file_info`.
        let commit = json!({
            "StorageNode": storage_node,
            "bucket": bucket,
            "fileId": file_id,
            "fileSize": size,
            "isMultipart": part > 1,
            "key": key,
            "uploadId": upload_id,
        });
        let done = self
            .call(
                "POST",
                &format!("{BASE}/b/api/file/upload_complete/v2"),
                Some(commit),
            )
            .await?;
        // `fileId` from `upload_request` is the session id; the file the drive
        // actually lists is the one in `file_info`.
        let final_id = done
            .pointer("/data/file_info/FileId")
            .and_then(|v| v.as_i64())
            .unwrap_or(file_id);
        crate::ylog!("123 upload: committed {name} as {final_id} (session {file_id})");
        Ok(Some(qualify(ProviderId::Pan123, &final_id.to_string())))
    }

    async fn quota(&self) -> AppResult<Option<crate::providers::QuotaInfo>> {
        let url = format!("{BASE}/a/api/user/info");        let Ok(value) = self.call("GET", &url, None).await else {
            return Ok(None);
        };
        let total = value
            .pointer("/data/SpacePermanent")
            .or_else(|| value.pointer("/data/SpaceTemp"))
            .and_then(|v| v.as_u64());
        let used = value.pointer("/data/SpaceUsed").and_then(|v| v.as_u64());
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(crate::providers::QuotaInfo {
                used,
                total,
            })),
            _ => Ok(None),
        }
    }

    /// Monthly download allowance, from the web API.
    ///
    /// `/b/api/user/info` reports `SelfDownloadTraffic` (what a free account
    /// still has this month), while the Android endpoint we use for storage
    /// quota does not carry it at all. The web endpoint wants `App-Version: 3`
    /// and `platform: web`; with the Android version header it answers 8006.
    async fn traffic(&self) -> AppResult<Option<crate::providers::TrafficInfo>> {
        let client = self.http()?;
        let creds = self.creds.read().await.clone();
        if creds.token.is_empty() {
            return Ok(None);
        }
        let res = client
            .get(format!("{BASE}/b/api/user/info"))
            .header("authorization", &creds.token)
            .header("app-version", "3")
            .header("platform", "web")
            .send()
            .await
            .map_err(|e| AppError::new(format!("查询 123 云盘下载流量失败：{e}")))?;
        let value: Value = res
            .json()
            .await
            .map_err(|e| AppError::new(format!("查询 123 云盘下载流量失败：{e}")))?;
        let data = value.pointer("/data").unwrap_or(&Value::Null);
        let read = |key: &str| -> u64 {
            data.get(key)
                .and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
                .unwrap_or(0)
        };
        if data.get("SelfDownloadTraffic").is_none() {
            return Ok(None);
        }
        Ok(Some(crate::providers::TrafficInfo {
            left: read("SelfDownloadTraffic"),
            share: read("ShareTraffic"),
            direct: read("DirectTraffic"),
        }))
    }

    // ----------------------------------------------------------- recycle ---

    async fn recycle_list(&self, offset: u32, limit: u32) -> AppResult<Vec<RecycleEntry>> {
        // The bin is the same listing endpoint with `trashed=true`; there is no
        // separate recycle endpoint and no offset parameter — it pages by
        // `Next` cursor, which we replay to reach `offset`.
        let size = limit.max(1);
        let mut cursor = String::from("0");
        let mut page = 1u32;
        let mut seen = 0u32;
        loop {
            let url = format!(
                "{BASE}/b/api/file/list/new?driveId=0&limit={size}&next={cursor}&orderBy=file_id\
                 &orderDirection=desc&parentFileId=0&trashed=true&Page={page}"
            );
            let value = self.call("GET", &url, None).await?;
            let rows = value
                .pointer("/data/InfoList")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if seen + rows.len() as u32 > offset {
                let skip = (offset - seen) as usize;
                return Ok(rows
                    .iter()
                    .skip(skip)
                    .map(|row| {
                        let native = row
                            .get("FileId")
                            .and_then(|v| v.as_i64())
                            .map(|v| v.to_string())
                            .unwrap_or_default();
                        RecycleEntry {
                            fid: qualify(ProviderId::Pan123, &native),
                            name: text_at(row, "FileName"),
                            is_dir: row.get("Type").and_then(|v| v.as_i64()) == Some(1),
                            size: row.get("Size").and_then(|v| v.as_u64()).unwrap_or(0),
                            deleted_at: super::parse_time(
                                row.get("UpdateAt").unwrap_or(&Value::Null),
                            ),
                            token: String::new(),
                            provider: ProviderId::Pan123.as_str().to_string(),
                        }
                    })
                    .collect());
            }
            seen += rows.len() as u32;
            let next = text_at(&value, "/data/Next");
            if rows.is_empty() || next.is_empty() || next == "0" || page > 50 {
                return Ok(Vec::new());
            }
            cursor = next;
            page += 1;
        }
    }

    async fn recycle_restore(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        let mut list: Vec<Value> = Vec::with_capacity(entries.len());
        for entry in entries {
            list.push(json!({ "FileId": numeric_id(&super::native_of(&entry.fid))? }));
        }
        if list.is_empty() {
            return Err(AppError::new("请先选择要恢复的内容"));
        }
        // `operation: false` restores; `true` is what `delete` uses to *send*
        // items to the bin.
        self.call(
            "POST",
            &format!("{BASE}/a/api/file/trash"),
            Some(json!({ "driveId": 0, "fileTrashInfoList": list, "operation": false })),
        )
        .await?;
        Ok(entries.len() as u32)
    }

    async fn recycle_purge(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        // The web client's "彻底删除" call, captured 2026-11. It is the *file*
        // delete endpoint with `event: recycleDelete` — the `/trash/delete` and
        // `/trash/clear` paths people assume exist are 404.
        if entries.is_empty() {
            return Err(AppError::new("请先选择要彻底删除的内容"));
        }
        let mut list: Vec<Value> = Vec::with_capacity(entries.len());
        for entry in entries {
            list.push(json!({ "fileId": numeric_id(&super::native_of(&entry.fid))? }));
        }
        self.call_accepting(
            "POST",
            &format!("{BASE}/b/api/file/delete"),
            Some(json!({
                "fileIdList": list,
                "event": "recycleDelete",
                "operatePlace": 1,
                "RequestSource": Value::Null,
            })),
            // 7301 = "已删除，系统释放空间需要一段时间".
            &[7301],
        )
        .await?;
        Ok(entries.len() as u32)
    }

    async fn recycle_clear(&self) -> AppResult<()> {
        // There is no "empty the bin" endpoint, so this is purge-everything in
        // chunks. Re-listing after each chunk is deliberate: ids that were
        // already freeing space are still listed for a while, and a blind
        // pagination walk would miss the ones that shift up.
        for _ in 0..200 {
            let items = self.recycle_list(0, 100).await?;
            if items.is_empty() {
                return Ok(());
            }
            self.recycle_purge(&items).await?;
        }
        Ok(())
    }

    // ---------------------------------------------------------- shares ---

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let info = self.share_info(id, passcode).await?;
        if info.get("Expired").and_then(|v| v.as_bool()) == Some(true) {
            return Err(AppError::new("分享链接已过期"));
        }
        let has_pwd = info.get("HasPwd").and_then(|v| v.as_bool()) == Some(true);
        if has_pwd && passcode.is_empty() {
            return Err(AppError::new("该分享需要提取码，请在链接后面加上 ?pwd="));
        }
        // A wrong passcode is only reported by the listing call.
        let listing = self.share_list_raw(id, passcode, ROOT, 0).await?;
        let file_num = listing
            .pointer("/data/Total")
            .or_else(|| listing.pointer("/data/Len"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let expired_at = info
            .get("Expiration")
            .map(|v| super::parse_time(v) as i64)
            .unwrap_or(0);
        let avatar = info
            .get("HeadImage")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string());
        let owner_avatar = match avatar.as_deref() {
            Some(url) => crate::share::fetch_avatar(&self.http()?, url).await,
            None => None,
        };
        Ok(ShareSession {
            provider: ProviderId::Pan123.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            stoken: passcode.to_string(),
            title: text_at(&info, "ShareName"),
            owner: text_at(&info, "UserNickName"),
            owner_avatar,
            file_num,
            total_size: 0,
            expired_at,
            first_fid: None,
            can_save: true,
            web_url: Some(format!("{SHARE_BASE}/s/{id}")),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<ShareListing> {
        let parent = if dir.is_empty() { ROOT } else { dir };
        let mut entries = Vec::new();
        let mut next = 0u64;
        let mut total = 0u64;
        loop {
            let value = self
                .share_list_raw(&share.pwd_id, &share.stoken, parent, next)
                .await?;
            let data = value.get("data").cloned().unwrap_or(Value::Null);
            if data.get("Expired").and_then(|v| v.as_bool()) == Some(true) {
                return Err(AppError::new("分享链接已过期"));
            }
            total = data
                .get("Total")
                .and_then(|v| v.as_u64())
                .unwrap_or(total);
            let rows = data
                .get("InfoList")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let got = rows.len();
            for item in &rows {
                let fid = item
                    .get("FileId")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_default()
                    .to_string();
                let name = text_at(item, "FileName");
                let is_dir = item.get("Type").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
                // `download/info` needs the s3 key flag, the etag and the size;
                // none of them is in `ShareEntry`, so pack them into the opaque
                // token field.
                let token = format!(
                    "{}|{}|{}",
                    text_at(item, "S3KeyFlag"),
                    text_at(item, "Etag"),
                    item.get("Size").and_then(|v| v.as_i64()).unwrap_or(0).max(0)
                );
                entries.push(ShareEntry {
                    fid,
                    token,
                    name,
                    size: item.get("Size").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u64,
                    is_dir,
                    updated_at: item
                        .get("UpdateAt")
                        .map(|v| super::parse_time(v) as i64)
                        .unwrap_or(0),
                });
            }
            let cursor = data.get("Next").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u64;
            if got == 0 || cursor == 0 || (total > 0 && entries.len() as u64 >= total) {
                break;
            }
            next = cursor;
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(ShareListing {
            pdir_fid: parent.to_string(),
            total: if total > 0 {
                total
            } else {
                entries.len() as u64
            },
            entries,
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
        // Only `fileList[0].parentFileID` is honoured — every other shape of the
        // target is silently ignored and the copy lands in the root.
        let list: Vec<Value> = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let parent = if index == 0 {
                    to.map(|v| v.parse::<i64>().unwrap_or(0)).unwrap_or(0)
                } else {
                    0
                };
                json!({
                    "fileID": entry.fid.parse::<i64>().unwrap_or(0),
                    "size": entry.size,
                    "etag": "",
                    "type": if entry.is_dir { 1 } else { 0 },
                    "parentFileID": parent,
                    "fileName": entry.name,
                    "driveID": 0,
                })
            })
            .collect();
        let body = json!({
            "fileList": list,
            "shareKey": share.pwd_id,
            "sharePwd": share.passcode,
            "currentLevel": 0,
            "superAdmin": Value::Null,
        });
        let value = self
            .call(
                "POST",
                &format!("{SHARE_BASE}/api/restful/goapi/v1/file/copy/save"),
                Some(body),
            )
            .await?;
        let task = value
            .pointer("/data/taskID")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if task == 0 {
            return Ok(entries.len() as u64);
        }
        self.poll_save_task(task).await?;
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
        queue.push_back((dir.to_string(), std::path::PathBuf::new(), 0usize));
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
                        token: entry.token,
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
        // The entry's s3 key flag and etag are required by `download/info`; they
        // were packed into the token by `share_list`.
        let (s3_key_flag, etag, size) = match share.fid_token.split_once('|') {
            Some((flag, rest)) => {
                let (etag, size) = match rest.split_once('|') {
                    Some((etag, size)) => (etag.to_string(), size.parse::<u64>().unwrap_or(0)),
                    None => (rest.to_string(), 0),
                };
                (flag.to_string(), etag, size)
            }
            None => (String::new(), String::new(), 0u64),
        };
        let body = json!({
            "shareKey": share.pwd_id,
            "SharePwd": share.passcode,
            "fileId": fid.parse::<i64>().unwrap_or(0),
            "s3keyFlag": s3_key_flag,
            "size": size,
            "etag": etag,
        });
        let value = self
            .call(
                "POST",
                &format!("{SHARE_BASE}/b/api/share/download/info"),
                Some(body),
            )
            .await?;
        let url = value
            .pointer("/data/DownloadURL")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| AppError::new("123 云盘没有返回分享下载地址"))?;
        Ok(ShareLink {
            url: resolve_cdn_url(url).await?,
            headers: vec![
                ("Referer".to_string(), "https://yun.123pan.com/".to_string()),
                ("User-Agent".to_string(), "Mozilla/5.0".to_string()),
            ],
            size: 0,
            cleanup_fid: None,
        })
    }

    async fn create_share(
        &self,
        fids: &[String],
        title: &str,
        days: u32,
        passcode: &str,
    ) -> AppResult<crate::share::ShareCreated> {
        if fids.is_empty() {
            return Err(AppError::new("请先选择要分享的内容"));
        }
        // 123 wants a comma-separated string, not an array, and the expiry is
        // mandatory and must carry a timezone offset — an empty one is rejected
        // with 「Expiration格式异常」. A century is how the web client says
        // "permanent".
        let list = fids
            .iter()
            .map(|fid| native_of(fid))
            .collect::<Vec<_>>()
            .join(",");
        let expiration = if days == 0 {
            "9999-12-31T23:59:59+08:00".to_string()
        } else {
            let when = chrono::Local::now() + chrono::Duration::days(i64::from(days));
            when.format("%Y-%m-%dT%H:%M:%S+08:00").to_string()
        };
        let code = if passcode.chars().count() == 4 {
            passcode.to_string()
        } else {
            String::new()
        };
        // The field is `sharePwd`; a body with `pwd` is accepted but silently
        // ignored, leaving a share that anyone can open (probe:
        // `live_123_passcode_guard`). `sharePwd` really does gate the listing.
        let body = json!({
            "driveId": 0,
            "fileIdList": list,
            "expiration": expiration,
            "shareName": if title.is_empty() { "分享" } else { title },
            "sharePwd": code,
            "shareType": 1,
        });
        let value = self
            .call("POST", &format!("{SHARE_BASE}/b/api/share/create"), Some(body))
            .await?;
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        // `ShareUrl` is always empty; the real links live in a nested list.
        let url = data
            .pointer("/shareLinkList/list/0")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .or_else(|| {
                data.pointer("/shareLinkList/standBy")
                    .and_then(|v| v.as_str())
                    .filter(|u| !u.is_empty())
            })
            .ok_or_else(|| AppError::new("123 云盘没有返回分享链接，请稍后重试"))?;
        let expired_at = if days == 0 {
            0
        } else {
            chrono::Local::now().timestamp_millis() + i64::from(days) * 86_400_000
        };
        Ok(crate::share::ShareCreated {
            url: url.to_string(),
            passcode: code,
            expired_at,
            share_id: data
                .get("ShareId")
                .map(|v| v.to_string())
                .unwrap_or_default(),
        })
    }

    /// `POST /b/api/share/delete` — `shareId` is an integer here.
    async fn revoke_share(&self, share_id: &str, _fids: &[String]) -> AppResult<()> {
        let id: i64 = share_id
            .trim_matches('"')
            .parse()
            .map_err(|_| AppError::new(format!("123 云盘的分享 ID 无效：{share_id}")))?;
        self.call(
            "POST",
            &format!("{SHARE_BASE}/b/api/share/delete"),
            Some(json!({
                "shareInfoList": [{ "shareId": id }],
                "driveId": 0,
            })),
        )
        .await
        .map(|_| ())
    }

    /// `GET /b/api/share/list` — the shares this account owns, newest first.
    ///
    /// `SharePwd` is echoed here even though `/b/api/share/info` hides it, and
    /// `Expiration` is an ISO string (`9999-12-31…` means permanent).
    async fn share_records(&self) -> AppResult<Vec<crate::share::ShareRecord>> {
        let value = self
            .call(
                "GET",
                &format!(
                    "{SHARE_BASE}/b/api/share/list?limit=100&next=0&orderBy=createAt&orderDirection=desc"
                ),
                None,
            )
            .await?;
        let list = value
            .pointer("/data/InfoList")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(list
            .iter()
            .map(|row| {
                let expiry = row.get("Expiration").and_then(|v| v.as_str()).unwrap_or("");
                crate::share::ShareRecord {
                    share_id: row
                        .get("ShareId")
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                    url: row
                        .pointer("/shareLinkList/list/0")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    title: text_at(row, "ShareName"),
                    passcode: text_at(row, "SharePwd"),
                    expired_at: parse_iso_ms(expiry),
                    created_at: parse_iso_ms(&text_at(row, "CreateAt")),
                    file_num: row
                        .get("FileIdList")
                        .and_then(|v| v.as_str())
                        .map(|v| v.split(',').filter(|p| !p.trim().is_empty()).count() as u32)
                        .unwrap_or(0),
                    size: 0,
                    visit_count: row
                        .get("PreviewCount")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                    save_count: row.get("SaveCount").and_then(|v| v.as_i64()).unwrap_or(0),
                    download_count: row
                        .get("DownloadCount")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                    expired: row
                        .get("Expired")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    fids: row
                        .get("FileIdList")
                        .and_then(|v| v.as_str())
                        .map(|v| {
                            v.split(',')
                                .map(|p| p.trim().to_string())
                                .filter(|p| !p.is_empty())
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect())
    }
}

/// Parses an ISO timestamp into ms since the epoch; 0 when unusable.
///
/// A permanent share is spelled `9999-12-31T23:59:59+08:00`; reporting that as
/// an expiry would render as a date in the year 9999, so anything from 2100 on
/// is folded back to "no expiry" (0).
fn parse_iso_ms(text: &str) -> i64 {
    const PERMANENT: i64 = 4_102_444_800_000; // 2100-01-01T00:00:00Z
    match chrono::DateTime::parse_from_rfc3339(text) {
        Ok(at) if at.timestamp_millis() < PERMANENT => at.timestamp_millis(),
        _ => 0,
    }
}

impl Pan123Provider {
    /// `share/info` — title, owner, expiry and whether a passcode is needed.
    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn debug_get(&self, url: &str) -> AppResult<Value> {
        self.call("GET", url, None).await
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn debug_post(&self, url: &str, body: Value) -> AppResult<Value> {
        self.call("POST", url, Some(body)).await
    }

    /// Copies files into `to`.
    ///
    /// The web UI sends lowercase `fileId` per item plus a single
    /// `targetFileId`; the destination is that field and nothing else. Sending
    /// the Android-style `{"FileId": n}` / `parentFileId` body instead makes the
    /// server answer `code=0` with a task that copies *nothing* — which looked
    /// like a silent no-op for a long time.
    async fn copy_entries(&self, fids: &[String], to: &str) -> AppResult<usize> {
        let target = numeric_id(to)?;
        let mut copied = 0usize;
        for fid in fids {
            let source = numeric_id(fid)?;
            let (parent, entry) = self.locate(fid).await?;
            let body = json!({
                "fileList": [{
                    "fileId": source,
                    "size": entry.size,
                    "etag": entry.etag,
                    "type": 0,
                    "parentFileId": parent,
                    "fileName": entry.name,
                    "driveId": 0,
                }],
                "targetFileId": target,
            });
            let started = self
                .call(
                    "POST",
                    &format!("{BASE}/api/restful/goapi/v1/file/copy/async"),
                    Some(body),
                )
                .await?;
            if let Some(task) = started.pointer("/data/taskId").and_then(|v| v.as_i64()) {
                self.poll_copy_task(task).await?;
            }
            copied += 1;
        }
        Ok(copied)
    }

    /// Waits for a `copy/async` task to finish.
    async fn poll_copy_task(&self, task: i64) -> AppResult<()> {
        for _ in 0..80 {
            let value = self
                .call(
                    "GET",
                    &format!(
                        "{BASE}/api/restful/goapi/v1/file/copy/task?taskId={task}"
                    ),
                    None,
                )
                .await?;
            match value.pointer("/data/status").and_then(|v| v.as_i64()) {
                Some(2) => return Ok(()),
                Some(3) | Some(4) => {
                    let reason = value
                        .pointer("/data/reason")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    return Err(AppError::new(format!("123 云盘复制失败：{reason}")));
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(700)).await,
            }
        }
        Err(AppError::new("复制超时，请稍后在「我的文件」中查看"))
    }

    async fn share_info(&self, key: &str, passcode: &str) -> AppResult<Value> {
        let url = format!(
            "{SHARE_BASE}/b/api/share/info?shareKey={key}&SharePwd={passcode}"
        );
        let value = self.call("GET", &url, None).await?;
        Ok(value.get("data").cloned().unwrap_or(Value::Null))
    }

    /// `/b/api/user/info` — display name and avatar.
    ///
    /// Note the host: the profile lives on the web host (`www.123pan.cn`), not
    /// on the Android host the file APIs use.
    async fn fetch_profile(&self) -> AppResult<AccountInfo> {
        let value = self.call("GET", &format!("{BASE}/b/api/user/info"), None).await?;
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let nickname = text_at(&data, "Nickname");
        let mut avatar = None;
        let url = text_at(&data, "HeadImage");
        if !url.is_empty() {
            avatar = crate::share::fetch_avatar(&self.http()?, &url).await;
        }
        if nickname.is_empty() && avatar.is_none() {
            return Err(AppError::new("123 云盘资料接口没有返回昵称"));
        }
        Ok(AccountInfo { nickname, avatar })
    }

    /// One page of a shared folder. `parentFileId` is mandatory.
    async fn share_list_raw(
        &self,
        key: &str,
        passcode: &str,
        parent: &str,
        next: u64,
    ) -> AppResult<Value> {
        let url = format!(
            "{SHARE_BASE}/b/api/share/get?limit=100&next={next}&orderBy=file_id&orderDirection=desc&shareKey={key}&SharePwd={passcode}&Page=1&parentFileId={parent}"
        );
        self.call("GET", &url, None).await
    }

    /// Waits for a share-save task to report success (`data.status == 2`).
    async fn poll_save_task(&self, task: i64) -> AppResult<()> {
        for _ in 0..80 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let url = format!(
                "{SHARE_BASE}/api/restful/goapi/v1/file/copy/save/get?taskID={task}"
            );
            let Ok(value) = self.call("GET", &url, None).await else {
                continue;
            };
            match value
                .pointer("/data/status")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
            {
                2 => return Ok(()),
                -1 => return Err(AppError::new("转存失败，请稍后重试")),
                _ => {}
            }
        }
        Err(AppError::new("转存超时，请稍后在「我的文件」中查看"))
    }
}

/// Parses a native 123 id, which is always numeric.
///
/// Used to fall back to `0` — the root — so a malformed id silently retargeted
/// the operation (rename/delete/upload) at the drive root.
fn numeric_id(fid: &str) -> AppResult<i64> {
    fid.trim()
        .parse::<i64>()
        .map_err(|_| AppError::new(format!("123 云盘的 id 无效：{fid}")))
}

/// Percent-encodes a query value (`SearchData` is a raw keyword).
fn enc(raw: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(raw, NON_ALPHANUMERIC).to_string()
}

fn text_at(value: &Value, pointer: &str) -> String {
    // Callers pass either a JSON pointer (`/data/Bucket`) or a plain key
    // (`FileName`); `Value::pointer` only understands the former.
    let found = if pointer.starts_with('/') {
        value.pointer(pointer)
    } else {
        value.get(pointer)
    };
    found.and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// Hashes a file and reports bytes hashed through `sink`.
async fn md5_file(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<(String, u64)> {
    use md5::{Digest, Md5};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Md5::new();
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
    Ok((format!("{:x}", hasher.finalize()), total))
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Finds a file by name anywhere in the drive and downloads it.
    ///
    /// `cargo test --lib -- --ignored --nocapture pan123_probe_find`
    #[tokio::test]
    #[ignore]
    async fn pan123_probe_find() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(ROOT.to_string());
        let mut seen = 0;
        while let Some(parent) = queue.pop_front() {
            seen += 1;
            if seen > 60 {
                break;
            }
            let Ok(listing) = drive.list(&parent).await else {
                continue;
            };
            for e in &listing.entries {
                if e.name.contains("融卡") {
                    let native = super::super::native_of(&e.fid);
                    println!("found {} {} size={}", e.fid, e.name, e.size);
                    match drive.resolve_download_url(&native).await {
                        Ok(url) => println!("  url = {}", &url[..url.len().min(140)]),
                        Err(err) => println!("  resolve ERR {err}"),
                    }
                    match drive.open_download(&native, 0).await {
                        Ok(h) => println!("  open ok size={}", h.size),
                        Err(err) => println!("  open ERR {err}"),
                    }
                }
                if e.is_dir && seen < 40 {
                    queue.push_back(super::super::native_of(&e.fid));
                }
            }
        }
    }

    /// Reproduces the user's failed upload of a real file.
    ///
    /// `cargo test --lib -- --ignored --nocapture pan123_probe_upload`
    #[tokio::test]
    #[ignore]
    async fn pan123_probe_upload_real_file() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");
        let local = std::path::Path::new(
            "/home/henvei/桌面/ChatGPT Image 2026年5月2日 09_23_48.png",
        );
        let root = drive.list(ROOT).await.unwrap();
        let umi = root
            .entries
            .iter()
            .find(|e| e.name == "Umi")
            .expect("Umi");
        for (label, parent) in [
            ("root", ROOT.to_string()),
            ("Umi", super::super::native_of(&umi.fid)),
        ] {
            let sink = std::sync::Arc::new(super::super::CounterSink::new(std::sync::Arc::new(
                std::sync::atomic::AtomicU64::new(0),
            )));
            let token = tokio_util::sync::CancellationToken::new();
            let name = "ChatGPT Image 2026年5月2日 09_23_48.png".to_string();
            // Twice: the second pass reuses the content the drive already has
            // (`Reuse:true`), which is exactly the path that used to answer
            // with the session id instead of the file id.
            for round in 0..2 {
                let token = token.clone();
                match drive.upload(local, &parent, &name, sink.clone(), token).await {
                    Ok(id) => {
                        println!("{label} round {round}: ok id={id:?}");
                        let listing = drive.list(&parent).await.unwrap();
                        for e in listing.entries.iter().filter(|e| e.name.starts_with("ChatGPT")) {
                            println!("{label}: listing {} {} {}", e.fid, e.size, e.name);
                        }
                        if let Some(id) = id {
                            let _ = drive.delete(&[super::super::native_of(&id)]).await;
                        }
                    }
                    Err(err) => println!("{label} round {round}: ERR {err}"),
                }
            }
        }
    }

    /// Live probe against the signed-in 123 云盘 account.
    ///
    /// `cargo test --lib pan123_live -- --ignored --nocapture`
    ///
    /// The account this was written against has an empty root, so the test
    /// tolerates "nothing there" and only insists that the call itself succeeds.
    #[tokio::test]
    #[ignore]
    async fn pan123_live_listing_and_quota() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");

        let root = drive.list(ROOT).await.expect("root listing");
        println!("root: {} entries (total {})", root.entries.len(), root.total);
        for e in root.entries.iter().take(10) {
            println!("  {} dir={} size={}", e.name, e.is_dir, e.size);
        }

        if let Some(folder) = root.entries.iter().find(|e| e.is_dir) {
            let sub = drive
                .list(&super::super::native_of(&folder.fid))
                .await
                .expect("subfolder listing");
            println!("subfolder {} -> {} entries", folder.name, sub.entries.len());
        }

        match drive.quota().await {
            Ok(Some(q)) => println!("quota: used {} / total {}", q.used, q.total),
            Ok(None) => println!("quota: none"),
            Err(err) => panic!("quota failed: {err}"),
        }
    }

    /// Monthly download allowance, the number the web UI shows as 剩余流量.
    ///
    /// `cargo test --lib pan123_live_traffic -- --ignored --nocapture`
    ///
    /// The Android endpoint behind `quota()` does not carry it; this one asks
    /// the web endpoint (`/b/api/user/info`) with `App-Version: 3`, which is
    /// what the Tampermonkey script on the desktop does. Read-only.
    #[tokio::test]
    #[ignore]
    async fn pan123_live_traffic() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");
        match drive.traffic().await {
            Ok(Some(t)) => {
                let gib = |b: u64| format!("{:.2} GB", b as f64 / 1024f64.powi(3));
                println!(
                    "traffic: left {} share {} direct {}",
                    gib(t.left),
                    gib(t.share),
                    gib(t.direct)
                );
                assert!(t.left > 0, "a free account always has some allowance left");
            }
            Ok(None) => panic!("no traffic field: is the account upgraded or the header wrong?"),
            Err(err) => panic!("traffic failed: {err}"),
        }
    }

    /// Recycle bin: delete a folder, find it, purge it for good.
    ///
    /// `cargo test --lib pan123_live_recycle -- --ignored --nocapture`
    ///
    /// Covers the endpoint the previous implementation could not find: "彻底
    /// 删除" is `POST /b/api/file/delete` with `event: recycleDelete`, not any
    /// of the `/trash/delete` paths. It also sweeps this project's own probe
    /// leftovers (`yunyu-*`) out of the bin, so a run leaves the account
    /// tidier than it found it. `recycle_clear` is deliberately **not** called:
    /// it empties the whole bin, including the user's own files.
    #[tokio::test]
    #[ignore]
    async fn pan123_live_recycle() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");

        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("yunyu-rb-{stamp}");

        let folder = drive.make_dir(ROOT, &name).await.expect("make_dir");
        drive
            .delete(std::slice::from_ref(&folder))
            .await
            .expect("delete into the bin");

        let items = drive.recycle_list(0, 100).await.expect("recycle_list");
        println!("bin: {} entries", items.len());
        let hit = items
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} should be in the bin"));
        assert!(hit.is_dir, "a deleted folder must be listed as a directory");
        drive
            .recycle_purge(std::slice::from_ref(hit))
            .await
            .expect("purge");

        let items = drive.recycle_list(0, 100).await.expect("recycle_list");
        assert!(
            !items.iter().any(|e| e.name == name),
            "a purged entry must not come back"
        );
        println!("purged {name}");

        // Sweep our own leftovers out of the bin.
        let leftovers: Vec<super::RecycleEntry> = items
            .into_iter()
            .filter(|e| e.name.starts_with("yunyu"))
            .collect();
        if !leftovers.is_empty() {
            let done = drive
                .recycle_purge(&leftovers)
                .await
                .expect("purge leftovers");
            println!("swept {done} probe leftovers");
        }
        let items = drive.recycle_list(0, 100).await.expect("recycle_list");
        println!("bin now: {} entries", items.len());
    }

    /// Upload → list → download → delete, checking the progress phases the UI
    /// shows and that the id we hand back resolves.
    ///
    /// `cargo test --lib pan123_live_upload -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn pan123_live_upload_progress_and_download() {
        use crate::providers::{CounterSink, UploadSink};
        use futures_util::StreamExt;
        use std::sync::atomic::AtomicU64;

        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = Pan123Provider::load(&dir).expect("pan123.json");

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("yunyu-upload-{nonce}.bin");
        let local = std::env::temp_dir().join(&name);
        let payload: Vec<u8> = (0..2 * 1024 * 1024u32)
            .map(|i| (i.wrapping_add(nonce as u32) % 251) as u8)
            .collect();
        std::fs::write(&local, &payload).unwrap();

        let done = Arc::new(AtomicU64::new(0));
        let inner = Arc::new(CounterSink::new(done.clone()));
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
            inner: inner.clone(),
            phases: phases.clone(),
        });

        let created = drive
            .upload(&local, ROOT, &name, spy, CancellationToken::new())
            .await
            .expect("upload");
        let seen = phases.lock().unwrap().clone();
        println!(
            "phases: {seen:?} done={}",
            done.load(std::sync::atomic::Ordering::Relaxed)
        );
        assert_eq!(seen.first().map(String::as_str), Some("hashing"));
        assert!(seen.iter().any(|p| p == "uploading"), "phases: {seen:?}");

        let fid = created.expect("upload should report an id");
        let native = super::super::native_of(&fid);
        // The listing is eventually consistent: the file shows up a moment after
        // `upload_complete` answers.
        let mut hit = None;
        for attempt in 0..10 {
            let listing = drive.list(ROOT).await.expect("root listing");
            hit = listing.entries.into_iter().find(|e| e.name == name);
            if hit.is_some() {
                break;
            }
            println!("listing attempt {attempt}: not there yet");
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        }
        let hit = hit.expect("uploaded file should be listed");
        println!("listed {} size={} fid={}", hit.name, hit.size, hit.fid);
        assert_eq!(hit.size, payload.len() as u64);

        let mut handle = drive.open_download(&native, 0).await.expect("download");
        let first = handle.stream.next().await.expect("chunk").expect("bytes");
        println!("download: {} bytes", first.len());
        assert_eq!(&first[..4], &payload[..4]);
        drop(handle);

        // Move into the first real folder and back: `mod_pid` is the only
        // transfer this API offers.
        let folders = drive.list(ROOT).await.expect("root listing");
        if let Some(folder) = folders.entries.iter().find(|e| e.is_dir) {
            let target = super::super::native_of(&folder.fid);
            drive
                .transfer_entries(std::slice::from_ref(&native), &target, false)
                .await
                .expect("move");
            let inside = drive.list(&target).await.expect("target listing");
            assert!(
                inside.entries.iter().any(|e| e.name == name),
                "moved file should be inside {}",
                folder.name
            );
            drive
                .transfer_entries(std::slice::from_ref(&native), ROOT, false)
                .await
                .expect("move back");
            println!("moved into {} and back", folder.name);
        }

        drive.delete(&[native]).await.expect("delete");
        let _ = std::fs::remove_file(&local);
        println!("deleted {name}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_round_trips_through_the_query_helpers() {
        let url = "https://cdn.example.com/f?x=1";
        let encoded = base64::engine::general_purpose::STANDARD.encode(url);
        assert_eq!(b64_decode(&encoded).unwrap(), url);
    }

    /// A plain CDN url is used as-is: there is no proxy host to route through
    /// any more (`web-pro2.123952.com` lost its DNS record).
    #[test]
    fn plain_urls_are_left_alone() {
        let raw = "https://cdn.123pan.cn/a/b?c=1";
        assert_eq!(unproxy(raw), raw);
    }

    /// A `web-pro*` url still has to work, because saved transfer rows and older
    /// `download_info` answers carry one: its base64 `params` *is* the target.
    #[test]
    fn proxied_urls_are_unwrapped_into_their_params() {
        let inner = "https://user-app-free-download-cdn.123773.com/123-932/x?auto_redirect=0";
        let proxied = format!(
            "https://web-pro2.123952.com/download-v2/?params={}&is_s3=0",
            base64::engine::general_purpose::STANDARD.encode(inner)
        );
        assert_eq!(unproxy(&proxied), inner);
    }

    /// The CDN answers `HTTP 210` with this body instead of a 302.
    #[test]
    fn reads_the_json_redirect_the_cdn_answers_with() {
        let body = r#"{"data":{"redirect_url":"https://42-4-59-152-v3.pd1.cjjd19.com/x"},"code":0,"message":"ok"}"#;
        assert_eq!(
            json_redirect(body).as_deref(),
            Some("https://42-4-59-152-v3.pd1.cjjd19.com/x")
        );
        assert_eq!(json_redirect("{}"), None);
    }

    #[test]
    fn reads_the_html_redirect_fallback() {
        let body = "<script>location.href='https://edge.example.com/f?x=1'</script>";
        assert_eq!(
            html_redirect(body).as_deref(),
            Some("https://edge.example.com/f?x=1")
        );
    }

    #[test]
    fn random_hex_is_hex_of_the_right_length() {
        let uuid = random_hex(16);
        assert_eq!(uuid.len(), 32);
        assert!(uuid.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(uuid, random_hex(16), "must not repeat");
    }
}

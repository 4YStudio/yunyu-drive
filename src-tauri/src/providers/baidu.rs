//! 百度网盘 (Baidu Netdisk).
//!
//! Unlike the other drives this one logs in through the **web** session: the
//! in-app webview signs the user into `pan.baidu.com` and we lift the `BDUSS` /
//! `STOKEN` cookies out of WebKit's jar. That avoids asking for a developer
//! `app_key`/`secret_key`, which the OpenAPI route (`openapi.baidu.com`) needs.
//!
//! Everything therefore speaks the browser's own endpoints:
//!
//! * `GET  /api/list`            — one folder page (needs `bdstoken`)
//! * `GET  /api/filemetas`       — `dlink` for a path
//! * `POST /api/filemanager`     — rename / move / copy / delete
//! * `POST /api/precreate` + `/api/create?a=commit` — upload bookends
//! * `POST https://d.pcs.baidu.com/rest/2.0/pcs/superfile2` — one slice
//! * `GET  /share/list`           — one page of someone else's share
//! * `POST /share/transfer`       — save a share entry into this account
//!
//! Two quirks drive the design:
//!
//! * The API is **path** addressed for mutations but **fs_id** addressed for
//!   listing, so a `fid -> path` map is kept (same trick as 139).
//! * `bdstoken` is required by the `/api/*` family and is only valid for the
//!   cookie that produced it, so it is fetched lazily and cached next to it.
//!
//! Protocol facts were taken from the MIT-licensed `baidupan-cli` (OpenAPI
//! variant) and from OpenList's Apache-2.0 `baidu_netdisk` driver (slice sizes,
//! `block_list` shape, `filelist` shape). No code from either is linked in.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    qualify, AuthState, Capabilities, DirListing, DownloadHandle, DriveProvider, EntryDto, PathNode,
    ProviderId, QuotaInfo, SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};
use crate::error::{AppError, AppResult};
use crate::share::{AccountInfo, ShareEntry, ShareListing, ShareSession};

const BASE: &str = "https://pan.baidu.com";
/// Slices for the upload endpoints live on their own host.
const UPLOAD_HOST: &str = "https://d.pcs.baidu.com";
/// Non-VIP accounts are pinned to 4 MiB slices; VIP accounts may go larger, but
/// 4 MiB works for everybody and keeps the block list under the 2048 limit for
/// files up to ~8 GiB.
const SLICE_SIZE: usize = 4 * 1024 * 1024;
/// `slice-md5` is the md5 of the first 256 KiB, not of the first slice.
const SLICE_MD5_SIZE: usize = 256 * 1024;
/// `limit` the web list endpoint accepts.
const PAGE_LIMIT: u32 = 100;
/// Root folder. The frontend always uses `<provider>:0` for it.
const ROOT: &str = "0";
/// Baidu's web endpoints reject requests whose `Referer` is missing or foreign;
/// the API is not a public one and is gated behind the site's own origin checks.
const REFERER: &str = "https://pan.baidu.com/disk/main";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                  Chrome/131.0.0.0 Safari/537.36";
/// Download links only answer to Baidu's own UA.
const DL_UA: &str = "pan.baidu.com";

/// Persisted login state: the web cookie plus what we learned from it.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct BaiduCredentials {
    /// Full `Cookie:` header (`BDUSS=…; STOKEN=…`).
    #[serde(default)]
    pub cookie: String,
    /// Web API token, derived from `cookie` on demand.
    #[serde(default)]
    pub bdstoken: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub avatar: String,
}

pub struct BaiduProvider {
    creds: RwLock<BaiduCredentials>,
    credential_path: String,
    /// `fs_id -> 绝对路径`, filled by listings so mutations can address files.
    paths: RwLock<HashMap<String, String>>,
    client: std::sync::OnceLock<reqwest::Client>,
}

impl BaiduProvider {
    pub fn new(creds: BaiduCredentials, credential_path: String) -> Self {
        Self {
            creds: RwLock::new(creds),
            credential_path,
            paths: RwLock::new(HashMap::new()),
            client: std::sync::OnceLock::new(),
        }
    }

    /// Loads `<config>/baidu.json`, or reports "not logged in".
    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let path = own_dir.join("baidu.json");
        if !path.exists() {
            return Err(AppError::new("尚未登录百度网盘"));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AppError::new(format!("读取百度网盘凭据失败：{e}")))?;
        let creds: BaiduCredentials = serde_json::from_str(&text)
            .map_err(|e| AppError::new(format!("百度网盘凭据格式错误：{e}")))?;
        if creds.cookie.trim().is_empty() {
            return Err(AppError::new("尚未登录百度网盘"));
        }
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    /// Stores a freshly harvested web cookie and proves it works.
    ///
    /// Returns the provider so the caller can report the login state; failing
    /// here (instead of on the first listing) is what makes "登录成功但列表为空"
    /// impossible.
    pub async fn save_cookie(own_dir: &Path, cookie: &str) -> AppResult<Self> {
        let cookie = cookie.trim();
        if cookie.is_empty() {
            return Err(AppError::new("没有拿到百度网盘的登录凭据"));
        }
        let provider = Self::new(
            BaiduCredentials {
                cookie: cookie.to_string(),
                ..Default::default()
            },
            String::new(),
        );
        crate::ylog!(
            "baidu login: fetching bdstoken with {} cookie pair(s)",
            cookie.matches('=').count()
        );
        let token = provider.fetch_bdstoken().await?;
        let (nickname, avatar) = provider.fetch_profile().await;
        crate::ylog!(
            "baidu login: bdstoken len={}, profile={}",
            token.len(),
            if nickname.is_empty() { "(empty)" } else { "ok" }
        );
        {
            let mut creds = provider.creds.write().await;
            creds.bdstoken = token;
            creds.nickname = nickname;
            creds.avatar = avatar;
        }
        provider.persist(own_dir).await?;
        Ok(provider)
    }

    async fn persist(&self, own_dir: &Path) -> AppResult<()> {
        let creds = self.creds.read().await.clone();
        std::fs::create_dir_all(own_dir)
            .map_err(|e| AppError::new(format!("创建配置目录失败：{e}")))?;
        let path = own_dir.join("baidu.json");
        let text = serde_json::to_string_pretty(&creds)
            .map_err(|e| AppError::new(format!("序列化凭据失败：{e}")))?;
        std::fs::write(&path, text).map_err(|e| AppError::new(format!("写入凭据失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        Ok(())
    }

    fn http(&self) -> AppResult<reqwest::Client> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            // No total timeout: this client also streams downloads.
            .read_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let _ = self.client.set(client.clone());
        Ok(client)
    }

    async fn cookie(&self) -> String {
        self.creds.read().await.cookie.clone()
    }

    /// `bdstoken`, fetched once per cookie.
    ///
    /// `/api/*` answers `errno: -6` ("not logged in") without it even when the
    /// session cookie is fine, so this has to succeed before anything else.
    async fn bdstoken(&self) -> AppResult<String> {
        let cached = self.creds.read().await.bdstoken.clone();
        if !cached.is_empty() {
            return Ok(cached);
        }
        let token = self.fetch_bdstoken().await?;
        self.creds.write().await.bdstoken = token.clone();
        // Persist so a restart does not need another round trip.
        if !self.credential_path.is_empty() {
            if let Some(dir) = Path::new(&self.credential_path).parent() {
                let _ = self.persist(dir).await;
            }
        }
        Ok(token)
    }

    async fn fetch_bdstoken(&self) -> AppResult<String> {
        let url = format!("{BASE}/api/gettemplatevariable?fields=%5B%22bdstoken%22%5D&web=1");
        let value = self
            .get_json(&url, "获取 bdstoken")
            .await
            .map_err(|err| AppError::new(format!("{err}（登录可能已过期）")))?;
        let token = value
            .pointer("/result/bdstoken")
            .or_else(|| value.pointer("/result/0/bdstoken"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            return Err(AppError::new("百度网盘没有返回 bdstoken，请重新登录"));
        }
        Ok(token)
    }

    /// Nickname + avatar, best effort: a failure here must not block login.
    async fn fetch_profile(&self) -> (String, String) {
        let url = format!("{BASE}/rest/2.0/xpan/nas?method=uinfo&web=1");
        let Ok(value) = self.get_json(&url, "读取用户信息").await else {
            return (String::new(), String::new());
        };
        let name = value
            .get("netdisk_name")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .or_else(|| value.get("baidu_name").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string();
        let avatar = value
            .get("avatar_url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        (name, avatar)
    }

    /// GET that unwraps the `{errno, ...}` envelope.
    async fn get_json(&self, url: &str, what: &str) -> AppResult<Value> {
        let client = self.http()?;
        let cookie = self.cookie().await;
        let res = client
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::REFERER, REFERER)
            .header(reqwest::header::ORIGIN, BASE)
            .send()
            .await
            .map_err(|e| AppError::new(format!("{what}失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("{what}失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let snippet: String = text.chars().take(160).collect();
            json!({ "errno": -1, "errmsg": format!("HTTP {}：{}", status.as_u16(), snippet) })
        });
        // `-6` means "not logged in" but says nothing about *why*; the raw
        // envelope is the only way to tell an expired session from a malformed
        // header, so log every failing call.
        if value.get("errno").and_then(|v| v.as_i64()).unwrap_or(0) != 0 {
            crate::ylog!(
                "baidu: GET {what} HTTP {} → {}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            );
        }
        check_errno(&value, what)?;
        Ok(value)
    }

    /// POST form that unwraps the `{errno, ...}` envelope.
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, String)],
        what: &str,
    ) -> AppResult<Value> {
        let client = self.http()?;
        let cookie = self.cookie().await;
        let res = client
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::REFERER, REFERER)
            .header(reqwest::header::ORIGIN, BASE)
            .form(form)
            .send()
            .await
            .map_err(|e| AppError::new(format!("{what}失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("{what}失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let snippet: String = text.chars().take(160).collect();
            json!({ "errno": -1, "errmsg": format!("HTTP {}：{}", status.as_u16(), snippet) })
        });
        if value.get("errno").and_then(|v| v.as_i64()).unwrap_or(0) != 0 {
            crate::ylog!(
                "baidu: POST {what} HTTP {} → {}",
                status.as_u16(),
                text.chars().take(200).collect::<String>()
            );
        }
        check_errno(&value, what)?;
        Ok(value)
    }

    /// Path of an id: cache first, then the API, then "assume it is a path".
    async fn path_of(&self, fid: &str) -> AppResult<String> {
        if fid == ROOT || fid.is_empty() {
            return Ok("/".to_string());
        }
        if let Some(path) = self.paths.read().await.get(fid) {
            return Ok(path.clone());
        }
        // Files created earlier in this session, or ids from saved settings,
        // are not in the map yet; ask the API for the canonical path.
        //
        // The documented `/rest/2.0/xpan/multimedia?method=filemetas` endpoint
        // answers `errno: 9019 (need verify)` for web cookies — it is meant for
        // developer tokens. The web app's own `/api/filemetas` takes `fsids`
        // and returns `path`, which is what we need here.
        let url = format!(
            "{BASE}/api/filemetas?fsids=%5B{}%5D&dlink=0&web=5",
            fid.trim()
        );
        if let Ok(value) = self.get_json(&url, "解析文件路径").await {
            if let Some(path) = value
                .pointer("/info/0/path")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
            {
                self.paths
                    .write()
                    .await
                    .insert(fid.to_string(), path.to_string());
                return Ok(path.to_string());
            }
        }
        Err(AppError::new(format!("找不到百度网盘中的这个文件（{fid}）")))
    }

    async fn remember(&self, fid: &str, path: &str) {
        self.paths
            .write()
            .await
            .insert(fid.to_string(), path.to_string());
    }

    /// One page of `/api/list`.
    async fn list_page(&self, dir_path: &str, start: u32) -> AppResult<(Vec<EntryDto>, u32)> {
        let token = self.bdstoken().await?;
        let url = format!(
            "{BASE}/api/list?dir={}&order=name&desc=0&start={start}&limit={PAGE_LIMIT}&web=1&bdstoken={token}",
            urlencode(dir_path)
        );
        let value = self.get_json(&url, "列出百度网盘目录").await?;
        let rows = value
            .get("list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            let fid = row
                .get("fs_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| AppError::new("百度网盘返回了没有 fs_id 的文件条目"))?;
            let name = row
                .get("server_filename")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let is_dir = row.get("isdir").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            let size = row.get("size").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as u64;
            let updated = super::parse_time(row.get("server_mtime").unwrap_or(&Value::Null));
            let created = super::parse_time(row.get("server_ctime").unwrap_or(&Value::Null));
            let full = join_path(dir_path, &name);
            self.remember(&fid.to_string(), &full).await;
            entries.push(EntryDto {
                fid: qualify(ProviderId::Baidu, &fid.to_string()),
                pdir_fid: qualify(ProviderId::Baidu, &Self::encode_path(dir_path)),
                ext: EntryDto::ext_of(&name),
                name,
                size,
                is_dir,
                format_type: if is_dir { "folder".into() } else { "file".into() },
                updated_at: updated,
                created_at: created.max(updated),
                provider: ProviderId::Baidu.as_str().to_string(),
            });
        }
        let total = value.get("total").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        Ok((entries, total))
    }

    /// `pdir_fid` for a directory path: the root is `"0"`, everything else is
    /// the path itself so it can travel through JSON and breadcrumbs.
    ///
    /// The leading slash is kept: stripping it would turn a top-level folder
    /// literally named `0` into the root id.
    fn encode_path(path: &str) -> String {
        if path == "/" || path.is_empty() {
            return ROOT.to_string();
        }
        if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        }
    }

    /// Directory id (an encoded path) back to a real path.
    fn decode_dir(dir: &str) -> String {
        if dir == ROOT || dir.is_empty() {
            return "/".to_string();
        }
        if dir.starts_with('/') {
            return dir.to_string();
        }
        format!("/{dir}")
    }

    /// Resolves a directory id to its path, preferring the `fid -> path` map.
    ///
    /// Ids are numeric `fs_id`s, so after a restart the map is empty and a bare
    /// id would be mistaken for a path (`"/652993820124008"`) — the drive then
    /// answers `errno: -7` ("file not found") for a folder that exists. Anything
    /// that is not a path therefore goes through [`Self::path_of`], which asks
    /// the API for the canonical path and caches it.
    async fn dir_path(&self, dir: &str) -> AppResult<String> {
        if dir == ROOT || dir.is_empty() {
            return Ok("/".to_string());
        }
        if let Some(path) = self.paths.read().await.get(dir) {
            return Ok(path.clone());
        }
        if dir.chars().all(|c| c.is_ascii_digit()) {
            return self.path_of(dir).await;
        }
        Ok(Self::decode_dir(dir))
    }

    /// `dlink` for a path, plus the canonical path the API reported.
    ///
    /// `origin=dlna` must **not** be sent: it returns a dlink that the CDN then
    /// rejects with `error_code: 31329 "hit black userlist, hit illeage dlna"`.
    /// Plain `web=5` yields a working `d.pcs.baidu.com` link (verified live,
    /// including `Range` → `206`).
    async fn file_meta(&self, path: &str) -> AppResult<(String, String)> {
        let token = self.bdstoken().await?;
        let url = format!(
            "{BASE}/api/filemetas?target={}&dlink=1&web=5&bdstoken={token}",
            urlencode(&format!("[\"{}\"]", path))
        );
        let value = self.get_json(&url, "获取下载地址").await?;
        let dlink = value
            .pointer("/info/0/dlink")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if dlink.is_empty() {
            return Err(AppError::new("百度网盘没有返回下载地址"));
        }
        let canonical = value
            .pointer("/info/0/path")
            .and_then(|v| v.as_str())
            .unwrap_or(path)
            .to_string();
        Ok((dlink, canonical))
    }

    /// Turns a `dlink` into a CDN url that answers without the session cookie.
    ///
    /// Baidu answers the `dlink` with a redirect to the real CDN host; the
    /// `Location` header is the url the downloader wants. This runs on the web
    /// session (cookie), not an OAuth token, so no `access_token` is appended —
    /// an empty one is rejected outright. If no redirect shows up (some
    /// accounts/files) the `dlink` itself is used, which also works with the
    /// cookie attached.
    async fn resolve_dlink(&self, dlink: &str) -> String {
        let client = match self.http() {
            Ok(client) => client,
            Err(_) => return dlink.to_string(),
        };
        let cookie = self.cookie().await;
        let res = client
            .head(dlink)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, DL_UA)
            .send()
            .await;
        match res {
            Ok(res) => res
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .filter(|v| v.starts_with("http"))
                .map(|v| v.to_string())
                .unwrap_or_else(|| dlink.to_string()),
            Err(_) => dlink.to_string(),
        }
    }

    /// `filelist` payload for `/api/filemanager`.
    ///
    /// `ondup` decides what happens when the target name is taken: `fail` makes
    /// the whole copy answer `errno=12`, while `newcopy` lets the drive pick a
    /// free name — which is what the other providers do (they all auto-rename),
    /// so copying a file into the folder that already holds it works here too.
    async fn manage(&self, opera: &str, filelist: Value) -> AppResult<()> {
        let ondup = if opera == "copy" { "newcopy" } else { "fail" };
        let token = self.bdstoken().await?;
        let url = format!("{BASE}/api/filemanager?opera={opera}&async=0&web=1&bdstoken={token}");
        let value = self
            .post_form(
                &url,
                &[
                    ("filelist", filelist.to_string()),
                    ("async", "0".to_string()),
                    ("ondup", ondup.to_string()),
                    ("bdstoken", token.clone()),
                ],
                &format!("百度网盘 {opera}"),
            )
            .await?;
        // Per-entry errors ride inside a 200 response; the top-level `errno` is
        // just a summary (12 = "some entry failed"), so read the entries.
        let mut failures = 0usize;
        if let Some(info) = value.get("info").and_then(|v| v.as_array()) {
            for item in info {
                let errno = item.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
                if errno == 0 {
                    continue;
                }
                failures += 1;
                if failures == 1 {
                    let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    // The two the user can actually act on get a hint; Baidu's
                    // errno alone reads like a bug in this app.
                    let hint = match errno {
                        2 => "（目标文件夹不存在，或要复制到的正是它所在的文件夹）",
                        12 => "（目标位置已有同名文件）",
                        _ => "",
                    };
                    return Err(AppError::new(format!(
                        "百度网盘{opera}失败：{path}（errno={errno}）{hint}"
                    )));
                }
            }
        }
        check_errno(&value, &format!("百度网盘{opera}"))?;
        Ok(())
    }
}

/// What the share landing page tells us about a link.
struct SharePage {
    /// `shareid` as the API spells it.
    shareid: String,
    /// Uploader's `uk` — the `uk=` and `from=` parameter of every share call.
    uk: String,
    /// Nickname of the uploader.
    owner: String,
    /// Avatar url of the uploader.
    avatar: Option<String>,
    /// Whether the logged-in account owns this share.
    owner_uk: String,
    my_uk: String,
}

impl BaiduProvider {
    /// Reads `shareid` and the uploader's `uk` off the share page.
    ///
    /// Neither is derivable from the link: `surl` is an opaque short id, and
    /// the page embeds the pair in a `locals-data` JSON blob.
    async fn share_page(&self, surl: &str) -> AppResult<SharePage> {
        // `parse_link` strips the leading `1` because the API's `surl` has no
        // it; the web page is the other way round and 404s without it.
        let surl = if surl.starts_with('1') {
            surl.to_string()
        } else {
            format!("1{surl}")
        };
        let url = format!("{BASE}/s/{surl}");
        let cookie = self.cookie().await;
        let res = self
            .http()?
            .get(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .send()
            .await
            .map_err(|e| AppError::new(format!("打开分享页失败：{e}")))?;
        let html = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("打开分享页失败：{e}")))?;
        let field = |name: &str| -> Option<String> {
            let key = format!("\"{name}\":");
            let start = html.find(&key)? + key.len();
            let rest = &html[start..];
            let rest = rest.trim_start();
            if let Some(stripped) = rest.strip_prefix('"') {
                let end = stripped.find('"')?;
                Some(stripped[..end].to_string())
            } else {
                let end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                Some(rest[..end].to_string())
            }
        };
        let shareid = field("shareid")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::new("没有从分享页里找到分享 ID，请检查链接"))?;
        let uk = field("share_uk")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::new("没有从分享页里找到分享者，请检查链接"))?;
        let owner = field("linkusername")
            .or_else(|| field("username"))
            .unwrap_or_default();
        let avatar = field("share_photo")
            .or_else(|| field("photo"))
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string());
        let owner_uk = field("share_uk").unwrap_or_default();
        let my_uk = field("uk").unwrap_or_default();
        Ok(SharePage {
            shareid,
            uk,
            owner,
            avatar,
            owner_uk,
            my_uk,
        })
    }

    /// Raw HTML of a share page, for the live probes.
    #[cfg(test)]
    pub async fn debug_share_page(&self, surl: &str) -> AppResult<String> {
        let surl = if surl.starts_with('1') {
            surl.to_string()
        } else {
            format!("1{surl}")
        };
        let url = format!("{BASE}/s/{surl}");
        let cookie = self.cookie().await;
        let res = self
            .http()?
            .get(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .send()
            .await
            .map_err(|e| AppError::new(format!("打开分享页失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("打开分享页失败：{e}")))?;
        Ok(format!("[{status}] {text}"))
    }

    /// Exchanges the passcode for `BDCLND`, which every later share call needs.
    ///
    /// The value is percent-encoded and must stay that way for `Cookie:`, but
    /// the `sekey` parameter wants it decoded once and then re-encoded.
    async fn share_verify(&self, surl: &str, passcode: &str, page: &SharePage) -> AppResult<String> {
        // A share without a passcode rejects `verify` outright (`errno: 105`),
        // and `/share/list` is happy without the cookie it would have set.
        if passcode.trim().is_empty() {
            let _ = page;
            return Ok(String::new());
        }
        let url = format!(
            "{BASE}/share/verify?surl={surl}&t={}&channel=chunlei&web=1&app_id=250528&clienttype=0",
            chrono::Utc::now().timestamp_millis()
        );
        let form = [
            ("pwd", passcode.to_string()),
            ("vcode", String::new()),
            ("vcode_str", String::new()),
        ];
        let cookie = self.cookie().await;
        let res = self
            .http()?
            .post(&url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .header(
                reqwest::header::REFERER,
                format!("{BASE}/share/init?surl={surl}"),
            )
            .form(&form)
            .send()
            .await
            .map_err(|e| AppError::new(format!("校验提取码失败：{e}")))?;
        // The share cookie arrives in `Set-Cookie`, the value in the body.
        let from_header = res
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| {
                let rest = v.trim_start_matches(|c: char| c != 'B');
                rest.strip_prefix("BDCLND=")
                    .map(|tail| tail.split(';').next().unwrap_or_default().to_string())
            });
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("校验提取码失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let errno = value.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
        if errno != 0 {
            let message = value
                .get("show_msg")
                .or_else(|| value.get("errmsg"))
                .and_then(|v| v.as_str())
                .unwrap_or("提取码不正确");
            return Err(AppError::new(format!("百度网盘：{message}")));
        }
        let randsk = value
            .get("randsk")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let sekey = from_header
            .or_else(|| Some(randsk).filter(|v| !v.is_empty()))
            .unwrap_or_default();
        if sekey.is_empty() {
            // Public shares hand out no cookie at all; an empty sekey is fine.
            return Ok(String::new());
        }
        let _ = page;
        Ok(sekey)
    }

    /// GET against the share host with the share cookie attached.
    async fn share_get(&self, url: &str, sekey: &str) -> AppResult<Value> {
        let cookie = self.share_cookie(sekey).await;
        let res = self
            .http()?
            .get(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::REFERER, BASE)
            .send()
            .await
            .map_err(|e| AppError::new(format!("读取分享失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取分享失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let snippet: String = text.chars().take(160).collect();
            json!({ "errno": -1, "errmsg": format!("HTTP {}：{}", status.as_u16(), snippet) })
        });
        check_errno(&value, "读取分享")?;
        Ok(value)
    }

    /// POST form against the share host with the share cookie attached.
    async fn share_post_form(
        &self,
        url: &str,
        form: &[(&str, String)],
        sekey: &str,
        referer: &str,
    ) -> AppResult<Value> {
        self.share_post_form_raw(url, form, sekey, referer, true).await
    }

    /// Same as `share_post_form`, but the caller inspects `errno` itself.
    async fn share_post_form_raw(
        &self,
        url: &str,
        form: &[(&str, String)],
        sekey: &str,
        referer: &str,
        checked: bool,
    ) -> AppResult<Value> {
        let cookie = self.share_cookie(sekey).await;
        let res = self
            .http()?
            .post(url)
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::REFERER, referer)
            .header(reqwest::header::ORIGIN, BASE)
            .header("X-Requested-With", "XMLHttpRequest")
            .form(form)
            .send()
            .await
            .map_err(|e| AppError::new(format!("提交分享请求失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("提交分享请求失败：{e}")))?;
        let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            let snippet: String = text.chars().take(160).collect();
            json!({ "errno": -1, "errmsg": format!("HTTP {}：{}", status.as_u16(), snippet) })
        });
        if checked {
            check_errno(&value, "提交分享请求")?;
        }
        Ok(value)
    }

    /// Account cookie plus `BDCLND`, which is scoped to the share host.
    async fn share_cookie(&self, sekey: &str) -> String {
        let cookie = self.cookie().await;
        if sekey.is_empty() {
            return cookie;
        }
        format!("{cookie}; BDCLND={sekey}")
    }
}

/// Maps Baidu's `errno` onto our error type.
fn check_errno(value: &Value, what: &str) -> AppResult<()> {
    let errno = value.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
    if errno == 0 {
        return Ok(());
    }
    let message = value
        .get("errmsg")
        .or_else(|| value.get("error_msg"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // -6 = not logged in, 111 = token expired, 20016/20017 = token problems.
    if matches!(errno, -6 | 111 | 20016 | 20017) {
        return Err(AppError::new("百度网盘登录已过期，请重新登录"));
    }
    if message.is_empty() {
        Err(AppError::new(format!("{what}失败（errno={errno}）")))
    } else {
        Err(AppError::new(format!("{what}失败：{message}（errno={errno}）")))
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" || dir.is_empty() {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Minimal percent-encoding for query values.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 3);
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[async_trait]
impl DriveProvider for BaiduProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Baidu
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            share_open: true,
            share_create: false,
            move_files: true,
            copy: true,
            search: true,
            rename: true,
            folders: true,
        }
    }

    async fn auth_state(&self) -> AuthState {
        let creds = self.creds.read().await;
        AuthState {
            provider: ProviderId::Baidu,
            authenticated: !creds.cookie.trim().is_empty(),
            source: "file".to_string(),
            masked: Some(mask_cookie(&creds.cookie)),
            credential_path: self.credential_path.clone(),
            nickname: Some(creds.nickname.clone()).filter(|n| !n.is_empty()),
            // Remote URL until `profile()` inlines it; the CSP forbids remote
            // images, so only an already-inlined one is reported here.
            avatar: Some(creds.avatar.clone()).filter(|a| a.starts_with("data:")),
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        {
            let mut creds = self.creds.write().await;
            creds.cookie.clear();
            creds.bdstoken.clear();
        }
        self.paths.write().await.clear();
        let path = Path::new(&self.credential_path);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let creds = self.creds.read().await;
        if creds.cookie.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccountInfo {
            nickname: creds.nickname.clone(),
            avatar: Some(creds.avatar.clone()).filter(|a| a.starts_with("data:")),
        }))
    }

    /// Inlines the avatar URL the login flow already stored.
    ///
    /// `baidu.json` carries `avatar` (`https://himg.bdimg.com/sys/portrait/…`)
    /// from the web login, but the CSP blocks remote images, so it has to be
    /// turned into a `data:` URL before the sidebar can use it.
    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        let (nickname, avatar) = {
            let creds = self.creds.read().await;
            (creds.nickname.clone(), creds.avatar.clone())
        };
        if avatar.starts_with("data:") || avatar.is_empty() {
            return self.account().await;
        }
        let Some(inlined) = crate::share::fetch_avatar(&self.http()?, &avatar).await else {
            return self.account().await;
        };
        {
            let mut creds = self.creds.write().await;
            creds.avatar = inlined.clone();
        }
        if !self.credential_path.is_empty() {
            if let Some(dir) = Path::new(&self.credential_path).parent() {
                if let Err(err) = self.persist(dir).await {
                    crate::ylog!("baidu avatar persist failed: {err}");
                }
            }
        }
        Ok(Some(AccountInfo {
            nickname,
            avatar: Some(inlined),
        }))
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let path = self.dir_path(dir).await?;
        let mut entries: Vec<EntryDto> = Vec::new();
        let mut start = 0u32;
        let mut total = 0u32;
        loop {
            let (chunk, reported) = self.list_page(&path, start).await?;
            total = total.max(reported);
            let got = chunk.len() as u32;
            entries.extend(chunk);
            // The web endpoint stops answering with rows once the folder is
            // exhausted, so an empty page is the real terminator.
            if got == 0 || start >= 200 * PAGE_LIMIT {
                break;
            }
            start += got;
        }
        let count = entries.len() as u32;
        super::sort_entries(&mut entries);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Baidu, &Self::encode_path(&path)),
            entries,
            total: total.max(count),
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        let target = self.dir_path(dir).await?;
        if target == "/" {
            return Ok(Vec::new());
        }
        // Walk down from the root one level at a time, which is the only way to
        // learn the fs_id of each intermediate folder.
        let mut chain: Vec<PathNode> = Vec::new();
        let mut current = String::new();
        for part in target.trim_matches('/').split('/') {
            if part.is_empty() {
                continue;
            }
            let parent = if current.is_empty() {
                "/".to_string()
            } else {
                current.clone()
            };
            let listing = self.list(&Self::encode_path(&parent)).await?;
            let Some(entry) = listing.entries.into_iter().find(|e| e.name == part) else {
                // The folder was renamed or deleted since the id was minted;
                // show what we know rather than an empty trail.
                break;
            };
            chain.push(PathNode {
                fid: entry.fid.clone(),
                name: entry.name.clone(),
            });
            current = join_path(&parent, part);
        }
        Ok(chain)
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = super::check_name(name)?;
        let parent_path = self.dir_path(parent).await?;
        let path = join_path(&parent_path, name);
        let token = self.bdstoken().await?;
        let url = format!("{BASE}/api/create?a=commit&bdstoken={token}&web=1");
        let value = self
            .post_form(
                &url,
                &[
                    ("path", path.clone()),
                    ("isdir", "1".to_string()),
                    ("size", "0".to_string()),
                    ("block_list", "[]".to_string()),
                    ("rtype", "3".to_string()),
                ],
                "新建文件夹",
            )
            .await?;
        let fid = value
            .get("fs_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AppError::new("百度网盘没有返回新建文件夹的 ID"))?;
        self.remember(&fid.to_string(), &path).await;
        Ok(qualify(ProviderId::Baidu, &fid.to_string()))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = super::check_name(name)?;
        let path = self.path_of(fid).await?;
        self.manage("rename", json!([{ "path": path, "newname": name }]))
            .await?;
        // Keep the cache honest so a second operation does not hit the old path.
        self.paths.write().await.remove(fid);
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let mut paths: Vec<Value> = Vec::with_capacity(fids.len());
        for fid in fids {
            paths.push(Value::String(self.path_of(fid).await?));
        }
        self.manage("delete", Value::Array(paths)).await?;
        let mut cache = self.paths.write().await;
        for fid in fids {
            cache.remove(fid);
        }
        Ok(())
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let dest = self.dir_path(to).await?;
        let mut list: Vec<Value> = Vec::with_capacity(fids.len());
        for fid in fids {
            let path = self.path_of(fid).await?;
            let name = path.rsplit('/').next().unwrap_or_default().to_string();
            list.push(json!({ "path": path, "dest": dest, "newname": name }));
        }
        self.manage(if copy { "copy" } else { "move" }, Value::Array(list))
            .await?;
        if copy {
            // A copy gets brand new `fs_id`s that we have never seen, and the
            // drive picks the final name (it appends "(1)" when taken), so the
            // only way to learn them is to re-read the destination folder.
            let _ = self.list(&Self::encode_path(&dest)).await;
        } else {
            let mut cache = self.paths.write().await;
            for fid in fids {
                cache.remove(fid);
            }
        }
        Ok(fids.len())
    }

    async fn search(&self, dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // The web API has no search endpoint for a plain cookie session, so walk
        // the tree like the other drives do.
        let mut hits = Vec::new();
        let mut queue: std::collections::VecDeque<(String, Vec<PathNode>)> =
            std::collections::VecDeque::new();
        queue.push_back((dir.to_string(), Vec::new()));
        let mut visited = 0;
        while let Some((parent, path)) = queue.pop_front() {
            visited += 1;
            if visited > 80 || hits.len() >= limit as usize {
                break;
            }
            // A transient failure on one folder must not abort the whole walk,
            // but it must not be invisible either — an empty result looks like
            // "no matches" when it really means "the listing broke".
            let listing = match self.list(&parent).await {
                Ok(listing) => listing,
                Err(err) => {
                    crate::ylog!("baidu: search skipped {parent}: {err}");
                    continue;
                }
            };
            for entry in listing.entries {
                if entry.name.to_lowercase().contains(&needle) {
                    // The hit is the last node of the chain; the UI renders
                    // `path.slice(0, -1)` as its folder.
                    let mut hit_path = path.clone();
                    hit_path.push(PathNode {
                        fid: entry.fid.clone(),
                        name: entry.name.clone(),
                    });
                    hits.push(SearchHit {
                        entry: entry.clone(),
                        path: hit_path,
                    });
                    if hits.len() >= limit as usize {
                        break;
                    }
                }
                if entry.is_dir && path.len() < 8 {
                    let mut next = path.clone();
                    next.push(PathNode {
                        fid: entry.fid.clone(),
                        name: entry.name.clone(),
                    });
                    match self.path_of(&entry.fid).await {
                        Ok(child) => queue.push_back((Self::encode_path(&child), next)),
                        Err(err) => crate::ylog!("baidu: search cannot resolve {}: {err}", entry.fid),
                    }
                }
            }
        }
        Ok(hits)
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let path = self.path_of(fid).await?;
        let (dlink, canonical) = self.file_meta(&path).await?;
        if canonical != path {
            self.remember(fid, &canonical).await;
        }
        let url = self.resolve_dlink(&dlink).await;
        let client = self.http()?;
        let mut request = client
            .get(&url)
            .header(reqwest::header::USER_AGENT, DL_UA)
            .header(reqwest::header::COOKIE, self.cookie().await);
        if start > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let res = request
            .send()
            .await
            .map_err(|e| AppError::new(format!("下载失败：{e}")))?;
        let status = res.status();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(format!("下载失败（HTTP {}）", status.as_u16())));
        }
        // A CDN that ignores `Range` answers 200 with the whole body; appending
        // that to an existing `.part` would corrupt the file, so refuse and let
        // the caller restart from zero.
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

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        use tokio::io::AsyncReadExt;

        let size = std::fs::metadata(local)
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?
            .len();
        use md5::Digest as _;
        // Baidu refuses empty files outright.
        if size == 0 {
            return Err(AppError::new("百度网盘不允许上传空文件"));
        }
        let parent_path = self.dir_path(parent).await?;
        let path = join_path(&parent_path, name);

        // 1. Hash every slice (the API wants the md5 of each), plus the whole
        //    file and the first 256 KiB.
        sink.phase("hashing");
        let mut file = tokio::fs::File::open(local).await?;
        let mut whole = md5::Md5::new();
        let mut slice_hash = md5::Md5::new();
        let mut blocks: Vec<String> = Vec::new();
        let mut first = Vec::with_capacity(SLICE_MD5_SIZE);
        let mut hashed = 0u64;
        loop {
            let mut buf = vec![0u8; SLICE_SIZE];
            let mut filled = 0usize;
            while filled < SLICE_SIZE {
                let n = tokio::select! {
                    biased;
                    _ = token.cancelled() => return Err(AppError::new("已取消")),
                    n = file.read(&mut buf[filled..]) => n?,
                };
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let chunk = &buf[..filled];
            if first.len() < SLICE_MD5_SIZE {
                let want = SLICE_MD5_SIZE - first.len();
                first.extend_from_slice(&chunk[..want.min(chunk.len())]);
            }
            whole.update(chunk);
            slice_hash.update(chunk);
            blocks.push(format!("{:x}", slice_hash.finalize_reset()));
            hashed += filled as u64;
            sink.progress(hashed);
        }
        if hashed != size {
            return Err(AppError::new(format!(
                "读取本地文件不完整（{hashed} / {size} 字节）"
            )));
        }
        let content_md5 = format!("{:x}", whole.finalize());
        let slice_md5 = {
            let mut hasher = md5::Md5::new();
            hasher.update(&first);
            format!("{:x}", hasher.finalize())
        };
        let blocks_json = serde_json::to_string(&blocks).unwrap_or_else(|_| "[]".to_string());

        // 2. Ask for an upload session; `return_type == 2` means the server
        //    already had the file (秒传). The parameters ride in the form body:
        //    `block_list` alone can exceed a query string's practical length on
        //    big files.
        let token_web = self.bdstoken().await?;
        let precreate = format!("{BASE}/api/precreate?bdstoken={token_web}&web=1");
        let value = self
            .post_form(
                &precreate,
                &[
                    ("path", path.clone()),
                    ("size", size.to_string()),
                    ("isdir", "0".to_string()),
                    ("autoinit", "1".to_string()),
                    ("rtype", "3".to_string()),
                    ("block_list", blocks_json.clone()),
                    ("content-md5", content_md5.clone()),
                    ("slice-md5", slice_md5.clone()),
                ],
                "申请上传会话",
            )
            .await
            .map_err(|err| AppError::new(format!("{err}（登录可能已过期）")))?;
        if value.get("return_type").and_then(|v| v.as_i64()) == Some(2) {
            sink.progress(size);
            let fid = value
                .get("fs_id")
                .and_then(|v| v.as_i64())
                .ok_or_else(|| AppError::new("百度网盘秒传成功但没有返回文件 ID"))?;
            self.remember(&fid.to_string(), &path).await;
            return Ok(Some(qualify(ProviderId::Baidu, &fid.to_string())));
        }
        let upload_id = value
            .get("uploadid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if upload_id.is_empty() {
            return Err(AppError::new("百度网盘没有返回上传会话"));
        }

        // 3. Upload each slice as one multipart part.
        sink.phase("uploading");
        let client = self.http()?;
        let cookie = self.cookie().await;
        let mut file = tokio::fs::File::open(local).await?;
        let mut sent = 0u64;
        for (index, _) in blocks.iter().enumerate() {
            if token.is_cancelled() {
                return Err(AppError::new("已取消"));
            }
            let mut buf = vec![0u8; SLICE_SIZE];
            let mut filled = 0usize;
            while filled < SLICE_SIZE {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            buf.truncate(filled);
            let url = format!(
                "{UPLOAD_HOST}/rest/2.0/pcs/superfile2?method=upload&app_id=250528&channel=chunlei&clienttype=0&web=1&type=tmpfile&path={}&uploadid={upload_id}&partseq={index}",
                urlencode(&path)
            );
            let part = reqwest::multipart::Part::bytes(buf)
                .file_name(name.to_string())
                .mime_str("application/octet-stream")
                .map_err(|e| AppError::new(format!("构造上传分片失败：{e}")))?;
            let form = reqwest::multipart::Form::new().part("file", part);
            let res = client
                .post(&url)
                .header(reqwest::header::COOKIE, cookie.clone())
                .header(reqwest::header::USER_AGENT, UA)
                .multipart(form)
                .send()
                .await
                .map_err(|e| AppError::new(format!("上传分片 {index} 失败：{e}")))?;
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(AppError::new(format!(
                    "上传分片 {index} 失败（HTTP {}）：{}",
                    status.as_u16(),
                    text.chars().take(120).collect::<String>()
                )));
            }
            let lower = text.to_lowercase();
            if lower.contains("uploadid")
                && (lower.contains("invalid") || lower.contains("expired") || lower.contains("not found"))
            {
                return Err(AppError::new("百度网盘上传会话已过期，请重试"));
            }
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                let errno = value.get("error_code").and_then(|v| v.as_i64()).unwrap_or(0);
                if errno != 0 {
                    return Err(AppError::new(format!(
                        "上传分片 {index} 失败（error_code={errno}）"
                    )));
                }
            }
            sent += filled as u64;
            sink.progress(sent);
        }

        // 4. Commit. Like `precreate`, the payload rides in the body: a large
        //    `block_list` does not fit comfortably in a query string.
        let create = format!("{BASE}/api/create?a=commit&bdstoken={token_web}&web=1");
        let value = self
            .post_form(
                &create,
                &[
                    ("path", path.clone()),
                    ("size", size.to_string()),
                    ("isdir", "0".to_string()),
                    ("rtype", "3".to_string()),
                    ("uploadid", upload_id.clone()),
                    ("block_list", blocks_json.clone()),
                ],
                "提交上传",
            )
            .await?;
        let fid = value
            .get("fs_id")
            .and_then(|v| v.as_i64())
            .ok_or_else(|| AppError::new("百度网盘没有返回上传文件的 ID"))?;
        self.remember(&fid.to_string(), &path).await;
        Ok(Some(qualify(ProviderId::Baidu, &fid.to_string())))
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        let url = format!("{BASE}/api/quota?checkfree=1&checkexpire=1&web=1");
        let Ok(value) = self.get_json(&url, "读取容量").await else {
            return Ok(None);
        };
        let total = value.get("total").and_then(|v| v.as_u64());
        let used = value
            .get("used")
            .and_then(|v| v.as_u64())
            .or_else(|| {
                let free = value.get("free").and_then(|v| v.as_u64())?;
                Some(total?.saturating_sub(free))
            });
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(QuotaInfo { used, total })),
            _ => Ok(None),
        }
    }

    // ---------------------------------------------------------- shares ---

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let page = self.share_page(id).await?;
        let sekey = self.share_verify(id, passcode, &page).await?;
        let info = self
            .share_get(
                &format!(
                    "{BASE}/share/list?uk={}&shareid={}&order=time&desc=1&showempty=0&web=1\
                     &page=1&num=100&dir=%2F&channel=chunlei&app_id=250528&clienttype=0&root=1",
                    page.uk, page.shareid
                ),
                &sekey,
            )
            .await?;
        let first = info
            .get("list")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first());
        let first_fid = first
            .and_then(|v| v.get("fs_id"))
            .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|i| i.to_string())));
        let title = info
            .get("title")
            .and_then(|v| v.as_str())
            .map(|v| v.trim_start_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| first.and_then(|v| v.get("server_filename")).and_then(|v| v.as_str()).map(|v| v.to_string()))
            .unwrap_or_default();
        let total = info.get("cur_total").and_then(|v| v.as_u64()).unwrap_or(0);
        Ok(ShareSession {
            provider: ProviderId::Baidu.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            // The verified share cookie; every later call replays it.
            stoken: sekey,
            title,
            owner: page.owner.clone(),
            owner_avatar: page.avatar.clone(),
            file_num: total,
            total_size: 0,
            expired_at: 0,
            first_fid,
            // Saving works for other people's shares, but Baidu refuses to save
            // a share back into the account that created it.
            can_save: page.owner_uk != page.my_uk,
            web_url: Some(format!("https://pan.baidu.com/s/1{}", id)),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<ShareListing> {
        let page = self.share_page(&share.pwd_id).await?;
        let parent = if dir.is_empty() { "/" } else { dir };
        let root = if parent == "/" { "&root=1" } else { "" };
        let mut entries = Vec::new();
        let mut page_no = 1u32;
        let mut total = 0u64;
        loop {
            let url = format!(
                "{BASE}/share/list?uk={}&shareid={}&order=time&desc=1&showempty=0&web=1\
                 &page={page_no}&num=100&dir={}{root}&channel=chunlei&app_id=250528&clienttype=0",
                page.uk,
                page.shareid,
                urlencode(parent),
            );
            let value = self.share_get(&url, &share.stoken).await?;
            let rows = value
                .get("list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            total = value
                .get("cur_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(total);
            for row in &rows {
                let fid = row
                    .get("fs_id")
                    .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|i| i.to_string())))
                    .unwrap_or_default();
                let name = row
                    .get("server_filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let is_dir = row
                    .get("isdir")
                    .map(|v| v.as_str() == Some("1") || v.as_i64() == Some(1))
                    .unwrap_or(false);
                // `/share/list` addresses folders by path, not by id, so a
                // folder entry carries its path in `fid` and drill-down works
                // without the caller knowing that.
                let fid = if is_dir {
                    row.get("path")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string())
                        .unwrap_or(fid)
                } else {
                    fid
                };
                entries.push(ShareEntry {
                    fid,
                    token: String::new(),
                    name,
                    size: row
                        .get("size")
                        .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
                        .unwrap_or(0),
                    is_dir,
                    updated_at: row
                        .get("server_mtime")
                        .and_then(|v| v.as_str().and_then(|s| s.parse::<u64>().ok()).or_else(|| v.as_u64()))
                        .map(|s| (s * 1000) as i64)
                        .unwrap_or(0),
                });
            }
            page_no += 1;
            if rows.is_empty() || page_no > 50 || (total > 0 && (entries.len() as u64) >= total) {
                break;
            }
        }
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        if total == 0 {
            total = entries.len() as u64;
        }
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
        let page = self.share_page(&share.pwd_id).await?;
        let token = self.bdstoken().await?;
        let target = match to {
            Some(dir) if !dir.is_empty() && dir != ROOT => self.dir_path(dir).await?,
            _ => "/".to_string(),
        };
        let fids: Vec<String> = entries
            .iter()
            .map(|e| format!("\"{}\"", e.fid))
            .collect();
        let url = format!(
            "{BASE}/share/transfer?shareid={}&from={}&sekey={}&ondup=newcopy&async=1\
             &channel=chunlei&web=1&app_id=250528&bdstoken={token}&clienttype=0",
            page.shareid,
            page.uk,
            urlencode(&crate::share::percent_decode(&share.stoken)),
        );
        let form = [
            ("fsidlist", format!("[{}]", fids.join(","))),
            ("path", target),
        ];
        // `errno: 2` is Baidu's catch-all for "cannot save this": the usual cause
        // is a share created by the logged-in account itself, so this call has
        // to inspect the error itself instead of letting `check_errno` flatten it.
        let value = self
            .share_post_form_raw(
                &url,
                &form,
                &share.stoken,
                &format!("{BASE}/s/1{}", share.pwd_id),
                false,
            )
            .await?;
        let errno = value.get("errno").and_then(|v| v.as_i64()).unwrap_or(0);
        if errno == 2 {
            return Err(AppError::new(
                "百度网盘无法转存：不能转存自己分享的文件，或文件已存在",
            ));
        }
        check_errno(&value, "转存")?;
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
            if dir.is_empty() { "/".to_string() } else { dir.to_string() },
            std::path::PathBuf::new(),
            0usize,
        ));
        while let Some((fid, prefix, depth)) = queue.pop_front() {
            if out.len() >= MAX_FILES {
                return Err(AppError::new("文件夹内文件过多（超过 5000 个），请分次下载"));
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
                    // Folder ids are already the full path (see `share_list`).
                    queue.push_back((entry.fid.clone(), next, depth + 1));
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
        // Baidu only hands out a direct link for files below ~52 MB: above that
        // `/api/sharedownload` returns an encrypted blob and the batch link
        // answers `error_code: 31090 "package is too large"`. Saving to the
        // drive first is the only way to fetch big shared files.
        let page = self.share_page(&share.pwd_id).await?;
        let token = self.bdstoken().await?;
        let config = self
            .share_get(
                &format!(
                    "{BASE}/share/tplconfig?surl=1{}&fields=sign,timestamp&view_mode=1\
                     &channel=chunlei&web=1&app_id=250528&bdstoken={token}&clienttype=0",
                    share.pwd_id
                ),
                &share.stoken,
            )
            .await?;
        let sign = config
            .pointer("/data/sign")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let timestamp = config
            .pointer("/data/timestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if sign.is_empty() {
            return Err(AppError::new("百度网盘没有返回分享签名，请稍后重试"));
        }
        let extra = serde_json::json!({
            "sekey": crate::share::percent_decode(&share.stoken)
        })
        .to_string();
        let url = format!(
            "{BASE}/api/sharedownload?sign={sign}&timestamp={timestamp}\
             &channel=chunlei&web=1&app_id=250528&clienttype=0"
        );
        let form = [
            ("encrypt", "0".to_string()),
            ("product", "share".to_string()),
            ("uk", page.uk.clone()),
            ("primaryid", page.shareid.clone()),
            ("fid_list", format!("[{fid}]")),
            ("extra", extra),
            ("vip", "0".to_string()),
            ("type", "dlink".to_string()),
        ];
        let value = self
            .share_post_form(&url, &form, &share.stoken, &format!("{BASE}/s/{}", share.pwd_id))
            .await?;
        let item = value
            .get("list")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first());
        let dlink = item
            .and_then(|v| v.get("dlink"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if dlink.is_empty() {
            return Err(AppError::new(
                "百度网盘只对小于 52 MB 的分享文件提供直链，请先转存到我的网盘再下载",
            ));
        }
        let size = item
            .and_then(|v| v.get("size"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        Ok(ShareLink {
            url: dlink,
            headers: vec![
                ("Referer".to_string(), format!("{BASE}/s/{}", share.pwd_id)),
                ("User-Agent".to_string(), DL_UA.to_string()),
            ],
            size,
            cleanup_fid: None,
        })
    }
}

/// Shows only the shape of a cookie header, never its contents.
fn mask_cookie(cookie: &str) -> String {
    let bduss = cookie
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| name.eq_ignore_ascii_case("BDUSS"))
        .map(|(_, value)| value)
        .unwrap_or_default();
    if bduss.is_empty() {
        return "已保存".to_string();
    }
    let tail: String = bduss
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("BDUSS=…{tail}")
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use futures_util::StreamExt;

    /// First chunk of a download stream.
    async fn first_chunk(mut handle: DownloadHandle) -> bytes::Bytes {
        handle
            .stream
            .next()
            .await
            .expect("stream ended")
            .expect("chunk")
    }

    /// End-to-end check against a real account.
    ///
    /// Reads the cookie header from `BAIDU_COOKIE` so nothing is committed, then
    /// exercises exactly what the app does after a web login: prove the cookie,
    /// persist it, list the root, open a sub-folder, and read the quota.
    #[tokio::test]
    #[ignore]
    async fn baidu_live_login_listing_and_quota() {
        let Ok(cookie) = std::env::var("BAIDU_COOKIE") else {
            println!("BAIDU_COOKIE not set, skipping");
            return;
        };
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = BaiduProvider::save_cookie(&dir, &cookie)
            .await
            .expect("save_cookie");
        println!("account: {:?}", drive.account().await);

        let root = drive.list("0").await.expect("root listing");
        println!("root: {} entries", root.entries.len());
        let mut folder = None;
        for entry in root.entries.iter().take(10) {
            println!("  {} dir={} size={}", entry.name, entry.is_dir, entry.size);
            if entry.is_dir && folder.is_none() {
                folder = Some(crate::providers::native_of(&entry.fid));
            }
        }

        // A *fresh* provider has an empty `fid -> path` cache, which is the
        // state after a restart: the id must still resolve.
        let restarted = BaiduProvider::load(&dir).expect("reload");
        let id = folder.expect("a folder in the root");
        let child = restarted.list(&id).await.expect("sub listing after restart");
        println!("subfolder {} -> {} entries", id, child.entries.len());
        for entry in child.entries.iter().take(5) {
            println!("   - {} dir={}", entry.name, entry.is_dir);
        }

        let quota = drive.quota().await.expect("quota");
        println!("quota: {quota:?}");

        // Download: the dlink must be usable without `origin=dlna`, and must
        // honour `Range` (the transfer engine relies on 206 for resume).
        let file = restarted
            .list(&id)
            .await
            .expect("listing")
            .entries
            .into_iter()
            .find(|entry| !entry.is_dir && entry.size > 1024)
            .expect("a file big enough to slice");
        let native = crate::providers::native_of(&file.fid);
        let first = first_chunk(drive.open_download(&native, 0).await.expect("download")).await;
        println!("download: {} bytes, first={:?}", first.len(), &first[..4.min(first.len())]);

        let second = first_chunk(
            drive
                .open_download(&native, 100)
                .await
                .expect("resumed download"),
        )
        .await;
        println!("resumed at 100: {} bytes", second.len());

        assert!(!root.entries.is_empty(), "root listing was empty");
        assert!(quota.is_some(), "quota missing");
        assert!(!first.is_empty(), "download returned nothing");
        assert!(!second.is_empty(), "resumed download returned nothing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_directory_ids() {
        assert_eq!(BaiduProvider::encode_path("/"), ROOT);
        // The leading slash stays: dropping it would make a top-level folder
        // named `0` indistinguishable from the root.
        assert_eq!(BaiduProvider::encode_path("/电影"), "/电影");
        assert_eq!(BaiduProvider::encode_path("电影"), "/电影");
        assert_eq!(BaiduProvider::decode_dir(ROOT), "/");
        assert_eq!(BaiduProvider::decode_dir("电影"), "/电影");
        // Already a path: leave it alone.
        assert_eq!(BaiduProvider::decode_dir("/电影"), "/电影");
    }

    #[test]
    fn joins_paths_without_doubling_slashes() {
        assert_eq!(join_path("/", "a"), "/a");
        assert_eq!(join_path("/a", "b"), "/a/b");
        assert_eq!(join_path("", "b"), "/b");
    }

    #[test]
    fn urlencodes_quotes_and_spaces() {
        assert_eq!(urlencode("[\"a b\"]"), "%5B%22a%20b%22%5D");
        assert_eq!(urlencode("plain-1_2.3~"), "plain-1_2.3~");
    }

    #[test]
    fn maps_expired_sessions_to_a_login_hint() {
        let expired = json!({ "errno": -6, "errmsg": "用户未登录" });
        let err = check_errno(&expired, "列出").unwrap_err();
        assert!(err.to_string().contains("重新登录"), "{err}");

        let ok = json!({ "errno": 0, "list": [] });
        assert!(check_errno(&ok, "列出").is_ok());

        let other = json!({ "errno": 31034, "errmsg": "请求过于频繁" });
        let err = check_errno(&other, "列出").unwrap_err();
        assert!(err.to_string().contains("请求过于频繁"), "{err}");
    }

    #[test]
    fn masks_the_cookie_without_leaking_it() {
        let masked = mask_cookie("BDUSS=abcdefgh1234; STOKEN=zzz");
        assert!(masked.starts_with("BDUSS=…"), "{masked}");
        assert!(masked.ends_with("1234"), "{masked}");
        assert!(!masked.contains("abcdefgh"), "{masked}");
        assert_eq!(mask_cookie("STOKEN=zzz"), "已保存");
    }

}

#[cfg(test)]
mod live_upload {
    use super::*;
    use crate::providers::UploadSink;
    use std::io::Write;

    struct Recorder {
        last: std::sync::atomic::AtomicU64,
    }

    impl UploadSink for Recorder {
        fn phase(&self, _phase: &str) {}
        fn progress(&self, bytes: u64) {
            self.last.store(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Uploads a small file end to end, then deletes it again.
    ///
    /// Baidu's upload is the most intricate of the five drives (precreate →
    /// per-slice PUT to `d.pcs.baidu.com` → commit), so a live check matters more
    /// here than anywhere else.
    #[tokio::test]
    #[ignore]
    async fn baidu_live_upload_and_delete() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let Ok(drive) = BaiduProvider::load(&dir) else {
            println!("no baidu credentials, skipping");
            return;
        };
        if !drive.auth_state().await.authenticated {
            println!("not logged in, skipping");
            return;
        }

        let name = format!("yunyu-upload-test-{}.txt", std::process::id());
        let local = std::env::temp_dir().join(&name);
        let body: String = (0..2000).map(|i| format!("line {i}\n")).collect();
        std::fs::File::create(&local)
            .and_then(|mut f| f.write_all(body.as_bytes()))
            .expect("write local file");

        let sink = std::sync::Arc::new(Recorder {
            last: std::sync::atomic::AtomicU64::new(0),
        });
        let token = tokio_util::sync::CancellationToken::new();
        let fid = drive
            .upload(&local, "/", &name, sink.clone(), token)
            .await
            .expect("upload");
        println!(
            "uploaded {name} -> {fid:?}, progress={}",
            sink.last.load(std::sync::atomic::Ordering::Relaxed)
        );

        let listing = drive.list("0").await.expect("listing after upload");
        let found = listing
            .entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("uploaded file missing from listing: {name}"));
        println!("listed back: {} size={}", found.name, found.size);
        assert_eq!(found.size as usize, body.len());

        // Download must return the very bytes we uploaded, and a resumed read
        // must start where it was asked to.
        let native = crate::providers::native_of(&found.fid);
        {
            use futures_util::StreamExt;
            let mut handle = drive.open_download(&native, 0).await.expect("download");
            let first = handle.stream.next().await.expect("chunk").expect("bytes");
            let head = String::from_utf8_lossy(&first[..first.len().min(12)]).to_string();
            println!("download: {} bytes, head={head:?}", first.len());
            assert!(body.starts_with(head.trim_end()), "download does not match upload");
            drop(handle);
            let mut resumed = drive.open_download(&native, 7).await.expect("resume");
            let chunk = resumed.stream.next().await.expect("chunk").expect("bytes");
            assert_eq!(&chunk[..4], &body.as_bytes()[7..11], "resume offset is wrong");
            println!("resume at 7: {} bytes", chunk.len());
        }

        // Move into a real folder and back: the filemanager endpoint shapes
        // differ per operation, so each one gets exercised at least once.
        let folders = drive.list("0").await.expect("root listing");
        if let Some(folder) = folders.entries.iter().find(|e| e.is_dir) {
            let target = crate::providers::native_of(&folder.fid);
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
            // Copy too: a different `opera` and a different `filelist` shape, so
            // a move working says nothing about a copy. The source sits inside
            // `folder` now, so a copy into the root must produce exactly one new
            // entry there — with a brand new `fs_id`.
            drive
                .transfer_entries(std::slice::from_ref(&native), "0", true)
                .await
                .expect("copy");
            let mut copy_id = String::new();
            for _ in 0..10 {
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
                let root = drive.list("0").await.expect("root listing after copy");
                if let Some(copied) = root.entries.iter().find(|e| e.name == name) {
                    copy_id = crate::providers::native_of(&copied.fid);
                    break;
                }
            }
            println!("copy in root: {copy_id:?}");
            assert!(!copy_id.is_empty(), "copy never showed up in the root");
            assert_ne!(copy_id, native, "copy reused the source id");
            drive.delete(&[copy_id]).await.expect("cleanup copy");
            drive
                .transfer_entries(std::slice::from_ref(&native), "0", false)
                .await
                .expect("move back");
            println!("moved into {} and back", folder.name);
        }

        drive.delete(&[native]).await.expect("delete");
        println!("deleted again");
        let _ = std::fs::remove_file(&local);
    }
}

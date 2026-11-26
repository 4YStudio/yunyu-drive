//! 115 网盘驱动。
//!
//! 115 的网页接口（`webapi.115.com`）只认 cookie（`UID`/`CID`/`SEID`/`KID`），
//! 不像天翼那样有会话签名，所以列表、新建、改名、删除、移动、回收站都是直球。
//! 下载直链是另一回事：`proapi.115.com/app/chrome/downurl` 的请求体要
//! RSA + AES-ECB + LZ4 + CRC32 加密，那部分单独在 `crypto` 模块里做。

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    check_name, native_of, qualify, sort_entries, AuthState, Capabilities, DirListing,
    DownloadHandle, DriveProvider, EntryDto, PathNode, ProviderId, QuotaInfo, RecycleEntry,
    SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};
use crate::error::{AppError, AppResult};
use crate::share::AccountInfo;

const WEB: &str = "https://webapi.115.com";
const PRO: &str = "https://proapi.115.com";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const ROOT: &str = "0";
const PAGE: u32 = 200;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pan115Credentials {
    /// 网页版 cookie 原文（含 `UID=…; CID=…; SEID=…; KID=…`）。
    pub cookies: String,
    pub user_id: String,
    pub user_name: String,
    pub avatar: Option<String>,
}

impl Pan115Credentials {
    fn usable(&self) -> bool {
        self.cookies.contains("UID=") && self.cookies.contains("SEID=")
    }
}

pub struct Pan115Provider {
    creds: std::sync::RwLock<Pan115Credentials>,
    credential_path: String,
    client: OnceLock<reqwest::Client>,
    profile_tried: AtomicBool,
}

impl Pan115Provider {
    pub fn new(creds: Pan115Credentials, credential_path: String) -> Self {
        Self {
            creds: std::sync::RwLock::new(creds),
            credential_path,
            client: OnceLock::new(),
            profile_tried: AtomicBool::new(false),
        }
    }

    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let path = own_dir.join("pan115.json");
        let text = std::fs::read_to_string(&path).map_err(|_| AppError::new("尚未登录 115 网盘"))?;
        let creds: Pan115Credentials =
            serde_json::from_str(&text).map_err(|e| AppError::new(format!("115 凭据无效：{e}")))?;
        if !creds.usable() {
            return Err(AppError::new("尚未登录 115 网盘"));
        }
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    pub fn save_cookie(own_dir: &Path, cookie: &str) -> AppResult<Self> {
        let cookie = cookie.trim();
        if !cookie.contains("UID=") || !cookie.contains("SEID=") {
            return Err(AppError::new(
                "115 的 Cookie 需要同时包含 UID 与 SEID，请从浏览器里整条复制",
            ));
        }
        std::fs::create_dir_all(own_dir).ok();
        let path = own_dir.join("pan115.json");
        let creds = Pan115Credentials {
            cookies: cookie.to_string(),
            ..Default::default()
        };
        let text = serde_json::to_string_pretty(&creds)
            .map_err(|e| AppError::new(format!("保存 115 凭据失败：{e}")))?;
        std::fs::write(&path, text).map_err(|e| AppError::new(format!("保存 115 凭据失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    fn http(&self) -> AppResult<crate::net::Http> {
        if let Some(client) = self.client.get() {
            return Ok(crate::net::Http::new(client.clone(), "115pan"));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let _ = self.client.set(client.clone());
        Ok(crate::net::Http::new(client, "115pan"))
    }

    fn cookie(&self) -> String {
        self.creds
            .read()
            .map(|c| c.cookies.clone())
            .unwrap_or_default()
    }

    async fn persist(&self) {
        let text = {
            let creds = match self.creds.read() {
                Ok(creds) => creds.clone(),
                Err(_) => return,
            };
            match serde_json::to_string_pretty(&creds) {
                Ok(text) => text,
                Err(_) => return,
            }
        };
        let path = self.credential_path.clone();
        let _ = tokio::fs::write(&path, text).await;
        crate::config::restrict_permissions_pub(Path::new(&path));
    }

    async fn call(&self, method: &str, url: &str, form: Option<Vec<(String, String)>>) -> AppResult<Value> {
        self.call_full(method, url, form, "https://115.com/", UA)
            .await
    }

    /// 带自定义 `Referer` / `User-Agent` 的版本：分享接口两者都挑剔，
    /// `share/downurl` 只认 115 自家客户端的 UA。
    /// 115 的 `user_id`：cookie 里 `UID` 下划线前面那段数字
    /// （`UID=103693237_A1_1795524966` → `103693237`），凭据里存过就直接用。
    fn user_id(&self) -> String {
        let (stored, cookies) = {
            let creds = self.creds.read().unwrap();
            (creds.user_id.clone(), creds.cookies.clone())
        };
        let stored = stored.trim().to_string();
        if !stored.is_empty() {
            return stored;
        }
        cookies
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
            .find(|(key, _)| key.eq_ignore_ascii_case("UID"))
            .map(|(_, value)| value.split('_').next().unwrap_or_default().to_string())
            .unwrap_or_default()
    }

    async fn call_full(
        &self,
        method: &str,
        url: &str,
        form: Option<Vec<(String, String)>>,
        referer: &str,
        ua: &str,
    ) -> AppResult<Value> {
        let mut req = match method {
            "POST" => self.http()?.post(url),
            _ => self.http()?.get(url),
        }
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::COOKIE, self.cookie())
        .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
        .header(reqwest::header::REFERER, referer);
        if let Some(form) = form {
            req = req.form(&form);
        }
        let res = req
            .send()
            .await
            .map_err(|e| AppError::new(format!("115 请求失败：{e}")))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取 115 响应失败：{e}")))?;
        let value: Value = serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
        if let Some(err) = resp_error(&value, status) {
            return Err(AppError::new(err));
        }
        Ok(value)
    }

    async fn get(&self, url: &str) -> AppResult<Value> {
        self.call("GET", url, None).await
    }

    /// 取 `userkey`：生成上传签名要用，只有 `app/uploadinfo` 给。
    async fn upload_identity(&self) -> AppResult<(String, String)> {
        let value = self
            .get(&format!("{PRO}/app/uploadinfo?t={}", crate::providers::cloud189::random_id()))
            .await?;
        let user_id = value
            .get("user_id")
            .map(|v| match v {
                Value::String(s) => s.trim().to_string(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let userkey = text_at(&value, &["userkey"]);
        if user_id.is_empty() || userkey.is_empty() {
            return Err(AppError::new("拿不到上传凭证（userkey），请重新登录"));
        }
        Ok((user_id, userkey))
    }

    async fn oss_put(
        &self,
        local: &Path,
        size: u64,
        reply: &InitReply,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<()> {
        let sts = self.oss_token().await?;
        let object = encode_object(&reply.object);
        let url = format!("https://{}.{OSS_ENDPOINT}/{}", reply.bucket, object);
        let date = chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let content_type = "application/octet-stream";
        let callback = base64_of(reply.callback.as_bytes());
        let callback_var = base64_of(reply.callback_var.as_bytes());

        // 阿里云 v1 签名：x-oss-* 头按名字排序拼进 string_to_sign。
        let canonical = format!(
            "x-oss-callback:{callback}\nx-oss-callback-var:{callback_var}\nx-oss-security-token:{}\n",
            sts.token
        );
        // 结尾不能多一个换行：OSS 的 StringToSign 到资源名就结束。
        let to_sign = format!(
            "PUT\n\n{content_type}\n{date}\n{canonical}/{}/{}",
            reply.bucket,
            reply.object.trim_start_matches('/')
        );
        let signature = base64_of(&hmac_sha1(&sts.secret, to_sign.as_bytes()));
        let authorization = format!("OSS {}:{signature}", sts.key);

        let file = std::fs::File::open(local)
            .map_err(|e| AppError::new(format!("打不开本地文件：{e}")))?;
        let total = std::cmp::max(size, 1);
        let mut sent = 0u64;
        let stream = tokio_util::io::ReaderStream::with_capacity(tokio::fs::File::from_std(file), 256 * 1024);
        let stream = futures_util::StreamExt::map(stream, move |chunk| {
            if let Ok(bytes) = &chunk {
                sent += bytes.len() as u64;
                sink.progress(std::cmp::min(sent, total));
            }
            chunk
        });

        let res = self
            .http()?
            .put(url)
            .header(reqwest::header::DATE, date)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .header(reqwest::header::CONTENT_LENGTH, size.to_string())
            .header(reqwest::header::USER_AGENT, OSS_UA)
            .header("x-oss-callback", callback)
            .header("x-oss-callback-var", callback_var)
            .header("x-oss-security-token", &sts.token)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .body(reqwest::Body::wrap_stream(stream))
            .send()
            .await
            .map_err(|e| {
                if token.is_cancelled() {
                    AppError::new("已取消")
                } else {
                    AppError::new(format!("上传中断：{e}"))
                }
            })?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::new(format!(
                "上传失败：HTTP {} {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            )));
        }
        // OSS 回调失败会返回 203 并在 body 里说明原因。
        if body.contains("<Error>") {
            return Err(AppError::new(format!(
                "上传回调失败：{}",
                body.chars().take(200).collect::<String>()
            )));
        }
        Ok(())
    }

    async fn oss_token(&self) -> AppResult<OssToken> {
        let value = self.get(&format!("{UPLOAD}/3.0/gettoken.php")).await?;
        let token = OssToken {
            key: text_at(&value, &["AccessKeyId", "AccessKeyID"]),
            secret: text_at(&value, &["AccessKeySecret"]),
            token: text_at(&value, &["SecurityToken"]),
        };
        if token.key.is_empty() || token.secret.is_empty() {
            return Err(AppError::new(format!(
                "拿不到 OSS 临时凭证：{}",
                text_at(&value, &["StatusCode", "message"])
            )));
        }
        Ok(token)
    }


    /// `webapi.115.com/files` 一页的结果，返回 `(条目, 服务端总数)`。
    async fn list_page(&self, cid: &str, offset: u32) -> AppResult<(Vec<EntryDto>, u32)> {
        let url = format!(
            "{WEB}/files?aid=1&cid={cid}&o=user_ptime&asc=0&offset={offset}&show_dir=1&limit={PAGE}&natsort=1&format=json&record_open_time=1&count_folders=1&type=&star=&source=&cp="
        );
        let value = self.get(&url).await?;
        let count = value.get("count").and_then(Value::as_u64).unwrap_or(0) as u32;
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entries: Vec<EntryDto> = rows.iter().map(|item| self.entry(item, cid)).collect();
        Ok((entries, count))
    }

    fn entry(&self, item: &Value, parent: &str) -> EntryDto {
        let name = text_at(item, &["n", "name"]);
        // 115 的文件和文件夹共用一套列表，靠 `fid` 区分：文件有 `fid`，
        // 文件夹只有 `cid`（115driver 的 `IsDirectory = FileID == ""`）。
        let file_id = text_at(item, &["fid", "file_id"]);
        let is_dir = file_id.is_empty();
        let id = if is_dir {
            text_at(item, &["cid", "id"])
        } else {
            file_id
        };
        let size = num_at(item, &["s", "size", "file_size"]);
        let updated = num_at(item, &["tu", "te", "t", "upt"]) * 1000;
        let thumb = item
            .pointer("/ico/s")
            .or_else(|| item.pointer("/thumb/s"))
            .and_then(Value::as_str)
            .filter(|v| v.starts_with("http"))
            .map(str::to_string);
        EntryDto {
            fid: qualify(ProviderId::Pan115, &id),
            name: name.clone(),
            pdir_fid: qualify(ProviderId::Pan115, parent),
            size,
            is_dir,
            format_type: if is_dir { "folder".into() } else { "file".into() },
            updated_at: updated,
            created_at: updated,
            ext: EntryDto::ext_of(&name),
            provider: ProviderId::Pan115.as_str().to_string(),
            thumb,
        }
    }

    /// 取单个条目的父目录（改名/移动冲突提示用）。
    /// 用 pickcode 换一次性下载直链（115 的加密通道）。
    /// `share/snap`：分享的封面信息 + 某一层的文件列表（`cid` 是分享内的目录 id）。
    async fn share_snap(
        &self,
        code: &str,
        receive: &str,
        cid: &str,
        offset: u32,
        limit: u32,
    ) -> AppResult<Value> {
        let url = format!(
            "{WEB}/share/snap?share_code={code}&receive_code={receive}&cid={cid}&limit={limit}&offset={offset}&asc=0&show_dir=1&format=json"
        );
        let referer = format!("https://115cdn.com/s/{code}?password={receive}&");
        let value = self
            .call_full("GET", &url, None, &referer, SHARE_UA)
            .await?;
        if !value.get("state").and_then(Value::as_bool).unwrap_or(false) {
            let message = text_at(&value, &["error", "errmsg"]);
            let message = if message.is_empty() {
                "分享打不开".to_string()
            } else {
                message
            };
            return Err(AppError::new(format!("115 分享：{message}")));
        }
        value
            .get("data")
            .cloned()
            .ok_or_else(|| AppError::new("115 分享没有返回内容"))
    }

    async fn download_url(&self, fid: &str) -> AppResult<String> {
        let id = api_id(fid);
        let info = self
            .get(&format!("{WEB}/files/get_info?file_id={id}&format=json"))
            .await?;
        let pick = info
            .pointer("/data/0/pc")
            .or_else(|| info.pointer("/data/pick_code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if pick.is_empty() {
            return Err(AppError::new("115 没有返回这个文件的 pickcode"));
        }
        let payload = serde_json::to_vec(&json!({ "pickcode": pick }))
            .map_err(|e| AppError::new(format!("115 请求体序列化失败：{e}")))?;
        let (data, rand_key) = super::pan115_crypto::encode(&payload)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let value = self
            .call(
                "POST",
                &format!("{PRO}/app/chrome/downurl?t={stamp}"),
                Some(vec![("data".to_string(), data)]),
            )
            .await?;
        let encoded = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("115 没有返回下载密文"))?;
        let plain = super::pan115_crypto::decode(encoded, &rand_key)?;
        let map: Value = serde_json::from_slice(&plain)
            .map_err(|e| AppError::new(format!("115 下载信息解析失败：{e}")))?;
        let first = map
            .as_object()
            .and_then(|obj| obj.values().next())
            .ok_or_else(|| AppError::new("115 下载信息为空"))?;
        let url = first
            .pointer("/url/url")
            .or_else(|| first.get("url"))
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("115 下载信息里没有地址"))?;
        Ok(url.to_string())
    }

    #[allow(dead_code)]
    async fn info(&self, fid: &str) -> AppResult<Value> {
        let url = format!("{WEB}/files/get_info?file_id={fid}&format=json");
        self.get(&url).await
    }

    async fn path_nodes(&self, cid: &str) -> AppResult<Vec<PathNode>> {
        let mut nodes = Vec::new();
        let mut cur = cid.to_string();
        for _ in 0..32 {
            if cur.is_empty() || cur == ROOT {
                break;
            }
            let value = self
                .get(&format!("{WEB}/files/get_info?file_id={cur}&format=json"))
                .await?;
            let name = value
                .pointer("/data/0/n")
                .or_else(|| value.pointer("/data/n"))
                .or_else(|| value.pointer("/data/0/name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let parent = value
                .pointer("/data/0/pid")
                .or_else(|| value.pointer("/data/cid"))
                .or_else(|| value.pointer("/data/pid"))
                .map(|v| v.to_string())
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            if name.is_empty() {
                break;
            }
            nodes.push(PathNode {
                fid: qualify(ProviderId::Pan115, &cur),
                name,
            });
            if parent.is_empty() || parent == ROOT {
                break;
            }
            cur = parent;
        }
        nodes.reverse();
        Ok(nodes)
    }
}

fn text_at(value: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(found) = value.get(*key) {
            if let Some(text) = found.as_str() {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
            if let Some(num) = found.as_u64() {
                return num.to_string();
            }
        }
    }
    String::new()
}

/// 115 的数值字段常在数字和字符串之间摇摆（`"dtime": "1795522258"`）。
fn num_at(value: &Value, keys: &[&str]) -> u64 {
    for key in keys {
        let Some(found) = value.get(*key) else { continue };
        if let Some(num) = found.as_u64() {
            return num;
        }
        if let Some(text) = found.as_str() {
            if let Ok(num) = text.trim().parse::<u64>() {
                return num;
            }
        }
    }
    0
}

fn resp_error(value: &Value, status: u16) -> Option<String> {
    let state = value.get("state");
    let failed = match state {
        Some(Value::Bool(ok)) => !*ok,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(1) == 0,
        _ => false,
    };
    if failed {
        let message = text_at(value, &["message", "error", "msg"]);
        let code = text_at(value, &["errno", "code"]);
        return Some(if code.is_empty() {
            format!("115 网盘：{message}")
        } else {
            format!("115 网盘：{message}（{code}）")
        });
    }
    if status == 401 || status == 403 {
        return Some("115 登录状态已失效，请重新登录".to_string());
    }
    if status >= 400 {
        return Some(format!("115 网盘请求失败（HTTP {status}）"));
    }
    None
}

fn api_id(raw: &str) -> String {
    let native = native_of(raw);
    if native.is_empty() || native == "root" {
        ROOT.to_string()
    } else {
        native
    }
}

#[async_trait]
impl DriveProvider for Pan115Provider {
    fn id(&self) -> ProviderId {
        ProviderId::Pan115
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            share_open: true,
            // 建分享 `share/send` + `ignore_warn=1`，撤销靠
            // `share/updateshare` 的 `action=cancel`；自定义提货码要会员权限，
            // 我们固定生成随机码（见 `create_share`）。
            share_create: true,
            recycle: true,
            ..Capabilities::BASIC
        }
    }

    async fn auth_state(&self) -> AuthState {
        let (authenticated, masked, nickname) = {
            let creds = match self.creds.read() {
                Ok(creds) => creds.clone(),
                Err(_) => Pan115Credentials::default(),
            };
            (
                creds.usable(),
                mask_cookie(&creds.cookies),
                Some(creds.user_name).filter(|v| !v.is_empty()),
            )
        };
        AuthState {
            provider: ProviderId::Pan115,
            authenticated,
            source: "file".to_string(),
            masked,
            credential_path: self.credential_path.clone(),
            nickname,
            avatar: None,
            capabilities: self.capabilities(),
        }
    }

    async fn logout(&self) -> AppResult<()> {
        if let Ok(mut creds) = self.creds.write() {
            creds.cookies.clear();
            creds.user_id.clear();
        }
        let _ = std::fs::remove_file(&self.credential_path);
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let nickname = self.creds.read().map(|c| c.user_name.clone()).unwrap_or_default();
        if nickname.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccountInfo {
            nickname,
            avatar: None,
        }))
    }

    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        if let Ok(Some(info)) = self.account().await {
            return Ok(Some(info));
        }
        if self.profile_tried.swap(true, Ordering::SeqCst) {
            return Ok(None);
        }
        let value = self
            .get(&format!("{PRO}/app/uploadinfo?t={}", crate::providers::cloud189::random_id()))
            .await
            .ok();
        let user_id = value
            .as_ref()
            .and_then(|v| v.get("user_id"))
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        if user_id.is_empty() {
            return Ok(None);
        }
        if let Ok(mut creds) = self.creds.write() {
            creds.user_id = user_id;
        }
        self.persist().await;
        Ok(None)
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let cid = api_id(dir);
        let mut entries = Vec::new();
        #[allow(unused_assignments)]
        let mut total = 0u32;
        let mut offset = 0u32;
        loop {
            let (mut rows, count) = self.list_page(&cid, offset).await?;
            total = count;
            let got = rows.len();
            entries.append(&mut rows);
            if got == 0 || entries.len() as u32 >= count || offset >= 4000 {
                break;
            }
            offset += got as u32;
        }
        sort_entries(&mut entries);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Pan115, &cid),
            entries,
            total,
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        self.path_nodes(&api_id(dir)).await
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = check_name(name)?;
        let value = self
            .call(
                "POST",
                &format!("{WEB}/files/add"),
                Some(vec![("pid".into(), api_id(parent)), ("cname".into(), name.to_string())]),
            )
            .await?;
        let cid = value
            .pointer("/cid")
            .or_else(|| value.pointer("/data/cid"))
            .or_else(|| value.pointer("/data/file_id"))
            .map(|v| v.to_string())
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        if cid.is_empty() {
            return Err(AppError::new("115 没有返回新文件夹的 id"));
        }
        Ok(qualify(ProviderId::Pan115, &cid))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = check_name(name)?;
        let id = api_id(fid);
        // 只有 `files_new_name[<id>]` 生效：带上 `files_id[]` 或不带 `aid`
        // 都返回 `state: true` 但什么也不改（实测）。
        let form = vec![
            ("aid".to_string(), "1".to_string()),
            (format!("files_new_name[{id}]"), name.to_string()),
        ];
        self.call("POST", &format!("{WEB}/files/batch_rename"), Some(form))
            .await?;
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        let mut form: Vec<(String, String)> = vec![("aid".into(), "1".into())];
        for fid in fids {
            form.push(("fid[]".into(), api_id(fid)));
        }
        if fids.is_empty() {
            return Ok(());
        }
        self.call("POST", &format!("{WEB}/rb/delete"), Some(form))
            .await?;
        Ok(())
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        let mut form: Vec<(String, String)> = vec![("pid".into(), api_id(to))];
        for fid in fids {
            form.push(("fid[]".into(), api_id(fid)));
        }
        let path = if copy { "copy" } else { "move" };
        self.call("POST", &format!("{WEB}/files/{path}"), Some(form))
            .await?;
        Ok(fids.len())
    }

    async fn search(&self, dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let url = format!(
            "{WEB}/files/search?search_value={}&cid={}&format=json&limit={}",
            percent_encoding::utf8_percent_encode(keyword, percent_encoding::NON_ALPHANUMERIC),
            api_id(dir),
            limit.clamp(1, 200)
        );
        let value = self.get(&url).await?;
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let cid = api_id(dir);
        Ok(rows
            .iter()
            .map(|item| SearchHit {
                entry: self.entry(item, &cid),
                path: Vec::new(),
            })
            .collect())
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let url = self.download_url(fid).await?;
        let mut req = self
            .http()?
            .get(&url)
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::COOKIE, self.cookie());
        if start > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let res = req
            .streaming()
            .send()
            .await
            .map_err(|e| AppError::new(format!("115 下载请求失败：{e}")))?;
        let status = res.status();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(format!("115 下载失败（HTTP {}）", status.as_u16())));
        }
        if start > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(RANGE_IGNORED));
        }
        let size = res
            .content_length()
            .map(|len| if start > 0 { len + start } else { len })
            .unwrap_or(0);
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

    // ---------------- 上传：initupload + OSS ----------------

    async fn upload(
        &self,
        local: &Path,
        parent: &str,
        name: &str,
        sink: Arc<dyn UploadSink>,
        token: CancellationToken,
    ) -> AppResult<Option<String>> {
        let name = check_name(name)?;
        let size = std::fs::metadata(local)
            .map(|meta| meta.len())
            .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?
            ;
        // 115 的秒传要两个 SHA1：前 128 KiB 的和整个文件的。
        sink.phase("hashing");
        let (pre_id, quick_id) = digest_115(local, sink.clone(), token.clone()).await?;
        if token.is_cancelled() {
            return Err(AppError::new("已取消"));
        }
        sink.phase("uploading");
        let (user_id, userkey) = self.upload_identity().await?;
        let dir = api_id(parent);

        let mut sign_key = String::new();
        let mut sign_val = String::new();
        let reply = loop {
            let cipher = super::pan115_ec::Cipher::new().map_err(AppError::new)?;
            // `t` 是**秒**：这个值会原样塞进 k_ec 的 4 个字节里，用毫秒会对不上。
            let stamp = now_secs();
            let target = format!("U_1_{dir}");
            let mut form: Vec<(String, String)> = vec![
                ("appid".into(), "0".into()),
                ("appversion".into(), APP_VER.into()),
                ("userid".into(), user_id.clone()),
                ("filename".into(), name.to_string()),
                ("filesize".into(), size.to_string()),
                ("fileid".into(), quick_id.clone()),
                ("target".into(), target.clone()),
                ("sig".into(), upload_sig(&user_id, &userkey, &quick_id, &target)),
                ("t".into(), stamp.to_string()),
                (
                    "token".into(),
                    upload_token(&user_id, &quick_id, &pre_id, size, &sign_key, &sign_val, stamp),
                ),
            ];
            if !sign_key.is_empty() {
                form.push(("sign_key".into(), sign_key.clone()));
                form.push(("sign_val".into(), sign_val.clone()));
            }
            // 服务端按 form 编码解析，顺序无所谓，但和参考实现一样排好序更稳。
            form.sort();
            let encoded = form_encode(&form);
            let body = cipher.encrypt(encoded.as_bytes());
            let url = format!("{UPLOAD}/4.0/initupload.php?k_ec={}", cipher.encode_token(stamp));
            let res = self
                .http()?
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(reqwest::header::USER_AGENT, format!("Mozilla/5.0 115disk/{APP_VER}"))
                .header(reqwest::header::COOKIE, self.cookie())
                .header(reqwest::header::REFERER, "https://115.com/")
                .body(body)
                .send()
                .await
                .map_err(|e| AppError::new(format!("上传初始化失败：{e}")))?;
            let status = res.status();
            let raw = res
                .bytes()
                .await
                .map_err(|e| AppError::new(format!("上传初始化响应读取失败：{e}")))?;
            if !status.is_success() && raw.is_empty() {
                return Err(AppError::new(format!("上传初始化失败：HTTP {}", status.as_u16())));
            }
            let plain = cipher
                .decrypt(&raw)
                .map_err(|e| AppError::new(format!("上传初始化响应解析失败：{e}")))?;
            let value: Value = serde_json::from_slice(&plain)
                .map_err(|e| AppError::new(format!("上传初始化响应不是 JSON：{e}")))?;
            if value.get("state").and_then(Value::as_bool) == Some(false) {
                return Err(AppError::new(format!(
                    "上传初始化被拒绝：{}",
                    text_at(&value, &["error", "message", "statusmsg"])
                )));
            }
            // 7 = 要补一段指定区间的 SHA1 再重来（大文件常见）。
            if value.get("status").and_then(Value::as_i64) == Some(7) {
                let check = text_at(&value, &["sign_check"]);
                let key = text_at(&value, &["sign_key"]);
                if check.is_empty() || key.is_empty() {
                    return Err(AppError::new("上传初始化要求二次签名，但没给区间"));
                }
                sign_key = key;
                sign_val = range_sha1(local, &check).await?;
                continue;
            }
            break InitReply::from(&value);
        };

        // status 2 = 秒传成功，文件已经在云上了。
        // 2 = 秒传命中，文件已经在云上，不用再传。
        if reply.status == 2 {
            return Ok(None);
        }
        if reply.bucket.is_empty() || reply.object.is_empty() {
            return Err(AppError::new(format!(
                "上传初始化没有返回 OSS 参数（status {}）",
                reply.status
            )));
        }
        self.oss_put(local, size, &reply, sink, token).await?;
        Ok(None)
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        // `app/uploadinfo` 只给 user_id，容量在 `files/index_info` 里。
        let value = self.get(&format!("{WEB}/files/index_info?ct=all_count")).await?;
        let used = value.pointer("/data/space_info/all_use/size").and_then(Value::as_u64);
        let total = value
            .pointer("/data/space_info/all_total/size")
            .and_then(Value::as_u64);
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(QuotaInfo { used, total })),
            _ => Ok(None),
        }
    }

    async fn recycle_list(&self, offset: u32, limit: u32) -> AppResult<Vec<RecycleEntry>> {
        // 回收站要 `aid=7`，字段名也换了一套（`file_name` / `file_size` / `dtime`）。
        let url = format!(
            "{WEB}/rb?aid=7&cid=0&offset={offset}&limit={}&format=json",
            limit.clamp(1, 100)
        );
        let value = self.get(&url).await?;
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entries: Vec<RecycleEntry> = rows
            .iter()
            .map(|item| {
                let rid = text_at(item, &["id", "rid"]);
                RecycleEntry {
                    fid: qualify(ProviderId::Pan115, &rid),
                    name: text_at(item, &["file_name", "n"]),
                    // `type` 为 "2" 是文件夹，"1" 是文件。
                    is_dir: text_at(item, &["type"]) == "2",
                    size: num_at(item, &["file_size", "s", "size"]),
                    deleted_at: num_at(item, &["dtime", "rptime"]) * 1000,
                    token: rid,
                    provider: ProviderId::Pan115.as_str().to_string(),
                }
            })
            .collect();
        Ok(entries)
    }

    async fn recycle_restore(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        if entries.is_empty() {
            return Ok(0);
        }
        let mut form: Vec<(String, String)> = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            // 索引形式 `rid[0]`，`rid[]` 服务端不认。
            form.push((format!("rid[{idx}]"), entry.token.clone()));
        }
        self.call("POST", &format!("{WEB}/rb/revert"), Some(form))
            .await?;
        Ok(entries.len() as u32)
    }

    async fn recycle_purge(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        if entries.is_empty() {
            return Ok(0);
        }
        let mut form: Vec<(String, String)> = vec![("password".into(), String::new())];
        for (idx, entry) in entries.iter().enumerate() {
            form.push((format!("rid[{idx}]"), entry.token.clone()));
        }
        // 彻底删除走 `/rb/clean`（不是 `/rb/delete`），空密码表示没设二次密码。
        self.call("POST", &format!("{WEB}/rb/clean"), Some(form))
            .await?;
        Ok(entries.len() as u32)
    }

    async fn recycle_clear(&self) -> AppResult<()> {
        self.call(
            "POST",
            &format!("{WEB}/rb/clean"),
            Some(vec![("password".to_string(), String::new())]),
        )
        .await?;
        Ok(())
    }

    // ------------------------------------------------------- 别人的分享 ---

    async fn share_open(
        &self,
        id: &str,
        passcode: &str,
    ) -> AppResult<crate::share::ShareSession> {
        let (code, receive) = split_share_code(id, passcode);
        if code.is_empty() {
            return Err(AppError::new("这个链接里找不到 115 分享码"));
        }
        let data = self.share_snap(&code, &receive, ROOT, 0, 20).await?;
        let info = data.get("shareinfo").cloned().unwrap_or(Value::Null);
        let user = data.get("userinfo").cloned().unwrap_or(Value::Null);
        let title = text_at(&info, &["share_title"]);
        let first = data
            .get("list")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .cloned();
        // 单文件分享直接把光标放在那个文件上，省掉一次列表请求。
        let first_fid = match &first {
            Some(row) if is_share_file(row) => Some(qualify(ProviderId::Pan115, &text_at(row, &["fid"]))),
            _ => None,
        };
        let face = text_at(&user, &["face"]);
        let avatar = if face.is_empty() {
            None
        } else {
            match self.http() {
                Ok(client) => crate::share::fetch_avatar(&client, &face).await,
                Err(_) => None,
            }
        };
        let title = if title.is_empty() {
            first
                .as_ref()
                .map(|row| text_at(row, &["n"]))
                .unwrap_or_default()
        } else {
            title
        };
        Ok(crate::share::ShareSession {
            provider: ProviderId::Pan115.as_str().to_string(),
            pwd_id: code.clone(),
            passcode: receive,
            stoken: String::new(),
            title,
            owner: text_at(&user, &["user_name"]),
            owner_avatar: avatar,
            file_num: data.get("count").and_then(Value::as_u64).unwrap_or(0),
            total_size: info.get("file_size").and_then(Value::as_u64).unwrap_or(0),
            expired_at: info.get("expire_time").and_then(Value::as_i64).unwrap_or(0),
            first_fid,
            can_save: true,
            web_url: Some(format!("https://115cdn.com/s/{code}")),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<crate::share::ShareListing> {
        let data = self
            .share_snap(
                &share.pwd_id,
                &share.passcode,
                &share_dir(dir),
                0,
                PAGE,
            )
            .await?;
        let rows = data.get("list").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(crate::share::ShareListing {
            pdir_fid: dir.to_string(),
            entries: rows.iter().filter_map(share_entry).collect(),
            total: data.get("count").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    async fn share_save(
        &self,
        share: &crate::transfer::ShareRef,
        entries: &[crate::share::ShareEntry],
        to: Option<&str>,
    ) -> AppResult<u64> {
        if entries.is_empty() {
            return Ok(0);
        }
        let ids = entries
            .iter()
            .map(|entry| api_id(&entry.fid).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let received = self
            .call_full(
                "POST",
                &format!("{WEB}/share/receive"),
                Some(vec![
                    ("share_code".to_string(), share.pwd_id.clone()),
                    ("receive_code".to_string(), share.passcode.clone()),
                    ("file_id".to_string(), ids),
                    ("cid".to_string(), to.map(|dir| api_id(dir).to_string()).unwrap_or_else(|| ROOT.to_string())),
                ]),
                &format!(
                    "https://115cdn.com/s/{}?password={}&",
                    share.pwd_id, share.passcode
                ),
                SHARE_UA,
            )
            .await;
        // 4200045「文件已接收，无需重复接收」表示之前就存过了，结果一样。
        let value = match received {
            Ok(value) => value,
            Err(err) if err.to_string().contains("已接收") => return Ok(1),
            Err(err) => return Err(err),
        };
        if !value.get("state").and_then(Value::as_bool).unwrap_or(false) {
            let message = text_at(&value, &["error"]);
            if message.contains("已接收") {
                return Ok(1);
            }
            return Err(AppError::new(format!("115 转存失败：{message}")));
        }
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let files = data.get("recv_file_count").and_then(Value::as_u64).unwrap_or(0);
        let dirs = data.get("recv_folder_count").and_then(Value::as_u64).unwrap_or(0);
        Ok(files.max(dirs).max(1))
    }

    /// 115 建分享。
    ///
    /// `POST https://webapi.115.com/share/send`，字段 `user_id`、`file_ids`
    /// （逗号分隔）、`ignore_warn`、`is_asc=0`、`order=user_ptime`。
    ///
    /// **`ignore_warn` 必须是 `1`**：填 `0` 时服务端只回一份"预检"载荷
    /// （`total_size`/`share_title`/`file_count`，`state:true` 但**没有
    /// `share_code`**），分享根本没建出来 —— 网页端是先弹提示再重发一次。
    ///
    /// 标题和提货码都管不了：`share_title` 取的是文件名，自定义提货码回
    /// `4100032 无法自定义访问码`（免费号没有这个权限），只能用服务端给的
    /// 4 位。时长默认 15 天，要改得在**建完之后**单独打一次
    /// `share/updateshare`（`share_duration`：`-1` 长期、否则天数）。
    async fn create_share(
        &self,
        fids: &[String],
        _title: &str,
        days: u32,
        _passcode: &str,
    ) -> AppResult<crate::share::ShareCreated> {
        if fids.is_empty() {
            return Err(AppError::new("请先选择要分享的内容"));
        }
        let user_id = self.user_id();
        if user_id.is_empty() {
            return Err(AppError::new("115 账号信息不完整，请重新登录"));
        }
        let list = fids
            .iter()
            .map(|fid| native_of(fid))
            .collect::<Vec<_>>()
            .join(",");
        let value = self
            .call_full(
                "POST",
                "https://webapi.115.com/share/send",
                Some(vec![
                    ("user_id".into(), user_id),
                    ("file_ids".into(), list),
                    ("ignore_warn".into(), "1".into()),
                    ("is_asc".into(), "0".into()),
                    ("order".into(), "user_ptime".into()),
                ]),
                "https://115.com/",
                UA,
            )
            .await?;
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let code = text_at(&data, &["share_code"]);
        if code.is_empty() {
            return Err(AppError::new(
                "115 没有返回分享码，可能触发了内容审核，请到网页端确认",
            ));
        }
        let mut expired_at = num_at(&data, &["share_ex_time"]) as i64 * 1000;
        // 115 只认 `share_duration` ∈ {1,3,5,7,15,-1}（`share_duration_options`
        // 就是这么给的），别的值会被静默忽略、分享仍留在 15 天，所以先夹档。
        let duration = match days {
            0 => "-1".to_string(),
            1 => "1".to_string(),
            2 | 3 => "3".to_string(),
            4 | 5 => "5".to_string(),
            6..=10 => "7".to_string(),
            _ => "15".to_string(),
        };
        // 15 天是服务端默认值；只有要别的时长时才多打一次。
        if duration != "15" {
            let updated = self
                .call_full(
                    "POST",
                    "https://webapi.115.com/share/updateshare",
                    Some(vec![
                        ("share_code".into(), code.clone()),
                        ("share_duration".into(), duration),
                    ]),
                    "https://115.com/",
                    UA,
                )
                .await?;
            let when = updated
                .pointer(&format!("/data/{code}/share_ex_time"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if when > 0 {
                expired_at = when * 1000;
            } else if days == 0 {
                expired_at = 0;
            }
        }
        Ok(crate::share::ShareCreated {
            url: format!("https://115cdn.com/s/{code}"),
            passcode: text_at(&data, &["receive_code"]),
            expired_at,
            share_id: code,
        })
    }

    /// 取消分享。
    ///
    /// 115 没有独立的取消路由——改分享状态一律走
    /// `POST https://webapi.115.com/share/updateshare`，`action=cancel` 就是撤销
    /// （网页端 http 抓包确认；`share/cancel`、`share/del` 那一堆路由都是 404）。
    /// 只需要 `share_code`，`fids` 用不上。
    async fn revoke_share(&self, share_id: &str, _fids: &[String]) -> AppResult<()> {
        let code = share_id.trim();
        if code.is_empty() {
            return Err(AppError::new("缺少分享码"));
        }
        let form = vec![
            ("share_code".to_string(), code.to_string()),
            ("action".to_string(), "cancel".to_string()),
        ];
        let value = self
            .call("POST", "https://webapi.115.com/share/updateshare", Some(form))
            .await?;
        if value.get("state").and_then(Value::as_bool) == Some(false) {
            let error = text_at(&value, &["error", "message"]);
            return Err(AppError::new(if error.is_empty() {
                "115 网盘：取消分享失败".to_string()
            } else {
                format!("115 网盘：{error}")
            }));
        }
        Ok(())
    }

    /// 「我的分享」列表。
    ///
    /// `GET https://webapi.115.com/share/slist?user_id=&offset=&limit=`
    /// （网页端 http 抓包里就是这个，`user_id` 同样来自 cookie 的 `UID`）。
    /// 返回 `count` + `list[]`，每项有 `share_code`、`share_title`、
    /// `receive_code`、`create_time`/`share_ex_time`（秒）、`file_count`/
    /// `folder_count`（都是字符串）、`file_size`、`receive_count`、
    /// `share_state`（1 正常，其余算失效）、`access_user_nums`。
    async fn share_records(&self) -> AppResult<Vec<crate::share::ShareRecord>> {
        let user_id = self.user_id();
        if user_id.is_empty() {
            return Err(AppError::new("115 账号信息不完整，请重新登录"));
        }
        let mut out = Vec::new();
        // 一页 30 条足够，页数封顶 20 页，避免分享特别多时打不完。
        for page in 0..20 {
            let offset = page * 30;
            let value = self
                .call(
                    "GET",
                    &format!(
                        "https://webapi.115.com/share/slist?user_id={user_id}&offset={offset}&limit=30"
                    ),
                    None,
                )
                .await?;
            let rows = value
                .get("list")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let total = num_at(&value, &["count"]);
            for row in &rows {
                let code = text_at(row, &["share_code"]);
                if code.is_empty() {
                    continue;
                }
                // 长期分享回 `share_ex_time = -1`，`num_at` 读负数会得到 0。
                let ex = num_at(row, &["share_ex_time"]) as i64;
                out.push(crate::share::ShareRecord {
                    url: {
                        let url = text_at(row, &["share_url"]);
                        if url.is_empty() {
                            format!("https://115cdn.com/s/{code}")
                        } else {
                            url
                        }
                    },
                    title: text_at(row, &["share_title"]),
                    passcode: text_at(row, &["receive_code"]),
                    // 秒 → 毫秒；`-1`（长期）和缺失都按 0 处理。
                    expired_at: if ex <= 0 { 0 } else { ex * 1000 },
                    created_at: num_at(row, &["create_time"]) as i64 * 1000,
                    file_num: (num_at(row, &["file_count"]) + num_at(row, &["folder_count"])) as u32,
                    size: num_at(row, &["file_size"]) as i64,
                    visit_count: num_at(row, &["access_user_nums"]) as i64,
                    save_count: num_at(row, &["receive_count"]) as i64,
                    download_count: 0,
                    expired: num_at(row, &["share_state"]) != 1,
                    fids: Vec::new(),
                    share_id: code,
                });
            }
            if out.len() as u64 >= total || rows.is_empty() {
                break;
            }
        }
        Ok(out)
    }

    async fn share_walk(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<Vec<ShareFile>> {
        let mut out = Vec::new();
        let mut stack = vec![(share_dir(dir), std::path::PathBuf::new())];
        while let Some((cid, relative)) = stack.pop() {
            let data = self
                .share_snap(&share.pwd_id, &share.passcode, &cid, 0, PAGE)
                .await?;
            let rows = data.get("list").and_then(Value::as_array).cloned().unwrap_or_default();
            for row in rows {
                let name = text_at(&row, &["n"]);
                let fid = share_id(&row);
                if name.is_empty() || fid.is_empty() {
                    continue;
                }
                let here = relative.join(&name);
                if is_share_file(&row) {
                    out.push(ShareFile {
                        relative: here,
                        fid: qualify(ProviderId::Pan115, &fid),
                        token: String::new(),
                        name,
                        size: row.get("s").and_then(Value::as_u64).unwrap_or(0),
                    });
                } else {
                    stack.push((fid, here));
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
        let payload = serde_json::to_vec(&json!({
            "share_code": share.pwd_id,
            "receive_code": share.passcode,
            "file_id": api_id(fid),
        }))
        .map_err(|e| AppError::new(format!("115 请求体序列化失败：{e}")))?;
        let (data, key) = super::pan115_crypto::encode(&payload)?;
        let referer = format!(
            "https://115cdn.com/s/{}?password={}&",
            share.pwd_id, share.passcode
        );
        let value = self
            .call_full(
                "POST",
                &format!("{PRO}/app/share/downurl?t={}", now_secs()),
                Some(vec![("data".to_string(), data)]),
                &referer,
                SHARE_UA,
            )
            .await?;
        let encoded = value
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::new("115 没有返回分享的下载密文"))?;
        let plain = super::pan115_crypto::decode(encoded, &key)?;
        let info: Value = serde_json::from_slice(&plain)
            .map_err(|e| AppError::new(format!("115 分享下载信息解析失败：{e}")))?;
        let url = info
            .pointer("/url/url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if url.is_empty() {
            return Err(AppError::new("115 没有给出分享文件的直链"));
        }
        Ok(ShareLink {
            url,
            headers: vec![
                ("User-Agent".to_string(), SHARE_UA.to_string()),
                ("Referer".to_string(), referer),
            ],
            // `fs` 有时是字符串（`"1387"`），按数字读会得到 0。
            size: num_at(&info, &["fs"]),
            cleanup_fid: None,
        })
    }
}

// ---------------------------------------------------------------- 分享 ---

/// `share/downurl` 只认 115 自家客户端的 UA，普通浏览器 UA 会回「当前版本过低」。
const SHARE_UA: &str = "Mozilla/5.0 115disk/30.5.1";

/// 从分享链接/纯分享码里拆出 `(share_code, 提取码)`。
///
/// 用户粘过来的是 `https://115cdn.com/s/swlwole3wdn?password=f977#`，
/// 也可能是 `115.com/s/xxx`、`115cdn.com/s/xxx?password=f977`，或者只有 `swlwole3wdn`。
fn split_share_code(input: &str, passcode: &str) -> (String, String) {
    let text = input.trim();
    let rest = text.split("/s/").nth(1).unwrap_or(text);
    // 分享码是纯字母数字：从 `/s/` 后面一路取到第一个非字母数字字符，
    // 这样连「复制这段内容…」那种整段分享文案也能猜出来。
    let code: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let mut pass = passcode.trim().to_string();
    if pass.is_empty() {
        for key in ["password=", "pwd=", "passcode=", "code="] {
            if let Some(at) = text.find(key) {
                let value: String = text[at + key.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !value.is_empty() {
                    pass = value;
                    break;
                }
            }
        }
    }
    (code, pass)
}

/// 分享里的目录 id：根目录在 115 这边就是 `0`。
fn share_dir(dir: &str) -> String {
    let bare = api_id(dir);
    if bare.is_empty() {
        ROOT.to_string()
    } else {
        bare.to_string()
    }
}

/// `fc == 1` 才是文件（没给 `fc` 时退一步看有没有 `fid`）。
fn is_share_file(row: &Value) -> bool {
    match row.get("fc").and_then(Value::as_u64) {
        Some(fc) => fc == 1,
        None => !text_at(row, &["fid"]).is_empty(),
    }
}

/// 分享条目的 id：**文件在 `fid`，文件夹在 `cid`**。
/// 文件夹根本没有 `fid` 这个键，早先只读 `fid` 会把整个文件夹吞掉。
fn share_id(row: &Value) -> String {
    let fid = text_at(row, &["fid"]);
    if !fid.is_empty() {
        fid
    } else {
        text_at(row, &["cid"])
    }
}

fn share_entry(row: &Value) -> Option<crate::share::ShareEntry> {
    let fid = share_id(row);
    let name = text_at(row, &["n"]);
    if fid.is_empty() || name.is_empty() {
        return None;
    }
    Some(crate::share::ShareEntry {
        fid: qualify(ProviderId::Pan115, &fid),
        token: String::new(),
        name,
        size: row.get("s").and_then(Value::as_u64).unwrap_or(0),
        is_dir: !is_share_file(row),
        updated_at: row
            .get("t")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0),
    })
}

// ---------------- 115 上传用的小工具 ----------------

const APP_VER: &str = "30.5.1";
const MD5_SALT: &str = "Qclm8MGWUv59TnrR0XPg";
const UPLOAD: &str = "https://uplb.115.com";
const OSS_ENDPOINT: &str = "cn-shenzhen.oss.aliyuncs.com";
const OSS_UA: &str = "aliyun-sdk-android/2.9.1";

struct InitReply {
    status: i64,
    bucket: String,
    object: String,
    callback: String,
    callback_var: String,
}

impl InitReply {
    fn from(value: &Value) -> Self {
        Self {
            status: value.get("status").and_then(Value::as_i64).unwrap_or(0),
            bucket: text_at(value, &["bucket"]),
            object: text_at(value, &["object"]),
            // 回调参数是嵌套的：{"callback": {"callback": …, "callback_var": …}}
            callback: value
                .pointer("/callback/callback")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            callback_var: value
                .pointer("/callback/callback_var")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }
    }
}

struct OssToken {
    key: String,
    secret: String,
    token: String,
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn md5_hex(text: &str) -> String {
    use md5::{Digest, Md5};
    hex::encode(Md5::digest(text.as_bytes()))
}

fn sha1_hex_upper(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    hex::encode_upper(Sha1::digest(data))
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    hex::encode(Sha1::digest(data))
}

/// 秒传签名：`SHA1(userkey + hex(SHA1(userid+fileid+target+"0")) + "000000")`，大写。
fn upload_sig(user_id: &str, userkey: &str, file_id: &str, target: &str) -> String {
    // 内层是小写 hex，只有最外层转大写 —— 全大写会得到 `sig invalid`（实测）。
    let inner = sha1_hex(format!("{user_id}{file_id}{target}0").as_bytes());
    sha1_hex_upper(format!("{userkey}{inner}000000").as_bytes())
}

fn upload_token(
    user_id: &str,
    file_id: &str,
    _pre_id: &str,
    size: u64,
    sign_key: &str,
    sign_val: &str,
    stamp: i64,
) -> String {
    let user_md5 = md5_hex(user_id);
    md5_hex(&format!(
        "{MD5_SALT}{file_id}{size}{sign_key}{sign_val}{user_id}{stamp}{user_md5}{APP_VER}"
    ))
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn hmac_sha1(key: &str, data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key.as_bytes()).expect("hmac key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// OSS 的 object 只编码路径段，`/` 保留。
fn encode_object(object: &str) -> String {
    let mut out = String::with_capacity(object.len() + 8);
    for byte in object.trim_start_matches('/').bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 115 秒传要两个 SHA1：前 128 KiB（`PreID`）和整个文件（`QuickID`）。
async fn digest_115(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<(String, String)> {
    use sha1::{Digest, Sha1};
    const PREFIX: usize = 128 * 1024;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> AppResult<(String, String)> {
        use std::io::Read;
        let mut file = std::fs::File::open(&path)
            .map_err(|e| AppError::new(format!("打不开本地文件：{e}")))?;
        let mut whole = Sha1::new();
        let mut prefix = Sha1::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut seen = 0usize;
        let mut done = 0u64;
        loop {
            if token.is_cancelled() {
                return Err(AppError::new("已取消"));
            }
            let read = file
                .read(&mut buf)
                .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?;
            if read == 0 {
                break;
            }
            whole.update(&buf[..read]);
            if seen < PREFIX {
                let take = std::cmp::min(PREFIX - seen, read);
                prefix.update(&buf[..take]);
                seen += take;
            }
            done += read as u64;
            sink.progress(done);
        }
        Ok((hex::encode_upper(prefix.finalize()), hex::encode_upper(whole.finalize())))
    })
    .await
    .map_err(|e| AppError::new(format!("计算校验值失败：{e}")))?
}

/// 服务端要求补一段区间的 SHA1（`sign_check` 形如 `0-1023`）。
async fn range_sha1(path: &Path, spec: &str) -> AppResult<String> {
    use std::io::{Read, Seek, SeekFrom};
    let (start, end) = spec
        .split_once('-')
        .ok_or_else(|| AppError::new(format!("区间格式不对：{spec}")))?;
    let start: u64 = start
        .trim()
        .parse()
        .map_err(|_| AppError::new(format!("区间起点不对：{spec}")))?;
    let end: u64 = end
        .trim()
        .parse()
        .map_err(|_| AppError::new(format!("区间终点不对：{spec}")))?;
    if end < start {
        return Err(AppError::new(format!("区间颠倒：{spec}")));
    }
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> AppResult<String> {
        use sha1::{Digest, Sha1};
        let mut file = std::fs::File::open(&path)
            .map_err(|e| AppError::new(format!("打不开本地文件：{e}")))?;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| AppError::new(format!("定位失败：{e}")))?;
        let mut hasher = Sha1::new();
        let mut left = end - start + 1;
        let mut buf = vec![0u8; 256 * 1024];
        while left > 0 {
            let want = std::cmp::min(left as usize, buf.len());
            let read = file
                .read(&mut buf[..want])
                .map_err(|e| AppError::new(format!("读取本地文件失败：{e}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            left -= read as u64;
        }
        Ok(hex::encode_upper(hasher.finalize()))
    })
    .await
    .map_err(|e| AppError::new(format!("计算区间校验失败：{e}")))?
}

/// 表单编码（`application/x-www-form-urlencoded`）：空格变 `+`。
fn form_encode(pairs: &[(String, String)]) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
    const SET: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'*')
        .remove(b'-')
        .remove(b'.')
        .remove(b'_');
    let mut out = String::new();
    for (idx, (key, value)) in pairs.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        out.push_str(&utf8_percent_encode(key, SET).to_string().replace("%20", "+"));
        out.push('=');
        out.push_str(&utf8_percent_encode(value, SET).to_string().replace("%20", "+"));
    }
    out
}

fn mask_cookie(cookie: &str) -> Option<String> {
    let uid = cookie
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(key, _)| *key == "UID")
        .map(|(_, value)| value.to_string())?;
    if uid.is_empty() {
        return None;
    }
    Some(if uid.len() <= 8 {
        "…".to_string()
    } else {
        format!("{}…{}", &uid[..4], &uid[uid.len() - 4..])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_the_uid_inside_a_cookie_string() {
        let masked = mask_cookie("UID=ABC123456789; CID=1; SEID=xyz");
        assert_eq!(masked.as_deref(), Some("ABC1…6789"));
        assert!(mask_cookie("CID=1").is_none());
    }

    #[test]
    fn maps_the_root_directory() {
        assert_eq!(api_id(""), ROOT);
        assert_eq!(api_id("root"), ROOT);
        assert_eq!(api_id("pan115:0"), ROOT);
        assert_eq!(api_id("pan115:12345"), "12345");
        assert_eq!(api_id("12345"), "12345");
    }

    #[test]
    fn reads_the_115_field_names() {
        let item = json!({ "fid": "42", "n": "a.txt", "s": 12, "pc": "abc", "upt": 1700000000 });
        let provider = Pan115Provider::new(Pan115Credentials::default(), String::new());
        let entry = provider.entry(&item, ROOT);
        assert_eq!(entry.name, "a.txt");
        assert_eq!(entry.size, 12);
        assert!(!entry.is_dir);
        assert_eq!(entry.fid, "pan115:42");
        assert_eq!(entry.updated_at, 1_700_000_000_000);
    }

    #[test]
    fn folders_are_rows_without_a_fid() {
        // 文件夹只有 `cid`（而且也有 `pc`，所以不能用 pickcode 判目录），
        // 文件才有 `fid` —— 和 115driver 的 `IsDirectory = FileID == ""` 一致。
        let provider = Pan115Provider::new(Pan115Credentials::default(), String::new());
        let folder = json!({ "cid": "7", "pid": "0", "n": "dir", "pc": "fbfl60c18u5zqywso8" });
        let entry = provider.entry(&folder, ROOT);
        assert!(entry.is_dir);
        assert_eq!(entry.fid, "pan115:7");
        let file = json!({ "fid": "8", "cid": "0", "pid": "7", "n": "a.bin", "s": "1024", "pc": "x" });
        let entry = provider.entry(&file, "7");
        assert!(!entry.is_dir);
        assert_eq!(entry.fid, "pan115:8");
        assert_eq!(entry.size, 1024);
        assert_eq!(entry.pdir_fid, "pan115:7");
    }


    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_walk() {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let root = drive.list("0").await.expect("root");
        println!("root: {} entries, total {}", root.entries.len(), root.total);

        let name = format!("yunyu-115-{stamp}");
        let made = drive.make_dir("0", &name).await.expect("mkdir");
        println!("mkdir -> {made}");

        let listing = drive.list("0").await.expect("list");
        let hit = listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} not listed: {:?}", listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()));
        println!("listed fid={} dir={} pdir={}", hit.fid, hit.is_dir, hit.pdir_fid);
        assert!(hit.is_dir);
        assert_eq!(hit.pdir_fid, "pan115:0");

        let renamed = format!("{name}-r");
        drive.rename(&hit.fid, &renamed).await.expect("rename");
        let listing = drive.list("0").await.expect("list after rename");
        assert!(
            listing.entries.iter().any(|e| e.name == renamed),
            "rename did not stick"
        );
        println!("rename -> {renamed}");

        // 复制到一个临时文件夹里，验证 move/copy 的 `pid` + `fid[]` 形状。
        let holder = drive.make_dir("0", &format!("yunyu-115-hold-{stamp}")).await.expect("mkdir2");
        let moved = drive.transfer_entries(std::slice::from_ref(&hit.fid), &holder, false).await.expect("move");
        assert_eq!(moved, 1);
        let inside = drive.list(&holder).await.expect("list holder");
        assert!(inside.entries.iter().any(|e| e.name == renamed), "move did not stick");
        println!("move -> {} entries in holder", inside.entries.len());

        let quota = drive.quota().await.expect("quota");
        println!("quota: {quota:?}");

        // 删除后进回收站，再恢复、再彻底删除。
        if let Err(err) = drive.delete(std::slice::from_ref(&hit.fid)).await {
            println!("delete inner failed: {}", err.0);
        }
        let folder = api_id(&holder);
        if let Err(err) = drive.delete(&[qualify(ProviderId::Pan115, &folder)]).await {
            println!("delete holder failed: {}", err.0);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let (bin, _) = {
            let rows = drive.recycle_list(0, 50).await.expect("recycle list");
            (rows.clone(), rows.len())
        };
        println!("recycle: {} entries", bin.len());
        if let Some(entry) = bin.iter().find(|e| e.name == renamed || e.name == name) {
            let restored = drive.recycle_restore(std::slice::from_ref(entry)).await.expect("restore");
            println!("restored {restored}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let rows = drive.recycle_list(0, 50).await.expect("recycle list 2");
        for entry in rows.iter().filter(|e| e.name.starts_with("yunyu-115") || e.name.starts_with(&name)) {
            match drive.recycle_purge(std::slice::from_ref(entry)).await {
                Ok(n) => println!("purged {} ({n})", entry.name),
                Err(err) => println!("purge failed on {}: {}", entry.name, err.0),
            }
        }
        // 恢复出来的目录也清掉，别在账号里留垃圾。
        if let Ok(listing) = drive.list("0").await {
            for entry in listing.entries.iter().filter(|e| e.name.starts_with("yunyu-115") || e.name == name || e.name == renamed) {
                let _ = drive.delete(std::slice::from_ref(&entry.fid)).await;
            }
        }
        println!("done");
    }


    #[test]
    fn splits_every_share_link_shape() {
        // 整段分享文案也要能猜出分享码。
        let (code, pass) = split_share_code(
            "https://115cdn.com/s/swlwole3wdn?password=f977#\n复制这段内容，可在115App中直接打开！",
            "",
        );
        assert_eq!(code, "swlwole3wdn");
        assert_eq!(pass, "f977");

        // 只给码，提取码另外填。
        let (code, pass) = split_share_code("swlwole3wdn", "f977");
        assert_eq!(code, "swlwole3wdn");
        assert_eq!(pass, "f977");

        // 前端会把手输的提取码拼成 `&pwd=`。
        let (code, pass) = split_share_code("https://115.com/s/abc123xyz?pwd=9x2f", "");
        assert_eq!(code, "abc123xyz");
        assert_eq!(pass, "9x2f");
    }

    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_share() {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");

        let session = drive
            .share_open("https://115cdn.com/s/swlwole3wdn?password=f977#", "")
            .await
            .expect("open share");
        println!(
            "share: title={} owner={} files={} size={} expired={} first_fid={:?}",
            session.title, session.owner, session.file_num, session.total_size,
            session.expired_at, session.first_fid
        );
        let share = crate::transfer::ShareRef {
            provider: "pan115".to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: String::new(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };

        let listing = drive.share_list(&share, "0").await.expect("list share");
        for entry in &listing.entries {
            println!("  {} [{}] dir={} {}", entry.name, entry.size, entry.is_dir, entry.fid);
        }
        let file = listing
            .entries
            .iter()
            .find(|entry| !entry.is_dir)
            .expect("share has a file");

        let link = drive.share_link(&share, &file.fid).await.expect("share link");
        println!("url: {}", link.url.chars().take(70).collect::<String>());
        let res = drive
            .http()
            .unwrap()
            .get(&link.url)
            .header(reqwest::header::USER_AGENT, SHARE_UA)
            .send()
            .await
            .expect("cdn fetch");
        println!("cdn: status={} bytes={}", res.status(), res.bytes().await.unwrap().len());

        let walk = drive.share_walk(&share, "0").await.expect("walk share");
        println!(
            "walk: {} files, first={:?}",
            walk.len(),
            walk.first().map(|f| f.relative.clone())
        );

        // 转存进一个临时目录再删掉，免得在账号里留垃圾。
        // （存到根目录时 115 会把「收到过的文件」塞进「最近接收」，不好收拾。）
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = drive
            .make_dir("0", &format!("yunyu-share-{stamp}"))
            .await
            .expect("temp dir");
        let saved = drive
            .share_save(&share, &listing.entries, Some(&target))
            .await
            .expect("save share");
        let inside = drive.list(&target).await.expect("temp list");
        println!("saved: {saved}, inside: {}", inside.entries.len());
        assert_eq!(inside.entries.len(), 1);
        drive.delete(&[target]).await.expect("cleanup");
    }

    /// 文件夹分享：`临时文件/MG/…` 这种多级结构，文件夹只有 `cid` 没有 `fid`。
    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_share_folder() {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");

        let session = drive
            .share_open("https://115cdn.com/s/swlwo7g3wdn?password=r061#", "")
            .await
            .expect("open folder share");
        println!(
            "folder share: title={} files={} size={} first_fid={:?}",
            session.title, session.file_num, session.total_size, session.first_fid
        );
        let share = crate::transfer::ShareRef {
            provider: "pan115".to_string(),
            pwd_id: session.pwd_id.clone(),
            stoken: String::new(),
            fid_token: String::new(),
            passcode: session.passcode.clone(),
        };

        let root = drive.share_list(&share, "0").await.expect("root");
        for entry in &root.entries {
            println!(
                "root: {} dir={} size={} {}",
                entry.name, entry.is_dir, entry.size, entry.fid
            );
        }
        let folder = root.entries.first().expect("root folder");
        assert!(folder.is_dir, "root should hold a folder");
        let inner = drive
            .share_list(&share, &folder.fid)
            .await
            .expect("inner list");
        println!("inner: {} entries", inner.entries.len());
        assert!(!inner.entries.is_empty());

        let walk = drive.share_walk(&share, "0").await.expect("walk");
        for file in &walk {
            println!("walk: {} [{}]", file.relative.display(), file.size);
        }
        assert!(!walk.is_empty(), "walk should find nested files");

        // 最深层的文件也要能出直链：验证「文件夹用 cid 当 id」这条链路。
        let deepest = walk
            .iter()
            .max_by_key(|file| file.relative.components().count())
            .expect("nested file");
        let link = drive
            .share_link(&share, &deepest.fid)
            .await
            .expect("link");
        let res = drive
            .http()
            .unwrap()
            .get(&link.url)
            .header(reqwest::header::USER_AGENT, SHARE_UA)
            .send()
            .await
            .expect("cdn fetch");
        let status = res.status();
        let body = res.bytes().await.unwrap();
        println!(
            "cdn: status={status} bytes={} expect={} body={}",
            body.len(),
            link.size,
            String::from_utf8_lossy(&body[..body.len().min(120)])
        );
        // 分享的每日下载配额用完时 CDN 会回 403（`115 pmt user`），
        // 那是环境问题，只提示不判失败。
        if !status.is_success() {
            println!("note: 直链下载被拒（多半是分享的当日下载配额），跳过字节校验");
        }

        // 整个文件夹转存一次，再删掉。
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let target = drive
            .make_dir("0", &format!("yunyu-sharef-{stamp}"))
            .await
            .expect("temp dir");
        let saved = drive
            .share_save(&share, &root.entries, Some(&target))
            .await
            .expect("save folder");
        let inside = drive.list(&target).await.expect("temp list");
        println!(
            "saved={saved}, inside {}: {:?}",
            inside.entries.len(),
            inside.entries.first().map(|e| e.name.clone())
        );
        // 115 对「以前收过的分享」直接空操作（回 4200045，按成功处理），
        // 所以这里不断言新目录一定有内容，只把它删掉收工。
        drive.delete(&[target]).await.expect("cleanup");
    }

    /// 取消分享：把测试留下的 `yunyu-share*` 分享全撤掉，列表里应随之消失。
    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_share_revoke() {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");
        let before = drive.share_records().await.expect("share records");
        let doomed: Vec<String> = before
            .iter()
            .filter(|record| record.title.starts_with("yunyu-share"))
            .map(|record| record.share_id.clone())
            .collect();
        println!("before: {} shares, revoking {}", before.len(), doomed.len());
        for code in &doomed {
            drive.revoke_share(code, &[]).await.expect("revoke");
        }
        let after = drive.share_records().await.expect("share records");
        println!("after: {} shares", after.len());
        for code in &doomed {
            assert!(
                !after.iter().any(|record| &record.share_id == code),
                "分享 {code} 没被撤掉"
            );
        }
        assert_eq!(after.len() + doomed.len(), before.len());
    }

    /// 「我的分享」列表能读回来，字段映射对得上。
    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_share_records() {
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");
        let records = drive.share_records().await.expect("share records");
        println!("records: {}", records.len());
        for record in records.iter().take(5) {
            println!(
                "  {} | {} | code={} | files={} size={} | created={} expired_at={} gone={} | save={} visit={}",
                record.share_id,
                record.title,
                record.passcode,
                record.file_num,
                record.size,
                record.created_at,
                record.expired_at,
                record.expired,
                record.save_count,
                record.visit_count
            );
        }
        for record in &records {
            assert!(!record.share_id.is_empty());
            assert!(
                record.url.contains(&record.share_id),
                "url 里没带分享码：{}",
                record.url
            );
            assert!(record.created_at > 0, "创建时间没解出来");
        }
    }

    /// 建分享 → 用自己刚建的链接打开 → 列表对得上。
    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_share_create() {
        struct Spy;
        impl super::UploadSink for Spy {
            fn phase(&self, _phase: &str) {}
            fn progress(&self, _bytes: u64) {}
        }
        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let folder = drive
            .make_dir("0", &format!("yunyu-shareplay-{stamp}"))
            .await
            .expect("make dir");
        let name = format!("yunyu-shareplay-{stamp}.txt");
        let local = std::env::temp_dir().join(&name);
        std::fs::write(&local, format!("yunyu share play {stamp}")).expect("write");
        let token = tokio_util::sync::CancellationToken::new();
        drive
            .upload(&local, &native_of(&folder), &name, std::sync::Arc::new(Spy), token)
            .await
            .unwrap_or_else(|e| panic!("upload failed: {}", e.0));
        let entry = drive
            .list(&folder)
            .await
            .expect("list")
            .entries
            .first()
            .cloned()
            .expect("uploaded file missing");

        let created = drive
            .create_share(std::slice::from_ref(&entry.fid), "", 7, "")
            .await
            .expect("create share");
        println!(
            "created url={} passcode={} expired_at={} id={}",
            created.url, created.passcode, created.expired_at, created.share_id
        );
        assert!(created.url.contains("/s/"), "url 不像分享地址");
        assert_eq!(created.passcode.len(), 4, "115 总会给一个 4 位提货码");
        // 7 天：留点余量，别因为几秒的时钟差打脸。
        let now = chrono::Utc::now().timestamp_millis();
        assert!(created.expired_at > now + 6 * 86_400_000, "时长没生效");
        assert!(created.expired_at < now + 8 * 86_400_000, "时长没生效");

        // 走一遍正式的打开流程，确认链接真的能用。刚建好的分享会有一两秒
        // 返回 `4100021 正在生成文件快照`，所以要重试。
        let link = format!("{}?password={}", created.url, created.passcode);
        let mut opened = None;
        for attempt in 0..5 {
            match drive.share_open(&link, "").await {
                Ok(session) => {
                    opened = Some(session);
                    break;
                }
                Err(e) if e.0.contains("快照") && attempt < 4 => {
                    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                }
                Err(e) => panic!("open own share: {}", e.0),
            }
        }
        let opened = opened.expect("share never became readable");
        println!(
            "reopened title={} files={} size={}",
            opened.title, opened.file_num, opened.total_size
        );
        assert_eq!(opened.file_num, 1);
        assert_eq!(opened.total_size, entry.size);

        // 先撤分享再删源文件，两边都不留痕迹。
        let code = created.url.rsplit('/').next().unwrap_or_default().to_string();
        drive.revoke_share(&code, &[]).await.expect("revoke");
        println!("revoked {code}");
        drive.delete(&[folder]).await.expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "hits the live 115 account"]
    async fn pan115_live_upload_roundtrip() {
        struct Spy;
        impl super::UploadSink for Spy {
            fn phase(&self, _phase: &str) {}
            fn progress(&self, _bytes: u64) {}
        }

        let dir = std::path::Path::new(&std::env::var("HOME").unwrap()).join(".config/yunyu-drive");
        let drive = Pan115Provider::load(&dir).expect("login first");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let name = format!("yunyu-115-{stamp}.txt");
        let local = std::env::temp_dir().join(&name);
        // 内容要随机：相同的字节会命中 115 的秒传，测不到真正的 OSS 通道。
        let payload: String = format!(
            "yunyu 115 upload probe {stamp} {}",
            (0..4000).map(|i| ((b'a' + (i % 26) as u8) as char).to_string()).collect::<String>()
        );
        std::fs::write(&local, &payload).expect("write local");

        let tokio_token = tokio_util::sync::CancellationToken::new();
        drive
            .upload(&local, "0", &name, std::sync::Arc::new(Spy), tokio_token)
            .await
            .unwrap_or_else(|e| panic!("upload failed: {}", e.0));
        println!("uploaded {name} ({} bytes)", payload.len());

        let listing = drive.list("0").await.expect("list");
        let hit = listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} not in listing: {:?}", listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()));
        println!("listed fid={} size={}", hit.fid, hit.size);
        assert_eq!(hit.size, payload.len() as u64);

        // 下载走的是 RSA + genKey 那条加密链路，上传成功才第一次有真文件可验。
        let handle = drive.open_download(&hit.fid, 0).await.expect("download");
        let bytes = {
            use futures_util::StreamExt;
            let mut collected = Vec::new();
            let mut stream = handle.stream;
            while let Some(chunk) = stream.next().await {
                collected.extend_from_slice(&chunk.expect("chunk"));
            }
            collected
        };
        println!("downloaded {} bytes", bytes.len());
        assert_eq!(String::from_utf8_lossy(&bytes), payload, "下载内容和上传的不一致");

        drive.delete(std::slice::from_ref(&hit.fid)).await.expect("cleanup");
        let _ = std::fs::remove_file(&local);
        println!("cleaned up");
    }

    #[test]
    fn surfaces_server_side_failures() {
        let value = json!({ "state": false, "message": "请先登录", "errno": 990001 });
        let err = resp_error(&value, 200).unwrap();
        assert!(err.contains("请先登录"), "{err}");
        assert!(resp_error(&json!({ "state": true }), 200).is_none());
        assert!(resp_error(&json!({}), 403).is_some());
    }
}

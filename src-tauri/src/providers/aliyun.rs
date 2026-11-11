//! 阿里云盘 (Aliyun Drive).
//!
//! Built on the **web** API (`https://api.aliyundrive.com`) — the same surface
//! the browser client uses. The openapi (`openapi.alipan.com`) was tried first
//! and abandoned: its OAuth token endpoint only accepts a *registered* app's
//! `client_id`, so a token obtained by scanning a QR code can never be refreshed
//! there. The web API refreshes with the refresh token alone, which is exactly
//! what a third-party client can hold.
//!
//! Request/response shapes were read from `tickstep/aliyunpan-api`
//! (`aliyunpan_web/`, Apache-2.0) — see NOTICE; no Go code is linked in.
//!
//! ## Signing
//!
//! Every file endpoint wants `x-device-id` + `x-signature`. The signature is a
//! secp256k1 ECDSA over `"{appId}:{deviceId}:{userId}:{nonce}"` with a locally
//! generated key, hex-encoded with a `"01"` suffix. The public key is registered
//! once per device by `POST /users/v1/users/device/create_session`; afterwards
//! the server accepts the signature and (for the same key) `renew_session` keeps
//! the device alive. Both the key and the device id are stored in `aliyun.json`
//! so a restart does not burn one of the account's 10 device slots.
//!
//! Everything is addressed by `file_id`, with `"root"` as the root folder.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::{
    qualify, AuthState, Capabilities, DirListing, DownloadHandle, DriveProvider, EntryDto, PathNode,
    ProviderId, QuotaInfo, RecycleEntry, SearchHit, ShareFile, ShareLink, UploadSink, RANGE_IGNORED,
};
use crate::error::{AppError, AppResult};
use crate::share::{AccountInfo, ShareEntry, ShareListing, ShareSession};

/// Web API root — file operations.
const API: &str = "https://api.aliyundrive.com";
/// User API root — `/v2/user/get` lives here, not under [`API`].
const USER_API: &str = "https://user.aliyundrive.com";
/// Auth API root — the refresh-token exchange lives here.
const AUTH: &str = "https://auth.aliyundrive.com";
/// The client id the web frontend signs with.
const APP_ID: &str = "25dzX3vbYqktVxyX";
/// 阿里云盘's native root id, as the API spells it.
const API_ROOT: &str = "root";
/// The root id the rest of the app uses for every drive (see `rootFid`).
const ROOT: &str = "0";
/// Part size for multipart uploads; 10 MiB keeps the request count sane.
const PART_SIZE: u64 = 10 * 1024 * 1024;
/// The web frontend reports itself as this device.
const MODEL_NAME: &str = "Windows网页版";
/// Client tags for `X-Canary`. `get_download_url` only hands out links for
/// large files to the android client.
const CANARY_ANDROID: &str = "client=android,app=adrive,version=v6.9.0";
const CANARY_WEB: &str = "client=web,app=adrive,version=v3.17.0";
/// Tag used on the share endpoints; the share backend rejects other tags.
const CANARY_SHARE: &str = "client=web,app=share,version=v2.3.1";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliyunCredentials {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub drive_id: String,
    /// The separate "resource" drive (`resource_drive_id`).
    ///
    /// Most accounts get a *Default* drive that stays empty plus a resource
    /// drive holding every file the user actually owns, so the two are probed
    /// and the one with content wins (see `adopt_resource_drive`).
    #[serde(default)]
    pub resource_drive_id: String,
    /// `true` once the drive in `drive_id` was picked deliberately — either by
    /// [`AliyunProvider::choose_drive`] or by the automatic switch in
    /// [`AliyunProvider::adopt_resource_drive`].
    ///
    /// Without this the choice had to be re-derived on every start, and the
    /// derivation ("is the default drive empty?") flipped as soon as anything
    /// was written to the default drive — a saved copy, an upload — which made
    /// the file list jump between the two drives.
    #[serde(default)]
    pub drive_chosen: bool,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub avatar: Option<String>,
    /// Epoch seconds; `0` means unknown.
    #[serde(default)]
    pub expires_at: i64,
    /// The signing identity registered with `create_session`. Generated on the
    /// first request and kept, so restarts reuse the same device slot.
    #[serde(default)]
    pub device_id: String,
    /// 32-byte secp256k1 secret key, hex. Empty until the first signature.
    #[serde(default)]
    pub private_key: String,
    /// `x-signature` computed for nonce 0, cached so a restart reuses it.
    ///
    /// The nonce is deliberately pinned at 0: the server only accepts the
    /// signature it was given at registration time, so a per-request counter
    /// would be rejected. (Advancing the nonce requires `renew_session`, which
    /// the reference client never calls either.)
    #[serde(default)]
    pub signature: String,
}

pub struct AliyunProvider {
    creds: RwLock<AliyunCredentials>,
    credential_path: String,
    /// Set when a switch between the default and the resource drive happened
    /// during this run, so the probe runs at most once per process instead of
    /// on every root listing.
    drive_settled: std::sync::atomic::AtomicBool,
    /// Built on first use and reused, so all requests share one pool.
    client: std::sync::OnceLock<reqwest::Client>,
}

impl AliyunProvider {
    pub fn new(creds: AliyunCredentials, credential_path: String) -> Self {
        Self {
            creds: RwLock::new(creds),
            credential_path,
            drive_settled: std::sync::atomic::AtomicBool::new(false),
            client: std::sync::OnceLock::new(),
        }
    }

    /// Loads `<config>/aliyun.json`, or reports "not logged in".
    pub fn load(own_dir: &Path) -> AppResult<Self> {
        let path = own_dir.join("aliyun.json");
        if !path.exists() {
            return Err(AppError::new("尚未登录阿里云盘"));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| AppError::new(format!("读取阿里云盘凭据失败：{e}")))?;
        let creds: AliyunCredentials = serde_json::from_str(&text)
            .map_err(|e| AppError::new(format!("阿里云盘凭据格式错误：{e}")))?;
        if creds.refresh_token.trim().is_empty() && creds.access_token.trim().is_empty() {
            return Err(AppError::new("尚未登录阿里云盘"));
        }
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    /// Stores a token pair obtained by the QR-code login flow.
    pub fn save_tokens(own_dir: &Path, creds: AliyunCredentials) -> AppResult<Self> {
        // Only a pair with *no* credential at all is unusable. The QR flow has
        // been seen to hand out an access token without a refresh token; that
        // still works until it expires, so it is stored rather than rejected.
        if creds.refresh_token.trim().is_empty() && creds.access_token.trim().is_empty() {
            return Err(AppError::new("阿里云盘没有拿到任何凭据，请重新扫码登录"));
        }
        if creds.refresh_token.trim().is_empty() {
            crate::ylog!("aliyun: token/get returned no refresh_token (access only)");
        }
        std::fs::create_dir_all(own_dir)
            .map_err(|e| AppError::new(format!("创建配置目录失败：{e}")))?;
        let path = own_dir.join("aliyun.json");
        let text = serde_json::to_string_pretty(&creds)
            .map_err(|e| AppError::new(format!("序列化凭据失败：{e}")))?;
        std::fs::write(&path, text).map_err(|e| AppError::new(format!("写入凭据失败：{e}")))?;
        crate::config::restrict_permissions_pub(&path);
        Ok(Self::new(creds, path.to_string_lossy().to_string()))
    }

    /// Shared client. Built once: a fresh `reqwest::Client` per call meant a new
    /// connection pool (and a new TLS handshake) for every request and every
    /// upload part.
    fn http(&self) -> AppResult<reqwest::Client> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        let _ = self.client.set(client.clone());
        Ok(client)
    }

    /// Persists the in-memory credentials (tokens, drive id, signing key).
    async fn persist(&self) {
        if self.credential_path.is_empty() {
            return;
        }
        let creds = self.creds.read().await.clone();
        if let Ok(text) = serde_json::to_string_pretty(&creds) {
            let _ = std::fs::write(&self.credential_path, text);
            crate::config::restrict_permissions_pub(Path::new(&self.credential_path));
        }
    }

    /// Returns a usable access token, refreshing it when it is about to expire.
    async fn token(&self) -> AppResult<String> {
        {
            let creds = self.creds.read().await;
            let now = chrono::Utc::now().timestamp();
            if !creds.access_token.is_empty()
                && (creds.expires_at == 0 || creds.expires_at - now > 120)
            {
                return Ok(creds.access_token.clone());
            }
        }
        self.refresh().await?;
        Ok(self.creds.read().await.access_token.clone())
    }

    /// The account's default drive id, fetched on first use.
    ///
    /// Every file endpoint needs it, and a token pair alone does not carry it,
    /// so an empty value triggers a `/v2/user/get`.
    async fn drive(&self) -> AppResult<String> {
        // Settle default-vs-resource before anything uses the id, so a restored
        // transfer or a saved path does not resolve against the wrong drive.
        // The probe is guarded by an atomic flag and runs at most once.
        self.adopt_resource_drive().await?;
        let existing = self.creds.read().await.drive_id.clone();
        if !existing.is_empty() {
            return Ok(existing);
        }
        let token = self.token().await?;
        self.fetch_user(&token).await?;
        let creds = self.creds.read().await;
        if creds.drive_id.is_empty() {
            return Err(AppError::new("阿里云盘没有返回默认网盘 ID，请重新登录"));
        }
        Ok(creds.drive_id.clone())
    }

    /// `used_size` of one drive, from `/v2/drive/get`.
    async fn drive_used_size(&self, drive: &str) -> AppResult<u64> {
        let token = self.token().await?;
        let value = self
            .raw_post(API, "/v2/drive/get", &json!({ "drive_id": drive }), &token, None)
            .await?;
        Ok(value.get("used_size").and_then(|v| v.as_u64()).unwrap_or(0))
    }

    /// Picks between the default and the resource drive once per process.
    ///
    /// 阿里云盘 hands most accounts two drives: a *Default* one and a
    /// *resource* one (`resource_drive_id`). The phone app shows the resource
    /// drive — that is where the user's real files live — while the default
    /// drive starts out empty and only collects whatever a client writes into
    /// it directly.
    ///
    /// The previous rule ("switch only when the default drive is empty") broke
    /// as soon as anything landed in the default drive: a saved share, an
    /// upload, a test copy. The default drive then looked non-empty and the app
    /// kept listing it, showing one stray file while the phone showed the whole
    /// drive. Comparing `used_size` instead is stable, because a drive that
    /// really is the user's main drive is orders of magnitude larger.
    ///
    /// Runs at most once per process, and never when the choice was already
    /// made deliberately (`drive_chosen`).
    ///
    /// Returns `true` when the drive was switched.
    async fn adopt_resource_drive(&self) -> AppResult<bool> {
        use std::sync::atomic::Ordering;
        if self.drive_settled.swap(true, Ordering::SeqCst) {
            return Ok(false);
        }
        let (current, resource, chosen) = {
            let creds = self.creds.read().await;
            (
                creds.drive_id.clone(),
                creds.resource_drive_id.clone(),
                creds.drive_chosen,
            )
        };
        if chosen {
            crate::ylog!("aliyun: drive {current} was chosen explicitly, not probing");
            return Ok(false);
        }
        if resource.is_empty() || resource == current {
            return Ok(false);
        }
        let have = self.drive_used_size(&current).await.unwrap_or(0);
        let more = self.drive_used_size(&resource).await.unwrap_or(0);
        if more <= have {
            crate::ylog!(
                "aliyun: keeping drive {current} ({have} B); resource drive {resource} holds {more} B"
            );
            return Ok(false);
        }
        crate::ylog!(
            "aliyun: switching to resource drive {resource} ({more} B) from {current} ({have} B)"
        );
        {
            let mut creds = self.creds.write().await;
            creds.drive_id = resource;
            creds.drive_chosen = true;
        }
        self.persist().await;
        Ok(true)
    }

    /// Exchanges the refresh token for a new pair and persists it.
    ///
    /// Two endpoints exist for this. `auth.aliyundrive.com/v2/account/token`
    /// is what the reference client uses and wants an `api_id` plus
    /// `grant_type`; `api.aliyundrive.com/token/refresh` takes the bare token.
    /// The first is tried, the second is the fallback.
    async fn refresh(&self) -> AppResult<()> {
        let refresh_token = self.creds.read().await.refresh_token.clone();
        if refresh_token.trim().is_empty() {
            return Err(AppError::new("阿里云盘登录已过期，请重新扫码登录"));
        }
        let attempts = [
            (
                format!("{AUTH}/v2/account/token"),
                json!({
                    "refresh_token": refresh_token,
                    "api_id": "pJZInNHN2dZWk8qg",
                    "grant_type": "refresh_token",
                }),
            ),
            (
                format!("{API}/token/refresh"),
                json!({ "refresh_token": refresh_token }),
            ),
        ];
        let mut last_error = AppError::new("刷新失败，请重新扫码登录");
        for (url, body) in attempts {
            let res = self
                .http()?
                .post(&url)
                .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
                .header(reqwest::header::REFERER, "https://www.aliyundrive.com/")
                .header(reqwest::header::ORIGIN, "https://www.aliyundrive.com")
                .header(reqwest::header::USER_AGENT, UA)
                .json(&body)
                .send()
                .await;
            let text = match res {
                Ok(res) => match res.text().await {
                    Ok(text) => text,
                    Err(err) => {
                        last_error = AppError::new(format!("刷新响应读取失败：{err}"));
                        continue;
                    }
                },
                Err(err) => {
                    last_error = AppError::new(format!("刷新登录失败：{err}"));
                    continue;
                }
            };
            let value = match parse_body(&text) {
                Ok(value) => value,
                Err(err) => {
                    last_error = err;
                    continue;
                }
            };
            let Some(access) = value.get("access_token").and_then(|v| v.as_str()) else {
                last_error = AppError::new("刷新失败，请重新扫码登录");
                continue;
            };
            let expires_in = value
                .get("expires_in")
                .and_then(|v| v.as_i64())
                .unwrap_or(7200);
            let new_refresh = value
                .get("refresh_token")
                .and_then(|v| v.as_str())
                .unwrap_or(&refresh_token)
                .to_string();
            {
                let mut creds = self.creds.write().await;
                creds.access_token = access.to_string();
                creds.refresh_token = new_refresh;
                creds.expires_at = chrono::Utc::now().timestamp() + expires_in;
                // A refresh response carries the whole profile, so use it to
                // fill in whatever a bare token pair was missing.
                let text = |key: &str| {
                    value
                        .get(key)
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                        .map(|v| v.to_string())
                };
                if let Some(v) = text("default_drive_id") {
                    if !creds.drive_chosen {
                        creds.drive_id = v;
                    }
                }
                if let Some(v) = text("resource_drive_id") {
                    creds.resource_drive_id = v;
                }
                if let Some(v) = text("user_id") {
                    creds.user_id = v;
                }
                if let Some(v) = text("nick_name") {
                    creds.nickname = v;
                }
            }
            self.persist().await;
            return Ok(());
        }
        Err(last_error)
    }

    /// Returns `(device_id, x-signature)`, computing the signature once.
    ///
    /// The signature covers `"{appId}:{deviceId}:{userId}:0"` with a locally
    /// generated secp256k1 key; it is stable for the life of the device, so it
    /// is cached in `aliyun.json` rather than recomputed per request.
    async fn sign(&self) -> AppResult<(String, String)> {
        use k256::ecdsa::signature::hazmat::PrehashSigner;
        use k256::ecdsa::{Signature, SigningKey};
        use sha2::{Digest, Sha256};

        let (device_id, user_id, key_hex, cached) = {
            let creds = self.creds.read().await;
            (
                creds.device_id.clone(),
                creds.user_id.clone(),
                creds.private_key.clone(),
                creds.signature.clone(),
            )
        };
        if device_id.is_empty() {
            return Err(AppError::new("阿里云盘签名设备尚未注册"));
        }
        if user_id.is_empty() {
            return Err(AppError::new("阿里云盘账号信息缺失，请重新登录"));
        }
        if !cached.is_empty() {
            return Ok((device_id, cached));
        }
        let key = if key_hex.is_empty() {
            // Fresh 32-byte secret: the whole valid secp256k1 scalar range.
            let mut bytes = [0u8; 32];
            use k256::elliptic_curve::rand_core::RngCore;
            k256::elliptic_curve::rand_core::OsRng.fill_bytes(&mut bytes);
            let hex = hex::encode(bytes);
            self.creds.write().await.private_key = hex.clone();
            hex
        } else {
            key_hex
        };
        let secret =
            hex::decode(&key).map_err(|e| AppError::new(format!("阿里云盘签名密钥损坏：{e}")))?;
        let signing = SigningKey::from_slice(&secret)
            .map_err(|e| AppError::new(format!("阿里云盘签名密钥无效：{e}")))?;

        let data = format!("{APP_ID}:{device_id}:{user_id}:0");
        // The reference client signs the SHA-256 digest and ships r||s without
        // DER framing, hex-encoded, with a "01" version suffix.
        let digest = Sha256::digest(data.as_bytes());
        let signature: Signature = signing
            .sign_prehash(&digest)
            .map_err(|e| AppError::new(format!("签名计算失败：{e}")))?;
        let value = format!("{}01", hex::encode(signature.to_bytes()));
        self.creds.write().await.signature = value.clone();
        Ok((device_id, value))
    }

    /// Public key in the form `create_session` expects: `04` + X + Y.
    async fn public_key(&self) -> AppResult<String> {
        use k256::ecdsa::SigningKey;
        let key = self.creds.read().await.private_key.clone();
        let secret =
            hex::decode(&key).map_err(|e| AppError::new(format!("阿里云盘签名密钥损坏：{e}")))?;
        let signing = SigningKey::from_slice(&secret)
            .map_err(|e| AppError::new(format!("阿里云盘签名密钥无效：{e}")))?;
        let point = signing.verifying_key().to_encoded_point(false);
        Ok(hex::encode(point.as_bytes()))
    }

    /// Makes sure a device slot exists, registering one on first use.
    ///
    /// Called lazily: the first signed request needs a device slot, and the
    /// server rejects an unknown `x-device-id`.
    async fn ensure_session(&self, token: &str) -> AppResult<()> {
        let (registered, user_id) = {
            let creds = self.creds.read().await;
            (!creds.device_id.is_empty(), creds.user_id.clone())
        };
        if registered && !user_id.is_empty() {
            return Ok(());
        }
        self.register_device(token).await
    }

    /// (Re)registers the signing key for this device.
    ///
    /// The *device id* is reused when one is already stored: each new id takes
    /// one of the account's ten device slots, so rotating it on every stale
    /// signature would eventually lock the user out of their own drive. Only the
    /// key and its signature are regenerated — the same thing the reference
    /// client does when it retries after `DeviceSessionSignatureInvalid`.
    async fn register_device(&self, token: &str) -> AppResult<()> {
        crate::ylog!("aliyun: registering device (create_session)");
        if self.creds.read().await.user_id.is_empty() {
            self.fetch_user(token).await?;
        }
        let device_id = {
            let existing = self.creds.read().await.device_id.clone();
            if existing.is_empty() {
                random_device_id()
            } else {
                existing
            }
        };
        {
            let mut creds = self.creds.write().await;
            creds.device_id = device_id.clone();
            creds.private_key.clear();
            creds.signature.clear();
        }
        let (device_id, signature) = self.sign().await?;
        let public_key = self.public_key().await?;
        let body = json!({
            "deviceName": hostname(),
            "modelName": MODEL_NAME,
            "pubKey": public_key,
        });
        let value = self
            .raw_post(
                API,
                "/users/v1/users/device/create_session",
                &body,
                token,
                Some((&device_id, &signature)),
            )
            .await?;
        if value.get("result").and_then(|v| v.as_bool()) == Some(false) {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("设备注册失败");
            return Err(AppError::new(format!(
                "阿里云盘设备注册失败：{message}（可能已超过 10 台设备上限）"
            )));
        }
        self.persist().await;
        crate::ylog!("aliyun: device {device_id} registered");
        Ok(())
    }

    /// Fetches the account profile and registers the signing device.
    ///
    /// Called right after login so a broken signing identity surfaces as a login
    /// failure rather than as an empty file list later on.
    pub async fn prime(&self) -> AppResult<()> {
        let token = self.token().await?;
        self.fetch_user(&token).await?;
        self.ensure_session(&token).await
    }

    /// Fetches `/v2/user/get` and caches id / nickname / avatar.
    async fn fetch_user(&self, token: &str) -> AppResult<Value> {
        let value = self
            .raw_post(USER_API, "/v2/user/get", &json!({}), token, None)
            .await?;
        {
            let mut creds = self.creds.write().await;
            if let Some(id) = value.get("user_id").and_then(|v| v.as_str()) {
                creds.user_id = id.to_string();
            }
            if let Some(id) = value.get("default_drive_id").and_then(|v| v.as_str()) {
                // Do not undo a drive the user (or `adopt_resource_drive`) picked:
                // `/v2/user/get` always reports the *default* drive, and writing
                // that back on every refresh is what made the listing flip back
                // to the near-empty default drive after a restart.
                if !id.is_empty() && !creds.drive_chosen {
                    creds.drive_id = id.to_string();
                }
            }
            if let Some(id) = value.get("resource_drive_id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    creds.resource_drive_id = id.to_string();
                }
            }
            if let Some(nick) = value
                .get("nick_name")
                .or_else(|| value.get("user_name"))
                .and_then(|v| v.as_str())
            {
                creds.nickname = nick.to_string();
            }
            if let Some(avatar) = value.get("avatar").and_then(|v| v.as_str()) {
                creds.avatar = Some(avatar.to_string());
            }
        }
        self.persist().await;
        Ok(value)
    }

    /// One POST with the standard headers, optionally signed.
    async fn raw_post(
        &self,
        base: &str,
        path: &str,
        body: &Value,
        token: &str,
        sign: Option<(&str, &str)>,
    ) -> AppResult<Value> {
        self.raw_post_as(base, path, body, token, sign, None, &[])
            .await
    }

    /// Same as [`Self::raw_post`], with an explicit `X-Canary` client tag and
    /// extra headers.
    ///
    /// The client tag is not cosmetic: `/v2/file/get_download_url` answers with
    /// an **empty** `url` for every file above a few tens of megabytes when the
    /// tag says `web` (and even when it is absent), while
    /// `client=android,app=adrive,version=v6.9.0` returns a working link for the
    /// same file. Small files slip through on any tag, which is why this looked
    /// like a size limit for a while.
    #[allow(clippy::too_many_arguments)]
    async fn raw_post_as(
        &self,
        base: &str,
        path: &str,
        body: &Value,
        token: &str,
        sign: Option<(&str, &str)>,
        canary: Option<&str>,
        extra: &[(&str, &str)],
    ) -> AppResult<Value> {
        let mut request = self
            .http()?
            .post(format!("{base}{path}"))
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .header(reqwest::header::REFERER, "https://www.aliyundrive.com/")
            .header(reqwest::header::ORIGIN, "https://www.aliyundrive.com")
            .header(reqwest::header::USER_AGENT, UA)
            .header(reqwest::header::CONTENT_TYPE, "application/json;charset=UTF-8")
            .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            .json(body);
        if let Some(tag) = canary {
            request = request.header("X-Canary", tag);
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        if let Some((device, signature)) = sign {
            request = request
                .header("x-device-id", device)
                .header("x-signature", signature);
        }
        let res = request
            .send()
            .await
            .map_err(|e| AppError::new(format!("请求失败：{e}")))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取响应失败：{e}")))?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            // 403 is what an expired/blocked bearer looks like on some gateways.
            crate::ylog!("aliyun: POST {path} HTTP {status} → 登录已过期");
            return Err(AppError::new("阿里云盘登录已过期，请重新扫码登录"));
        }
        if !status.is_success() {
            crate::ylog!(
                "aliyun: POST {path} HTTP {status} body={}",
                &text.chars().take(300).collect::<String>()
            );
        }
        parse_body(&text)
    }

    /// POSTs to a signed file endpoint, registering the device first if needed.
    ///
    /// Three recoverable failures are retried once each:
    /// * `DeviceSessionSignatureInvalid` — the device slot was dropped, so the
    ///   identity is rotated and re-registered;
    /// * `TokenExpired` / `AccessTokenInvalid` — the access token is refreshed;
    /// * `TooManyRequests` — backed off and retried.
    async fn call(&self, path: &str, body: Value) -> AppResult<Value> {
        let mut stale_identity = false;
        let mut stale_token = false;
        let mut throttled = false;
        for attempt in 0..4 {
            if throttled {
                // The server asks for a pause, not a different request.
                tokio::time::sleep(std::time::Duration::from_millis(800 * (attempt as u64 + 1)))
                    .await;
                throttled = false;
            }
            if stale_token {
                self.refresh().await?;
                stale_token = false;
            }
            let token = self.token().await?;
            if stale_identity {
                self.register_device(&token).await?;
                stale_identity = false;
            } else {
                self.ensure_session(&token).await?;
            }
            let (device, signature) = self.sign().await?;
            match self
                .raw_post(API, path, &body, &token, Some((&device, &signature)))
                .await
            {
                Ok(value) => return Ok(value),
                Err(err) => {
                    let message = err.to_string();
                    if is_stale_signature(&message) {
                        stale_identity = true;
                    } else if message.contains("TokenExpired")
                        || message.contains("AccessTokenInvalid")
                    {
                        stale_token = true;
                    } else if message.contains("TooManyRequests") {
                        throttled = true;
                    } else {
                        return Err(err);
                    }
                }
            }
        }
        Err(AppError::new("阿里云盘请求失败，请稍后重试"))
    }

    /// Mints a byte url for one file of the user's own drive.
    ///
    /// `/v2/file/get_download_url` answers with an **empty** `url` for files
    /// above a few tens of megabytes when the request carries the web client
    /// tag (or none at all); `client=android,app=adrive,version=v6.9.0` returns
    /// a working link for the same file. Small files slip through on any tag,
    /// which is why this looked like a size limit at first. Both tags are tried
    /// — android first, web as a fallback in case the policy flips back.
    async fn download_url(&self, fid: &str) -> AppResult<(String, u64)> {
        let drive = self.drive().await?;
        let body = json!({ "drive_id": drive, "file_id": fid, "expire_sec": 14400 });
        let mut last = String::from("未知原因");
        for tag in [CANARY_ANDROID, CANARY_WEB] {
            let token = self.token().await?;
            let (device, signature) = self.sign().await?;
            let value = self
                .raw_post_as(
                    API,
                    "/v2/file/get_download_url",
                    &body,
                    &token,
                    Some((&device, &signature)),
                    Some(tag),
                    &[],
                )
                .await?;
            let size = value.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
            let url = value
                .get("url")
                .or_else(|| value.get("cdn_url"))
                .and_then(|v| v.as_str())
                .filter(|u| !u.is_empty());
            match url {
                Some(url) => return Ok((url.to_string(), size)),
                None => last = format!("size={size}"),
            }
        }
        Err(AppError::new(format!(
            "阿里云盘没有返回下载地址（{last}）"
        )))
    }

    fn entry(&self, item: &Value, parent: &str) -> EntryDto {
        let id = item
            .get("file_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let is_dir = item.get("type").and_then(|v| v.as_str()) == Some("folder");
        let size = item.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let updated = parse_rfc3339(item.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""));
        let created = parse_rfc3339(item.get("created_at").and_then(|v| v.as_str()).unwrap_or(""));
        EntryDto {
            fid: qualify(ProviderId::Aliyun, &id),
            pdir_fid: qualify(ProviderId::Aliyun, app_id(parent)),
            ext: EntryDto::ext_of(&name),
            name,
            size,
            is_dir,
            format_type: if is_dir { "folder".into() } else { "file".into() },
            updated_at: updated,
            created_at: created.max(updated),
            provider: ProviderId::Aliyun.as_str().to_string(),
        }
    }

    /// One page of `/adrive/v3/file/list`.
    async fn list_page(&self, dir: &str, marker: &str) -> AppResult<(Vec<EntryDto>, String)> {
        let drive = self.drive().await?;
        let mut body = json!({
            "drive_id": drive,
            "parent_file_id": api_id(dir),
            "limit": 100,
            "all": false,
            "url_expire_sec": 1600,
            "image_thumbnail_process": "image/resize,w_400/format,jpeg",
            "image_url_process": "image/resize,w_1920/format,jpeg",
            "video_thumbnail_process": "video/snapshot,t_0,f_jpg,ar_auto,w_800",
            "fields": "*",
            "order_by": "updated_at",
            "order_direction": "DESC",
        });
        if !marker.is_empty() {
            body["marker"] = json!(marker);
        }
        let value = self.call("/adrive/v3/file/list", body).await?;
        let items = value
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let next = value
            .get("next_marker")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        Ok((
            items.iter().map(|item| self.entry(item, dir)).collect(),
            next,
        ))
    }

    /// Sends one batch action (`/file/move`, `/recyclebin/trash`, …).
    async fn batch(&self, action: &str, drive: &str, fids: &[String]) -> AppResult<()> {
        let requests: Vec<Value> = fids
            .iter()
            .map(|fid| {
                json!({
                    "id": fid,
                    "method": "POST",
                    "url": action,
                    "headers": { "Content-Type": "application/json" },
                    "body": { "drive_id": drive, "file_id": fid },
                })
            })
            .collect();
        let value = self
            .call(
                "/adrive/v4/batch",
                json!({ "requests": requests, "resource": "file" }),
            )
            .await?;
        let responses = value
            .get("responses")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for response in &responses {
            let status = response.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
            if status >= 400 {
                let message = response
                    .get("body")
                    .and_then(|b| b.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("批量操作失败");
                return Err(AppError::new(format!("阿里云盘：{message}")));
            }
        }
        Ok(())
    }
}

/// Parses a response body, surfacing the API's own error message.
///
/// The web API reports business failures with HTTP 200 and `{code, message}`, so
/// the body itself has to be inspected.
fn parse_body(text: &str) -> AppResult<Value> {
    let value: Value = serde_json::from_str(text).unwrap_or_else(|_| {
        let snippet: String = text.chars().take(180).collect();
        json!({ "code": "PARSE_ERROR", "message": format!("响应解析失败：{snippet}") })
    });
    // HTTP 200 with a business error is normal for this API, and the code is not
    // always a string: `{"code":400}` must fail just like `{"code":"…"}`.
    let code = value.get("code").filter(|v| !v.is_null());
    if let Some(code) = code {
        let is_ok = matches!(code.as_i64(), Some(0))
            || matches!(code.as_u64(), Some(0))
            || code.as_str().is_some_and(|s| s.is_empty());
        if !is_ok {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("接口返回异常");
            let code = match code {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            return Err(AppError::new(format!("阿里云盘：{message}（{code}）")));
        }
    }
    Ok(value)
}

/// True when the error is the server saying "that device signature is unknown".
fn is_stale_signature(message: &str) -> bool {
    message.contains("DeviceSessionSignatureInvalid")
        || message.contains("签名过期")
        || message.contains("DeviceSessionSignature")
}

fn parse_rfc3339(raw: &str) -> u64 {
    if raw.is_empty() {
        return 0;
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}

/// 24 random alphanumerics, matching the reference client's device ids.
fn random_device_id() -> String {
    const SET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 24];
    {
        use k256::elliptic_curve::rand_core::RngCore;
        k256::elliptic_curve::rand_core::OsRng.fill_bytes(&mut bytes);
    }
    bytes
        .iter()
        .map(|b| SET[*b as usize % SET.len()] as char)
        .collect()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Linux".to_string())
}

/// Maps the app's root id onto 阿里云盘's, leaving real ids alone.
///
/// Every other drive uses `"0"` for the root; 阿里云盘 spells it `"root"`. Doing
/// the translation here keeps the frontend's `<provider>:0` convention intact.
fn api_id(id: &str) -> &str {
    if id.is_empty() || id == ROOT || id == API_ROOT {
        API_ROOT
    } else {
        id
    }
}

/// The inverse of [`api_id`]: the root comes back as `"0"`.
fn app_id(id: &str) -> &str {
    if id == API_ROOT {
        ROOT
    } else {
        id
    }
}

#[async_trait]
impl DriveProvider for AliyunProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Aliyun
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Share links are read through the anonymous share endpoints and
            // saved via `/adrive/v2/batch`; creating a share is not implemented.
            share_open: true,
            share_create: false,
            recycle: true,
            ..Capabilities::BASIC
        }
    }

    async fn auth_state(&self) -> AuthState {
        let creds = self.creds.read().await;
        AuthState {
            provider: ProviderId::Aliyun,
            authenticated: !creds.refresh_token.trim().is_empty(),
            source: "file".to_string(),
            masked: Some(creds.nickname.clone()).filter(|n| !n.is_empty()),
            credential_path: self.credential_path.clone(),
            nickname: Some(creds.nickname.clone()).filter(|n| !n.is_empty()),
            // Only an already-inlined avatar: a remote URL would be blocked by
            // the CSP. `profile()` replaces it with a `data:` URL once.
            avatar: creds
                .avatar
                .clone()
                .filter(|a| a.starts_with("data:")),
            capabilities: self.capabilities(),
        }
    }

    /// `creds.avatar` is a remote URL, but the CSP only allows `data:` — inline
    /// it once and remember the result.
    async fn profile(&self) -> AppResult<Option<AccountInfo>> {
        let (nickname, avatar) = {
            let creds = self.creds.read().await;
            (creds.nickname.clone(), creds.avatar.clone())
        };
        let avatar = avatar.unwrap_or_default();
        if avatar.starts_with("data:") {
            return Ok(Some(AccountInfo {
                nickname,
                avatar: Some(avatar),
            }));
        }
        if avatar.is_empty() {
            return self.account().await;
        }
        let Some(inlined) = crate::share::fetch_avatar(&self.http()?, &avatar).await else {
            return self.account().await;
        };
        {
            let mut creds = self.creds.write().await;
            creds.avatar = Some(inlined.clone());
        }
        // Persist so the next launch does not refetch.
        self.persist().await;
        Ok(Some(AccountInfo {
            nickname,
            avatar: Some(inlined),
        }))
    }

    async fn logout(&self) -> AppResult<()> {
        {
            let mut creds = self.creds.write().await;
            creds.access_token.clear();
            creds.refresh_token.clear();
        }
        let path = Path::new(&self.credential_path);
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }

    async fn account(&self) -> AppResult<Option<AccountInfo>> {
        let creds = self.creds.read().await;
        if creds.nickname.is_empty() {
            return Ok(None);
        }
        Ok(Some(AccountInfo {
            nickname: creds.nickname.clone(),
            // Remote URLs are useless to the UI (CSP); `profile()` inlines it.
            avatar: creds
                .avatar
                .clone()
                .filter(|a| a.starts_with("data:")),
        }))
    }

    async fn list(&self, dir: &str) -> AppResult<DirListing> {
        let dir = api_id(dir);
        let mut entries = Vec::new();
        let mut marker = String::new();
        let mut pages = 0u32;
        loop {
            let (page, next) = self.list_page(dir, &marker).await?;
            let count = page.len();
            entries.extend(page);
            marker = next;
            pages += 1;
            // A server that keeps handing out markers used to loop forever.
            if marker.is_empty() || count < 100 || pages >= 200 {
                break;
            }
        }
        // The phone app shows the resource drive, so settle which drive to use
        // before reporting the root — an empty default drive and a default
        // drive holding one stray file both used to hide the real content.
        if dir == API_ROOT && self.adopt_resource_drive().await? {
            return self.list(ROOT).await;
        }
        let total = entries.len() as u32;
        super::sort_entries(&mut entries);
        Ok(DirListing {
            pdir_fid: qualify(ProviderId::Aliyun, app_id(dir)),
            entries,
            total,
        })
    }

    async fn resolve_path(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        if dir.is_empty() || dir == ROOT || dir == API_ROOT {
            return Ok(Vec::new());
        }
        // `get_path` returns the chain leaf-first, with `root` last.
        let drive = self.drive().await?;
        let value = self
            .call(
                "/adrive/v1/file/get_path",
                json!({ "drive_id": drive, "file_id": api_id(dir) }),
            )
            .await?;
        let items = value
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut chain: Vec<PathNode> = items
            .iter()
            .rev()
            .filter_map(|item| {
                let id = item.get("file_id").and_then(|v| v.as_str())?;
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                // The synthetic root entry has no useful name.
                if id == API_ROOT {
                    return None;
                }
                Some(PathNode {
                    fid: qualify(ProviderId::Aliyun, id),
                    name: name.to_string(),
                })
            })
            .collect();
        if chain.is_empty() {
            // Fall back to a tree walk for drives where `get_path` is sparse.
            return self.walk_to(api_id(dir)).await;
        }
        chain.shrink_to_fit();
        Ok(chain)
    }

    async fn make_dir(&self, parent: &str, name: &str) -> AppResult<String> {
        let name = super::check_name(name)?;
        let drive = self.drive().await?;
        let value = self
            .call(
                "/adrive/v2/file/createWithFolders",
                json!({
                    "drive_id": drive,
                    "parent_file_id": api_id(parent),
                    "name": name,
                    "type": "folder",
                    "check_name_mode": "refuse",
                }),
            )
            .await?;
        let id = value
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::new("阿里云盘没有返回新建文件夹的 ID"))?;
        Ok(qualify(ProviderId::Aliyun, id))
    }

    async fn rename(&self, fid: &str, name: &str) -> AppResult<()> {
        let name = super::check_name(name)?;
        let drive = self.drive().await?;
        self.call(
            "/adrive/v3/file/update",
            json!({
                "drive_id": drive,
                "file_id": fid,
                "name": name,
                "check_name_mode": "refuse",
            }),
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, fids: &[String]) -> AppResult<()> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let drive = self.drive().await?;
        self.batch("/recyclebin/trash", &drive, fids).await
    }

    async fn transfer_entries(&self, fids: &[String], to: &str, copy: bool) -> AppResult<usize> {
        if fids.is_empty() {
            return Err(AppError::new("未选择任何文件"));
        }
        let drive = self.drive().await?;
        if copy {
            // Copy is a batch action too, but carries the destination instead of
            // just the file id.
            let requests: Vec<Value> = fids
                .iter()
                .map(|fid| {
                    json!({
                        "id": fid,
                        "method": "POST",
                        "url": "/file/copy",
                        "headers": { "Content-Type": "application/json" },
                        "body": {
                            "drive_id": drive,
                            "file_id": fid,
                            "to_parent_file_id": api_id(to),
                            "auto_rename": true,
                        },
                    })
                })
                .collect();
            let value = self
                .call(
                    "/adrive/v4/batch",
                    json!({ "requests": requests, "resource": "file" }),
                )
                .await?;
            check_batch(&value)?;
        } else {
            // Move takes the destination in the per-request body.
            let requests: Vec<Value> = fids
                .iter()
                .map(|fid| {
                    json!({
                        "id": fid,
                        "method": "POST",
                        "url": "/file/move",
                        "headers": { "Content-Type": "application/json" },
                        "body": {
                            "drive_id": drive,
                            "file_id": fid,
                            "to_drive_id": drive,
                            "to_parent_file_id": api_id(to),
                        },
                    })
                })
                .collect();
            let value = self
                .call(
                    "/adrive/v4/batch",
                    json!({ "requests": requests, "resource": "file" }),
                )
                .await?;
            check_batch(&value)?;
        }
        Ok(fids.len())
    }

    /// Server-side search via `/adrive/v3/file/search`.
    ///
    /// The query language is not free text: the server only accepts
    /// `name match "x"` / `name = "x"` and rejects a bare word with
    /// `InvalidParameter.Query`. `order_by` is rejected too, so the only
    /// pagination knob is `next_marker` (echoed back as `next_marker`).
    async fn search(&self, _dir: &str, keyword: &str, limit: u32) -> AppResult<Vec<SearchHit>> {
        let needle = keyword.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let drive = self.drive().await?;
        let query = format!("name match \"{}\"", needle.replace('"', " "));
        let mut hits = Vec::new();
        let mut marker = String::new();
        for _ in 0..20 {
            let mut body = json!({
                "drive_id": drive,
                "query": query,
                "limit": 100,
                "fields": "*",
            });
            if !marker.is_empty() {
                body["next_marker"] = json!(marker);
            }
            let value = self.call("/adrive/v3/file/search", body).await?;
            let items = value
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let count = items.len();
            for item in items {
                let parent = item
                    .get("parent_file_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let entry = self.entry(&item, &parent);
                let mut path = self.path_up(&parent, 0).await;
                path.push(PathNode {
                    fid: entry.fid.clone(),
                    name: entry.name.clone(),
                });
                hits.push(SearchHit { entry, path });
                if hits.len() >= limit as usize {
                    return Ok(hits);
                }
            }
            marker = value
                .get("next_marker")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if count < 100 || marker.is_empty() {
                break;
            }
        }
        Ok(hits)
    }

    async fn open_download(&self, fid: &str, start: u64) -> AppResult<DownloadHandle> {
        let (url, size) = self.download_url(fid).await?;

        let mut request = self
            .http()?
            .get(&url)
            // The CDN rejects bare requests: it wants the same browser-shaped
            // headers the rest of the API gets, otherwise every download comes
            // back as HTTP 400.
            .header(reqwest::header::REFERER, "https://www.aliyundrive.com/")
            .header(reqwest::header::USER_AGENT, UA);
        if start > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={start}-"));
        }
        let res = request
            .send()
            .await
            .map_err(|e| AppError::new(format!("下载失败：{e}")))?;
        if !res.status().is_success() && res.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            crate::ylog!("aliyun: download HTTP {status} body={}", &body[..body.len().min(300)]);
            return Err(AppError::new(format!("下载失败（HTTP {status}）：{body}")));
        }
        // A CDN that ignores `Range` answers 200 with the whole body. Appending
        // that to an existing `.part` would corrupt the file, so refuse: the
        // caller drops the partial and starts over.
        if start > 0 && res.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(AppError::new(RANGE_IGNORED));
        }
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
        let (sha1, size) = sha1_file(local, sink.clone(), token.clone()).await?;
        let parts = size.div_ceil(PART_SIZE).max(1);
        sink.phase("uploading");

        let drive = self.drive().await?;
        // 1. Create the upload slot; a known hash makes rapid-upload possible.
        let part_info: Vec<Value> = (1..=parts).map(|n| json!({ "part_number": n })).collect();
        let created = self
            .call(
                "/adrive/v2/file/createWithFolders",
                json!({
                    "drive_id": drive,
                    "parent_file_id": api_id(parent),
                    "name": name,
                    "type": "file",
                    "size": size,
                    "check_name_mode": "auto_rename",
                    "part_info_list": part_info,
                    "content_hash_name": "sha1",
                    "content_hash": sha1,
                    "proof_version": "v1",
                }),
            )
            .await?;
        let file_id = created
            .get("file_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::new("阿里云盘没有返回文件 ID"))?
            .to_string();
        if created.get("rapid_upload").and_then(|v| v.as_bool()) == Some(true) {
            sink.progress(size);
            return Ok(Some(qualify(ProviderId::Aliyun, &file_id)));
        }

        let upload_id = created
            .get("upload_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        // 2. PUT each part to its presigned url.
        let mut file = tokio::fs::File::open(local).await?;
        let mut sent = 0u64;
        for part in 1..=parts {
            if token.is_cancelled() {
                return Err(AppError::new("已取消"));
            }
            // Read exactly one part, bounded by what is left of the file: a
            // short read must not be mistaken for the end of the file, because
            // committing an upload with a missing part silently truncates it.
            let want = PART_SIZE.min(size.saturating_sub(sent));
            let mut buf = vec![0u8; want as usize];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                return Err(AppError::new(format!(
                    "本地文件在上传第 {part} 片时提前结束（已发送 {sent} / {size} 字节）"
                )));
            }
            // Refresh the presigned url right before each PUT: they expire, and
            // a long upload can outlive the first batch.
            let urls = self
                .call(
                    "/v2/file/get_upload_url",
                    json!({
                        "drive_id": drive,
                        "file_id": file_id,
                        "upload_id": upload_id,
                        "part_info_list": [{ "part_number": part }],
                    }),
                )
                .await?;
            let part_url = urls
                .get("part_info_list")
                .and_then(|v| v.as_array())
                .and_then(|list| list.first())
                .and_then(|p| p.get("upload_url"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| AppError::new(format!("没有拿到分片 {part} 的上传地址")))?;
            let res = self
                .http()?
                .put(part_url)
                .header(reqwest::header::REFERER, "https://www.aliyundrive.com/")
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
        }
        if sent != size {
            return Err(AppError::new(format!(
                "上传不完整（{sent} / {size} 字节），已放弃提交"
            )));
        }

        // 3. Commit. `ignoreError` matches the reference client and makes the
        // server tolerate a part that was already accounted for.
        self.call(
            "/v2/file/complete",
            json!({
                "ignoreError": true,
                "drive_id": drive,
                "file_id": file_id,
                "upload_id": upload_id,
            }),
        )
        .await?;
        Ok(Some(qualify(ProviderId::Aliyun, &file_id)))
    }

    async fn quota(&self) -> AppResult<Option<QuotaInfo>> {
        let token = self.token().await?;
        let value = self
            .raw_post(API, "/v2/databox/get_personal_info", &json!({}), &token, None)
            .await?;
        let total = value
            .pointer("/personal_space_info/total_size")
            .and_then(|v| v.as_u64());
        let used = value
            .pointer("/personal_space_info/used_size")
            .and_then(|v| v.as_u64());
        match (used, total) {
            (Some(used), Some(total)) if total > 0 => Ok(Some(QuotaInfo { used, total })),
            _ => Ok(None),
        }
    }

    // ---------------------------------------------------------- recycle ---

    async fn recycle_list(&self, offset: u32, limit: u32) -> AppResult<Vec<RecycleEntry>> {
        // The bin is paged by `marker`; replay it to reach `offset`.
        let drive = self.drive().await?;
        let size = limit.max(1);
        let mut marker = String::new();
        let mut seen = 0u32;
        loop {
            let value = self
                .call(
                    "/adrive/v2/recyclebin/list",
                    json!({ "drive_id": drive, "limit": size, "marker": marker }),
                )
                .await?;
            let rows = value
                .get("items")
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
                            .get("file_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        RecycleEntry {
                            fid: qualify(ProviderId::Aliyun, &native),
                            name: row
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            is_dir: row.get("type").and_then(|v| v.as_str()) == Some("folder"),
                            size: row.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                            deleted_at: row
                                .get("trashed_at")
                                .map(super::parse_time)
                                .unwrap_or(0),
                            token: String::new(),
                            provider: ProviderId::Aliyun.as_str().to_string(),
                        }
                    })
                    .collect());
            }
            seen += rows.len() as u32;
            marker = value
                .get("next_marker")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if rows.is_empty() || marker.is_empty() {
                return Ok(Vec::new());
            }
        }
    }

    async fn recycle_restore(&self, entries: &[RecycleEntry]) -> AppResult<u32> {
        if entries.is_empty() {
            return Err(AppError::new("请先选择要恢复的内容"));
        }
        let drive = self.drive().await?;
        let fids: Vec<String> = entries.iter().map(|e| super::native_of(&e.fid)).collect();
        self.batch("/recyclebin/restore", &drive, &fids).await?;
        Ok(entries.len() as u32)
    }

    async fn recycle_clear(&self) -> AppResult<()> {
        // Per-item purge is not exposed by the API at all (`/v2/recyclebin/delete`
        // and the batch variant both 404/400), but emptying the bin is — and it
        // lives under `/v2/`, not `/adrive/v2/`.
        let drive = self.drive().await?;
        let token = self.token().await?;
        self.raw_post_as(
            API,
            "/v2/recyclebin/clear",
            &json!({ "drive_id": drive }),
            &token,
            None,
            Some(CANARY_ANDROID),
            &[],
        )
        .await?;
        Ok(())
    }

    // ---------------------------------------------------------- shares ---

    async fn share_open(&self, id: &str, passcode: &str) -> AppResult<ShareSession> {
        let token = self.share_token(id, passcode).await?;
        let info = self
            .share_call(
                "/adrive/v3/share_link/get_share_by_anonymous",
                &json!({ "share_id": id }),
                &token,
            )
            .await?;
        let creator = info
            .get("creator_name")
            .or_else(|| info.get("display_name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let avatar = info
            .get("avatar")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string());
        let owner_avatar = match avatar {
            Some(url) => crate::share::fetch_avatar(&self.http()?, &url).await,
            None => None,
        };
        let title = info
            .get("share_name")
            .or_else(|| info.get("share_title"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let first_fid = info
            .pointer("/file_infos/0/file_id")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        Ok(ShareSession {
            provider: ProviderId::Aliyun.as_str().to_string(),
            pwd_id: id.to_string(),
            passcode: passcode.to_string(),
            // The share token is what every later call carries.
            stoken: token,
            title,
            owner: creator,
            owner_avatar,
            file_num: info.get("file_count").and_then(|v| v.as_u64()).unwrap_or(0),
            total_size: 0,
            expired_at: 0,
            first_fid,
            can_save: true,
            web_url: Some(format!("https://www.alipan.com/s/{id}")),
        })
    }

    async fn share_list(
        &self,
        share: &crate::transfer::ShareRef,
        dir: &str,
    ) -> AppResult<ShareListing> {
        let parent = if dir.is_empty() { API_ROOT } else { dir };
        let mut marker = String::new();
        let mut entries = Vec::new();
        let mut total = 0u64;
        loop {
            let value = self
                .share_call(
                    "/adrive/v3/file/list",
                    &json!({
                        "share_id": share.pwd_id,
                        "parent_file_id": parent,
                        "limit": 100,
                        "marker": marker,
                        "order_by": "name",
                        "order_direction": "ASC",
                        "fields": "*",
                    }),
                    &share.stoken,
                )
                .await?;
            let items = value
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            total = value
                .get("total_file_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(total);
            for item in &items {
                let fid = item
                    .get("file_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                entries.push(ShareEntry {
                    fid,
                    token: String::new(),
                    name,
                    size: item.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                    is_dir: item.get("type").and_then(|v| v.as_str()) == Some("folder"),
                    updated_at: super::parse_time(
                        item.get("updated_at").unwrap_or(&Value::Null),
                    ) as i64,
                });
            }
            marker = value
                .get("next_marker")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if marker.is_empty() || items.len() < 100 || entries.len() as u64 >= total {
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
        let drive = self.drive().await?;
        let parent = match to {
            Some(id) if !id.is_empty() && id != ROOT => super::native_of(id).to_string(),
            _ => API_ROOT.to_string(),
        };
        let mut saved = 0u64;
        for entry in entries {
            // One request per file: `/file/copy` reports failures per item, and
            // a batch that fails half-way is harder to explain to the user.
            let body = json!({
                "requests": [{
                    "id": "0",
                    "method": "POST",
                    "url": "/file/copy",
                    "headers": { "Content-Type": "application/json" },
                    "body": {
                        "share_id": share.pwd_id,
                        "file_id": entry.fid,
                        "auto_rename": true,
                        "to_drive_id": drive,
                        "to_parent_file_id": parent,
                    },
                }],
                "resource": "file",
            });
            let value = self
                .share_call("/adrive/v2/batch", &body, &share.stoken)
                .await?;
            let status = value
                .pointer("/responses/0/status")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if !(200..300).contains(&status) {
                let message = value
                    .pointer("/responses/0/body/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("未知原因");
                return Err(AppError::new(format!("转存「{}」失败：{message}", entry.name)));
            }
            saved += 1;
        }
        Ok(saved)
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
            if dir.is_empty() { API_ROOT.to_string() } else { dir.to_string() },
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
        // Aliyun never exposes a share file as bytes: `get_share_link_download_url`
        // answers 410 with an empty body. The only route is to copy the file
        // into the user's own drive, download that copy and delete it again —
        // which is what `cleanup_fid` exists for.
        let drive = self.drive().await?;
        let body = json!({
            "requests": [{
                "id": "0",
                "method": "POST",
                "url": "/file/copy",
                "headers": { "Content-Type": "application/json" },
                "body": {
                    "share_id": share.pwd_id,
                    "file_id": fid,
                    "auto_rename": true,
                    "to_drive_id": drive,
                    "to_parent_file_id": API_ROOT,
                },
            }],
            "resource": "file",
        });
        let value = self
            .share_call("/adrive/v2/batch", &body, &share.stoken)
            .await?;
        let status = value
            .pointer("/responses/0/status")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let copy = value
            .pointer("/responses/0/body/file_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if !(200..300).contains(&status) || copy.is_empty() {
            let message = value
                .pointer("/responses/0/body/message")
                .and_then(|v| v.as_str())
                .unwrap_or("未知原因");
            return Err(AppError::new(format!("转存失败：{message}")));
        }
        // A freshly copied file is transcoded for a moment and reports an empty
        // url until it settles, so give it a few tries.
        let mut last = String::from("未知原因");
        for attempt in 0..6 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            }
            match self.download_url(&copy).await {
                Ok((url, size)) => {
                    return Ok(ShareLink {
                        url,
                        headers: vec![
                            ("Referer".to_string(), "https://www.aliyundrive.com/".to_string()),
                            ("User-Agent".to_string(), UA.to_string()),
                        ],
                        size,
                        cleanup_fid: Some(copy),
                    });
                }
                Err(err) => last = err.to_string(),
            }
        }
        Err(AppError::new(format!(
            "阿里云盘分享文件无法获取下载地址（{last}）"
        )))
    }

    async fn share_cleanup(&self, fid: &str) {
        let Ok(drive) = self.drive().await else {
            return;
        };
        let body = json!({ "drive_id": drive, "file_id": fid });
        if let Err(err) = self.call("/v2/file/delete", body).await {
            crate::ylog!("aliyun: share copy {fid} not deleted: {err}");
        }
    }
}

impl AliyunProvider {
    /// Exchanges a share link (plus passcode) for a share token.
    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn debug_download_url(&self, fid: &str) -> AppResult<(String, u64)> {
        self.download_url(fid).await
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn debug_call(&self, path: &str, body: Value) -> AppResult<Value> {
        self.call(path, body).await
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub async fn debug_drive_id(&self) -> String {
        self.drive().await.unwrap_or_default()
    }

    async fn share_token(&self, id: &str, passcode: &str) -> AppResult<String> {
        let token = self.token().await?;
        let mut body = json!({ "share_id": id });
        if !passcode.is_empty() {
            body["share_pwd"] = json!(passcode);
        }
        let value = self
            .raw_post_as(
                API,
                "/v2/share_link/get_share_token",
                &body,
                &token,
                None,
                Some(CANARY_SHARE),
                &[],
            )
            .await?;
        value
            .get("share_token")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string())
            .ok_or_else(|| AppError::new("无法获取分享信息，请检查链接和提取码"))
    }

    /// Calls a share-scoped endpoint with `x-share-token`.
    async fn share_call(
        &self,
        path: &str,
        body: &Value,
        share_token: &str,
    ) -> AppResult<Value> {
        let token = self.token().await?;
        self.raw_post_as(
            API,
            path,
            body,
            &token,
            None,
            Some(CANARY_SHARE),
            &[("x-share-token", share_token)],
        )
        .await
    }

    /// Breadcrumb from the root down to `fid`, climbing `/v2/file/get`.
    ///
    /// Search only reports `parent_file_id`; the web API has no
    /// "path of this file" call, so we walk upwards (bounded, one request per
    /// level).
    async fn path_up(&self, fid: &str, _depth: u32) -> Vec<PathNode> {
        let mut acc = Vec::new();
        let mut cursor = fid.to_string();
        for _ in 0..12 {
            if cursor.is_empty() || cursor == API_ROOT || cursor == ROOT {
                break;
            }
            let Ok(value) = self
                .call(
                    "/v2/file/get",
                    json!({ "drive_id": self.drive().await.unwrap_or_default(), "file_id": cursor }),
                )
                .await
            else {
                break;
            };
            let name = value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            acc.push(PathNode {
                fid: qualify(ProviderId::Aliyun, &cursor),
                name,
            });
            cursor = value
                .get("parent_file_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
        }
        acc.reverse();
        acc
    }

    /// Tree-walk fallback for [`DriveProvider::resolve_path`].
    async fn walk_to(&self, dir: &str) -> AppResult<Vec<PathNode>> {
        let mut queue: std::collections::VecDeque<(String, Vec<(String, String)>)> =
            std::collections::VecDeque::new();
        queue.push_back((API_ROOT.to_string(), Vec::new()));
        let mut visited = 0;
        while let Some((parent, path)) = queue.pop_front() {
            visited += 1;
            if visited > 200 {
                break;
            }
            let Ok(entries) = self.list(&parent).await else {
                continue;
            };
            for entry in entries.entries {
                if !entry.is_dir {
                    continue;
                }
                let native = super::native_of(&entry.fid);
                if native == dir {
                    let mut chain: Vec<PathNode> = path
                        .iter()
                        .map(|(fid, name)| PathNode {
                            fid: qualify(ProviderId::Aliyun, fid),
                            name: name.clone(),
                        })
                        .collect();
                    chain.push(PathNode {
                        fid: qualify(ProviderId::Aliyun, &native),
                        name: entry.name,
                    });
                    return Ok(chain);
                }
                if path.len() < 10 {
                    let mut next = path.clone();
                    next.push((native.to_string(), entry.name.clone()));
                    queue.push_back((native.to_string(), next));
                }
            }
        }
        Ok(Vec::new())
    }
}

/// Rejects a batch response whose individual actions failed.
fn check_batch(value: &Value) -> AppResult<()> {
    let responses = value
        .get("responses")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for response in &responses {
        // A missing `status` used to fall back to 0, i.e. "fine". Treat an
        // unreadable entry as a failure: a batch that reports success for an
        // operation the server rejected leaves the UI lying about the result.
        let Some(status) = response.get("status").and_then(|v| v.as_u64()) else {
            let message = response
                .get("body")
                .and_then(|b| b.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("批量操作返回了无法识别的结果");
            return Err(AppError::new(format!("阿里云盘：{message}")));
        };
        if status >= 400 {
            let message = response
                .get("body")
                .and_then(|b| b.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("批量操作失败");
            return Err(AppError::new(format!("阿里云盘：{message}")));
        }
    }
    Ok(())
}

/// Hashes a file with SHA-1, reporting bytes hashed through `sink`.
async fn sha1_file(
    path: &Path,
    sink: Arc<dyn UploadSink>,
    token: CancellationToken,
) -> AppResult<(String, u64)> {
    use sha1::{Digest, Sha1};
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha1::new();
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
mod tests {
    use super::*;

    /// The app's root is `"0"`; 阿里云盘's is `"root"`. Both spellings must
    /// round-trip, and real ids must pass through untouched.
    #[test]
    fn translates_the_root_id_both_ways() {
        assert_eq!(api_id("0"), "root");
        assert_eq!(api_id(""), "root");
        assert_eq!(api_id("root"), "root");
        assert_eq!(api_id("6321a1b2c3"), "6321a1b2c3");
        assert_eq!(app_id("root"), "0");
        assert_eq!(app_id("6321a1b2c3"), "6321a1b2c3");
    }

    #[test]
    fn parses_timestamps_and_tolerates_garbage() {
        assert_eq!(parse_rfc3339(""), 0);
        assert_eq!(parse_rfc3339("not a date"), 0);
        assert!(parse_rfc3339("2026-11-08T10:00:00.000Z") > 1_700_000_000_000);
    }

    /// The web API reports errors in `code`/`message` with HTTP 200, so the body
    /// itself has to be inspected.
    #[test]
    fn surfaces_api_errors_from_a_200_body() {
        let err = parse_body(r#"{"code":"NotFound.File","message":"文件不存在"}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("文件不存在"), "{err}");
        assert!(parse_body(r#"{"file_id":"abc"}"#).is_ok());
    }

    #[test]
    fn recognises_a_stale_device_signature() {
        assert!(is_stale_signature(
            "阿里云盘：签名过期（DeviceSessionSignatureInvalid）"
        ));
        assert!(!is_stale_signature("阿里云盘：文件不存在"));
    }

    #[test]
    fn device_ids_look_like_the_reference_client() {
        let id = random_device_id();
        assert_eq!(id.len(), 24);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(id, random_device_id());
    }

    /// The signature format is fixed by the server: r||s (64 bytes) hex-encoded
    /// with a `"01"` version suffix, and a public key prefixed with `"04"`.
    #[tokio::test]
    async fn signature_and_public_key_have_the_expected_shape() {
        let creds = AliyunCredentials {
            refresh_token: "r".into(),
            access_token: "a".into(),
            drive_id: "d".into(),
            user_id: "u".into(),
            device_id: "d".repeat(24),
            ..Default::default()
        };
        let provider = AliyunProvider::new(creds, String::new());
        let (device, signature) = provider.sign().await.unwrap();
        assert_eq!(device.len(), 24);
        // 64 bytes of r||s, hex (128 chars), plus the "01" suffix.
        assert_eq!(signature.len(), 130, "{signature}");
        assert!(signature.ends_with("01"));
        assert!(signature[..128].chars().all(|c| c.is_ascii_hexdigit()));

        let public = provider.public_key().await.unwrap();
        // "04" + 32-byte X + 32-byte Y, hex.
        assert_eq!(public.len(), 130, "{public}");
        assert!(public.starts_with("04"));

        // Cached: a second call must not mint a new signature.
        assert_eq!(provider.sign().await.unwrap().1, signature);
    }

    /// Pins the wire format against an independent implementation (RFC 6979
    /// deterministic ECDSA, low-s normalised, r||s hex + "01").
    #[tokio::test]
    async fn signature_matches_a_reference_implementation() {
        let creds = AliyunCredentials {
            refresh_token: "r".into(),
            access_token: "a".into(),
            user_id: "user-1".into(),
            device_id: "a".repeat(24),
            private_key: "01".repeat(32),
            ..Default::default()
        };
        let provider = AliyunProvider::new(creds, String::new());
        let (_, signature) = provider.sign().await.unwrap();
        assert_eq!(
            signature,
            "a92d7167232f92bd88cedda3a67ceeb638488f9ec69ced1d4b02c078756b39c46d87a863507c76ae15d0cf9e0e23e356999f280f13ff4a0589a8a9817c8130e001"
        );
        let public = provider.public_key().await.unwrap();
        assert_eq!(
            public,
            "041b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f70beaf8f588b541507fed6a642c5ab42dfdf8120a7f639de5122d47a69a8e8d1"
        );
    }

    #[tokio::test]
    async fn signing_needs_a_registered_device() {
        let creds = AliyunCredentials {
            refresh_token: "r".into(),
            user_id: "u".into(),
            ..Default::default()
        };
        let provider = AliyunProvider::new(creds, String::new());
        let err = provider.sign().await.unwrap_err().to_string();
        assert!(err.contains("设备尚未注册"), "{err}");
    }

    /// Live check of the refresh endpoints and their headers.
    ///
    /// A bogus token must come back as the API's own "invalid refresh token"
    /// error — anything else means the URL, the headers or the body shape is
    /// wrong. Ignored by default because it needs the network:
    /// `cargo test --lib live_refresh -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_refresh_reports_the_api_error() {
        let creds = AliyunCredentials {
            refresh_token: "bogus-refresh-token".into(),
            ..Default::default()
        };
        let provider = AliyunProvider::new(creds, String::new());
        let err = provider.refresh().await.unwrap_err().to_string();
        println!("refresh error: {err}");
        assert!(
            err.contains("InvalidParameter") || err.contains("refresh_token"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unwraps_token_pairs() {
        let creds = AliyunCredentials {
            refresh_token: "r".into(),
            access_token: "a".into(),
            ..Default::default()
        };
        let provider = AliyunProvider::new(creds, String::new());
        assert!(provider.capabilities().folders);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Reproduces the user's HTTP 400 download of `refind.zip`.
    ///
    /// `cargo test --lib -- --ignored --nocapture aliyun_probe_download`
    #[tokio::test]
    #[ignore]
    async fn aliyun_probe_download() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = AliyunProvider::load(&dir).expect("aliyun.json");
        let listing = drive.list("root").await.expect("root listing");
        println!("root: {} entries, drive={:?}", listing.entries.len(), drive.drive().await);
        for e in listing.entries.iter() {
            println!("  {} {} size={} dir={}", e.fid, e.name, e.size, e.is_dir);
        }
        let mut target: Option<EntryDto> = listing
            .entries
            .iter()
            .find(|e| e.name.contains("refind"))
            .cloned();
        if target.is_none() {
            // Search one level deep for the reported name.
            for e in listing.entries.iter().filter(|e| e.is_dir).take(6) {
                let native = crate::providers::native_of(&e.fid);
                if let Ok(sub) = drive.list(&native).await {
                    if let Some(hit) = sub.entries.iter().find(|x| x.name.contains("refind")) {
                        println!("found in {}: {} {}", e.name, hit.fid, hit.name);
                        target = Some(hit.clone());
                        break;
                    }
                }
            }
        }
        let Some(target) = target else {
            println!("refind.zip not found; trying the first file");
            let Some(first) = listing.entries.iter().find(|e| !e.is_dir).cloned() else {
                return;
            };
            probe(&drive, &first).await;
            return;
        };
        probe(&drive, &target).await;

        async fn probe(drive: &AliyunProvider, entry: &EntryDto) {
            let native = crate::providers::native_of(&entry.fid);
            println!("target {} fid={} size={}", entry.name, native, entry.size);
            let value = drive
                .call(
                    "/v2/file/get_download_url",
                    json!({ "drive_id": drive.drive().await.unwrap(), "file_id": native, "expire_sec": 14400 }),
                )
                .await;
            match value {
                Ok(v) => {
                    let url = v.get("url").and_then(|u| u.as_str()).unwrap_or_default();
                    println!("url = {}", &url[..url.len().min(150)]);
                    for (label, headers) in [
                        ("bare", vec![]),
                        ("referer+ua", vec![("referer", "https://www.aliyundrive.com/"), ("user-agent", UA)]),
                        ("range only", vec![("range", "bytes=0-")]),
                        ("referer+ua+range", vec![("referer", "https://www.aliyundrive.com/"), ("user-agent", UA), ("range", "bytes=100-")]),
                    ] {
                        let mut req = drive.http().unwrap().get(url);
                        for (k, val) in headers {
                            req = req.header(k, val);
                        }
                        match req.send().await {
                            Ok(res) => {
                                let status = res.status().as_u16();
                                let body = res.text().await.unwrap_or_default();
                                println!("  {label}: HTTP {status} body={}", &body[..body.len().min(160)]);
                            }
                            Err(err) => println!("  {label}: ERR {err}"),
                        }
                    }
                }
                Err(err) => println!("get_download_url ERR {err}"),
            }
        }
    }

    /// Live probe against whatever 阿里云盘 account this machine is signed into.
    ///
    /// `cargo test --lib aliyun_live -- --ignored --nocapture`
    ///
    /// Reproduces the login tail end step by step, each under a timeout, so a
    /// hang names the call that hung instead of stalling the whole test run.
    #[tokio::test]
    #[ignore]
    async fn aliyun_live_prime_steps() {
        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = AliyunProvider::load(&dir).expect("aliyun.json");

        {
            let creds = drive.creds.read().await;
            println!(
                "creds: access_len={} refresh_len={} drive_id={:?} user_id={:?} device_len={} key_len={} sig_len={} expires_at={}",
                creds.access_token.len(),
                creds.refresh_token.len(),
                creds.drive_id,
                creds.user_id,
                creds.device_id.len(),
                creds.private_key.len(),
                creds.signature.len(),
                creds.expires_at,
            );
        }

        async fn step<F>(name: &str, fut: F) -> bool
        where
            F: std::future::Future<Output = AppResult<()>>,
        {
            match tokio::time::timeout(std::time::Duration::from_secs(20), fut).await {
                Ok(Ok(())) => {
                    println!("{name}: ok");
                    true
                }
                Ok(Err(err)) => {
                    println!("{name}: error {err}");
                    false
                }
                Err(_) => {
                    println!("{name}: TIMED OUT after 20s");
                    false
                }
            }
        }

        let token = drive.token().await.expect("token");
        println!("token: len={}", token.len());

        if !step("fetch_user", async {
            drive.fetch_user(&token).await.map(|_| ())
        })
        .await
        {
            return;
        }

        if !step("ensure_session", async {
            drive.ensure_session(&token).await
        })
        .await
        {
            return;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(20), drive.list(ROOT)).await {
            Ok(Ok(listing)) => {
                println!("list root: {} entries", listing.entries.len());
                for e in listing.entries.iter().take(10) {
                    println!("  {} dir={} size={}", e.name, e.is_dir, e.size);
                }
                // Every real account has *something*; an empty root means the
                // drive id is wrong (default drive instead of the resource one).
                assert!(
                    !listing.entries.is_empty(),
                    "root listing came back empty — drive selection is broken"
                );
                if let Some(folder) = listing.entries.iter().find(|e| e.is_dir) {
                    let sub = drive
                        .list(&super::super::native_of(&folder.fid))
                        .await
                        .expect("subfolder listing");
                    println!("subfolder {} -> {} entries", folder.name, sub.entries.len());
                    assert!(!sub.entries.is_empty(), "subfolder listed empty");
                }
            }
            Ok(Err(err)) => println!("list root: error {err}"),
            Err(_) => println!("list root: TIMED OUT"),
        }

        match tokio::time::timeout(std::time::Duration::from_secs(20), drive.quota()).await {
            Ok(Ok(Some(q))) => println!("quota: used {} / total {}", q.used, q.total),
            Ok(Ok(None)) => println!("quota: none"),
            Ok(Err(err)) => println!("quota: error {err}"),
            Err(_) => println!("quota: TIMED OUT"),
        }
    }

    /// Upload → list → download → delete on the real account, checking that the
    /// phases and byte counts the UI relies on are actually produced.
    ///
    /// `cargo test --lib aliyun_live_upload -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn aliyun_live_upload_progress_and_download() {
        use crate::providers::{CounterSink, UploadSink};
        use futures_util::StreamExt;
        use std::sync::atomic::AtomicU64;

        let dir = crate::config::AppPaths::resolve().unwrap().config_dir;
        let drive = AliyunProvider::load(&dir).expect("aliyun.json");

        // Unique content so the server cannot answer with a rapid upload: the
        // point here is the chunked path.
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
        let listing = drive.list(ROOT).await.expect("root listing");
        let hit = listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .expect("uploaded file should be listed");
        println!("listed {} size={} fid={}", hit.name, hit.size, hit.fid);
        assert_eq!(hit.size, payload.len() as u64);

        let native = super::super::native_of(&fid);
        let mut handle = drive.open_download(&native, 0).await.expect("download");
        let first = handle.stream.next().await.expect("chunk").expect("bytes");
        println!("download: {} bytes", first.len());
        assert_eq!(&first[..4], &payload[..4]);
        drop(handle);
        let mut resumed = drive.open_download(&native, 100).await.expect("resume");
        let chunk = resumed.stream.next().await.expect("chunk").expect("bytes");
        assert_eq!(&chunk[..8], &payload[100..108]);
        drop(resumed);

        // Rename, then move into a real folder and back: both go through the
        // batch endpoint, whose per-item status handling is easy to get wrong.
        let renamed = format!("{name}.renamed");
        drive.rename(&native, &renamed).await.expect("rename");
        let listing = drive.list(ROOT).await.expect("root listing after rename");
        assert!(
            listing.entries.iter().any(|e| e.name == renamed),
            "rename did not show up in the listing"
        );
        if let Some(folder) = listing.entries.iter().find(|e| e.is_dir) {
            let target = super::super::native_of(&folder.fid);
            drive
                .transfer_entries(std::slice::from_ref(&native), &target, false)
                .await
                .expect("move");
            let inside = drive.list(&target).await.expect("target listing");
            assert!(
                inside.entries.iter().any(|e| e.name == renamed),
                "moved file should be inside {}",
                folder.name
            );
            drive
                .transfer_entries(std::slice::from_ref(&native), ROOT, false)
                .await
                .expect("move back");
            println!("moved {renamed} into {} and back", folder.name);
        }

        // Commands hand providers *native* ids; a qualified id here is how a
        // delete would land on a non-existent file.
        drive.delete(&[native]).await.expect("delete");
        let _ = std::fs::remove_file(&local);
        println!("deleted {renamed}");
    }
}

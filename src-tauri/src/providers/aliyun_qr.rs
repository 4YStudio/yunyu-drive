//! 阿里云盘扫码登录（Web 端 token）。
//!
//! 开放平台（openapi）的 token 需要注册应用才有 client_id，而普通用户拿不到。
//! 所以这里走网页版那条路：passport 出二维码 → 手机 App 扫码确认 → 用
//! `bizExt` 里的 App 令牌换 authorization code → 换到 web access/refresh token。
//!
//! 流程来自 `gngpp/aliyundrive-cli`（MIT，见 NOTICE），用 Rust 重新实现。
//! 关键点：
//! * 二维码生成返回 `t` + `ck`，查询时原样回传，缺一不可；
//! * 扫描状态依次是 NEW → SCANED → CONFIRMED，只有 CONFIRMED 才带 `bizExt`；
//! * `bizExt` 是 base64，解出来是 App 端登录结果，里面才有 `accessToken`；
//! * 换 web token 需要 `SESSIONID` cookie，它来自 `auth.aliyundrive.com` 的授权页。
//!
//! 这个模块只做「拿 token 并交给 [`super::aliyun::AliyunProvider`]」，
//! 之后所有文件操作都走网页版接口（`api.aliyundrive.com`）。

use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::error::{AppError, AppResult};

const GENERATE: &str = "https://passport.aliyundrive.com/newlogin/qrcode/generate.do\
?appName=aliyun_drive&fromSite=52&appEntrance=web";
const QUERY: &str = "https://passport.aliyundrive.com/newlogin/qrcode/query.do\
?appName=aliyun_drive&fromSite=52&_bx-v=2.0.31";
const AUTH_PAGE: &str = "https://auth.aliyundrive.com/v2/oauth/authorize\
?client_id=25dzX3vbYqktVxyX\
&redirect_uri=https%3A%2F%2Fwww.aliyundrive.com%2Fsign%2Fcallback\
&response_type=code&login_type=custom\
&state=%7B%22origin%22%3A%22https%3A%2F%2Fwww.aliyundrive.com%22%7D";
const TOKEN_LOGIN: &str = "https://auth.aliyundrive.com/v2/oauth/token_login";
const WEB_TOKEN: &str = "https://api.aliyundrive.com/token/get";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Passport rejects the QR endpoints without a `mini_login.htm` referer, and it
/// compares the whole string, so these mirror the web client exactly.
const PASSPORT_REFERER: &str = "https://passport.aliyundrive.com/mini_login.htm\
?lang=zh_cn&appName=aliyun_drive&appEntrance=web&styleType=auto&bizParams=\
&notLoadSsoView=false&notKeepLogin=false&isMobile=false\
&ad__pass__q__rememberLogin=true&ad__pass__q__rememberLoginDefaultValue=true\
&ad__pass__q__forgotPassword=true&ad__pass__q__licenseMargin=true\
&ad__pass__q__loginType=normal&hidePhoneCode=true&rnd=0.20099676922221987";
/// `token_login` wants the authorize URL as `Origin`, not just the host.
const TOKEN_LOGIN_ORIGIN: &str = AUTH_PAGE;

/// One QR code plus the `t`/`ck` pair the status query needs.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrCode {
    /// The string to render as a QR image.
    pub content: String,
    /// Passport ticket; must be echoed on every poll.
    pub t: i64,
    /// Passport cookie value; must be echoed on every poll.
    pub ck: String,
}

impl QrCode {
    /// Rebuilds the descriptor from what the frontend stored.
    pub fn from_parts(content: String, t: i64, ck: String) -> Self {
        Self { content, t, ck }
    }
}

/// Result of one status poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanState {
    /// Freshly generated, not scanned yet.
    New,
    /// Scanned, waiting for the user to confirm on their phone.
    Scanned,
    /// Confirmed: the frontend should stop polling.
    Confirmed,
    /// Timed out.
    Expired,
    /// The passport service reported an error.
    Failed(String),
}

/// A web login session: keeps the `SESSIONID` cookie across the two-step token
/// exchange.
pub struct AliyunQrSession {
    client: reqwest::Client,
    session_id: RwLock<String>,
    /// `bizExt` from the CONFIRMED poll. Passport invalidates the QR code the
    /// moment it is confirmed, so the next poll reports EXPIRED — the only way
    /// to retry a failed exchange is to keep the payload here.
    confirmed: RwLock<Option<String>>,
}

impl AliyunQrSession {
    pub fn new() -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .user_agent(UA)
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(30))
            .cookie_store(true)
            .build()
            .map_err(|e| AppError::new(format!("网络初始化失败：{e}")))?;
        Ok(Self {
            client,
            session_id: RwLock::new(String::new()),
            confirmed: RwLock::new(None),
        })
    }

    /// Asks passport for a QR code.
    pub async fn generate(&self) -> AppResult<QrCode> {
        crate::ylog!("aliyun qr: generate → GET passport generate.do");
        let res = self
            .client
            .get(GENERATE)
            .header(reqwest::header::REFERER, PASSPORT_REFERER)
            .send()
            .await
            .map_err(|e| AppError::new(format!("获取二维码失败：{e}")))?;
        let value: Value = res
            .json()
            .await
            .map_err(|e| AppError::new(format!("二维码响应解析失败：{e}")))?;
        let data = value
            .pointer("/content/data")
            .ok_or_else(|| AppError::new("二维码响应缺少数据"))?;
        let content = data
            .get("codeContent")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AppError::new("二维码响应缺少 codeContent"))?;
        let t = data.get("t").and_then(|v| v.as_i64()).unwrap_or(0);
        let ck = data
            .get("ck")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        crate::ylog!(
            "aliyun qr: generate ok t={t} ck_len={} content_len={}",
            ck.len(),
            content.len()
        );
        Ok(QrCode {
            content: content.to_string(),
            t,
            ck,
        })
    }

    /// Polls the scan status. `bizExt` is kept internally for [`Self::exchange`].
    pub async fn poll(&self, qr: &QrCode) -> AppResult<(ScanState, Option<String>)> {
        let res = self
            .client
            .post(QUERY)
            .header(reqwest::header::ORIGIN, "https://passport.aliyundrive.com")
            .header(reqwest::header::REFERER, PASSPORT_REFERER)
            .form(&[("t", qr.t.to_string()), ("ck", qr.ck.clone())])
            .send()
            .await
            .map_err(|e| AppError::new(format!("查询扫码状态失败：{e}")))?;
        let value: Value = res
            .json()
            .await
            .map_err(|e| AppError::new(format!("状态响应解析失败：{e}")))?;

        if value.get("hasError").and_then(|v| v.as_bool()) == Some(true) {
            return Ok((ScanState::Failed("扫码服务返回错误".to_string()), None));
        }
        let data = match value.pointer("/content/data") {
            Some(data) => data,
            None => return Ok((ScanState::Failed("状态响应缺少数据".to_string()), None)),
        };
        let status = data
            .get("qrCodeStatus")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_ascii_uppercase();
        let biz_ext = data
            .get("bizExt")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty())
            .map(|v| v.to_string());
        let state = match status.as_str() {
            "NEW" => ScanState::New,
            "SCANED" | "SCANNED" => ScanState::Scanned,
            "CONFIRMED" => ScanState::Confirmed,
            "EXPIRED" => ScanState::Expired,
            // Passport adds states over time (CANCELED, SCANED_…). Treating an
            // unknown one as a hard failure would kill a login that is actually
            // still in flight, so keep waiting and let the deadline decide.
            _ => ScanState::New,
        };
        if status.is_empty() {
            crate::ylog!("aliyun qr: poll returned no qrCodeStatus");
        }
        crate::ylog!(
            "aliyun qr: poll status={} biz_ext_len={}",
            status,
            biz_ext.as_ref().map(|v| v.len()).unwrap_or(0)
        );
        Ok((state, biz_ext))
    }

    /// Remembers the payload from a CONFIRMED poll so the exchange can be
    /// retried after the QR code itself has been invalidated.
    pub async fn remember_confirmed(&self, biz_ext: &str) {
        *self.confirmed.write().await = Some(biz_ext.to_string());
    }

    /// The remembered payload, if a scan has been confirmed.
    ///
    /// Deliberately a peek, not a take: a failed exchange must be retryable,
    /// because the QR code itself is already spent by then.
    pub async fn confirmed_payload(&self) -> Option<String> {
        self.confirmed.read().await.clone()
    }

    /// Turns a confirmed `bizExt` into a web token pair.
    ///
    /// Two hops: `bizExt` → App access token → authorization code (needs the
    /// `SESSIONID` cookie from the authorize page) → web token.
    pub async fn exchange(&self, biz_ext: &str) -> AppResult<super::aliyun::AliyunCredentials> {
        let app_token = decode_biz_ext(biz_ext)?;
        crate::ylog!("aliyun qr: bizExt decoded, app token len={}", app_token.len());

        // The authorize page hands out the SESSIONID the next hop needs.
        self.acquire_session().await?;
        let session = self.session_id.read().await.clone();
        if session.is_empty() {
            return Err(AppError::new("没有拿到阿里云盘会话，请重试"));
        }

        let res = self
            .client
            .post(TOKEN_LOGIN)
            .header(reqwest::header::ORIGIN, TOKEN_LOGIN_ORIGIN)
            .header(reqwest::header::REFERER, "https://auth.aliyundrive.com")
            .header(reqwest::header::COOKIE, format!("SESSIONID={session}"))
            .json(&json!({ "token": app_token }))
            .send()
            .await
            .map_err(|e| AppError::new(format!("换取授权码失败：{e}")))?;
        let (status_code, value) = read_json(res, "授权码").await?;
        crate::ylog!(
            "aliyun qr: token_login HTTP {status_code} body={}",
            snippet(&value)
        );
        let code = extract_code(&value)
            .ok_or_else(|| {
                AppError::new(format!(
                    "授权码接口没有返回 code（HTTP {status_code}）：{}",
                    snippet(&value)
                ))
            })?;

        let res = self
            .client
            .post(WEB_TOKEN)
            .header(reqwest::header::ORIGIN, "https://www.aliyundrive.com/")
            .header(reqwest::header::REFERER, "https://www.aliyundrive.com")
            .header(reqwest::header::COOKIE, format!("SESSIONID={session}"))
            .json(&json!({ "code": code, "loginType": "normal" }))
            .send()
            .await
            .map_err(|e| AppError::new(format!("换取令牌失败：{e}")))?;
        let (status_code, value) = read_json(res, "令牌").await?;
        crate::ylog!(
            "aliyun qr: token/get HTTP {status_code} body={}",
            snippet(&value)
        );
        let access = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::new(format!(
                    "令牌接口没有返回 access_token（HTTP {status_code}）：{}",
                    snippet(&value)
                ))
            })?;
        let refresh = value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let expires_in = value
            .get("expires_in")
            .and_then(|v| v.as_i64())
            .unwrap_or(7200);
        let drive_id = value
            .get("default_drive_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let nickname = value
            .get("nick_name")
            .or_else(|| value.get("user_name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let avatar = value
            .get("avatar")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());

        Ok(super::aliyun::AliyunCredentials {
            access_token: access.to_string(),
            // Keep this empty when the server did not hand one out. Storing the
            // App access token here (as an earlier version did) makes every
            // later refresh fail with `InvalidParameter.RefreshToken`, which
            // surfaced to users as "登录不了" long after a successful scan.
            refresh_token: refresh.to_string(),
            drive_id,
            resource_drive_id: String::new(),
            // A fresh login has no drive preference yet; `adopt_resource_drive`
            // decides on the first root listing.
            drive_chosen: false,
            user_id: String::new(),
            device_id: String::new(),
            private_key: String::new(),
            signature: String::new(),
            nickname,
            avatar,
            expires_at: chrono::Utc::now().timestamp() + expires_in,
        })
    }

    /// Visits the authorize page so the cookie jar collects `SESSIONID`.
    async fn acquire_session(&self) -> AppResult<()> {
        // The reference client allows 10 s for this hop; 3 s was too tight on a
        // slow link and surfaced as "没有拿到阿里云盘会话".
        for _ in 0..12 {
            let res = self
                .client
                .get(AUTH_PAGE)
                .header(reqwest::header::REFERER, "https://www.aliyundrive.com")
                .send()
                .await;
            if let Ok(res) = res {
                for cookie in res.cookies() {
                    if cookie.name() == "SESSIONID" && !cookie.value().is_empty() {
                        *self.session_id.write().await = cookie.value().to_string();
                        return Ok(());
                    }
                }
            }
            crate::ylog!("aliyun qr: authorize page gave no SESSIONID, retrying");
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        }
        Err(AppError::new("没有拿到阿里云盘会话（SESSIONID）"))
    }
}

/// Reads a JSON body, keeping the status code so errors can name the step.
async fn read_json(res: reqwest::Response, what: &str) -> AppResult<(u16, Value)> {
    let status = res.status().as_u16();
    let text = res
        .text()
        .await
        .map_err(|e| AppError::new(format!("{what}响应读取失败：{e}")))?;
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((status, value))
}

/// Short, single-line preview of a JSON payload for error messages.
fn snippet(value: &Value) -> String {
    let text = value.to_string();
    let cut: String = text.chars().take(200).collect();
    if cut.chars().count() < text.chars().count() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// Pulls the authorization code out of the token-login response.
///
/// The response is `{"goto": "https://www.aliyundrive.com/sign/callback?code=…"}`,
/// so the code is a query parameter of that URL rather than a JSON field. Direct
/// fields are still accepted in case the shape changes.
fn extract_code(value: &Value) -> Option<String> {
    if let Some(goto) = value.get("goto").and_then(|v| v.as_str()) {
        if let Some(code) = query_param(goto, "code") {
            return Some(code);
        }
        // A few responses put the code straight into `goto`.
        if !goto.contains("://") && !goto.is_empty() {
            return Some(goto.to_string());
        }
    }
    value
        .get("authorization_code")
        .and_then(|v| v.as_str())
        .or_else(|| value.pointer("/data/authorization_code").and_then(|v| v.as_str()))
        .map(|v| v.to_string())
}

/// Reads one `key=value` pair out of a URL, without pulling in a URL crate.
fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    for pair in query.split('&') {
        // A bare flag (`&foo`) has no `=`: skip it instead of abandoning the
        // whole query, which used to drop a valid `code` that came after it.
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name == key && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// `bizExt` is base64 JSON carrying the App-side login result.
///
/// Passport has shipped this field in more than one shape, and a failure here
/// used to reach the user as a bare "二维码已过期" with nothing in the log, so
/// every step now records what it actually saw.
fn decode_biz_ext(raw: &str) -> AppResult<String> {
    let raw = raw.trim();
    crate::ylog!(
        "aliyun qr: bizExt len={} head={} tail={}",
        raw.len(),
        head(raw, 96),
        tail(raw, 48)
    );

    // Some deployments hand back the JSON itself instead of base64 of it.
    let text = if raw.starts_with('{') {
        raw.to_string()
    } else {
        // `bizExt` has been seen both padded/standard and url-safe/unpadded, so
        // try every alphabet instead of failing on a `-` or a missing `=`.
        let decoded = decode_any_base64(raw)
            .or_else(|| decode_any_base64(&percent_decode(raw)))
            .ok_or_else(|| AppError::new("扫码结果解析失败（bizExt 不是合法 base64）"))?;
        crate::ylog!(
            "aliyun qr: bizExt decoded to {} bytes, hex head={}",
            decoded.len(),
            hex_head(&decoded, 32)
        );
        decode_text(&decoded)
    };
    crate::ylog!("aliyun qr: bizExt json head={}", head(&text, 160));

    let value = parse_biz_json(&text)?;

    let token = value
        .pointer("/pds_login_result/accessToken")
        .or_else(|| value.pointer("/pds_login_result/access_token"))
        .or_else(|| value.pointer("/pdsLoginResult/accessToken"))
        .or_else(|| value.get("accessToken"))
        .or_else(|| value.get("access_token"))
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        // Last resort: the field has moved around between passport releases, so
        // look for the key anywhere in the tree instead of failing outright.
        .map(|v| v.to_string())
        .or_else(|| find_key(&value, "accessToken"));
    let Some(token) = token else {
        crate::ylog!(
            "aliyun qr: bizExt has no accessToken, keys={:?}",
            top_keys(&value)
        );
        return Err(AppError::new("扫码结果里没有 accessToken"));
    };
    Ok(token)
}

/// Parses the decoded `bizExt` text, tolerating a BOM, one extra layer of
/// JSON-in-a-string, and one extra layer of base64 — all of which have been
/// seen from passport, and all of which used to fail silently.
fn parse_biz_json(text: &str) -> AppResult<Value> {
    let cleaned = text.trim_start_matches('\u{feff}').trim();
    let value: Value = match serde_json::from_str(cleaned) {
        Ok(value) => value,
        Err(err) => {
            // `bizExt` = base64(base64(json)) in some responses.
            let Some(bytes) = decode_any_base64(cleaned) else {
                crate::ylog!("aliyun qr: bizExt is not JSON: {err}");
                return Err(AppError::new("扫码结果解析失败（不是 JSON）"));
            };
            let inner = decode_text(&bytes);
            crate::ylog!(
                "aliyun qr: bizExt was base64 twice, inner head={}",
                head(&inner, 160)
            );
            serde_json::from_str(inner.trim()).map_err(|err| {
                crate::ylog!("aliyun qr: bizExt inner is not JSON: {err}");
                AppError::new("扫码结果解析失败（不是 JSON）")
            })?
        }
    };
    // `{"…":"{\"pds_login_result\":…}"}` — unwrap the inner document once.
    if let Value::String(inner) = &value {
        if inner.trim_start().starts_with('{') {
            if let Ok(nested) = serde_json::from_str::<Value>(inner) {
                crate::ylog!("aliyun qr: bizExt was double-encoded JSON");
                return Ok(nested);
            }
        }
    }
    Ok(value)
}

/// Depth-first search for a string field anywhere in a JSON tree, descending
/// into string values that themselves hold a JSON document.
fn find_key(value: &Value, key: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(|v| v.as_str()) {
                if !found.is_empty() {
                    return Some(found.to_string());
                }
            }
            map.values().find_map(|v| find_key(v, key))
        }
        Value::Array(items) => items.iter().find_map(|v| find_key(v, key)),
        Value::String(text) => {
            // `{"pds_login_result":"{\"accessToken\":…}"}` — the inner document
            // arrives as a string in some passport builds.
            let trimmed = text.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(nested) = serde_json::from_str::<Value>(trimmed) {
                    return find_key(&nested, key);
                }
            }
            None
        }
        _ => None,
    }
}

/// Turns decoded bytes into text.
///
/// JSON is UTF-8 by definition, so invalid bytes mean the payload itself is
/// damaged. Decode lossily rather than bailing out here: the JSON parser then
/// reports the real problem, instead of a "not UTF-8" dead end that says
/// nothing about what passport actually sent.
fn decode_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(err) => {
            crate::ylog!("aliyun qr: bizExt is not valid UTF-8 ({err}), decoding lossily");
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

/// Undoes `%XX` escaping, which passport applies to `bizExt` in some responses.
/// A stray `%` is kept as-is rather than dropped.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &raw[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Hex preview of the first `n` bytes, for logging.
fn hex_head(bytes: &[u8], n: usize) -> String {
    bytes
        .iter()
        .take(n)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// First `n` characters of a string, for logging.
fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Last `n` characters of a string, for logging.
fn tail(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// Top-level keys of a JSON object, for logging an unexpected shape.
fn top_keys(value: &Value) -> Vec<String> {
    match value.as_object() {
        Some(map) => map.keys().cloned().collect(),
        None => vec![format!("<{}>", type_name(value))],
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Decodes base64 in any of the four common alphabets/padding conventions.
fn decode_any_base64(raw: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{
        STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
    };
    use base64::Engine;
    if let Ok(bytes) = STANDARD.decode(raw) {
        return Some(bytes);
    }
    if let Ok(bytes) = STANDARD_NO_PAD.decode(raw) {
        return Some(bytes);
    }
    if let Ok(bytes) = URL_SAFE.decode(raw) {
        return Some(bytes);
    }
    URL_SAFE_NO_PAD.decode(raw).ok()
}

/// What the frontend gets while polling.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrPollResult {
    /// `new` | `scanned` | `confirmed` | `expired` | `failed`
    pub state: String,
    /// Set once the exchange finished.
    pub authenticated: bool,
    pub message: Option<String>,
}

impl QrPollResult {
    pub fn from_state(state: &ScanState) -> Self {
        let (name, message) = match state {
            ScanState::New => ("new", None),
            ScanState::Scanned => ("scanned", None),
            ScanState::Confirmed => ("confirmed", None),
            ScanState::Expired => ("expired", Some("二维码已过期，请刷新".to_string())),
            ScanState::Failed(msg) => ("failed", Some(msg.clone())),
        };
        Self {
            state: name.to_string(),
            authenticated: false,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    /// `bizExt` must be decoded and the App access token extracted; a malformed
    /// payload is an error rather than a silent empty token.
    #[test]
    fn decodes_biz_ext() {
        let inner = json!({ "pds_login_result": { "accessToken": "app-token-123" } }).to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode(inner);
        assert_eq!(decode_biz_ext(&encoded).unwrap(), "app-token-123");
    }

    /// Live smoke test for the passport hops (network required):
    /// `cargo test --lib live_qr -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_qr_generate_and_poll() {
        let session = AliyunQrSession::new().unwrap();
        let qr = session.generate().await.expect("generate");
        println!("qr t={} ck_len={} content={}", qr.t, qr.ck.len(), qr.content);
        assert!(qr.content.contains("qrcodeCheck"));
        let (state, biz) = session.poll(&qr).await.expect("poll");
        println!("poll state={state:?} biz_ext={:?}", biz.map(|v| v.len()));
        assert_eq!(state, ScanState::New);
        // The session hop is what used to fail silently on the 阿里 login.
        session.acquire_session().await.expect("session");
        println!("session id len={}", session.session_id.read().await.len());
    }

    #[test]
    fn rejects_garbage_biz_ext() {
        assert!(decode_biz_ext("not base64!!").is_err());
        let no_token = base64::engine::general_purpose::STANDARD.encode(r#"{"x":1}"#);
        assert!(decode_biz_ext(&no_token).is_err());
    }

    /// The real `bizExt` is unpadded/url-safe often enough that a strict
    /// standard-alphabet decode would fail a perfectly good login.
    #[test]
    fn decodes_url_safe_and_unpadded_biz_ext() {
        let inner = json!({ "pds_login_result": { "accessToken": "app-token-xyz" } }).to_string();
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(inner);
        assert_eq!(decode_biz_ext(&encoded).unwrap(), "app-token-xyz");
    }

    /// The authorization code lives in the `goto` URL, not in a JSON field.
    #[test]
    fn extracts_code_from_goto_url() {
        let value = json!({
            "goto": "https://www.aliyundrive.com/sign/callback?code=abc123def&state=%7B%7D"
        });
        assert_eq!(extract_code(&value).as_deref(), Some("abc123def"));

        // A plain field still works.
        let direct = json!({ "authorization_code": "zzz" });
        assert_eq!(extract_code(&direct).as_deref(), Some("zzz"));

        // Nothing usable.
        assert!(extract_code(&json!({ "goto": "https://x/y" })).is_none());
        assert!(extract_code(&json!({})).is_none());
    }

    #[test]
    fn maps_states_for_the_frontend() {
        assert_eq!(QrPollResult::from_state(&ScanState::New).state, "new");
        assert_eq!(
            QrPollResult::from_state(&ScanState::Scanned).state,
            "scanned"
        );
        let confirmed = QrPollResult::from_state(&ScanState::Confirmed);
        assert_eq!(confirmed.state, "confirmed");
        assert!(confirmed.message.is_none());
        let expired = QrPollResult::from_state(&ScanState::Expired);
        assert_eq!(expired.state, "expired");
        assert!(expired.message.is_some());
    }

    /// The App access token has moved between `pds_login_result`, a nested
    /// `pdsLoginResult`, and a double-encoded document. Any of them must work,
    /// because the failure only shows up after the QR code is already spent.
    #[test]
    fn finds_the_app_token_in_every_shape() {
        let nested = json!({ "pdsLoginResult": { "accessToken": "nested-token" } }).to_string();
        assert_eq!(decode_biz_ext(&nested).unwrap(), "nested-token");

        let flat = json!({ "accessToken": "flat-token" }).to_string();
        assert_eq!(decode_biz_ext(&flat).unwrap(), "flat-token");

        let deep = json!({ "a": { "b": [{ "accessToken": "deep-token" }] } }).to_string();
        assert_eq!(decode_biz_ext(&deep).unwrap(), "deep-token");

        let inner = json!({ "pds_login_result": { "accessToken": "wrapped" } }).to_string();
        let double = serde_json::to_string(&inner).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(double);
        assert_eq!(decode_biz_ext(&encoded).unwrap(), "wrapped");
    }

    /// `bizExt` = base64(base64(json)) — the third shape passport has shipped.
    #[test]
    fn tolerates_a_double_base64_biz_ext() {
        let inner = json!({ "pds_login_result": { "accessToken": "twice" } }).to_string();
        let once = base64::engine::general_purpose::STANDARD.encode(inner);
        let twice = base64::engine::general_purpose::STANDARD.encode(once);
        assert_eq!(decode_biz_ext(&twice).unwrap(), "twice");
    }

    /// A payload that is valid base64 but not UTF-8 must fail with a message
    /// about the JSON body, not a misleading "not UTF-8" dead end.
    #[test]
    fn tolerates_bytes_that_are_not_utf8() {
        let inner = json!({ "accessToken": "t" }).to_string();
        let mut bytes = inner.into_bytes();
        bytes.insert(1, 0xff);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let err = decode_biz_ext(&encoded).unwrap_err().to_string();
        assert!(
            err.contains("不是 JSON"),
            "expected a JSON error, got {err}"
        );
    }
}

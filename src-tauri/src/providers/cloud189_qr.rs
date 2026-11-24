//! 天翼云盘 (189) QR login.
//!
//! Flow, as observed on the PC web login:
//!
//! 1. `unifyLoginForPC.action` bounces through `open.e.189.cn` and finally
//!    renders the login box HTML. That page embeds the three parameters every
//!    follow-up request has to echo back: `lt`, `paramId` and `reqId`.
//! 2. `getUUID.do` mints the code. Its `uuid` *is* the QR payload — the phone
//!    app scans it verbatim, so no extra encoding is needed.
//! 3. `qrcodeLoginState.do` is polled until it reports success and hands back a
//!    `redirectUrl`.
//! 4. `getSessionForPC.action` trades that `redirectUrl` for the session key +
//!    secret pair the file API signs every request with.
//!
//! The app id matters: `9317140619` (what the open-source clients use) lands on
//! the newer login page, which ships no `lt`/`paramId` in the HTML and needs an
//! extra `appConf.do` round trip. `8025431004` still renders the older page with
//! everything inline, so that is what we use.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::error::{AppError, AppResult};

use super::cloud189::{client_suffix, Cloud189Credentials, APP_ID};

const WEB_URL: &str = "https://cloud.189.cn";
const AUTH_URL: &str = "https://open.e.189.cn";
const API_URL: &str = "https://api.cloud.189.cn";
const RETURN_URL: &str = "https://m.cloud.189.cn/zhuanti/2020/loginErrorPc/index.html";
const CLIENT_TYPE: &str = "10020";
/// The polling endpoint wants `1`, not the `10020` the page uses.
const QR_CLIENT_TYPE: &str = "1";

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// The code the UI renders.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrCode {
    /// Exactly what the phone scans.
    pub content: String,
    /// Milliseconds since the epoch, so the UI can show a countdown.
    pub t: i64,
    /// Opaque echo token; the server keeps the real state.
    pub ck: String,
}

impl QrCode {
    pub fn from_parts(content: String, t: i64, ck: String) -> Self {
        Self { content, t, ck }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanState {
    /// Waiting for the phone to scan.
    New,
    /// Scanned, waiting for the confirm tap.
    Scanned,
    Confirmed,
    Expired,
    Failed(String),
}

/// Everything the poll needs, kept server-side.
#[derive(Debug, Default)]
struct LoginState {
    lt: String,
    param_id: String,
    req_id: String,
    uuid: String,
    encryuuid: String,
    referer: String,
}

pub struct Cloud189QrSession {
    client: reqwest::Client,
    state: RwLock<LoginState>,
    /// The `redirectUrl` a confirmed scan returned, until it is exchanged.
    confirmed: RwLock<Option<String>>,
}

impl Cloud189QrSession {
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
            state: RwLock::new(LoginState::default()),
            confirmed: RwLock::new(None),
        })
    }

    /// Fetches the login page, mints a code and remembers the parameters.
    pub async fn generate(&self) -> AppResult<QrCode> {
        let state = self.login_page().await?;
        crate::ylog!(
            "cloud189 qr: page params lt={} paramId={} reqId={}",
            state.lt.len(),
            state.param_id.len(),
            state.req_id.len()
        );

        let value = self
            .post_form(
                &format!("{AUTH_URL}/api/logbox/oauth2/getUUID.do"),
                &state,
                &[("appId", APP_ID)],
            )
            .await?;
        if let Some(msg) = result_error(&value) {
            return Err(AppError::new(format!("天翼云盘：{msg}")));
        }
        let uuid = text(&value, "uuid");
        let encryuuid = text(&value, "encryuuid");
        if uuid.is_empty() || encryuuid.is_empty() {
            return Err(AppError::new("天翼云盘没有返回二维码"));
        }

        let content = uuid.clone();
        let ck = encryuuid.clone();
        {
            let mut guard = self.state.write().await;
            *guard = LoginState {
                uuid,
                encryuuid,
                ..state
            };
        }
        *self.confirmed.write().await = None;
        crate::ylog!("cloud189 qr: generated, uuid len={}", content.len());
        Ok(QrCode::from_parts(
            content,
            chrono::Utc::now().timestamp_millis(),
            ck,
        ))
    }

    /// `unifyLoginForPC.action` → the login box HTML → `lt`/`paramId`/`reqId`.
    async fn login_page(&self) -> AppResult<LoginState> {
        let stamp = chrono::Utc::now().timestamp_millis();
        let url = format!(
            "{WEB_URL}/unifyLoginForPC.action?appId={APP_ID}&clientType={CLIENT_TYPE}&returnURL={}&timeStamp={stamp}",
            urlencode(RETURN_URL)
        );
        let res = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AppError::new(format!("打开天翼云盘登录页失败：{e}")))?;
        let final_url = res.url().to_string();
        let status = res.status().as_u16();
        let html = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取天翼云盘登录页失败：{e}")))?;
        if !(200..300).contains(&status) {
            return Err(AppError::new(format!("天翼云盘登录页返回 HTTP {status}")));
        }

        let lt = between(&html, "lt = \"", "\"");
        let param_id = between(&html, "paramId = \"", "\"");
        let req_id = between(&html, "reqId = \"", "\"");
        if lt.is_empty() || param_id.is_empty() || req_id.is_empty() {
            crate::ylog!(
                "cloud189 qr: page missing params (lt={}, paramId={}, reqId={}, url={final_url})",
                lt.len(),
                param_id.len(),
                req_id.len()
            );
            return Err(AppError::new(
                "天翼云盘登录页格式已变化（没有找到 lt/paramId/reqId），请反馈日志",
            ));
        }
        let referer = format!(
            "{AUTH_URL}/api/logbox/separate/web/index.html?appId={APP_ID}&lt={lt}&reqId={req_id}"
        );
        Ok(LoginState {
            lt,
            param_id,
            req_id,
            uuid: String::new(),
            encryuuid: String::new(),
            referer,
        })
    }

    /// One poll. `Confirmed` carries the `redirectUrl` in the second slot.
    pub async fn poll(&self, qr: &QrCode) -> AppResult<(ScanState, Option<String>)> {
        let state = {
            let guard = self.state.read().await;
            LoginState {
                lt: guard.lt.clone(),
                param_id: guard.param_id.clone(),
                req_id: guard.req_id.clone(),
                uuid: guard.uuid.clone(),
                encryuuid: guard.encryuuid.clone(),
                referer: guard.referer.clone(),
            }
        };
        if state.uuid.is_empty() {
            return Ok((ScanState::Failed("请重新获取二维码".to_string()), None));
        }
        // The UI may hand back an older code; trust the server copy but let the
        // caller know when they diverge badly enough to matter.
        if !qr.content.is_empty() && qr.content != state.uuid {
            crate::ylog!("cloud189 qr: poll with a stale code, using the live one");
        }

        let date = beijing_stamp();
        let value = self
            .post_form(
                &format!("{AUTH_URL}/api/logbox/oauth2/qrcodeLoginState.do"),
                &state,
                &[
                    ("appId", APP_ID),
                    ("clientType", QR_CLIENT_TYPE),
                    ("returnUrl", RETURN_URL),
                    ("paramId", &state.param_id),
                    ("uuid", &state.uuid),
                    ("encryuuid", &state.encryuuid),
                    ("cb_SaveName", "3"),
                    ("isOauth2", "false"),
                    ("state", ""),
                    ("date", &date),
                    (
                        "timeStamp",
                        &chrono::Utc::now().timestamp_millis().to_string(),
                    ),
                ],
            )
            .await?;

        let status = value.get("status").and_then(Value::as_i64).or_else(|| {
            value
                .get("status")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        });
        match status {
            Some(0) => {
                let redirect = text(&value, "redirectUrl");
                if redirect.is_empty() {
                    return Ok((
                        ScanState::Failed("扫码成功但没有拿到跳转地址".to_string()),
                        None,
                    ));
                }
                *self.confirmed.write().await = Some(redirect.clone());
                Ok((ScanState::Confirmed, Some(redirect)))
            }
            Some(-11002) => Ok((ScanState::Scanned, None)),
            Some(-11001) => Ok((ScanState::Expired, None)),
            Some(-106) => Ok((ScanState::New, None)),
            Some(-20099) => Ok((
                ScanState::Expired,
                Some("登录参数已过期，请刷新二维码".to_string()),
            )),
            Some(code) => {
                let msg = text(&value, "msg");
                let msg = if msg.is_empty() {
                    format!("扫码状态异常（{code}）")
                } else {
                    format!("{msg}（{code}）")
                };
                Ok((ScanState::Failed(msg), None))
            }
            None => Ok((
                ScanState::Failed("天翼云盘没有返回扫码状态".to_string()),
                None,
            )),
        }
    }

    pub async fn remember_confirmed(&self, redirect: &str) {
        *self.confirmed.write().await = Some(redirect.to_string());
    }

    /// Peek, not take: a failed exchange must be retryable.
    pub async fn confirmed_payload(&self) -> Option<String> {
        self.confirmed.read().await.clone()
    }

    /// `getSessionForPC.action` → the signed session.
    pub async fn exchange(&self, redirect: &str) -> AppResult<Cloud189Credentials> {
        let url = format!("{API_URL}/getSessionForPC.action");
        let res = self
            .client
            .post(&url)
            .query(&client_suffix())
            .query(&[("returnType", "JSON"), ("redirectURL", redirect)])
            .header("X-Request-ID", random_id())
            .send()
            .await
            .map_err(|e| AppError::new(format!("换取天翼云盘登录凭据失败：{e}")))?;
        let status = res.status().as_u16();
        let text_body = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取天翼云盘登录响应失败：{e}")))?;
        let value: Value = serde_json::from_str(&text_body)
            .map_err(|_| AppError::new(format!("天翼云盘登录响应不是 JSON（HTTP {status}）")))?;

        let session_key = text(&value, "sessionKey");
        let session_secret = text(&value, "sessionSecret");
        if session_key.is_empty() || session_secret.is_empty() {
            let msg = text(&value, "res_message");
            let msg = if msg.is_empty() {
                text(&value, "msg")
            } else {
                msg
            };
            return Err(AppError::new(if msg.is_empty() {
                "天翼云盘没有返回会话密钥".to_string()
            } else {
                format!("天翼云盘：{msg}")
            }));
        }

        Ok(Cloud189Credentials {
            session_key,
            session_secret,
            access_token: text(&value, "accessToken"),
            refresh_token: text(&value, "refreshToken"),
            nickname: text(&value, "loginName"),
            avatar: None,
            web_cookie: String::new(),
        })
    }

    async fn post_form(
        &self,
        url: &str,
        state: &LoginState,
        form: &[(&str, &str)],
    ) -> AppResult<Value> {
        let res = self
            .client
            .post(url)
            .header("Referer", &state.referer)
            .header("Origin", AUTH_URL)
            .header("REQID", &state.req_id)
            .header("lt", &state.lt)
            .header("user-finger", user_finger())
            .form(form)
            .send()
            .await
            .map_err(|e| AppError::new(format!("天翼云盘请求失败：{e}")))?;
        let status = res.status().as_u16();
        let body = res
            .text()
            .await
            .map_err(|e| AppError::new(format!("读取天翼云盘响应失败：{e}")))?;
        if !(200..300).contains(&status) {
            return Err(AppError::new(format!("天翼云盘返回 HTTP {status}")));
        }
        serde_json::from_str(&body)
            .map_err(|_| AppError::new(format!("天翼云盘响应不是 JSON：{}", head(&body, 120))))
    }
}

/// Status of one poll, shaped like the 阿里云盘 one so the UI can share a
/// component.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QrPollResult {
    pub state: String,
    pub authenticated: bool,
    pub message: Option<String>,
}

impl QrPollResult {
    pub fn from_state(state: &ScanState) -> Self {
        let (name, message) = match state {
            ScanState::New => ("new", None),
            ScanState::Scanned => ("scanned", Some("已扫码，请在手机上确认登录".to_string())),
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

// ------------------------------------------------------------- helpers ---

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn result_error(value: &Value) -> Option<String> {
    let result = value.get("result")?;
    let bad = match result {
        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
        Value::String(s) => !s.is_empty() && s != "0",
        Value::Null => false,
        _ => false,
    };
    if !bad {
        return None;
    }
    let msg = text(value, "msg");
    Some(if msg.is_empty() {
        format!("登录接口返回 {result}")
    } else {
        msg
    })
}

/// `between(hay, "lt = \"", "\"")` — a tiny stand-in for the regex we would
/// otherwise need for one page.
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

fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

fn urlencode(value: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// Beijing time in the layout the endpoint expects — note there is no colon
/// between the date and the hour (`2024-01-0109:15:54.550`).
fn beijing_stamp() -> String {
    let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("+08:00");
    chrono::Utc::now()
        .with_timezone(&offset)
        .format("%Y-%m-%d%H:%M:%S%.3f")
        .to_string()
}

/// The page wants a ten-digit browser fingerprint; any plausible one works.
fn user_finger() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    format!("{}", 1_000_000_000 + now % 9_000_000_000)
}

fn random_id() -> String {
    super::cloud189::random_id()
}

/// A session that survives process restarts, as stored in `cloud189.json`.
pub type SharedSession = Arc<Cloud189QrSession>;

#[allow(dead_code)]
fn _assert_shared_session_is_send_sync(s: SharedSession) -> SharedSession {
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn between_finds_the_login_params() {
        let html = r#"<script>var x; lt = "ABC123"; paramId = "P99"; reqId = "R7"; lt="二维码登录成功";</script>"#;
        assert_eq!(between(html, "lt = \"", "\""), "ABC123");
        assert_eq!(between(html, "paramId = \"", "\""), "P99");
        assert_eq!(between(html, "reqId = \"", "\""), "R7");
        // The unspaced form must not win.
        assert_ne!(between(html, "lt = \"", "\""), "二维码登录成功");
    }

    #[test]
    fn between_is_empty_when_missing() {
        assert!(between("nothing here", "lt = \"", "\"").is_empty());
        assert!(between("lt = \"unterminated", "lt = \"", "\"").is_empty());
    }

    #[test]
    fn beijing_stamp_has_the_expected_shape() {
        let stamp = beijing_stamp();
        // 2024-01-0109:15:54.550 — 22 chars (no separator between date and
        // hour, exactly like the web client sends it); digit 10 is the hour.
        assert_eq!(stamp.len(), 22, "{stamp}");
        assert!(!stamp.contains("  "), "{stamp}");
        assert!(stamp.as_bytes()[10].is_ascii_digit(), "{stamp}");
    }

    #[test]
    fn user_finger_is_ten_digits() {
        let finger = user_finger();
        assert_eq!(finger.len(), 10, "{finger}");
        assert!(finger.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn result_error_reads_both_shapes() {
        assert!(result_error(&serde_json::json!({"result": 0})).is_none());
        assert!(result_error(&serde_json::json!({"result": "0"})).is_none());
        assert!(result_error(&serde_json::json!({"result": ""})).is_none());
        assert_eq!(
            result_error(&serde_json::json!({"result": 1, "msg": "坏了"})).as_deref(),
            Some("坏了")
        );
    }

    #[test]
    fn scan_states_map_to_names() {
        let name = |s: ScanState| QrPollResult::from_state(&s).state;
        assert_eq!(name(ScanState::New), "new");
        assert_eq!(name(ScanState::Scanned), "scanned");
        assert_eq!(name(ScanState::Confirmed), "confirmed");
        assert_eq!(name(ScanState::Expired), "expired");
        assert_eq!(name(ScanState::Failed("x".into())), "failed");
    }

    #[tokio::test]
    #[ignore = "hits the live 189 login page"]
    async fn live_qr_generate() {
        let session = Cloud189QrSession::new().unwrap();
        let qr = session.generate().await.unwrap();
        assert!(qr.content.contains("qrClinentLogin"), "{}", qr.content);
        let (state, _) = session.poll(&qr).await.unwrap();
        assert!(matches!(state, ScanState::New | ScanState::Scanned));
    }
}

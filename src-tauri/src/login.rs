//! In-app web login.
//!
//! Instead of asking the user to dig a cookie out of the browser devtools, we
//! open a real webview pointed at pan.quark.cn, let the user sign in normally,
//! and then lift the resulting cookies straight out of WebKit's cookie store.
//! The login webview shares the same `WebContext` (and therefore the same
//! cookie jar) as the main window, so the harvested header is the real thing.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::webview::Cookie;
use tauri::{
    AppHandle, Emitter, Manager, State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
};

use crate::error::{AppError, AppResult};
use crate::providers::DriveProvider as _;
use crate::state::AppState;

pub const LOGIN_LABEL: &str = "quark-login";
/// The 139 login window is a separate label so both can be open at once.
pub const LOGIN_LABEL_139: &str = "yun139-login";
/// 百度网盘 login window.
pub const LOGIN_LABEL_BAIDU: &str = "baidu-login";
/// Separate window label for the 115 web login.
pub const LOGIN_LABEL_115: &str = "pan115-login";
/// Emitted with a [`LoginEvent`] payload once the flow ends, either way.
pub const EVENT_LOGIN: &str = "auth://web-login";
/// Same shape as [`EVENT_LOGIN`], but for the 139 window.
pub const EVENT_LOGIN_139: &str = "auth://yun139-web-login";
/// Same shape as [`EVENT_LOGIN`], but for the 百度网盘 window.
pub const EVENT_LOGIN_BAIDU: &str = "auth://baidu-web-login";
/// Same shape as [`EVENT_LOGIN`], but for the 115 window.
pub const EVENT_LOGIN_115: &str = "auth://pan115-web-login";

/// The drive web app itself: it renders the login sheet when signed out and
/// lands on "all files" once signed in, so the user only ever sees one screen.
/// The drive web app. Signed out it renders the QR / phone login sheet in place,
/// and the session cookies (`__puus`, `__pus`) land on `pan.quark.cn`, which is
/// exactly the jar we harvest from.
const START_URL: &str = "https://pan.quark.cn/";
/// 139 web drive. Login lands on the file list with a session cookie; the
/// harvest pulls the `authorization` cookie (the exact `Basic …` header the
/// drive API wants — see the module docs of `harvest_139`).
const START_URL_139: &str = "https://yun.139.com/w/";
/// 百度网盘 web drive. Its session lives in the `BDUSS` / `STOKEN` cookies on
/// `pan.baidu.com`, which is what the harvest lifts out.
const START_URL_BAIDU: &str = "https://pan.baidu.com/disk/main";
const START_URL_115: &str = "https://115.com/";

/// WebKitGTK's default UA is easily mistaken for an unsupported browser, which
/// makes Quark serve a stripped-down page. Present as desktop Chrome instead.
const DESKTOP_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
const POLL_INTERVAL: Duration = Duration::from_millis(1200);
const MAX_POLLS: usize = 260;
/// Reading cookies spins WebKit's own loop, so it must not block forever.
const HARVEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginEvent {
    pub ok: bool,
    pub message: String,
}

fn event(ok: bool, message: impl Into<String>) -> LoginEvent {
    LoginEvent {
        ok,
        message: message.into(),
    }
}

/// Turns the cookie jar into a `Cookie:` header, keeping only Quark hosts and
/// preferring the most specific domain when the same name appears twice.
fn cookie_header(cookies: &[Cookie<'static>]) -> Option<String> {
    let mut pairs: Vec<(String, String, usize)> = Vec::new();
    for cookie in cookies {
        let domain = cookie.domain().unwrap_or_default();
        if !domain.ends_with("quark.cn") {
            continue;
        }
        let value = cookie.value();
        if value.trim().is_empty() {
            continue;
        }
        let name = cookie.name().to_string();
        let rank = domain.len();
        match pairs.iter_mut().find(|pair| pair.0 == name) {
            Some(existing) if rank > existing.2 => {
                existing.1 = value.to_string();
                existing.2 = rank;
            }
            Some(_) => {}
            None => pairs.push((name, value.to_string(), rank)),
        }
    }

    // The drive API session token; without it the header is useless.
    let has_session = pairs
        .iter()
        .any(|(name, value, _)| (name == "__puus" || name == "__pus") && value.len() > 8);
    if !has_session {
        return None;
    }

    Some(
        pairs
            .into_iter()
            .map(|(name, value, _)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Turns the cookie jar into a `Cookie:` header for `pan.baidu.com`.
///
/// Baidu's web session is not a single cookie. `BDUSS` alone, `BDUSS` +
/// `STOKEN`, and even `BDUSS` + `STOKEN` + `BAIDUID` all answer `errno: -6`
/// ("not logged in") — verified against a real logged-in jar. The session
/// only works once `PANPSC` (the pan-specific signed blob) is included too.
///
/// Guessing the required set is therefore a losing game: Baidu adds and renames
/// these over time (`BDUSS_BFESS`, `STOKEN_BFESS`, …). So forward *every*
/// `baidu.com` cookie except analytics beacons, and merely require that a
/// `BDUSS*` session cookie exists before reporting success.
fn cookie_header_baidu(cookies: &[Cookie<'static>]) -> String {
    let mut pairs: Vec<(String, String, usize)> = Vec::new();
    for cookie in cookies {
        let domain = cookie.domain().unwrap_or_default();
        // `pan.baidu.com`, `.baidu.com` and host-only (no domain) all count;
        // anything else (`hm.baidu.com` beacons, other sites) does not.
        if !domain.is_empty() && !domain.ends_with("baidu.com") {
            continue;
        }
        if domain.contains("hm.baidu.com") || domain.contains("miao.baidu.com") {
            continue;
        }
        let name = cookie.name();
        // Analytics and click-tracking beacons: noise the API does not need.
        if name.starts_with("Hm_") || name == "__bid_n" {
            continue;
        }
        let value = cookie.value();
        if value.trim().is_empty() {
            continue;
        }
        let rank = cookie_rank(domain);
        match pairs.iter_mut().find(|pair| pair.0 == name) {
            Some(existing) if rank > existing.2 => {
                existing.1 = value.to_string();
                existing.2 = rank;
            }
            Some(_) => {}
            None => pairs.push((name.to_string(), value.to_string(), rank)),
        }
    }
    // The account session, whatever Baidu currently calls it (`BDUSS` or
    // `BDUSS_BFESS`). Without one there is nothing to harvest yet.
    let has_session = pairs
        .iter()
        .any(|(name, value, _)| name.starts_with("BDUSS") && value.len() > 20);
    // `BDUSS` is set on `.baidu.com` the moment the passport login succeeds, but
    // the pan session (`PANPSC` and friends, scoped to `pan.baidu.com`) lands a
    // beat later. Harvesting in between produces a header the drive rejects with
    // `errno: -6`, so wait for a pan-scoped cookie too.
    let has_pan_cookie = cookies.iter().any(|cookie| {
        cookie
            .domain()
            .unwrap_or_default()
            .ends_with("pan.baidu.com")
    });
    if !has_session || !has_pan_cookie {
        crate::ylog!(
            "baidu login: waiting (session={has_session} pan={has_pan_cookie}, {} cookies: {})",
            pairs.len(),
            pairs
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        return String::new();
    }
    let count = pairs.len();
    let header = pairs
        .into_iter()
        .map(|(name, value, _)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    crate::ylog!(
        "baidu login: harvested {count} cookie(s), {} bytes",
        header.len()
    );
    header
}


/// 115 only ever needs these four cookies, and it wants them verbatim.
fn cookie_header_115(cookies: &[Cookie<'static>]) -> String {
    let mut uid = String::new();
    let mut cid = String::new();
    let mut seid = String::new();
    let mut kid = String::new();
    for cookie in cookies {
        let domain = cookie.domain().unwrap_or_default();
        if !domain.is_empty() && !domain.ends_with("115.com") {
            continue;
        }
        let value = cookie.value().to_string();
        if value.trim().is_empty() {
            continue;
        }
        match cookie.name() {
            // `.115.com` beats the host-only copy that `115.com` sets first.
            "UID" if uid.is_empty() || domain.starts_with('.') => uid = value,
            "CID" if cid.is_empty() || domain.starts_with('.') => cid = value,
            "SEID" if seid.is_empty() || domain.starts_with('.') => seid = value,
            "KID" if kid.is_empty() || domain.starts_with('.') => kid = value,
            _ => {}
        }
    }
    // `UID` appears the moment the QR lands, but `SEID` (the actual session)
    // arrives a beat later; harvesting in between yields a cookie the drive
    // rejects with "请先登录".
    if uid.is_empty() || seid.is_empty() {
        crate::ylog!(
            "pan115 login: waiting (uid={} seid={} cid={} kid={})",
            !uid.is_empty(),
            !seid.is_empty(),
            !cid.is_empty(),
            !kid.is_empty()
        );
        return String::new();
    }
    let mut pairs = vec![format!("UID={uid}"), format!("CID={cid}"), format!("SEID={seid}")];
    if !kid.is_empty() {
        pairs.push(format!("KID={kid}"));
    }
    let header = pairs.join("; ");
    crate::ylog!("pan115 login: harvested cookie, {} bytes", header.len());
    header
}

/// How much a cookie's domain is worth when the same name appears several
/// times: the pan host wins, then the shared `.baidu.com`, then the rest.
fn cookie_rank(domain: &str) -> usize {
    if domain.ends_with("pan.baidu.com") {
        3
    } else if domain.ends_with("baidu.com") && !domain.contains("passport") {
        2
    } else {
        1
    }
}

/// Reads a login window's cookie jar (shared with the main webview).
async fn harvest_cookies(window: WebviewWindow) -> AppResult<Vec<Cookie<'static>>> {
    let join = tauri::async_runtime::spawn_blocking(move || window.cookies());
    match tokio::time::timeout(HARVEST_TIMEOUT, join).await {
        Ok(Ok(Ok(cookies))) => Ok(cookies),
        Ok(Ok(Err(err))) => Err(AppError::new(format!("读取登录信息失败：{err}"))),
        Ok(Err(err)) => Err(AppError::new(format!("读取登录信息失败：{err}"))),
        Err(_) => Err(AppError::new("读取登录信息超时")),
    }
}

/// The marketing page only reveals its QR login card above a certain width, so
/// the window opens wide; `QUARK_LOGIN_SIZE` (e.g. `"1180x800"`) overrides it
/// while tuning.
fn login_size() -> (f64, f64) {
    if let Ok(raw) = std::env::var("QUARK_LOGIN_SIZE") {
        if let Some((w, h)) = raw.split_once('x') {
            if let (Ok(w), Ok(h)) = (w.trim().parse::<f64>(), h.trim().parse::<f64>()) {
                if w >= 400.0 && h >= 400.0 {
                    return (w, h);
                }
            }
        }
    }
    (1240.0, 860.0)
}

/// Opens the Quark web login window and watches it until the user is signed in.
#[tauri::command]
pub async fn open_web_login(app: AppHandle, state: State<'_, Arc<AppState>>) -> AppResult<()> {
    spawn_login_watch(
        app.clone(),
        Arc::clone(&state),
        LOGIN_LABEL,
        "登录夸克网盘账号",
        Url::parse(&std::env::var("QUARK_LOGIN_URL").unwrap_or_else(|_| START_URL.to_string()))
            .map_err(|err| AppError::new(format!("地址解析失败：{err}")))?,
        login_size().0,
        login_size().1,
        cookie_header,
        |state, header| Box::pin(async move { state.set_cookie(header).await }),
        EVENT_LOGIN,
    )
    .await
}

/// Opens the 139 web login window and watches it until `authorization` shows up.
#[tauri::command]
pub async fn open_web_login_139(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> AppResult<()> {
    spawn_login_watch(
        app.clone(),
        Arc::clone(&state),
        LOGIN_LABEL_139,
        "登录中国移动云盘账号",
        Url::parse(START_URL_139)
            .map_err(|err| AppError::new(format!("地址解析失败：{err}")))?,
        1180.0,
        800.0,
        |cookies| Some(cookie_value_139(cookies)),
        |state, token| {
            Box::pin(async move {
                // Keep whatever token worked before: if the fresh one turns out
                // to be unusable, roll back instead of leaving the drive broken.
                let file = state.paths.config_dir.join("yun139.toml");
                let previous = std::fs::read(&file).ok();
                let config =
                    crate::providers::Yun139Provider::save_token(&state.paths.config_dir, &token)
                        .await?;
                state.invalidate_providers().await;
                // A stored token the drive rejects would look like a success and
                // then fail on the first listing, so check it right away and
                // drop it again when it does not work.
                let drive = crate::providers::Yun139Provider::new(
                    config,
                    state
                        .paths
                        .config_dir
                        .join("yun139.toml")
                        .to_string_lossy()
                        .to_string(),
                );
                if let Err(err) = drive.list("0").await {
                    let _ = drive.logout().await;
                    match previous {
                        Some(bytes) => {
                            let _ = std::fs::write(&file, bytes);
                        }
                        None => {
                            let _ = std::fs::remove_file(&file);
                        }
                    }
                    state.invalidate_providers().await;
                    return Err(AppError::new(format!(
                        "已取得登录凭据，但中国移动云盘拒绝了它：{err}"
                    )));
                }
                Ok(())
            })
        },
        EVENT_LOGIN_139,
    )
    .await
}

/// Opens the 百度网盘 web login window and watches it until `BDUSS` shows up.
#[tauri::command]
pub async fn open_web_login_baidu(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> AppResult<()> {
    spawn_login_watch(
        app.clone(),
        Arc::clone(&state),
        LOGIN_LABEL_BAIDU,
        "登录百度网盘账号",
        Url::parse(START_URL_BAIDU)
            .map_err(|err| AppError::new(format!("地址解析失败：{err}")))?,
        login_size().0,
        login_size().1,
        |cookies| Some(cookie_header_baidu(cookies)).filter(|v| !v.is_empty()),
        |state, cookie| {
            Box::pin(async move {
                // `save_cookie` fetches `bdstoken` and the profile and proves the
                // session works, so a bogus harvest fails here instead of on the
                // first listing.
                //
                // It can still fail for a beat: the cookie jar is shared with
                // the login window, which is mid-redirect right after sign-in.
                // Retry a few times before giving up on the harvest.
                let mut last = None;
                for attempt in 0..4 {
                    match crate::providers::BaiduProvider::save_cookie(
                        &state.paths.config_dir,
                        &cookie,
                    )
                    .await
                    {
                        Ok(drive) => {
                            state.invalidate_providers().await;
                            return drive.list("0").await.map(|_| ());
                        }
                        Err(err) => {
                            crate::ylog!("baidu login: save attempt {} failed: {err}", attempt + 1);
                            last = Some(err);
                            tokio::time::sleep(Duration::from_millis(800)).await;
                        }
                    }
                }
                Err(last.unwrap_or_else(|| AppError::new("百度网盘登录失败")))
            })
        },
        EVENT_LOGIN_BAIDU,
    )
    .await
}

/// Shared body of both web-login commands: build the window, poll its cookie
/// jar, hand the harvested credential to `save`, emit `event` when done.
#[allow(clippy::too_many_arguments)]
async fn spawn_login_watch<F, S>(
    app: AppHandle,
    state: Arc<AppState>,
    label: &str,
    title: &str,
    url: Url,
    width: f64,
    height: f64,
    harvest_cred: F,
    save: S,
    event_name: &str,
) -> AppResult<()>
where
    F: Fn(&[Cookie<'static>]) -> Option<String> + Send + Sync + 'static,
    S: Fn(Arc<AppState>, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<()>> + Send>>
        + Send
        + Sync
        + 'static,
{
    if let Some(existing) = app.get_webview_window(label) {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(());
    }

    let window = WebviewWindowBuilder::new(&app, label, WebviewUrl::External(url))
        .title(title)
        .inner_size(width, height)
        .min_inner_size(760.0, 600.0)
        .user_agent(DESKTOP_UA)
        .center()
        .decorations(true)
        .build()
        .map_err(|err| AppError::new(format!("无法打开登录窗口：{err}")))?;
    let _ = window.set_focus();

    let label = label.to_string();
    let event_name = event_name.to_string();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..MAX_POLLS {
            tokio::time::sleep(POLL_INTERVAL).await;

            let Some(window) = handle.get_webview_window(&label) else {
                // The user closed the window: surface it so the frontend stops
                // its "waiting for login" state instead of hanging forever.
                let _ = handle.emit(&event_name, event(false, "登录窗口已关闭"));
                return;
            };

            let cookies = match harvest_cookies(window.clone()).await {
                Ok(cookies) => cookies,
                // A transient read failure must not kill the wait.
                Err(_) => continue,
            };
            if let Some(cred) = harvest_cred(&cookies) {
                match save(Arc::clone(&state), cred).await {
                    Ok(()) => {
                        let _ = window.close();
                        let _ = handle.emit(&event_name, event(true, "登录成功"));
                        return;
                    }
                    Err(err) => {
                        // Close the window too: the loop has stopped, so leaving
                        // it open would look like "logged in but nothing works".
                        let _ = window.close();
                        let _ = handle.emit(&event_name, event(false, err.to_string()));
                        return;
                    }
                }
            }
            // Not signed in yet: keep waiting.
        }

        if let Some(window) = handle.get_webview_window(&label) {
            let _ = window.close();
        }
        let _ = handle.emit(&event_name, event(false, "登录超时，请重试"));
    });

    Ok(())
}

/// The `authorization` cookie on `yun.139.com`, minus the `Basic ` prefix.
///
/// After a web login the site stores the ready-made drive-API header as a
/// cookie named `authorization` (escaped value). Our `cloud139` client sends
/// `Basic {authorization}`, so the prefix is stripped here. Anything shorter
/// than a real token is treated as "not logged in yet".
fn cookie_value_139(cookies: &[Cookie<'static>]) -> String {
    for cookie in cookies {
        if cookie.name() != "authorization" {
            continue;
        }
        // 139 writes host-only cookies (`path=/` with no Domain), so the domain
        // is often absent rather than `.139.com`; only reject a known-foreign one.
        let domain = cookie.domain().unwrap_or_default();
        if !domain.is_empty() && !domain.ends_with("139.com") {
            continue;
        }
        let value = percent_decode(cookie.value());
        let value = value.trim();
        let value = value
            .strip_prefix("Basic ")
            .or_else(|| value.strip_prefix("basic "))
            .unwrap_or(value)
            .trim();
        if value.len() > 40 {
            return value.to_string();
        }
    }
    String::new()
}

/// Lets the UI cancel an in-flight login (the poll loop notices the window is
/// gone and reports it as cancelled).
#[tauri::command]
pub async fn close_web_login(app: AppHandle) -> AppResult<()> {
    for label in [LOGIN_LABEL, LOGIN_LABEL_139, LOGIN_LABEL_BAIDU] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.close();
        }
    }
    Ok(())
}

/// The `escape()`d cookie values 139 writes need a light percent-decode; this
/// covers the `%XX` escapes that matter for a base64 token.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
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

#[cfg(test)]
mod tests {
    use super::{cookie_header, cookie_header_baidu, cookie_value_139, percent_decode};

    fn cookie(name: &'static str, value: &'static str, domain: &'static str) -> tauri::webview::Cookie<'static> {
        let mut cookie = tauri::webview::Cookie::new(name, value);
        cookie.set_domain(domain);
        cookie
    }



    #[test]
    fn baidu_harvest_keeps_everything_except_beacons() {
        let cookies = vec![
            cookie("BDUSS", "session-value-long-enough", ".baidu.com"),
            cookie("PANPSC", "signed-blob", ".pan.baidu.com"),
            cookie("SOMETHING_NEW", "future-cookie", ".baidu.com"),
            cookie("HMACCOUNT", "beacon", ".hm.baidu.com"),
            cookie("Hm_lvt_abc", "beacon", ".baidu.com"),
            cookie("OTHER", "not-baidu", ".example.com"),
        ];
        let header = cookie_header_baidu(&cookies);
        assert!(header.contains("PANPSC=signed-blob"), "{header}");
        assert!(header.contains("SOMETHING_NEW=future-cookie"), "{header}");
        assert!(!header.contains("HMACCOUNT"), "{header}");
        assert!(!header.contains("Hm_lvt"), "{header}");
        assert!(!header.contains("OTHER="), "{header}");
    }

    #[test]
    fn baidu_harvest_prefers_the_pan_host_for_duplicates() {
        let cookies = vec![
            cookie("BDUSS", "session-value-long-enough", ".baidu.com"),
            cookie("STOKEN", "pan-token", ".pan.baidu.com"),
            cookie("STOKEN", "passport-token", ".passport.baidu.com"),
        ];
        let header = cookie_header_baidu(&cookies);
        assert!(header.contains("STOKEN=pan-token"), "{header}");
        assert!(!header.contains("passport-token"), "{header}");
    }

    #[test]
    fn baidu_harvest_needs_a_session_cookie() {
        let cookies = vec![cookie("BAIDUID", "uid", ".baidu.com")];
        assert!(cookie_header_baidu(&cookies).is_empty());
    }

    /// `BDUSS` arrives before the pan session: harvesting in that window hands
    /// the drive a header it answers with `errno: -6`.
    #[test]
    fn baidu_harvest_waits_for_the_pan_session() {
        let early = vec![cookie("BDUSS", "session-value-long-enough", ".baidu.com")];
        assert!(cookie_header_baidu(&early).is_empty());

        let ready = vec![
            cookie("BDUSS", "session-value-long-enough", ".baidu.com"),
            cookie("PANPSC", "signed-blob", ".pan.baidu.com"),
        ];
        assert!(cookie_header_baidu(&ready).contains("PANPSC=signed-blob"));
    }

    #[test]
    fn keeps_only_quark_cookies_and_requires_a_session_token() {
        let jar = vec![
            cookie("__puus", "session-value-123456", "pan.quark.cn"),
            cookie("tfstk", "tracker", "quark.cn"),
            cookie("NID", "google", "google.com"),
        ];
        let header = cookie_header(&jar).expect("header");
        assert!(header.contains("__puus=session-value-123456"));
        assert!(header.contains("tfstk=tracker"));
        assert!(!header.contains("NID"));

        let jar = vec![cookie("tfstk", "tracker", "quark.cn")];
        assert!(cookie_header(&jar).is_none());
    }

    #[test]
    fn prefers_the_most_specific_domain() {
        let jar = vec![
            cookie("__puus", "broad-value-1234", "quark.cn"),
            cookie("__puus", "narrow-value-1234", "pan.quark.cn"),
        ];
        let header = cookie_header(&jar).expect("header");
        assert!(header.contains("__puus=narrow-value-1234"));
        assert!(!header.contains("broad-value-1234"));
    }

    #[test]
    fn harvests_139_authorization_cookie() {
        let mut auth = tauri::webview::Cookie::new(
            "authorization",
            "Basic cGM6MTM4MDA6dG9rZW4xMjM0NTY3ODkwMTIzNDU2Nzg5",
        );
        auth.set_domain("yun.139.com");
        assert_eq!(
            cookie_value_139(&[auth]),
            "cGM6MTM4MDA6dG9rZW4xMjM0NTY3ODkwMTIzNDU2Nzg5",
            "the Basic prefix must be stripped"
        );

        // A short/garbage value means "not logged in yet".
        let mut short = tauri::webview::Cookie::new("authorization", "Basic short");
        short.set_domain("yun.139.com");
        assert_eq!(cookie_value_139(&[short]), "");

        // A host-only cookie (no Domain attribute) still counts.
        let host_only =
            tauri::webview::Cookie::new("authorization", "Basic cGM6cGM6dG9rZW4tMTIzNDU2Nzg5MDEyMzQ1Njc4OTAx");
        assert_eq!(
            cookie_value_139(&[host_only]),
            "cGM6cGM6dG9rZW4tMTIzNDU2Nzg5MDEyMzQ1Njc4OTAx"
        );

        // A cookie for a foreign host is ignored.
        let mut other = tauri::webview::Cookie::new(
            "authorization",
            "Basic cGM6MTM4MDA6dG9rZW4xMjM0NTY3ODkwMTIzNDU2Nzg5",
        );
        other.set_domain("example.com");
        assert_eq!(cookie_value_139(&[other]), "");
    }

    #[test]
    fn percent_decodes_escaped_cookies() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
        assert_eq!(percent_decode("no-escapes"), "no-escapes");
        // A trailing incomplete escape passes through untouched.
        assert_eq!(percent_decode("ab%"), "ab%");
    }
}

/// Opens the 115 web login window and watches it until `SEID` shows up.
#[tauri::command]
pub async fn open_web_login_115(
    app: AppHandle,
    state: State<'_, Arc<AppState>>,
) -> AppResult<()> {
    spawn_login_watch(
        app.clone(),
        Arc::clone(&state),
        LOGIN_LABEL_115,
        "登录 115 网盘账号",
        Url::parse(START_URL_115)
            .map_err(|err| AppError::new(format!("地址解析失败：{err}")))?,
        login_size().0,
        login_size().1,
        |cookies| Some(cookie_header_115(cookies)).filter(|v| !v.is_empty()),
        |state, cookie| {
            Box::pin(async move {
                let dir = state.paths.config_dir.clone();
                let drive = crate::providers::Pan115Provider::save_cookie(&dir, &cookie)?;
                if let Err(err) = drive.list("0").await {
                    let _ = drive.logout().await;
                    state.invalidate_providers().await;
                    return Err(AppError::new(format!(
                        "已取得登录凭据，但 115 网盘拒绝了它：{err}"
                    )));
                }
                state.invalidate_providers().await;
                Ok(())
            })
        },
        EVENT_LOGIN_115,
    )
    .await
}

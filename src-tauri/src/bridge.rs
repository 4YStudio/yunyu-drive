//! The `desktop_*` HTTP bridge the Quark website talks to.
//!
//! The share page's 「打开客户端」 button does not rely on the custom protocol
//! alone: after firing the `qklink://` iframe it probes a set of loopback ports
//! (`http://127.0.0.1:{9125..9130}`) for endpoints such as `/desktop_info`,
//! `/desktop_caller?deeplink=…` and `/desktop_sharelink`. With nothing listening
//! the page falls back to "download the client" and the app never wakes up —
//! that is exactly the reported symptom.
//!
//! So we answer on those ports and, permissively, on a port of our own. Every
//! handler returns the `{success, code, msg, data}` envelope the page expects,
//! and `Access-Control-Allow-Origin: *` because the request comes from
//! `pan.quark.cn` to `127.0.0.1`.

use std::sync::Arc;

use serde_json::{json, Value};
use tauri::Manager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::deeplink;
use crate::state::AppState;

/// Ports the web client probes, in the order it tries them.
const PORTS: [u16; 6] = [9125, 9126, 9127, 9128, 9129, 9130];

/// Starts the bridge on the first free port of the list. Failure is not fatal:
/// the custom protocol remains available.
pub fn spawn(app: tauri::AppHandle, on_link: Arc<dyn Fn(String) + Send + Sync>) {
    tauri::async_runtime::spawn(async move {
        for port in PORTS {
            let addr = format!("127.0.0.1:{port}");
            let Ok(listener) = TcpListener::bind(&addr).await else {
                continue;
            };
            let handler = Arc::clone(&on_link);
            let app = app.clone();
            tokio::spawn(async move { serve(listener, app, handler).await });
            return;
        }
        eprintln!("deeplink bridge: no free port in {PORTS:?}, custom protocol only");
    });
}

async fn serve(
    listener: TcpListener,
    app: tauri::AppHandle,
    on_link: Arc<dyn Fn(String) + Send + Sync>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let handler = Arc::clone(&on_link);
        let app = app.clone();
        tokio::spawn(async move {
            let _ = handle(stream, app, handler).await;
        });
    }
}

async fn handle(
    mut stream: tokio::net::TcpStream,
    app: tauri::AppHandle,
    on_link: Arc<dyn Fn(String) + Send + Sync>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 16 * 1024];
    let n = match tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
        .await
    {
        Ok(Ok(n)) if n > 0 => n,
        _ => return Ok(()),
    };
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let Some(line) = request.lines().next() else {
        return Ok(());
    };
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    // The page answers a CORS preflight before any of this.
    if method == "OPTIONS" {
        return reply(&mut stream, 204, "").await;
    }

    let body = match path.trim_end_matches('/') {
        "/desktop_info" => json!({
            "success": true,
            "code": 0,
            "msg": "",
            "data": {
                "version": env!("CARGO_PKG_VERSION"),
                "isLogin": is_logged_in(&app).await,
                "os": "linux",
                "platform": "clouddrive",
            }
        }),
        "/desktop_caller" => {
            let link = query_param(&query, "deeplink")
                .as_deref()
                .and_then(deeplink::normalize);
            if let Some(link) = link {
                on_link(link);
            }
            ok(json!(true))
        }
        "/desktop_sharelink" => {
            // `{url, pwd_id, passcode, stash_id}` — whichever the page sent.
            let pwd_id = query_param(&query, "pwd_id")
                .or_else(|| query_param(&query, "pwdid"))
                .unwrap_or_default();
            let passcode = query_param(&query, "passcode").unwrap_or_default();
            let direct = query_param(&query, "url")
                .as_deref()
                .and_then(deeplink::normalize);
            let link = direct.or_else(|| {
                if pwd_id.is_empty() {
                    return None;
                }
                let mut url = format!("https://pan.quark.cn/s/{pwd_id}");
                if !passcode.is_empty() {
                    url.push_str("?pwd=");
                    url.push_str(&passcode);
                }
                deeplink::normalize(&url)
            });
            if let Some(link) = link {
                on_link(link);
            }
            ok(json!(true))
        }
        "/desktop_focus" => ok(json!(true)),
        "/desktop_check_account" => json!({
            "success": true,
            "code": 0,
            "msg": "",
            "data": { "same_account": true }
        }),
        // Endpoints the page may poke at that we deliberately no-op.
        "/desktop_devicetoken" | "/desktop_postlogin" | "/desktop_weblogin_confirm" | "/desktop_st"
        | "/desktop_webtoken" | "/desktop_webtokenid" | "/desktop_user_info"
        | "/desktop_imforward_show" | "/desktop_share_visiting" | "/desktop_buwang_config" => ok(json!({})),
        _ => {
            return reply(&mut stream, 404, "{\"success\":false,\"code\":404,\"msg\":\"not found\"}")
                .await
        }
    };
    reply(&mut stream, 200, &body.to_string()).await
}

async fn is_logged_in(app: &tauri::AppHandle) -> bool {
    let state: Arc<AppState> = app.state::<Arc<AppState>>().inner().clone();
    let logged_in = state.cookie.read().await.is_some();
    logged_in
}

fn ok(data: Value) -> Value {
    json!({ "success": true, "code": 0, "msg": "", "data": data })
}

fn query_param(query: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    for pair in query.split('&') {
        if let Some(raw) = pair.strip_prefix(&needle) {
            return Some(percent_decode(raw));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
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

async fn reply(stream: &mut tokio::net::TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Access-Control-Allow-Headers: *\r\n\
         Access-Control-Allow-Private-Network: true\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

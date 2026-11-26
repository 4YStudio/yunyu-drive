//! Network recorder.
//!
//! Every drive talks to its own API, and when a call goes wrong all the user
//! sees is "上传失败" — the interesting part (what the server actually answered,
//! which header it disliked, which file it refused) is gone. This module keeps
//! a ring buffer of requests and responses — method, redacted url, request
//! headers and body, status, timing, response headers and body — so the log
//! panel can show something close to Chrome's network tab.
//!
//! Providers use [`Http`] instead of `reqwest::Client`; every `send()` on a
//! [`Call`] lands here. Bodies are captured for textual responses only (JSON,
//! text, XML) and capped, so a 40 GB download never ends up in memory.
//!
//! Secrets are masked, never dropped: a cookie reads `abcd…yz（312 字节）`. The
//! log file is a plain file on the user's disk, so a token in it must not be
//! usable.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Body, Method, RequestBuilder, Response};
use serde::Serialize;

/// How many calls the viewer can scroll back through.
const MAX_RECORDS: usize = 400;
/// Longest request/response body kept per call.
const MAX_BODY: usize = 12 * 1024;

/// One request/response pair.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub seq: u64,
    /// `HH:MM:SS.mmm`, local time.
    pub at: String,
    /// Who made the call: `cloud189`, `quark`, `download`, …
    pub tag: String,
    pub method: String,
    /// Redacted: secret query values are masked like headers.
    pub url: String,
    pub status: Option<u16>,
    pub ms: u64,
    pub req_headers: Vec<(String, String)>,
    pub req_body: String,
    pub resp_headers: Vec<(String, String)>,
    pub resp_body: String,
    pub bytes: u64,
    pub error: Option<String>,
}

static RECORDS: OnceLock<Mutex<VecDeque<Record>>> = OnceLock::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

fn records() -> &'static Mutex<VecDeque<Record>> {
    RECORDS.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_RECORDS)))
}

fn store(record: Record) {
    if let Ok(mut buf) = records().lock() {
        if buf.len() >= MAX_RECORDS {
            buf.pop_front();
        }
        buf.push_back(record);
    }
}

/// Records newer than `since`, oldest first.
pub fn tail(since: u64, limit: usize, failed_only: bool) -> Vec<Record> {
    let Ok(buf) = records().lock() else {
        return Vec::new();
    };
    let mut out: Vec<Record> = buf
        .iter()
        .filter(|r| r.seq > since)
        .filter(|r| !failed_only || r.is_failure())
        .cloned()
        .collect();
    if out.len() > limit {
        out.drain(..out.len() - limit);
    }
    out
}

pub fn clear() {
    if let Ok(mut buf) = records().lock() {
        buf.clear();
    }
}

pub fn count() -> u64 {
    records().lock().map(|b| b.len() as u64).unwrap_or(0)
}

impl Record {
    /// Anything the network panel should paint red: a transport error, an HTTP
    /// error status, or an API envelope that says it failed.
    pub fn is_failure(&self) -> bool {
        if self.error.is_some() {
            return true;
        }
        match self.status {
            Some(code) if code >= 400 => true,
            // Drives answer `200` with `{"state":false}` or `{"code":-1}`, so
            // the body has to have a say too.
            _ => {
                let body = self.resp_body.as_str();
                body.contains("\"state\":false")
                    || body.contains("\"success\":false")
                    || body.contains("\"code\":-1")
                    || body.contains("\"res_code\":-")
            }
        }
    }
}

// ------------------------------------------------------------- redaction ---

/// Header/parameter names whose value must never be written out as-is.
fn is_secret(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cookie"
            | "set-cookie"
            | "authorization"
            | "signature"
            | "sessionkey"
            | "seid"
            | "kid"
            | "cid"
            | "uid"
            | "token"
            | "access_token"
            | "accesstoken"
            | "access-token"
            | "passwd"
            | "password"
            | "sign"
            | "userkey"
            | "x-oss-security-token"
            | "x-oss-credential"
            | "x-oss-signature"
            | "ossaccesskeyid"
            | "securityoken"
            | "session_secret"
            | "share_code"
    ) || name.contains("token")
        || name.contains("secret")
        || name.contains("cookie")
        || name.contains("signature")
        || name.contains("password")
}

fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let len = chars.len();
    if len == 0 {
        return String::new();
    }
    if len <= 8 {
        return format!("…（{len} 字节）");
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[len - 2..].iter().collect();
    format!("{head}…{tail}（{len} 字节）")
}

fn redact_url(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let cleaned: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) if is_secret(key) => format!("{key}={}", mask(value)),
            _ => pair.to_string(),
        })
        .collect();
    format!("{path}?{}", cleaned.join("&"))
}

fn headers_of(map: &HeaderMap) -> Vec<(String, String)> {
    map.iter()
        .map(|(name, value)| {
            let text = value.to_str().unwrap_or("<二进制>");
            let shown = if is_secret(name.as_str()) {
                mask(text)
            } else {
                text.to_string()
            };
            (name.as_str().to_string(), shown)
        })
        .collect()
}

// ------------------------------------------------------------------ bodies --

fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_BODY {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_BODY * 2 / 3).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(MAX_BODY / 6)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}\n…（省略 {0} 字符）…\n{tail}", text.chars().count() - head.chars().count() - tail.chars().count())
}

/// JSON is the norm here; pretty-printing it is what makes the panel readable.
fn clip_body(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => clip(&serde_json::to_string_pretty(&value).unwrap_or_else(|_| text.to_string())),
        Err(_) => clip(text),
    }
}

fn body_of(request: &reqwest::Request) -> String {
    match request.body() {
        None => String::new(),
        Some(body) => match body.as_bytes() {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => text.to_string(),
                Err(_) => format!("（{} 字节二进制请求体）", bytes.len()),
            },
            None => "（流式请求体，未记录）".to_string(),
        },
    }
}

// ---------------------------------------------------------------- wrapper ---

/// A `reqwest::Client` that records what it sends and receives.
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    tag: &'static str,
}

impl Http {
    pub fn new(client: reqwest::Client, tag: &'static str) -> Self {
        Self { client, tag }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> Call {
        self.call(Method::GET, url)
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> Call {
        self.call(Method::POST, url)
    }

    pub fn put(&self, url: impl reqwest::IntoUrl) -> Call {
        self.call(Method::PUT, url)
    }

    pub fn delete(&self, url: impl reqwest::IntoUrl) -> Call {
        self.call(Method::DELETE, url)
    }

    pub fn head(&self, url: impl reqwest::IntoUrl) -> Call {
        self.call(Method::HEAD, url)
    }

    pub fn request(&self, method: Method, url: impl reqwest::IntoUrl) -> Call {
        self.call(method, url)
    }

    fn call(&self, method: Method, url: impl reqwest::IntoUrl) -> Call {
        let text = url.as_str().to_string();
        Call {
            inner: self.client.request(method.clone(), url),
            tag: self.tag,
            method,
            url: text,
            streaming: false,
            started: Instant::now(),
        }
    }
}

/// One in-flight request. Every chainable method just forwards to `reqwest`,
/// `send()` is where the recording happens.
pub struct Call {
    inner: RequestBuilder,
    tag: &'static str,
    method: Method,
    url: String,
    /// Transfer calls (file bodies) never buffer their response.
    streaming: bool,
    started: Instant,
}

impl Call {
    /// Marks this as a file transfer: response bodies are not read into memory.
    pub fn streaming(mut self) -> Self {
        self.streaming = true;
        self
    }

    pub fn header<K, V>(mut self, key: K, value: V) -> Self
    where
        HeaderName: TryFrom<K>,
        <HeaderName as TryFrom<K>>::Error: Into<http::Error>,
        HeaderValue: TryFrom<V>,
        <HeaderValue as TryFrom<V>>::Error: Into<http::Error>,
    {
        self.inner = self.inner.header(key, value);
        self
    }

    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.inner = self.inner.headers(headers);
        self
    }

    pub fn query<T: serde::Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.inner = self.inner.query(query);
        self
    }

    pub fn json<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        self.inner = self.inner.json(value);
        self
    }

    pub fn form<T: serde::Serialize + ?Sized>(mut self, value: &T) -> Self {
        self.inner = self.inner.form(value);
        self
    }

    pub fn body<T: Into<Body>>(mut self, body: T) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    pub fn multipart(mut self, form: reqwest::multipart::Form) -> Self {
        self.inner = self.inner.multipart(form);
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.inner = self.inner.timeout(timeout);
        self
    }

    pub fn bearer_auth<T: std::fmt::Display>(mut self, token: T) -> Self {
        self.inner = self.inner.bearer_auth(token);
        self
    }

    pub fn basic_auth<U: std::fmt::Display>(mut self, user: U, password: Option<&str>) -> Self {
        self.inner = self.inner.basic_auth(user, password);
        self
    }

    pub fn build(self) -> reqwest::Result<reqwest::Request> {
        self.inner.build()
    }

    /// Sends, records, and hands the response back untouched (or rebuilt from
    /// the bytes that were read for the log).
    pub async fn send(self) -> reqwest::Result<Response> {
        let Call {
            inner,
            tag,
            method,
            url,
            streaming,
            started,
        } = self;

        // Snapshot what the request carries before `inner` consumes it.
        let snapshot = inner.try_clone().and_then(|b| b.build().ok());
        let req_headers = snapshot
            .as_ref()
            .map(|r| headers_of(r.headers()))
            .unwrap_or_default();
        let req_body = snapshot.as_ref().map(body_of).unwrap_or_default();

        match inner.send().await {
            Ok(response) => Ok(capture(
                tag,
                &method,
                &url,
                started,
                req_headers,
                req_body,
                streaming,
                response,
            )
            .await),
            Err(err) => {
                let record = Record {
                    seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
                    at: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
                    tag: tag.to_string(),
                    method: method.as_str().to_string(),
                    url: redact_url(&url),
                    status: None,
                    ms: started.elapsed().as_millis() as u64,
                    req_headers,
                    req_body,
                    resp_headers: Vec::new(),
                    resp_body: String::new(),
                    bytes: 0,
                    error: Some(err.to_string()),
                };
                announce(&record);
                store(record);
                Err(err)
            }
        }
    }
}

async fn capture(
    tag: &'static str,
    method: &Method,
    url: &str,
    started: Instant,
    req_headers: Vec<(String, String)>,
    req_body: String,
    streaming: bool,
    response: Response,
) -> Response {
    let status = response.status();
    let resp_headers = headers_of(response.headers());
    let declared = response.content_length();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let textual = content_type.contains("json")
        || content_type.contains("text")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("x-www-form-urlencoded");
    let fits = declared.map(|len| len <= MAX_BODY as u64).unwrap_or(true);

    let mut record = Record {
        seq: NEXT_SEQ.fetch_add(1, Ordering::Relaxed),
        at: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
        tag: tag.to_string(),
        method: method.as_str().to_string(),
        url: redact_url(url),
        status: Some(status.as_u16()),
        ms: started.elapsed().as_millis() as u64,
        req_headers,
        req_body,
        resp_headers,
        resp_body: String::new(),
        bytes: declared.unwrap_or(0),
        error: None,
    };

    if streaming || !textual || !fits {
        record.resp_body = match declared {
            Some(len) => format!("（{len} 字节响应体未记录）"),
            None => "（流式响应体未记录）".to_string(),
        };
        announce(&record);
        store(record);
        return response;
    }

    let status_code = status;
    let headers = response.headers().clone();
    let Ok(bytes) = response.bytes().await else {
        record.resp_body = "（读取响应体失败）".to_string();
        announce(&record);
        store(record);
        return empty_like(status_code, headers);
    };
    record.bytes = bytes.len() as u64;
    record.resp_body = clip_body(&String::from_utf8_lossy(&bytes));
    announce(&record);
    store(record);
    rebuild(status_code, headers, bytes.to_vec())
}

/// Mirrors the call into the plain text log so both views agree.
fn announce(record: &Record) {
    let path = match record.url.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(at) => rest[at..].split('?').next().unwrap_or("/"),
            None => "/",
        },
        None => record.url.as_str(),
    };
    let status = match record.status {
        Some(code) => code.to_string(),
        None => "无响应".to_string(),
    };
    let level = if record.is_failure() { "warn" } else { "net" };
    crate::logging::push(
        level,
        &record.tag,
        format!(
            "{} {path} → {status} · {} ms · {} B",
            record.method, record.ms, record.bytes
        ),
    );
}

fn rebuild(status: reqwest::StatusCode, headers: HeaderMap, bytes: Vec<u8>) -> Response {
    let mut builder = http::Response::builder().status(status.as_u16());
    if let Some(map) = builder.headers_mut() {
        *map = headers;
    }
    match builder.body(Body::from(bytes)) {
        Ok(response) => Response::from(response),
        Err(_) => empty_like(status, HeaderMap::new()),
    }
}

fn empty_like(status: reqwest::StatusCode, headers: HeaderMap) -> Response {
    let mut builder = http::Response::builder().status(status.as_u16());
    if let Some(map) = builder.headers_mut() {
        *map = headers;
    }
    Response::from(
        builder
            .body(Body::from(Vec::new()))
            .unwrap_or_else(|_| http::Response::new(Body::empty())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets_but_keeps_their_shape() {
        assert_eq!(mask("short"), "…（5 字节）");
        assert_eq!(mask("abcdefghij"), "abcd…ij（10 字节）");
        assert_eq!(mask(""), "");
    }

    #[test]
    fn redacts_secret_query_values_only() {
        let url = "https://api.example.com/v2/file?fid=123&sessionKey=SUPERSECRETVALUE&size=9";
        let redacted = redact_url(url);
        assert!(redacted.contains("fid=123"), "{redacted}");
        assert!(redacted.contains("size=9"), "{redacted}");
        assert!(!redacted.contains("SUPERSECRETVALUE"), "{redacted}");
    }

    #[test]
    fn clips_long_bodies_in_the_middle() {
        let text = "x".repeat(MAX_BODY * 2);
        let clipped = clip(&text);
        assert!(clipped.chars().count() < text.chars().count());
        assert!(clipped.contains("省略"));
    }

    #[test]
    fn flags_api_envelopes_that_say_no() {
        let mut record = Record {
            seq: 1,
            at: String::new(),
            tag: "quark".into(),
            method: "POST".into(),
            url: String::new(),
            status: Some(200),
            ms: 0,
            req_headers: Vec::new(),
            req_body: String::new(),
            resp_headers: Vec::new(),
            resp_body: "{\"state\":false,\"error\":\"拒绝\"}".into(),
            bytes: 0,
            error: None,
        };
        assert!(record.is_failure());
        record.resp_body = "{\"state\":true}".into();
        assert!(!record.is_failure());
    }
}

//! Normalising the several deep links that can hand us a share.
//!
//! The web client on `pan.quark.cn` has its own protocol family and the app is
//! launchable through three different spellings, depending on which button the
//! page decided to use:
//!
//! * `qklink://www.myquark.cn/?qk_tech=electron&qk_biz=cloud&qk_module=/cloud/schema&qk_params={...}`
//!   — the "打开客户端" iframe. The percent-encoded `qk_params` JSON carries
//!   another link in its `url` field.
//! * `qklink://openShareModal?pwdid=<id>&passcode=<pwd>&…` — the inner link.
//! * `qkclouddrive://…` — the same thing under the older scheme name.
//! * our own `yunyu-drive://share?url=<encoded https link>`, plus the
//!   pre-rebrand `quark-drive://share?…` which must keep working.
//!
//! Everything is folded into our current form, which `share::parse_link`
//! already understands, so the rest of the app never sees the difference.

/// Scheme we emit today.
pub const SCHEME: &str = "yunyu-drive";
/// Scheme emitted by releases before the rebrand.
pub const LEGACY_SCHEME: &str = "quark-drive";

/// Folds any supported link into `yunyu-drive://share?url=…`, or `None` when the
/// argument carries no share at all (the web client also sends `…/desktop_info`).
pub fn normalize(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let lower = raw.to_ascii_lowercase();
    // Already in our own form (current or pre-rebrand).
    if lower.starts_with(&format!("{SCHEME}://")) {
        return Some(raw.to_string());
    }
    if lower.starts_with(&format!("{LEGACY_SCHEME}://")) {
        // Re-tag the scheme but keep the `share?url=…` payload as is.
        let rest = &raw[LEGACY_SCHEME.len() + 3..];
        return Some(format!("{SCHEME}://{rest}"));
    }

    // A plain share page URL (that is what a copied browser link looks like).
    if lower.starts_with("http") && raw.contains("/s/") {
        return Some(wrap(raw));
    }

    // The wrapper: `…&qk_params={"url":"qklink://openShareModal?…"}`.
    if let Some(params) = query_value(raw, "qk_params") {
        let decoded = percent_decode(&params);
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&decoded) {
            if let Some(inner) = value.get("url").and_then(|v| v.as_str()) {
                return normalize(inner);
            }
        }
    }

    // The inner `openShareModal` call names the share directly.
    if raw.contains("openShareModal") {
        let pwd_id = query_value(raw, "pwd_id")
            .or_else(|| query_value(raw, "pwdid"))
            .unwrap_or_default();
        let passcode = query_value(raw, "passcode")
            .or_else(|| query_value(raw, "pwd"))
            .unwrap_or_default();
        if pwd_id.is_empty() {
            return None;
        }
        let mut url = format!("https://pan.quark.cn/s/{pwd_id}");
        if !passcode.is_empty() {
            url.push_str("?pwd=");
            url.push_str(&passcode);
        }
        return Some(wrap(&url));
    }

    None
}

fn wrap(url: &str) -> String {
    format!("{SCHEME}://share?url={}", percent_encode(url))
}

/// Value of `key=` in the query string, up to the next `&`.
fn query_value(raw: &str, key: &str) -> Option<String> {
    let (_, query) = raw.split_once('?')?;
    let needle = format!("{key}=");
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix(&needle) {
            return Some(value.to_string());
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

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_plain_share_pages() {
        let link = normalize("https://pan.quark.cn/s/af886c338574?pwd=6M1m").unwrap();
        assert_eq!(
            link,
            "yunyu-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Faf886c338574%3Fpwd%3D6M1m"
        );
    }

    #[test]
    fn unwraps_the_web_client_wrapper() {
        // `qk_params` is the JSON-encoded inner link, itself percent-encoded.
        let inner = "qklink://openShareModal?pwdid=af886c338574&pwd_id=af886c338574&passcode=6M1m&stash_id=x";
        let json = format!(r#"{{"url":"{inner}"}}"#);
        let wrapper = format!(
            "qklink://www.myquark.cn/?qk_tech=electron&qk_biz=cloud&qk_module=/cloud/schema&qk_params={}",
            percent_encode(&json)
        );
        let link = normalize(&wrapper).unwrap();
        assert_eq!(
            link,
            "yunyu-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Faf886c338574%3Fpwd%3D6M1m"
        );
    }

    #[test]
    fn reads_the_inner_scheme_directly() {
        let link =
            normalize("qklink://openShareModal?pwd_id=abc123&passcode=9zZ9").unwrap();
        assert_eq!(
            link,
            "yunyu-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Fabc123%3Fpwd%3D9zZ9"
        );
    }

    #[test]
    fn keeps_pre_rebrand_links_working() {
        // Releases before the rename emitted `quark-drive://`; those links may
        // still sit in someone's browser or shell history.
        let link = normalize("quark-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Fabc").unwrap();
        assert_eq!(
            link,
            "yunyu-drive://share?url=https%3A%2F%2Fpan.quark.cn%2Fs%2Fabc"
        );
    }

    #[test]
    fn ignores_non_share_arguments() {
        assert!(normalize("--hidden").is_none());
        assert!(normalize("qklink://openShareModal").is_none());
        assert!(normalize("").is_none());
    }
}

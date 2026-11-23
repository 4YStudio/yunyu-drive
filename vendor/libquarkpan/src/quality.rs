//! How good a minted download url looks.
//!
//! The same file can be minted through more than one client entry, and the
//! urls that come back are *not* interchangeable: they can point at different
//! CDN nodes, and an unhealthy one comes back as a short url that serves an
//! error page instead of the file. Picking the best of several is worth the
//! extra request when the alternative is a download that crawls or dies.
//!
//! This is a port of the scoring the reference downloader (GeZi) applies to
//! the urls its three entries return.

/// A url that scores at least this well is good enough to stop asking: it is
/// https plus one signature parameter, which every healthy url carries.
///
/// Without the early exit every download costs three api calls instead of one,
/// which at a few hundred segments per file is the difference between a
/// download that starts instantly and one that spends seconds minting.
pub const FAST_PATH_SCORE: i32 = 130;

/// Scores a url, higher is better.
///
/// The weights are deliberately coarse: this decides *which* of a handful of
/// urls to use, not whether a download is possible.
pub fn score(url: &str) -> i32 {
    // Shorter than this cannot be a signed download url; it is an error string
    // the api slipped into the field.
    if url.len() <= 20 {
        return -1;
    }
    let lower = url.to_lowercase();
    let mut score = 0;
    if lower.starts_with("https://") {
        score += 100;
    }
    // Signature parameters. A url with none of them is usually a placeholder
    // that will answer 403.
    if lower.contains("auth_key=") {
        score += 30;
    }
    if lower.contains("expires=") {
        score += 15;
    }
    if lower.contains("signature=") {
        score += 15;
    }
    if lower.contains(".quark.cn") {
        score += 10;
    }
    if lower.contains(".uc.cn") {
        score += 6;
    }
    if lower.contains("cdn") {
        score += 5;
    }
    // A url that says it failed is worse than no url at all: it looks like a
    // success to the caller and only fails once the transfer starts.
    if ["error", "denied", "forbidden"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        score -= 200;
    }
    if lower.len() < 60 {
        score -= 20;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "https://dl-pc-cf.quark.cn/file/abc?auth_key=1700000000-0-0-9f8e7d6c5b4a&Expires=1700000000&Signature=Zm9vYmFyYmF6";

    #[test]
    fn a_signed_https_url_reaches_the_fast_path() {
        assert!(score(GOOD) >= FAST_PATH_SCORE);
    }

    #[test]
    fn plain_http_loses_to_https() {
        let plain = GOOD.replace("https://", "http://");
        assert!(score(&plain) < score(GOOD));
    }

    #[test]
    fn an_error_url_is_ranked_below_everything() {
        let bad = "https://dl.quark.cn/error?denied=1&forbidden=true&error=nope&auth_key=x";
        assert!(score(bad) < 0);
        assert!(score(bad) < score(GOOD));
    }

    #[test]
    fn a_stub_url_is_refused() {
        assert_eq!(score("https://x.cn/a"), -1);
        assert_eq!(score(""), -1);
    }

    #[test]
    fn a_cdn_host_beats_a_bare_one() {
        let cdn = GOOD.replace("dl-pc-cf.quark.cn", "cdn.quark.cn");
        assert!(score(&cdn) > score(GOOD));
    }
}

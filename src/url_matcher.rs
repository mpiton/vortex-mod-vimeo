//! Vimeo URL detection and classification.
//!
//! ## Accepted URL shapes
//!
//! - `vimeo.com/<id>` — public video
//! - `vimeo.com/<id>/<hash>` — private video share link (hash is a token)
//! - `vimeo.com/showcase/<id>` or `vimeo.com/album/<id>` — playlist
//! - `vimeo.com/ondemand/<slug>` — rejected (paid content, out of scope)
//! - `player.vimeo.com/video/<id>` — embedded player URL
//!
//! `vimeo.com/<user>` artist profiles are rejected here because Vimeo's
//! public HTML for profiles is inconsistent; the MVP focuses on video
//! and showcase extraction.

use std::sync::OnceLock;

use regex::Regex;

/// Kind of Vimeo resource identified from a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    /// Single video: `vimeo.com/<id>` or `player.vimeo.com/video/<id>`
    Video,
    /// Private video share: `vimeo.com/<id>/<hash>`
    PrivateVideo,
    /// Showcase / album: `vimeo.com/showcase/<id>` or `vimeo.com/album/<id>`
    Showcase,
    /// Not a recognised Vimeo URL.
    Unknown,
}

/// Returns `true` if the URL is any form of recognised Vimeo resource.
pub fn is_vimeo_url(url: &str) -> bool {
    !matches!(classify_url(url), UrlKind::Unknown)
}

/// Classify the URL into a [`UrlKind`].
pub fn classify_url(url: &str) -> UrlKind {
    let Some((host_lower, path)) = validate_and_split(url) else {
        return UrlKind::Unknown;
    };

    if !is_vimeo_host(&host_lower) {
        return UrlKind::Unknown;
    }

    let path_only = normalize_path(path);

    // player.vimeo.com/video/<id>
    if host_lower == "player.vimeo.com" {
        return if player_video_regex().is_match(path_only) {
            UrlKind::Video
        } else {
            UrlKind::Unknown
        };
    }

    // vimeo.com family
    if showcase_or_album_regex().is_match(path_only) {
        return UrlKind::Showcase;
    }
    if private_video_regex().is_match(path_only) {
        return UrlKind::PrivateVideo;
    }
    if video_regex().is_match(path_only) {
        return UrlKind::Video;
    }
    UrlKind::Unknown
}

/// Strip query string, fragment, and trailing slash from a raw
/// path-and-query slice. `path#frag?q` (malformed but tolerated) is
/// handled by splitting on `#` first.
fn normalize_path(path: &str) -> &str {
    let no_frag = path.split('#').next().unwrap_or("");
    let no_query = no_frag.split('?').next().unwrap_or("");
    no_query.trim_end_matches('/')
}

fn is_vimeo_host(host: &str) -> bool {
    matches!(
        host,
        "vimeo.com" | "www.vimeo.com" | "player.vimeo.com" | "m.vimeo.com"
    )
}

// All four URL-classification regexes are compile-time constants:
// `.expect` documents the invariant and honours the crate-wide
// policy that production code paths must not `.unwrap()`.

fn video_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^/(\d{6,})$").expect("video_regex: compile-time constant regex must compile")
    })
}

fn private_video_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^/(\d{6,})/([a-f0-9]{8,})$")
            .expect("private_video_regex: compile-time constant regex must compile")
    })
}

fn showcase_or_album_regex() -> &'static Regex {
    // Fully anchored — trailing junk like `/foo/bar` after the numeric
    // ID must not match. Callers normalise query/fragment/trailing
    // slash before passing the path to this regex.
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^/(?:showcase|album)/(\d+)$")
            .expect("showcase_or_album_regex: compile-time constant regex must compile")
    })
}

fn player_video_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"^/video/(\d{6,})$")
            .expect("player_video_regex: compile-time constant regex must compile")
    })
}

/// Extract the numeric video ID from a URL or return `None` if the URL is
/// not a video / private-video shape. Used by the oEmbed request builder.
pub fn extract_video_id(url: &str) -> Option<String> {
    let (_, path) = validate_and_split(url)?;
    let path_only = normalize_path(path);

    if let Some(caps) = private_video_regex().captures(path_only) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    if let Some(caps) = video_regex().captures(path_only) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    if let Some(caps) = player_video_regex().captures(path_only) {
        return caps.get(1).map(|m| m.as_str().to_string());
    }
    None
}

/// Extract the private-share hash token from a Vimeo URL, or `None`
/// when the URL carries no hash. Used to keep the share-link token on
/// the embed URL when the `/config` JSON endpoint is refused and the
/// plugin falls back to scraping the embed HTML.
///
/// Two shapes are recognised:
///
/// - **Share link**: `vimeo.com/<id>/<hash>` — hash is the second path
///   segment.
/// - **Player iframe URL**: `player.vimeo.com/video/<id>?h=<hash>` —
///   hash is the `h` query parameter. `extract_video_id` already
///   accepts this URL shape, so we must also harvest the hash here
///   or a caller arriving at the embed fallback via a player URL
///   would silently drop the token and fail on restricted embeds.
pub fn extract_private_hash(url: &str) -> Option<String> {
    let (host, path) = validate_and_split(url)?;

    let path_only = normalize_path(path);
    if let Some(hash) = private_video_regex()
        .captures(path_only)
        .and_then(|c| c.get(2).map(|m| m.as_str().to_string()))
    {
        return Some(hash);
    }

    if host == "player.vimeo.com" {
        return extract_h_query_param(path);
    }

    None
}

/// Pull the `h=<hash>` value out of a raw path-and-query slice. Keeps
/// the same `[a-f0-9]{8,}` shape enforced by `private_video_regex` so
/// arbitrary query junk can't spoof a share token.
fn extract_h_query_param(path_and_query: &str) -> Option<String> {
    // Fragment comes *before* any query in URL grammar, so a `?` that
    // shows up after a `#` is part of the fragment and must not be
    // treated as the query delimiter. Strip the fragment first, then
    // look for `?`.
    let without_fragment = path_and_query.split('#').next().unwrap_or("");
    let query = without_fragment.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("h=") {
            if value.len() >= 8 && value.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Extract the numeric showcase / album ID from a URL or return `None`.
pub fn extract_showcase_id(url: &str) -> Option<String> {
    let (_, path) = validate_and_split(url)?;
    let path_only = normalize_path(path);
    showcase_or_album_regex()
        .captures(path_only)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
}

fn validate_and_split(url: &str) -> Option<(String, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return None;
    }
    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    };
    let authority_no_user = authority.rsplit('@').next().unwrap_or(authority);
    let host = extract_host(authority_no_user)?;
    Some((host.to_ascii_lowercase(), path_and_query))
}

/// Extract the host portion (without port) from an authority string.
/// Handles both plain hosts/IPv4 and bracketed IPv6 literals — see
/// the equivalent helper in the gallery plugin for the full policy.
fn extract_host(authority: &str) -> Option<&str> {
    if authority.is_empty() {
        return None;
    }
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        Some(&authority[..=close])
    } else {
        let host = authority.split(':').next().unwrap_or(authority);
        if host.is_empty() {
            None
        } else {
            Some(host)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("https://vimeo.com/123456789", UrlKind::Video)]
    #[case("https://www.vimeo.com/123456789", UrlKind::Video)]
    #[case("https://vimeo.com/123456789/abcdef1234", UrlKind::PrivateVideo)]
    #[case("https://vimeo.com/showcase/98765", UrlKind::Showcase)]
    #[case("https://vimeo.com/album/54321", UrlKind::Showcase)]
    #[case("https://player.vimeo.com/video/123456789", UrlKind::Video)]
    #[case("https://vimeo.com/123456789?autoplay=1", UrlKind::Video)]
    #[case("https://vimeo.com/123456789/", UrlKind::Video)]
    #[case("https://vimeo.com/ondemand/example", UrlKind::Unknown)]
    #[case("https://vimeo.com/user/foo", UrlKind::Unknown)]
    #[case("https://example.com/?u=vimeo.com/123", UrlKind::Unknown)]
    #[case("not a url", UrlKind::Unknown)]
    // Fragment stripping: `#t=30s` timestamps must not reclassify the URL.
    #[case("https://vimeo.com/123456789#t=30", UrlKind::Video)]
    #[case(
        "https://vimeo.com/123456789/abcdef1234#comment",
        UrlKind::PrivateVideo
    )]
    #[case("https://vimeo.com/showcase/98765#intro", UrlKind::Showcase)]
    // Showcase regex is anchored — junk after the numeric id is rejected.
    #[case("https://vimeo.com/showcase/98765/extra/segments", UrlKind::Unknown)]
    fn test_classify_url(#[case] url: &str, #[case] expected: UrlKind) {
        assert_eq!(classify_url(url), expected);
    }

    #[test]
    fn extract_video_id_public() {
        assert_eq!(
            extract_video_id("https://vimeo.com/123456789"),
            Some("123456789".into())
        );
    }

    #[test]
    fn extract_video_id_private() {
        assert_eq!(
            extract_video_id("https://vimeo.com/123456789/abcdef1234"),
            Some("123456789".into())
        );
    }

    #[test]
    fn extract_private_hash_from_share_link() {
        assert_eq!(
            extract_private_hash("https://vimeo.com/123456789/abcdef1234"),
            Some("abcdef1234".into())
        );
    }

    #[test]
    fn extract_private_hash_none_for_public_video() {
        assert!(extract_private_hash("https://vimeo.com/123456789").is_none());
    }

    #[test]
    fn extract_private_hash_none_for_showcase() {
        assert!(extract_private_hash("https://vimeo.com/showcase/98765").is_none());
    }

    #[test]
    fn extract_private_hash_strips_query_and_fragment() {
        assert_eq!(
            extract_private_hash("https://vimeo.com/123456789/abcdef1234?autoplay=1"),
            Some("abcdef1234".into())
        );
        assert_eq!(
            extract_private_hash("https://vimeo.com/123456789/abcdef1234#t=30"),
            Some("abcdef1234".into())
        );
    }

    #[test]
    fn extract_private_hash_from_player_query() {
        // `player.vimeo.com/video/<id>?h=<hash>` is the URL shape the
        // Vimeo player iframe uses. It needs to round-trip through
        // `extract_private_hash` or the embed-HTML fallback would omit
        // the token for restricted embeds.
        assert_eq!(
            extract_private_hash("https://player.vimeo.com/video/123456789?h=fba859c46b"),
            Some("fba859c46b".into())
        );
    }

    #[test]
    fn extract_private_hash_from_player_query_with_other_params() {
        assert_eq!(
            extract_private_hash(
                "https://player.vimeo.com/video/123456789?app_id=122963&h=fba859c46b&autoplay=0"
            ),
            Some("fba859c46b".into())
        );
        assert_eq!(
            extract_private_hash(
                "https://player.vimeo.com/video/123456789?h=fba859c46b&app_id=122963"
            ),
            Some("fba859c46b".into())
        );
    }

    #[test]
    fn extract_private_hash_from_player_query_rejects_non_hex() {
        // Arbitrary `?h=…` junk must not spoof a hash.
        assert!(
            extract_private_hash("https://player.vimeo.com/video/123456789?h=NOT-A-HASH").is_none()
        );
    }

    #[test]
    fn extract_private_hash_from_player_query_rejects_too_short() {
        assert!(extract_private_hash("https://player.vimeo.com/video/123456789?h=abc").is_none());
    }

    #[test]
    fn extract_private_hash_from_player_query_ignores_fragment() {
        // `?` inside a fragment must not be mistaken for the query
        // delimiter — the fragment (`#`) comes first in URL grammar.
        // This URL has no real query, only a fragment that happens to
        // contain `?h=…`; the hash in the fragment must not be accepted.
        assert!(
            extract_private_hash("https://player.vimeo.com/video/123456789#?h=deadbeefcafe")
                .is_none()
        );
    }

    #[test]
    fn extract_private_hash_from_player_query_with_trailing_fragment() {
        // Real query followed by a fragment — hash extraction must still
        // pick the query's value, not the fragment's.
        assert_eq!(
            extract_private_hash("https://player.vimeo.com/video/123456789?h=fba859c46b#t=30"),
            Some("fba859c46b".into())
        );
    }

    #[test]
    fn extract_private_hash_ignores_h_query_on_non_player_host() {
        // `vimeo.com` doesn't use the `?h=` query shape — only
        // `player.vimeo.com` does. Matching `?h=` on the main host
        // would accept spoofed URLs like `vimeo.com/foo?h=deadbeef…`
        // that aren't share links.
        assert!(
            extract_private_hash("https://vimeo.com/123456789?h=fba859c46b").is_none(),
            "vimeo.com should use path-segment hash only"
        );
    }

    #[test]
    fn extract_video_id_player() {
        assert_eq!(
            extract_video_id("https://player.vimeo.com/video/123456789"),
            Some("123456789".into())
        );
    }

    #[test]
    fn extract_video_id_rejects_showcase() {
        assert_eq!(extract_video_id("https://vimeo.com/showcase/1"), None);
    }

    #[test]
    fn extract_showcase_id_matches_showcase_and_album() {
        assert_eq!(
            extract_showcase_id("https://vimeo.com/showcase/98765"),
            Some("98765".into())
        );
        assert_eq!(
            extract_showcase_id("https://vimeo.com/album/54321"),
            Some("54321".into())
        );
    }

    #[test]
    fn is_vimeo_url_sanity() {
        assert!(is_vimeo_url("https://vimeo.com/1234567"));
        assert!(!is_vimeo_url("https://example.com/"));
    }
}

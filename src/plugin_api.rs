//! WASM-only module: `#[plugin_fn]` exports and `#[host_fn]` imports.

use extism_pdk::*;

use crate::error::PluginError;
use crate::parser::{
    build_embed_html_request, build_oembed_request, build_player_config_request,
    extract_player_config_from_html, parse_http_response, parse_oembed, parse_player_config,
};
use crate::url_matcher::{extract_private_hash, extract_video_id};
use crate::{
    build_media_variants_response, build_single_video_response, ensure_single_video,
    filter_audio_only, handle_can_handle, handle_supports_playlist, pick_variant_for_quality,
    MediaVariant, MediaVariantsResponse, VariantKind,
};

#[host_fn]
extern "ExtismHost" {
    fn http_request(req: String) -> String;
    fn get_config(key: String) -> String;
    /// JSON in → JSON out — see `yt_dlp::SubprocessRequest` /
    /// `yt_dlp::SubprocessResponse`. Used by `download_to_file` for
    /// the HLS/DASH adaptive fallback.
    fn run_subprocess(req: String) -> String;
}

#[plugin_fn]
pub fn can_handle(url: String) -> FnResult<String> {
    Ok(handle_can_handle(&url))
}

#[plugin_fn]
pub fn supports_playlist(url: String) -> FnResult<String> {
    Ok(handle_supports_playlist(&url))
}

#[plugin_fn]
pub fn extract_links(url: String) -> FnResult<String> {
    // Use `ensure_single_video` rather than `ensure_vimeo_url` so that
    // showcase URLs are rejected at the entrypoint and never reach
    // `build_single_video_response`. Showcase extraction is handled by
    // `extract_playlist`, which currently returns a clear unsupported
    // error until the token-gated showcase endpoint is wired up.
    ensure_single_video(&url).map_err(error_to_fn_error)?;

    let oembed = fetch_oembed(&url)?;
    // Pass the original URL through so private share links
    // (`vimeo.com/<id>/<hash>`) retain their hash token.
    let response = build_single_video_response(oembed, &url);
    Ok(serde_json::to_string(&response)?)
}

#[plugin_fn]
pub fn get_media_variants(url: String) -> FnResult<String> {
    ensure_single_video(&url).map_err(error_to_fn_error)?;

    let video_id = extract_video_id(&url)
        .ok_or_else(|| error_to_fn_error(PluginError::UnsupportedUrl(url.clone())))?;
    let hash = extract_private_hash(&url);
    let config = fetch_player_config(&video_id, hash.as_deref())?;
    let variants = build_media_variants_response(config);
    let filtered = if audio_only_preference() {
        filter_audio_only(variants)
    } else {
        variants
    };
    // Honour the user-configured `default_quality` by hoisting the
    // best matching variant to the head of the list. The host renders
    // the first entry as the default selection in the UI, so a stable
    // ordering plus a hoist gives us both deterministic output and
    // respect for the configured preference.
    let reordered = apply_quality_preference(filtered);
    Ok(serde_json::to_string(&reordered)?)
}

/// Resolve a direct CDN stream URL for a single video.
///
/// Input JSON: `{ "url", "quality"?, "format"?, "audio_only"? }`.
/// Returns the raw CDN URL string so the host can pass it directly to the
/// download engine. For progressive variants this is an MP4 CDN link; for
/// the adaptive stream it is an HLS m3u8 manifest URL.
///
/// `quality` is matched against progressive variant heights (e.g. `"720p"`).
/// When no progressive variant matches, the HLS adaptive stream is returned.
/// `audio_only` filters to audio-only variants when set. `format` is not
/// currently supported as Vimeo exposes only one format per quality level.
#[plugin_fn]
pub fn resolve_stream_url(input: String) -> FnResult<String> {
    #[derive(serde::Deserialize)]
    struct Input {
        url: String,
        #[serde(default)]
        quality: String,
        #[serde(default)]
        audio_only: bool,
    }

    let params: Input =
        serde_json::from_str(&input).map_err(|e| error_to_fn_error(PluginError::SerdeJson(e)))?;

    ensure_single_video(&params.url).map_err(error_to_fn_error)?;

    let video_id = extract_video_id(&params.url)
        .ok_or_else(|| error_to_fn_error(PluginError::UnsupportedUrl(params.url.clone())))?;
    let hash = extract_private_hash(&params.url);

    let config = fetch_player_config(&video_id, hash.as_deref())?;
    let variants = build_media_variants_response(config);

    // Audio-only mode: returns dedicated audio variants when present, or the
    // adaptive HLS stream when no dedicated audio variant exists (filter_audio_only
    // retains Adaptive entries for downstream demuxing).
    if params.audio_only {
        let cdn_url = filter_audio_only(variants)
            .variants
            .into_iter()
            .next()
            .map(|v| v.url)
            .ok_or_else(|| error_to_fn_error(PluginError::NoVariantsFound))?;
        return Ok(cdn_url);
    }

    // Prefer the highest progressive MP4; signal `AdaptiveStreamOnly`
    // when the only remaining option is HLS/DASH so the host falls
    // back to `download_to_file` (yt-dlp + ffmpeg merge). Returning
    // the `.m3u8` URL directly would make the generic download engine
    // save the playlist text file as if it were the video — what the
    // user hit before this change.
    let progressive_fallback = variants
        .variants
        .iter()
        .rev()
        .find(|v| matches!(v.kind, VariantKind::Video));
    let has_adaptive = variants
        .variants
        .iter()
        .any(|v| matches!(v.kind, VariantKind::Adaptive));

    let selected = if !params.quality.is_empty() {
        pick_variant_for_quality(&variants.variants, &params.quality)
            .and_then(|v| {
                if matches!(v.kind, VariantKind::Video) {
                    Some(v)
                } else {
                    None
                }
            })
            .or(progressive_fallback)
    } else {
        progressive_fallback
    };

    match selected {
        Some(v) => Ok(v.url.clone()),
        None if has_adaptive => Err(error_to_fn_error(PluginError::AdaptiveStreamOnly)),
        None => Err(error_to_fn_error(PluginError::NoVariantsFound)),
    }
}

/// Download a Vimeo video via yt-dlp when `resolve_stream_url` returned
/// `AdaptiveStreamOnly`. yt-dlp pulls the HLS/DASH manifests, downloads
/// the segment streams, and merges them with ffmpeg into a single file
/// at `output_dir/<id>.<ext>`. The merged path is returned as a raw
/// string.
///
/// Input: JSON `{ "url", "quality"?, "format"?, "output_dir", "audio_only"? }`
/// Output: absolute path of the merged file.
#[plugin_fn]
pub fn download_to_file(input: String) -> FnResult<String> {
    #[derive(serde::Deserialize)]
    struct Input {
        url: String,
        #[serde(default)]
        quality: String,
        #[serde(default)]
        format: String,
        output_dir: String,
        #[serde(default)]
        audio_only: bool,
    }

    let params: Input =
        serde_json::from_str(&input).map_err(|e| error_to_fn_error(PluginError::SerdeJson(e)))?;

    ensure_single_video(&params.url).map_err(error_to_fn_error)?;

    let args = crate::yt_dlp::yt_dlp_args_for_download_to_file(
        &params.url,
        &params.quality,
        &params.format,
        &params.output_dir,
        params.audio_only,
    );
    let req_json = crate::yt_dlp::build_download_request(args).map_err(error_to_fn_error)?;

    // SAFETY: `run_subprocess` is resolved by the Vortex plugin host
    // at load time (see src-tauri/src/adapters/driven/plugin/
    // host_functions.rs: `make_run_subprocess_function`). Invariants:
    //   1. The host registers the symbol in the `ExtismHost`
    //      namespace before any `#[plugin_fn]` export is callable.
    //   2. The ABI is `(I64) -> I64`; the `#[host_fn]` macro marshals
    //      `String` in/out through Extism memory handles.
    //   3. The host gates the call on the `subprocess = ["yt-dlp"]`
    //      capability declared in `plugin.toml`; a missing
    //      capability causes the call to fail before the binary is
    //      spawned.
    //   4. Inputs/outputs are owned JSON strings — no aliasing.
    let resp_json = unsafe { run_subprocess(req_json)? };
    let stdout =
        crate::yt_dlp::parse_subprocess_response(&resp_json).map_err(error_to_fn_error)?;
    crate::yt_dlp::parse_download_path_from_stdout(&stdout).map_err(error_to_fn_error)
}

#[plugin_fn]
pub fn extract_playlist(_url: String) -> FnResult<String> {
    // Showcase / album extraction is not implemented in the MVP — the
    // oEmbed endpoint does not enumerate showcase entries and the
    // relevant API endpoint requires an access token. Return a clear
    // error so the UI can surface an appropriate message.
    Err(error_to_fn_error(PluginError::UnsupportedUrl(
        "showcase extraction is not implemented yet".into(),
    )))
}

// ── Host function wiring ──────────────────────────────────────────────────────

fn fetch_oembed(video_url: &str) -> FnResult<crate::parser::OembedResponse> {
    let req = build_oembed_request(video_url).map_err(error_to_fn_error)?;
    // SAFETY: `http_request` is resolved by the Vortex plugin host at
    // load time (see src-tauri/src/adapters/driven/plugin/host_functions.rs:
    // `make_http_request_function`). Invariants:
    //   1. The host registers `http_request` in the `ExtismHost`
    //      namespace before any `#[plugin_fn]` export is callable.
    //   2. The ABI is `(I64) -> I64`; the `#[host_fn]` macro marshals
    //      `String` in/out through Extism memory handles.
    //   3. The host gates the call on the `http` capability from
    //      `plugin.toml`; rejections return an error which `?` surfaces.
    //   4. Inputs/outputs are owned JSON strings — no aliasing.
    let raw = unsafe { http_request(req)? };
    let resp = parse_http_response(&raw).map_err(error_to_fn_error)?;
    let body = resp.into_success_body().map_err(error_to_fn_error)?;
    parse_oembed(&body).map_err(error_to_fn_error)
}

fn fetch_player_config(
    video_id: &str,
    hash: Option<&str>,
) -> FnResult<crate::parser::PlayerConfig> {
    // Fast path: the JSON `/config` endpoint is authoritative and cheap
    // to parse. Returns 200 for any publicly playable video.
    let req = build_player_config_request(video_id).map_err(error_to_fn_error)?;
    // SAFETY: identical host-function invariants to `fetch_oembed`
    // above — the host-side symbol, ABI, capability gate, and owned
    // JSON I/O all apply unchanged. See `fetch_oembed` for the full
    // list.
    let raw = unsafe { http_request(req)? };
    let resp = parse_http_response(&raw).map_err(error_to_fn_error)?;
    match resp.into_success_body() {
        Ok(body) => match parse_player_config(&body) {
            Ok(cfg) => Ok(cfg),
            Err(_) => {
                // The `/config` endpoint occasionally returns HTML
                // rather than JSON (observed as a geo-blocked
                // intermediate page). Extract the embedded
                // `playerConfig` from that HTML before giving up.
                let json = extract_player_config_from_html(&body).map_err(error_to_fn_error)?;
                parse_player_config(json).map_err(error_to_fn_error)
            }
        },
        Err(PluginError::Private(_)) => {
            // Domain-restricted / privacy-gated videos refuse the JSON
            // `/config` endpoint (401/403) but still expose their
            // streams via the embed HTML page, which carries the same
            // `playerConfig` block inline in a `<script>` tag.
            //
            // This is exactly what yt-dlp does as its Vimeo primary
            // strategy: scrape the embed HTML instead of relying on
            // the JSON API that the player uses from an approved
            // origin.
            fetch_player_config_via_embed(video_id, hash)
        }
        Err(e) => Err(error_to_fn_error(e)),
    }
}

/// Fallback that scrapes the player embed HTML page for its inline
/// `playerConfig`. Returns a `Private` error if the embed itself is
/// refused (e.g. login-gated video) — at that point the plugin has
/// genuinely no anonymous way through.
fn fetch_player_config_via_embed(
    video_id: &str,
    hash: Option<&str>,
) -> FnResult<crate::parser::PlayerConfig> {
    let req = build_embed_html_request(video_id, hash).map_err(error_to_fn_error)?;
    // SAFETY: same invariants as `fetch_oembed` / the /config fetch.
    let raw = unsafe { http_request(req)? };
    let resp = parse_http_response(&raw).map_err(error_to_fn_error)?;
    let body = resp.into_success_body().map_err(error_to_fn_error)?;
    let json = extract_player_config_from_html(&body).map_err(error_to_fn_error)?;
    parse_player_config(json).map_err(error_to_fn_error)
}

/// Hoist the variant matching the user's `default_quality` preference
/// to the front of the list. The remaining entries keep their original
/// sort order from `build_media_variants_response`. If the config key
/// is missing, empty, or matches no progressive variant, the list is
/// returned unchanged.
fn apply_quality_preference(mut response: MediaVariantsResponse) -> MediaVariantsResponse {
    let preferred = default_quality_preference();
    if preferred.is_empty() {
        return response;
    }
    let Some(target_url) =
        pick_variant_for_quality(&response.variants, &preferred).map(|v| v.url.clone())
    else {
        return response;
    };
    // Re-order in place: pull the match out, push it to the front.
    if let Some(pos) = response
        .variants
        .iter()
        .position(|v: &MediaVariant| v.url == target_url)
    {
        let picked = response.variants.remove(pos);
        response.variants.insert(0, picked);
    }
    response
}

fn default_quality_preference() -> String {
    // SAFETY: identical host-function invariants to
    // `audio_only_preference` below — the host symbol is registered,
    // the ABI is `(I64) -> I64`, capability gating is manifest-driven,
    // and the returned string is owned.
    unsafe { get_config("default_quality".to_string()) }.unwrap_or_default()
}

/// Accepted truthy string values for boolean config keys sourced via
/// `get_config("extract_audio_only")` and any future boolean host
/// setting. The comparison is case-insensitive (values are lowercased
/// before the match), and any value outside this list falls back to
/// the documented default of `false`.
///
/// Keeping this list in one place makes the convention discoverable
/// and prevents drift if another config key later adopts the same
/// parser.
const TRUTHY_VALUES: &[&str] = &["true", "1", "yes"];

fn is_truthy(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    TRUTHY_VALUES.iter().any(|&v| v == lower)
}

fn audio_only_preference() -> bool {
    // Reads `get_config("extract_audio_only")` and interprets the
    // returned string via [`is_truthy`] / [`TRUTHY_VALUES`].
    //
    // SAFETY: `get_config` is registered host-side before plugin exports
    // run (see src-tauri/src/adapters/driven/plugin/host_functions.rs:
    // `make_get_config_function`). Invariants:
    //   1. The symbol is registered in the `ExtismHost` namespace
    //      before any `#[plugin_fn]` export is callable.
    //   2. The ABI is `(I64) -> I64`; the `#[host_fn]` macro marshals
    //      `String` in/out.
    //   3. A missing key or transient error yields the empty default
    //      which falls through to `false` — the documented default for
    //      `extract_audio_only`.
    //   4. Inputs/outputs are owned JSON strings — no aliasing concerns.
    let value = unsafe { get_config("extract_audio_only".to_string()) }.unwrap_or_default();
    is_truthy(&value)
}

fn error_to_fn_error(err: PluginError) -> WithReturnCode<extism_pdk::Error> {
    extism_pdk::Error::msg(err.to_string()).into()
}

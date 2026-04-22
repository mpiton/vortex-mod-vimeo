//! Plugin error type.

use thiserror::Error;

/// Errors raised by the Vimeo plugin.
#[derive(Debug, Error)]
pub enum PluginError {
    /// JSON parsing failure with contextual message.
    #[error("Vimeo JSON parse error: {0}")]
    ParseJson(String),

    /// Direct serde_json failure.
    #[error("JSON error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    /// `http_request` host function returned a non-2xx status.
    #[error("Vimeo API returned status {status}: {message}")]
    HttpStatus { status: u16, message: String },

    /// Vimeo player config JSON not found on the page HTML.
    #[error("Vimeo player config not found on page")]
    PlayerConfigNotFound,

    /// Host function returned an invalid response envelope.
    #[error("host function response invalid: {0}")]
    HostResponse(String),

    /// URL could not be classified as a Vimeo resource.
    #[error("URL is not a recognised Vimeo resource: {0}")]
    UnsupportedUrl(String),

    /// Vimeo video is private or requires authentication.
    #[error("Vimeo resource is private: {0}")]
    Private(String),

    /// Player config contained no usable variants.
    #[error("no playable variants found for this Vimeo video")]
    NoVariantsFound,

    /// Vimeo only exposes the video as an HLS/DASH adaptive stream at
    /// the requested quality. The host's generic HTTP download engine
    /// can't process an .m3u8 playlist, so the caller must fall back
    /// to `download_to_file` which delegates to yt-dlp + ffmpeg.
    ///
    /// The message is load-bearing: Vortex core matches the literal
    /// string `"adaptive stream (HLS/DASH)"` to recognise this case
    /// (see the `is_adaptive_stream_error` helper in
    /// `src-tauri/src/adapters/driven/plugin/extism_loader.rs`).
    #[error(
        "video is only available as an adaptive stream (HLS/DASH) at this quality; use download_to_file"
    )]
    AdaptiveStreamOnly,

    /// yt-dlp subprocess returned a non-zero exit code.
    #[error("yt-dlp failed (exit code {exit_code}): {stderr}")]
    Subprocess { exit_code: i32, stderr: String },
}

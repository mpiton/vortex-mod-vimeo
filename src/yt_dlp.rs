//! yt-dlp subprocess request/response helpers for the HLS/DASH
//! fallback path used by `download_to_file`.
//!
//! Vimeo serves most videos as HLS-only at quality ≥ 720p; the
//! Vortex download engine only knows how to fetch a single HTTPS
//! URL, so when `resolve_stream_url` can't find a progressive
//! variant it surfaces [`PluginError::AdaptiveStreamOnly`] and the
//! host delegates to yt-dlp through `download_to_file`. This
//! module builds the request/parses the response — the actual
//! host-function call is in `plugin_api.rs`.
//!
//! The design mirrors `vortex-mod-youtube::extractor` so the two
//! plugins stay consistent; yt-dlp quirks only need to be fixed
//! once.

use serde::{Deserialize, Serialize};

use crate::error::PluginError;

/// JSON request shape expected by the host's `run_subprocess` function.
#[derive(Debug, Serialize)]
pub struct SubprocessRequest {
    pub binary: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
}

/// JSON response shape returned by the host's `run_subprocess` function.
#[derive(Debug, Deserialize)]
pub struct SubprocessResponse {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Default timeout for a full video download+merge — 30 minutes.
/// Vimeo's HLS segments can be slow to fetch when the CDN is warming
/// up; 30 minutes gives plenty of headroom without leaving hung
/// processes if yt-dlp genuinely can't make progress.
pub const DEFAULT_DOWNLOAD_TIMEOUT_MS: u64 = 1_800_000;

/// Build yt-dlp args for a full video download+merge.
///
/// Writes the merged file to `output_dir/<id>.<ext>` and prints the
/// absolute path on the `after_move` hook so the caller can parse it
/// from stdout. `--print after_move:%(filepath)s` is the canonical
/// yt-dlp way to return "the file you just wrote" without relying on
/// template interpolation at the shell level.
///
/// `--` is used as a sentinel before the URL so a URL accidentally
/// starting with `-` can never be interpreted as a yt-dlp option.
pub fn yt_dlp_args_for_download_to_file(
    url: &str,
    quality: &str,
    format: &str,
    output_dir: &str,
    audio_only: bool,
) -> Vec<String> {
    let selector = build_download_format_selector(quality, format, audio_only);
    let merge_format = if format.is_empty() || !format.chars().all(|c| c.is_ascii_alphanumeric()) {
        "mp4".to_string()
    } else {
        format.to_string()
    };
    let output_template = format!("{output_dir}/%(id)s.%(ext)s");

    vec![
        "--format".into(),
        selector,
        "--merge-output-format".into(),
        merge_format,
        "--output".into(),
        output_template,
        "--print".into(),
        "after_move:%(filepath)s".into(),
        "--no-playlist".into(),
        "--no-warnings".into(),
        "--quiet".into(),
        // Fragment retries matter more for Vimeo than the HTTP
        // retries: HLS/DASH pulls dozens of .ts or .m4s chunks and
        // a single 503 would otherwise kill the whole download.
        "--retries".into(),
        "3".into(),
        "--fragment-retries".into(),
        "3".into(),
        "--".into(),
        url.into(),
    ]
}

/// Build a yt-dlp format selector for HLS/DASH download+merge.
///
/// Video: `bestvideo[height<=H]+bestaudio` (DASH video + audio merged
/// via ffmpeg). Falls back to `best[height<=H]` when the two-stream
/// combo isn't available. Audio-only: `bestaudio[ext=FORMAT]/bestaudio`.
fn build_download_format_selector(quality: &str, format: &str, audio_only: bool) -> String {
    let height: Option<u32> = quality.trim_end_matches('p').parse().ok();
    let has_format = !format.is_empty() && format.chars().all(|c| c.is_ascii_alphanumeric());

    if audio_only {
        if has_format {
            format!("bestaudio[ext={format}]/bestaudio")
        } else {
            "bestaudio".into()
        }
    } else {
        match height {
            Some(h) => format!(
                "bestvideo[height<={h}]+bestaudio/bestvideo[height<={h}]+bestaudio[ext=m4a]/best[height<={h}]"
            ),
            None => "bestvideo+bestaudio/best".into(),
        }
    }
}

/// Build the subprocess request JSON with the download-scale timeout.
pub fn build_download_request(args: Vec<String>) -> Result<String, PluginError> {
    let req = SubprocessRequest {
        binary: "yt-dlp".into(),
        args,
        timeout_ms: DEFAULT_DOWNLOAD_TIMEOUT_MS,
    };
    Ok(serde_json::to_string(&req)?)
}

/// Parse the host's subprocess response and extract stdout, or map
/// non-zero exit to [`PluginError::Subprocess`] with a bounded-size
/// stderr excerpt so error messages don't balloon.
pub fn parse_subprocess_response(response_json: &str) -> Result<String, PluginError> {
    let resp: SubprocessResponse = serde_json::from_str(response_json)?;
    if resp.exit_code != 0 {
        return Err(PluginError::Subprocess {
            exit_code: resp.exit_code,
            stderr: truncate_stderr(&resp.stderr),
        });
    }
    Ok(resp.stdout)
}

/// The absolute path of the merged output file is the last non-empty
/// line yt-dlp prints via `--print after_move:%(filepath)s`. Taking
/// the last (not first) line guards against any leading output that
/// may slip through even with `--quiet`.
pub fn parse_download_path_from_stdout(stdout: &str) -> Result<String, PluginError> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or(PluginError::NoVariantsFound)
}

/// Cap stderr at 512 characters on **character** boundaries so
/// multi-byte output (non-ASCII filenames, localised messages) can't
/// trip a WASM panic.
fn truncate_stderr(stderr: &str) -> String {
    const MAX_CHARS: usize = 512;
    let trimmed = stderr.trim();
    let char_count = trimmed.chars().count();
    if char_count <= MAX_CHARS {
        trimmed.to_string()
    } else {
        let truncated: String = trimmed.chars().take(MAX_CHARS).collect();
        format!("{truncated}… [truncated]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_args_include_url_and_format() {
        let args =
            yt_dlp_args_for_download_to_file("https://vimeo.com/123", "1080p", "mp4", "/tmp", false);
        // URL appears last, after the `--` sentinel.
        assert_eq!(args.last().map(String::as_str), Some("https://vimeo.com/123"));
        let sep_idx = args.iter().position(|a| a == "--").unwrap();
        let url_idx = args.iter().rposition(|a| a == "https://vimeo.com/123").unwrap();
        assert!(url_idx > sep_idx, "URL must follow the -- sentinel");
        // Quality height threaded into the selector.
        assert!(args.iter().any(|a| a.contains("height<=1080")));
        // mp4 is requested as the merge output.
        let merge_idx = args.iter().position(|a| a == "--merge-output-format").unwrap();
        assert_eq!(args.get(merge_idx + 1).map(String::as_str), Some("mp4"));
    }

    #[test]
    fn download_args_default_merge_format_when_format_empty() {
        let args = yt_dlp_args_for_download_to_file("https://vimeo.com/1", "", "", "/tmp", false);
        let merge_idx = args.iter().position(|a| a == "--merge-output-format").unwrap();
        assert_eq!(args.get(merge_idx + 1).map(String::as_str), Some("mp4"));
    }

    #[test]
    fn download_args_reject_non_alphanum_format() {
        // A hostile `format` like `mp4;rm -rf /` must NOT end up as a
        // yt-dlp argument; we fall back to the default mp4 merge.
        let args = yt_dlp_args_for_download_to_file(
            "https://vimeo.com/1",
            "",
            "mp4;rm -rf /",
            "/tmp",
            false,
        );
        let merge_idx = args.iter().position(|a| a == "--merge-output-format").unwrap();
        assert_eq!(args.get(merge_idx + 1).map(String::as_str), Some("mp4"));
    }

    #[test]
    fn download_args_audio_only() {
        let args =
            yt_dlp_args_for_download_to_file("https://vimeo.com/1", "", "m4a", "/tmp", true);
        let fmt_idx = args.iter().position(|a| a == "--format").unwrap();
        let sel = args.get(fmt_idx + 1).map(String::as_str).unwrap();
        assert!(sel.contains("bestaudio"));
        assert!(sel.contains("[ext=m4a]"));
    }

    #[test]
    fn parse_path_picks_last_non_empty_line() {
        let stdout = "\n[info] some chatter\n/tmp/output/1234.mp4\n\n";
        assert_eq!(parse_download_path_from_stdout(stdout).unwrap(), "/tmp/output/1234.mp4");
    }

    #[test]
    fn parse_path_errors_on_empty_stdout() {
        assert!(parse_download_path_from_stdout("").is_err());
        assert!(parse_download_path_from_stdout("\n\n   \n").is_err());
    }

    #[test]
    fn parse_response_propagates_non_zero_exit() {
        let json = r#"{"exit_code":1,"stdout":"","stderr":"boom"}"#;
        let err = parse_subprocess_response(json).unwrap_err();
        assert!(matches!(err, PluginError::Subprocess { exit_code: 1, .. }));
    }

    #[test]
    fn parse_response_ok_returns_stdout() {
        let json = r#"{"exit_code":0,"stdout":"/tmp/out.mp4\n","stderr":""}"#;
        assert_eq!(parse_subprocess_response(json).unwrap(), "/tmp/out.mp4\n");
    }

    #[test]
    fn truncate_stderr_handles_multibyte_boundaries() {
        // A long string of emoji characters would crash if truncation
        // used a byte offset rather than a char boundary.
        let long: String = "🔥".repeat(600);
        let out = truncate_stderr(&long);
        assert!(out.ends_with("[truncated]"));
        // No panic, string has valid UTF-8.
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn build_request_serialises_with_yt_dlp_binary() {
        let req = build_download_request(vec!["--version".into()]).unwrap();
        assert!(req.contains("\"binary\":\"yt-dlp\""));
        assert!(req.contains("\"timeout_ms\":1800000"));
    }
}

//! Typed request/response helpers for the host-managed yt-dlp broker.
//!
//! Vimeo serves most videos as HLS-only at quality ≥ 720p; the
//! Vortex download engine only knows how to fetch a single HTTPS
//! URL, so when `resolve_stream_url` can't find a progressive
//! variant it surfaces [`PluginError::AdaptiveStreamOnly`] and the
//! host delegates to yt-dlp through `download_to_file`.

use serde::{Deserialize, Serialize};

use crate::error::PluginError;

/// Closed request contract accepted by Vortex's `run_ytdlp` host function.
///
/// Process selection, command-line arguments, timeouts, environment, and the
/// working directory are deliberately absent: those controls belong to the
/// trusted host.
#[derive(Debug, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum YtDlpRequest<'a> {
    Download {
        url: &'a str,
        quality: Option<u32>,
        format: Option<&'a str>,
        output_dir: &'a str,
        audio_only: bool,
    },
}

/// JSON response shape returned by the host's `run_ytdlp` function.
#[derive(Debug, Deserialize)]
struct YtDlpResponse {
    exit_code: i32,
    stdout: String,
    stderr: String,
}

/// Serialize the Vimeo download operation for the trusted broker.
pub fn build_download_request(
    url: &str,
    quality: &str,
    format: &str,
    output_dir: &str,
    audio_only: bool,
) -> Result<String, PluginError> {
    let request = YtDlpRequest::Download {
        url,
        quality: parse_quality(quality),
        format: if audio_only {
            optional_audio_format(format)
        } else {
            None
        },
        output_dir,
        audio_only,
    };
    Ok(serde_json::to_string(&request)?)
}

fn parse_quality(quality: &str) -> Option<u32> {
    let quality = quality.trim();
    if quality.is_empty() || quality.eq_ignore_ascii_case("best") {
        return None;
    }
    quality.trim_end_matches('p').parse().ok()
}

fn optional_audio_format(format: &str) -> Option<&'static str> {
    const AUDIO_FORMATS: [&str; 9] = [
        "aac", "flac", "m4a", "mp3", "ogg", "opus", "vorbis", "wav", "webm",
    ];
    let format = format.trim();
    AUDIO_FORMATS
        .into_iter()
        .find(|allowed| format.eq_ignore_ascii_case(allowed))
}

/// Parse the host broker's response and extract stdout, or map
/// non-zero exit to [`PluginError::Subprocess`] with a bounded-size
/// stderr excerpt so error messages don't balloon.
pub fn parse_ytdlp_response(response_json: &str) -> Result<String, PluginError> {
    let resp: YtDlpResponse = serde_json::from_str(response_json)?;
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
///
/// Returns [`PluginError::EmptyDownloadPath`] (not `NoVariantsFound`)
/// when stdout is empty — the download pipeline ran, it just didn't
/// tell us where the file landed. Surfacing the right distinction in
/// the error message keeps logs honest when debugging yt-dlp quirks.
pub fn parse_download_path_from_stdout(stdout: &str) -> Result<String, PluginError> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or(PluginError::EmptyDownloadPath)
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
    fn parse_path_picks_last_non_empty_line() {
        let stdout = "\n[info] some chatter\n/tmp/output/1234.mp4\n\n";
        assert_eq!(
            parse_download_path_from_stdout(stdout).unwrap(),
            "/tmp/output/1234.mp4"
        );
    }

    #[test]
    fn parse_path_errors_with_empty_download_path_on_blank_stdout() {
        // The distinction matters: yt-dlp ran successfully (exit 0 was
        // already handled upstream) but didn't print a path. Reporting
        // `NoVariantsFound` here would mislead users into thinking the
        // video has no streams.
        assert!(matches!(
            parse_download_path_from_stdout(""),
            Err(PluginError::EmptyDownloadPath)
        ));
        assert!(matches!(
            parse_download_path_from_stdout("\n\n   \n"),
            Err(PluginError::EmptyDownloadPath)
        ));
    }

    #[test]
    fn parse_response_propagates_non_zero_exit() {
        let json = r#"{"exit_code":1,"stdout":"","stderr":"boom"}"#;
        let err = parse_ytdlp_response(json).unwrap_err();
        assert!(matches!(err, PluginError::Subprocess { exit_code: 1, .. }));
    }

    #[test]
    fn parse_response_ok_returns_stdout() {
        let json = r#"{"exit_code":0,"stdout":"/tmp/out.mp4\n","stderr":""}"#;
        assert_eq!(parse_ytdlp_response(json).unwrap(), "/tmp/out.mp4\n");
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
    fn download_request_uses_typed_broker_contract() {
        let req = build_download_request(
            "https://vimeo.com/123",
            "1080p",
            "m3u8",
            "/tmp/downloads",
            false,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&req).unwrap();

        assert_eq!(json["action"], "download");
        assert_eq!(json["url"], "https://vimeo.com/123");
        assert_eq!(json["quality"], 1080);
        assert!(json["format"].is_null());
        assert_eq!(json["output_dir"], "/tmp/downloads");
        assert_eq!(json["audio_only"], false);
        for forbidden in ["binary", "args", "timeout_ms"] {
            assert!(
                json.get(forbidden).is_none(),
                "unexpected process control: {forbidden}"
            );
        }
    }

    #[test]
    fn download_request_serialises_empty_options_as_null() {
        let req = build_download_request("https://vimeo.com/123", "", "", "/tmp/downloads", true)
            .unwrap();
        let json: serde_json::Value = serde_json::from_str(&req).unwrap();

        assert!(json["quality"].is_null());
        assert!(json["format"].is_null());
        assert_eq!(json["audio_only"], true);
    }

    #[test]
    fn audio_request_only_serialises_supported_audio_formats() {
        let req = build_download_request(
            "https://vimeo.com/123",
            "audio_only",
            "m4a",
            "/tmp/downloads",
            true,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&req).unwrap();
        assert_eq!(json["format"], "m4a");

        let req = build_download_request(
            "https://vimeo.com/123",
            "audio_only",
            "m3u8",
            "/tmp/downloads",
            true,
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_str(&req).unwrap();
        assert!(json["format"].is_null());
    }
}

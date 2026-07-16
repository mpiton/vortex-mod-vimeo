//! Real ABI smoke tests for the release WASM artifact.

use std::path::PathBuf;

use extism::{Function, UserData, Val, PTR};

const WASM_REL_PATH: &str = "target/wasm32-wasip1/release/vortex_mod_vimeo.wasm";
const VIMEO_URL: &str = "https://vimeo.com/123456789";

const OEMBED_BODY: &str = r#"{
    "type":"video",
    "title":"Sintel trailer",
    "description":"Blender demo",
    "author_name":"Blender Foundation",
    "thumbnail_url":"https://i.vimeocdn.com/video/1.jpg",
    "duration":52,
    "video_id":123456789
}"#;

const PLAYER_CONFIG_BODY: &str = r#"{
    "request":{"files":{
        "progressive":[
            {
                "profile":164,
                "quality":"360p",
                "width":640,
                "height":360,
                "fps":24.0,
                "mime":"video/mp4",
                "url":"https://vod.vimeo.com/360.mp4"
            },
            {
                "profile":165,
                "quality":"720p",
                "width":1280,
                "height":720,
                "fps":24.0,
                "mime":"video/mp4",
                "url":"https://vod.vimeo.com/720.mp4"
            }
        ],
        "hls":{
            "cdns":{"akfire":{"url":"https://akamai.vimeo.com/master.m3u8"}},
            "default_cdn":"akfire"
        }
    }},
    "video":{"id":123456789,"title":"Sintel trailer","duration":52}
}"#;

fn wasm_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(WASM_REL_PATH);
    assert!(
        path.is_file(),
        "missing release WASM artifact at {}; run `cargo build --target wasm32-wasip1 --release` first",
        path.display()
    );
    path
}

fn stub_http_request() -> Function {
    Function::new(
        "http_request",
        [PTR],
        [PTR],
        UserData::<()>::default(),
        |plugin, inputs, outputs, _user_data: UserData<()>| {
            let input = inputs[0]
                .i64()
                .ok_or_else(|| extism::Error::msg("expected i64 HTTP request input"))?;
            let request: String = plugin.memory_get_val(&Val::I64(input))?;
            let request: serde_json::Value = serde_json::from_str(&request)?;
            if request["method"] != "GET" {
                return Err(extism::Error::msg("expected Vimeo GET request"));
            }
            let url = request["url"]
                .as_str()
                .ok_or_else(|| extism::Error::msg("HTTP request URL is missing"))?;
            let body = if url.starts_with("https://vimeo.com/api/oembed.json?") {
                OEMBED_BODY
            } else if url == "https://player.vimeo.com/video/123456789/config" {
                PLAYER_CONFIG_BODY
            } else {
                return Err(extism::Error::msg(format!(
                    "unexpected Vimeo HTTP request: {url}"
                )));
            };
            let response = serde_json::json!({
                "status": 200,
                "headers": {},
                "body": body,
            })
            .to_string();
            let handle = plugin.memory_new(&response)?;
            outputs[0] = Val::I64(handle.offset() as i64);
            Ok(())
        },
    )
}

fn stub_get_config() -> Function {
    Function::new(
        "get_config",
        [PTR],
        [PTR],
        UserData::<()>::default(),
        |plugin, inputs, outputs, _user_data: UserData<()>| {
            let input = inputs[0]
                .i64()
                .ok_or_else(|| extism::Error::msg("expected i64 config input"))?;
            let key: String = plugin.memory_get_val(&Val::I64(input))?;
            let value = match key.as_str() {
                "default_quality" => "720p",
                "extract_audio_only" => "false",
                _ => "",
            };
            let handle = plugin.memory_new(value)?;
            outputs[0] = Val::I64(handle.offset() as i64);
            Ok(())
        },
    )
}

fn stub_run_ytdlp() -> Function {
    Function::new(
        "run_ytdlp",
        [PTR],
        [PTR],
        UserData::<()>::default(),
        |plugin, inputs, outputs, _user_data: UserData<()>| {
            let input = inputs[0]
                .i64()
                .ok_or_else(|| extism::Error::msg("expected i64 yt-dlp request input"))?;
            let request: String = plugin.memory_get_val(&Val::I64(input))?;
            let request: serde_json::Value = serde_json::from_str(&request)?;
            for forbidden in ["binary", "args", "timeout_ms"] {
                if request.get(forbidden).is_some() {
                    return Err(extism::Error::msg(format!(
                        "plugin exposed forbidden process control: {forbidden}"
                    )));
                }
            }
            if request["action"] != "download"
                || request["url"] != VIMEO_URL
                || request["quality"] != 1080
                || !request["format"].is_null()
                || request["audio_only"] != false
            {
                return Err(extism::Error::msg("unexpected typed yt-dlp request"));
            }
            let output_dir = request["output_dir"]
                .as_str()
                .ok_or_else(|| extism::Error::msg("output_dir is missing"))?;
            let response = serde_json::json!({
                "exit_code": 0,
                "stdout": format!("{output_dir}/123456789.mp4\n"),
                "stderr": "",
            })
            .to_string();
            let handle = plugin.memory_new(&response)?;
            outputs[0] = Val::I64(handle.offset() as i64);
            Ok(())
        },
    )
}

fn load_plugin() -> extism::Plugin {
    let manifest = extism::Manifest::new([extism::Wasm::file(wasm_path())]);
    extism::Plugin::new(
        &manifest,
        [stub_http_request(), stub_get_config(), stub_run_ytdlp()],
        true,
    )
    .expect("load Vimeo release WASM")
}

#[test]
fn wasm_routing_exports_are_callable() {
    let mut plugin = load_plugin();
    let can_handle: String = plugin.call("can_handle", VIMEO_URL).expect("can_handle");
    let supports_playlist: String = plugin
        .call("supports_playlist", VIMEO_URL)
        .expect("supports_playlist");

    assert_eq!(can_handle.trim(), "true");
    assert_eq!(supports_playlist.trim(), "false");
}

#[test]
fn wasm_metadata_exports_are_callable() {
    let mut plugin = load_plugin();
    let links: String = plugin
        .call("extract_links", VIMEO_URL)
        .expect("extract_links");
    let variants: String = plugin
        .call("get_media_variants", VIMEO_URL)
        .expect("get_media_variants");
    let links: serde_json::Value = serde_json::from_str(&links).expect("extract_links JSON");
    let variants: serde_json::Value =
        serde_json::from_str(&variants).expect("get_media_variants JSON");

    assert_eq!(links["kind"], "video");
    assert_eq!(links["videos"][0]["id"], "123456789");
    assert_eq!(
        variants["variants"].as_array().map(Vec::len),
        Some(3),
        "two progressive variants plus the HLS fallback"
    );
}

#[test]
fn wasm_media_exports_use_the_typed_broker() {
    let mut plugin = load_plugin();
    let resolve_input = r#"{"url":"https://vimeo.com/123456789","quality":"720p","format":"mp4","audio_only":false}"#;
    let download_input = r#"{"url":"https://vimeo.com/123456789","quality":"1080p","format":"m3u8","output_dir":"/tmp/vortex-downloads/job","audio_only":false}"#;
    let direct_url: String = plugin
        .call("resolve_stream_url", resolve_input)
        .expect("resolve_stream_url");
    let path: String = plugin
        .call("download_to_file", download_input)
        .expect("download_to_file");

    assert_eq!(direct_url, "https://vod.vimeo.com/720.mp4");
    assert_eq!(path, "/tmp/vortex-downloads/job/123456789.mp4");
}

#[test]
fn wasm_playlist_export_reports_the_documented_unsupported_case() {
    let mut plugin = load_plugin();
    let error = plugin
        .call::<_, String>("extract_playlist", "https://vimeo.com/showcase/123")
        .expect_err("showcase extraction is intentionally unsupported");

    assert!(error
        .to_string()
        .contains("showcase extraction is not implemented yet"));
}

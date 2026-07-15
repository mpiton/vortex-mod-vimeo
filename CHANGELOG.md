# Changelog

All notable changes to vortex-mod-vimeo will be documented here.
Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.4.0] - 2026-07-15

### Added

- Added a real Extism smoke test for the release WASM artifact, covering every
  crawler export and the typed yt-dlp broker boundary.

### Security

- Replaced the generic `run_subprocess` import with Vortex 0.2's typed
  `run_ytdlp` broker. The plugin now sends only a download action, URL, media
  preferences, and output directory; the trusted host owns the binary,
  command-line arguments, timeout, environment, and working directory.

## [1.3.1] - previous release

- Previous releases were not documented in this file.

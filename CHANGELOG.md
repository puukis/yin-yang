# Changelog

All notable changes to this project will be documented in this file.

The format follows Keep a Changelog and the project uses SemVer-style tags for GitHub releases.
Detailed curated release bodies live under `docs/releases/`.

## Unreleased

## 0.2.0-alpha.1 - 2026-04-02

### Added

- Parity-protected QUIC video datagrams with client-side fragment recovery and rate-limited IDR fallback.
- Client/server telemetry plus adaptive bitrate and FPS controls across the host pipeline and macOS client.

### Changed

- GitHub releases now publish curated markdown from `docs/releases/` instead of generated boilerplate.

## 0.1.0-alpha.1 - 2026-04-02

### Added

- Initial public alpha release.
- QUIC datagram video transport with control, input, and cursor channels over a single connection.
- Linux Wayland and Windows host paths with NVENC-backed video encode.
- macOS client path with VideoToolbox decode, Metal presentation, and HID-based input forwarding.
- GitHub release automation, prerelease packaging, and repository health files for easier adoption.

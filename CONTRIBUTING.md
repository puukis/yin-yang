# Contributing

streamd is a small Rust workspace with real release automation and CI across the supported build surfaces. Contributions are welcome, especially for:

* Bug reports with reproduction steps
* Platform-specific fixes such as compositor quirks, driver edge cases, or input oddities
* Performance improvements in the capture, encode, transport, or decode pipeline
* Windows host path improvements
* Protocol extensions such as additional codecs, negotiation, or telemetry fields
* Packaging, docs, and release engineering improvements that make the project easier to adopt

## Getting started

```bash
git clone https://github.com/puukis/streamd.git
cd streamd
cargo check --workspace
cargo test --workspace
```

## Code style

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

Both are enforced by CI. Format before opening a PR.

## Release-aware changes

Releases are cut from git tags and built by `cargo-dist`.

If you touch packaging, binary metadata, or release automation, also run:

```bash
dist generate
dist plan
```

`cargo-dist` 0.31.0 currently needs a small manual fix in `.github/workflows/release.yml` for the SBOM upload step, so always inspect that diff before committing regenerated release CI.

If your change should be called out in release notes, apply one of the standard labels:

* `enhancement`
* `bug`
* `documentation`
* `dependencies`
* `breaking-change`

## Changing the protocol

`streamd-proto` defines all on-wire types. If you change `ControlMsg`, `VideoPacketHeader`, `InputPacket`, or any other packet type:

* Update both client and server in the same commit
* Bump `PROTOCOL_VERSION` in `crates/streamd-proto/src/packets.rs`
* Document the protocol impact in your PR description

The server rejects connections from clients built against a different protocol version, so both sides must always be in sync.

## Pull requests

* Keep PRs focused, one logical change per PR
* Include a short description of what changed and why
* If the change affects the video pipeline or transport, describe how you tested it: hardware, compositor, codec, and LAN or WAN conditions
* If you change CLI behavior, packaging, or user-facing docs, update `README.md` and `CHANGELOG.md` in the same PR when appropriate

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md). The most useful reports include:

* OS, compositor, GPU, and driver versions on the host
* macOS version and hardware on the client
* `RUST_LOG=debug` output from both sides
* Whether the issue is LAN-only or also happens over WAN

For setup questions or general usage help, prefer [GitHub Discussions](https://github.com/puukis/streamd/discussions).

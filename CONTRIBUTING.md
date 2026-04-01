# Contributing

streamd is a personal project, but issues and pull requests are welcome, especially for:

* Bug reports with reproduction steps
* Platform specific fixes (compositor quirks, driver edge cases)
* Performance improvements to the capture / encode / transport pipeline
* Windows server path improvements
* Protocol extensions (additional codecs, resolution negotiation, etc.)

## Getting started

```bash
git clone https://github.com/puukis/streamd.git
cd streamd
cargo check --workspace
```

## Code style

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
```

Both are enforced by CI. Format before opening a PR.

## Changing the protocol

`streamd-proto` defines all on wire types. If you change `ControlMsg`, `VideoPacketHeader`, `InputPacket`, or any other packet type:

* Update both client and server in the same commit
* Bump `PROTOCOL_VERSION` in `crates/streamd-proto/src/packets.rs`
* Document the change in your PR description

The server rejects connections from clients built against a different protocol version, so both sides must always be in sync.

## Pull requests

* Keep PRs focused, one logical change per PR
* Include a short description of what changed and why
* If the change affects the video pipeline or transport, describe how you tested it (hardware, compositor, network conditions)

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.md). The most useful bug reports include:

* OS, compositor, GPU, and driver versions on the server
* macOS version on the client
* `RUST_LOG=debug` output from both server and client
* Whether the issue is LAN only or also happens over WAN

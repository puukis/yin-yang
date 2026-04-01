---
name: Bug report
about: Something isn't working
labels: bug
---

## Environment

**Server**
* OS / distro:
* Compositor (Wayland compositor name and version):
* GPU + driver version (`nvidia-smi`):
* Rust toolchain version (`rustup show`):

**Client**
* macOS version:
* Mac model / GPU:
* Rust toolchain version:

## What happened

Describe the bug. What did you expect versus what actually happened?

## Steps to reproduce

1.
2.
3.

## Logs

Run both sides with RUST_LOG=debug and paste the relevant output.

<details>
<summary>Server log</summary>

```
paste here
```
</details>

<details>
<summary>Client log</summary>

```
paste here
```
</details>

## Additional context

* LAN or WAN?
* Capture mode (DMA BUF or SHM fallback, check server log for "capture mode"):
* Codec negotiated (check server log for "SessionAccept"):

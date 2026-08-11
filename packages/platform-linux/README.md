# remotelink-platform-linux

Linux host platform layer for RemoteLink: **PipeWire screen capture** and
**PipeWire / PulseAudio monitor** system-audio capture.

## Secondary platform status

| Aspect | Status |
|--------|--------|
| Role | **Secondary** host platform (DESIGN G6: GA candidate; Windows is closed-beta primary) |
| CI | Builds and unit-tests on **all** targets via mocks; native PipeWire is `cfg(target_os = "linux")` |
| Native capture | Skeleton only — documents portal/PipeWire open sequence; returns `Unsupported` / `NativeUnavailable` until linked |
| Mock path | Always available: synthetic RGB frames + PCM monitor packets |
| Input injection | Out of scope for this package (uinput/libei later) |
| Packaging | Not packaged for end users yet; post-beta track |

RemoteLink v1 closed beta is **Windows host first**. Linux host is intentional
post-beta / secondary work so core latency and identity work is not blocked.

## Public surface

- **Video:** [`open_capture`](src/capture/mod.rs) with `CaptureBackend::{Mock, Platform}`
- **Audio:** [`open_monitor`](src/audio/mod.rs) with `MonitorOpenMode::{PreferNative, StubOnly, NativeOnly}`
- Both implement `remotelink_media::{VideoSource, AudioSource}` (shared PTS / frame types)

## Mock / CI contract

On non-Linux (including Windows GNU CI):

- `CaptureBackend::Mock` and `MonitorOpenMode::StubOnly` **always succeed**
- `CaptureBackend::Platform` / `NativeOnly` return structured errors (`Unsupported` / `NativeUnavailable`)
- No PipeWire / Pulse system libraries are required to build or test

On Linux builds, the same mock path works; native open remains a documented
skeleton until `libpipewire` (and portal session) bindings land.

## Design references

- DESIGN § Host screen capture: PipeWire / X11 / Wayland portals
- DESIGN § System audio: PipeWire / Pulse **monitor** sources (not mic)
- DESIGN PR 22: `feat(host-linux): PipeWire capture and audio monitor`
- KD5: capture lives in the session agent process; control IPC carries no media bytes

## Build

```bash
cargo test -p remotelink-platform-linux
cargo clippy -p remotelink-platform-linux --all-targets -- -D warnings
```

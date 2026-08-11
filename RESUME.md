# RemoteLink — Resume handoff

**Saved:** 2026-08-11  
**Plan ID:** `35709e22`  
**Primary tree:** branch **`main`** at `C:\Users\Linked\Documents\remotelink`

## Status

| Metric | Value |
|--------|--------|
| PR plan | **PRs 1–27 complete** (8b optional skipped) |
| **Integrated monorepo** | **Yes** — `cargo test --workspace` green (default features) |
| PeerTransport backends | **mock** (CI default) · **live TCP** (default feature) · **webrtc-rs** (opt-in feature) |
| Real AnyDesk product | **~65%** — SessionManager/viewer factory wiring for mock/live/webrtc; real ICE/DTLS DataChannels; media still interim DC NALU/Opus (not SampleBuilder RTP); no full installers |

## Day-to-day development

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

# CI-safe default (mock + live)
cargo test --workspace

# Real webrtc-rs backend unit tests
cargo test -p remotelink-net --features webrtc-rs

# Host / viewer demos
cargo run -p remotelink-host -- --role=agent --transport=mock
cargo run -p remotelink-host -- --role=agent --transport=live
cargo run -p remotelink-host --features webrtc-rs -- --role=agent --transport=webrtc
cargo run -p remotelink-viewer -- --live-demo
cargo run -p remotelink-viewer --features webrtc-rs -- --webrtc-demo

# Lab stack
docker compose -f deploy/docker-compose.yml up -d --build
```

## Transport selection

| `REMOTELINK_TRANSPORT` / `--transport` | Backend |
|----------------------------------------|---------|
| unset / `mock` | In-process mock (CI) |
| `live` | Length-prefixed TCP |
| `webrtc` | webrtc-rs (needs feature `webrtc-rs`) |
| `auto` | webrtc (if feature) → live → mock |

## Next best steps

1. **SampleBuilder H.264 / Opus RTP tracks** (replace interim `media-video` / `media-audio` DataChannels)  
2. Multi-process host/viewer over **real WSS** signaling (SessionManager + ViewerSession already take factory transports)  
3. Push to GitHub remote; real MSI/codesign (`deploy/packaging/`)  

Historical PR tips remain on `execute-plan/35709e22-pr-*` and `progress/*` branches / worktrees.

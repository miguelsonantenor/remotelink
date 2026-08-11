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
| Real AnyDesk product | **~98.5%** — H.264 encode module (SW mock + HW stub); DXGI + WASAPI stub; tray; KD5 |

## Day-to-day development

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

# CI-safe default (mock + live)
cargo test --workspace

# Real webrtc-rs backend unit tests
cargo test -p remotelink-net --features webrtc-rs

# Host / viewer demos (in-process)
cargo run -p remotelink-host -- --role=agent --transport=mock
cargo run -p remotelink-host -- --role=agent --transport=live
cargo run -p remotelink-viewer -- --live-demo

# Multi-process lab (3 terminals; memory server if DATABASE_URL unset)
cargo run -p remotelink-server
# Persistent host (unlimited sessions + reconnect; prints OTP; saves .remotelink-host.json):
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 --transport=live
# Restart reuses .remotelink-host.json (token refresh best-effort). --fresh to re-register.
# Host prints Mode A OTP + public_id; then:
cargo run -p remotelink-viewer -- --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp CODE --transport=live

# Lab stack (Postgres + server)
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

1. Native WASAPI COM loopback + real HW H.264 (NVENC/QSV/AMF)  
2. Run WiX MSI in release pipeline + Authenticode  
3. Optional: webrtc-rs e2e over WSS+agent IPC  

### Encode

| Backend | Status |
|---------|--------|
| `software_mock` (default) | MH264 Annex-B mock encoder (CI-safe) |
| `hardware` | Stub — always falls back to software until SDK linked |

```rust
mgr.request_video_keyframe();      // PLI/FIR
mgr.set_video_bitrate_bps(2_000_000); // GCC feedback
```

### Capture (Windows)

| Kind | Backend |
|------|---------|
| `WindowsMock` (default video) | Desktop-shaped BGRA mock (CI-safe) |
| `WindowsDxgi` | DXGI Desktop Duplication (interactive session) |
| `WindowsWasapiStub` (default audio) | Synthetic loopback PCM + exclusive-mode hooks |
| `WindowsWasapiPreferNative` | Prefer COM WASAPI; falls back to stub until linked |
| `Synthetic` | Media color bars / A440 |

```rust
// Agent SessionManager defaults on Windows: WindowsMock + WindowsWasapiStub
mgr.set_video_kind(VideoCaptureKind::WindowsDxgi);
mgr.set_audio_kind(AudioCaptureKind::WindowsWasapiPreferNative);
```

### Package stage

```powershell
.\scripts\package-release.ps1
# → dist\remotelink-<version>\bin\*.exe + package-manifest.json
# WiX: deploy/packaging/Product.wxs (see packaging README)
```

### Control boot secret (production KD5)

```powershell
$secret = -join ((1..32) | ForEach-Object { '{0:x2}' -f (Get-Random -Max 256) })
# Agent
cargo run -p remotelink-host -- --role=agent --control-listen=pipe --boot-secret=$secret --transport=live
# Service
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 `
  --transport=live --agent-control=pipe --boot-secret=$secret
```

### Control IPC (KD5) — WSS service + agent media

```powershell
# Demo: service client + agent server over TCP (mock media)
cargo run -p remotelink-host -- --role=ipc-colocate

# Split processes — TCP (CI/dev):
cargo run -p remotelink-host -- --role=agent --control-listen=tcp:0 --transport=live
# note CONTROL_LISTEN=tcp:PORT
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 `
  --transport=live --agent-control=tcp:PORT

# Split processes — Windows named pipe (production-style ACL):
cargo run -p remotelink-host -- --role=agent --control-listen=pipe --transport=live
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 `
  --transport=live --agent-control=pipe

# Viewer
cargo run -p remotelink-viewer -- --ws-connect --server=http://127.0.0.1:8080 `
  --host PUBLIC_ID --otp CODE --transport=live

# E2E (one process, two threads):
cargo test -p remotelink-e2e --test ws_agent_ipc -- --nocapture
```

Historical PR tips remain on `execute-plan/35709e22-pr-*` and `progress/*` branches / worktrees.

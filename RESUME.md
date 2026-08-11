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
| Real AnyDesk product | **~88%** — KD5 control IPC (TCP) service↔agent; host creds+OTP; WSS service; RTP; no MSI/named-pipe ACL yet |

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

1. Windows named-pipe ACL backend for control IPC (TCP works for CI/dev)  
2. Wire WSS service path to dial agent control IPC (split process)  
3. Tray/GUI for OTP; MSI/codesign; GitHub remote  

### Control IPC (KD5)

```powershell
# Demo: service client + agent server over TCP (mock media)
cargo run -p remotelink-host -- --role=ipc-colocate

# Split processes:
# Terminal A — agent control server
cargo run -p remotelink-host -- --role=agent --control-listen=tcp:0 --transport=mock
# note CONTROL_LISTEN=tcp:PORT
# Terminal B — service uses ServiceAgentClient::connect (WSS path wiring next)
```

Historical PR tips remain on `execute-plan/35709e22-pr-*` and `progress/*` branches / worktrees.

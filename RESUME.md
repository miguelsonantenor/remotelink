# RemoteLink — Resume handoff

**Saved:** 2026-08-12 (Phase 3 live session window)  
**Primary tree:** branch **`main`** at `C:\Users\Linked\Documents\remotelink`  
**GitHub:** https://github.com/miguelsonantenor/remotelink  

## Git policy (user)

- **OK:** local `git commit` anytime  
- **Push:** user authorized 2026-08-12 (Phase 1 commits are on origin)  
- **NOT OK without explicit permission:** GitHub releases, remote tags

| Local commit | Summary |
|--------------|---------|
| `6b3f1ae` | Phase 1 product shell (`remotelink-app` — This PC + Connect) |
| `851982c` | Host runs as **child process** so WSS stays online (`host_offline` fix) |

Remote still has older core release (`v0.1.0` portable zip **without** product shell).

---

## Where we stopped

### Done

| Item | Notes |
|------|--------|
| Core remote stack | Signaling, host, viewer CLI, DXGI/WASAPI/MF H.264, packaging |
| GitHub release `v0.1.0` | Portable zip (pre–Phase-1 binaries) |
| **Phase 1 shell** | `apps/desktop` → binary **`remotelink-app`** (egui) |
| Host under app | Spawns `remotelink-host.exe` child; status JSON + OTP in UI |
| Lab connect verified | `hello` → accept → offer/answer → live pump (no more burst hangup) |
| **Phase 3 live window** | `remotelink-app` Connect stays open and paints decoded frames |

### Not done (pick up later)

| Priority | Work |
|----------|------|
| **Phase 2** (user: when finished) | Hosted signaling + STUN/TURN (internet / NAT) |
| **Phase 4** | MSI + Authenticode; updates (auto-start done) |
| Polish | Real H.264 decode (today: mock software encoder so pixels reconstruct) |

### Lab quirks on this machine

- Lab default is **`http://127.0.0.1:18080`** (8080 is often taken by other Windows services)
- Live sessions remint a new OTP after hangup so the next viewer can connect

---

## Status

| Metric | Value |
|--------|--------|
| PR plan | **PRs 1–27 complete** (8b optional skipped) |
| **Integrated monorepo** | **Yes** |
| PeerTransport backends | **mock** · **live TCP** · **webrtc-rs** (opt-in) |
| **Core product** | **COMPLETE** |
| **Phase 1 product shell** | **DONE** (on origin) |
| **Phase 3 live window** | **DONE** (session stays open; Disconnect hangs up) |
| **Live Mode A identity** | **DONE** (OTP bind + remint; input gated until bound) |
| **Lab default port** | **`127.0.0.1:18080`** |
| **Live picture** | 1280×720 preview + larger window; DXGI logged |
| **Start with Windows** | HKCU Run + `--autostart` (minimized host) |
| Shippable artifact | Portable zip (`scripts/package-release.ps1`) — rebuild to include app |
| MSI | Optional via WiX — unsigned until codesign |

### Core product includes

- Signaling server (WSS SDP/ICE relay, OTP, creds)
- Host: enrollment, tray (OTP / End session / Exit), Mode A OTP
- KD5 service↔agent control IPC (TCP + named-pipe ACL + boot secret)
- Media: DXGI capture, WASAPI COM loopback, Media Foundation H.264 (+ software fallback)
- Viewer WSS connect path (+ library export for desktop)
- **`remotelink-app`** product shell (This PC + Connect)
- E2E live media tests, Windows + Linux CI

### Optional release polish

- Authenticode / EV code signing  
- Direct NVENC/QSV/AMF SDKs  
- Full webrtc-rs multi-process e2e  
- Secure desktop / UAC remote (documented v1 gap)

---

## Pick up later — product shell lab

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
# or: C:\msys64\mingw64\bin
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

# Terminal 1 — signaling (use 18080 if 8080 is busy)
$env:LISTEN_ADDR = "127.0.0.1:18080"
cargo run -p remotelink-server

# Terminal 2 — product shell (host + connect UI)
cargo run -p remotelink-desktop -- --server=http://127.0.0.1:18080
# binary: target\debug\remotelink-app.exe
# needs sibling: target\debug\remotelink-host.exe
```

Two clients for view test: two app instances with different `REMOTELINK_DATA_DIR`, same server; copy ID+OTP from host instance into Connect on the other.

Data/config: `%LOCALAPPDATA%\RemoteLink` or `REMOTELINK_DATA_DIR` (creds, `host-status.json`, `host-service.log`).

---

## Ship the product

```powershell
$env:Path = "C:\msys64\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

# Build portable core product zip (includes remotelink-app when rebuilt)
.\scripts\package-release.ps1
# → dist\RemoteLink-0.1.0-portable.zip
# → dist\remotelink-0.1.0\  (QUICKSTART, install-portable, lab-start)

# Optional MSI if WiX v3 installed
.\scripts\build-msi.ps1 -SkipStage
```

## Day-to-day development

```powershell
$env:Path = "C:\msys64\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

cargo test --workspace
cargo test -p remotelink-e2e --test ws_cli_live --test ws_agent_ipc
```

### Lab (CLI / advanced)

```powershell
$env:LISTEN_ADDR = "127.0.0.1:18080"
cargo run -p remotelink-server
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:18080 --transport=live
cargo run -p remotelink-viewer -- --ws-connect --server=http://127.0.0.1:18080 --host PUBLIC_ID --otp CODE --transport=live
```

### KD5 split (production-style)

```powershell
cargo run -p remotelink-host -- --role=agent --control-listen=pipe --boot-secret=SECRET --transport=live
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 --transport=live --agent-control=pipe --boot-secret=SECRET
```

## Transport

| Mode | Backend |
|------|---------|
| `mock` | In-process (CI) |
| `live` | Length-prefixed TCP |
| `webrtc` | webrtc-rs (feature `webrtc-rs`) |
| `auto` | webrtc → live → mock |

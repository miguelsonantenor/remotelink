# RemoteLink — Resume handoff

**Saved:** 2026-08-12 (parked after successful two-client live connect)  
**Primary tree:** branch **`main`** at `C:\Users\Linked\Documents\remotelink`  
**GitHub:** https://github.com/miguelsonantenor/remotelink  
**Tip of `main`:** see `git log -1` (CI green on 193c0f6; live-session UI after that)

## Git policy (user)

- **OK:** local `git commit` anytime  
- **OK:** `git push` to GitHub  
- **NOT OK without explicit permission:** GitHub releases, remote tags

Lab processes were **stopped** on park (server + both `remotelink-app` / host children).

---

## Where we stopped

### Done (and user-verified)

| Item | Notes |
|------|--------|
| Core remote stack | Signaling, host, viewer CLI, DXGI/WASAPI/MF H.264, packaging |
| GitHub release `v0.1.0` | Portable zip (pre–Phase-1 binaries) |
| **Phase 1 shell** | `remotelink-app` This PC + Connect |
| **Phase 3 live window** | Session stays open and paints decoded frames |
| **Two-client lab** | **Worked** 2026-08-12: OTP + bind + live picture, session stayed up |
| **GitHub CI** | **Green** on `193c0f6` (Linux + Windows + package/MSI) |
| **Live session chrome** | Session takes over the window; Fullscreen; Copy ID + OTP |
| Live OTP bind | Mode A DC challenge; remint after hangup; retry until host accepts |
| Lab default port | `http://127.0.0.1:18080` (8080 is taken on this PC) |
| Live picture | 1280×720 preview, window fills view, mouse 0..1 |
| Start with Windows | Advanced checkbox → HKCU Run `--autostart` |
| Lab fixes during test | Skip idle WASAPI audio; drop colliding ICE `signal_seq` |

### Not done (pick up later)

| Priority | Work |
|----------|------|
| **Phase 2** | Self-host stack + ICE advertise (`docs/HOSTING.md`); you still need a public VPS |
| **Phase 4** | MSI + Authenticode; updates (auto-start is done) |
| Polish | Real H.264 decode (today: mock software encoder so pixels reconstruct) |

### Lab notes

- Always copy **Your ID + OTP from the other window**, not from old chat. Restart remints a new OTP.
- Two instances need **different** `REMOTELINK_DATA_DIR`.
- In-memory server forgets devices on restart; host re-registers if token refresh fails.
- Agent helper commands can kill GUI children when they exit — keep a long-lived server/app process, or launch via `cmd /c start`.

---

## Status

| Metric | Value |
|--------|--------|
| PR plan | **PRs 1–27 complete** (8b optional skipped) |
| **Integrated monorepo** | **Yes** |
| PeerTransport backends | **mock** · **live TCP** · **webrtc-rs** (product default) |
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

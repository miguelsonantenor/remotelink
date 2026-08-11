# RemoteLink — Resume handoff

**Saved:** 2026-08-11  
**Plan ID:** `35709e22`  
**Primary tree:** branch **`main`** at `C:\Users\Linked\Documents\remotelink`  
**GitHub:** https://github.com/miguelsonantenor/remotelink

## Status

| Metric | Value |
|--------|--------|
| PR plan | **PRs 1–27 complete** (8b optional skipped) |
| **Integrated monorepo** | **Yes** |
| PeerTransport backends | **mock** · **live TCP** · **webrtc-rs** (opt-in) |
| **Core product** | **COMPLETE (~100%)** |
| Shippable artifact | **Portable zip** (`scripts/package-release.ps1`) |
| MSI | Optional via WiX (`scripts/build-msi.ps1`) — unsigned until release codesign |

### Core product includes

- Signaling server (WSS SDP/ICE relay, OTP, creds)
- Host: enrollment, tray (OTP / End session / Exit), Mode A OTP
- KD5 service↔agent control IPC (TCP + named-pipe ACL + boot secret)
- Media: DXGI capture, WASAPI COM loopback, Media Foundation H.264 (+ software fallback)
- Viewer WSS connect path
- E2E live media tests, Windows + Linux CI

### Not required for “core done” (optional release polish)

- Authenticode / EV code signing certificate  
- Direct NVENC/QSV/AMF SDKs (MF H.264 covers system encode)  
- Full webrtc-rs multi-process e2e  
- Secure desktop / UAC remote (documented v1 gap)

## Ship the product

```powershell
$env:Path = "C:\msys64\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink

# Build portable core product zip
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

### Lab (from source)

```powershell
cargo run -p remotelink-server
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 --transport=live
cargo run -p remotelink-viewer -- --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp CODE --transport=live
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

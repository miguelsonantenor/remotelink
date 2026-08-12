# Progress

**2026-08-11:** Integrated monorepo on `main` (`integrate/v1`). Workspace tests pass.
Plan PRs 1–27 done.

**Later same day:**
- Live TCP `PeerTransport` + docker-compose stack
- webrtc-rs `PeerTransport` behind feature `webrtc-rs` (ICE/DTLS + DataChannels; interim media on DCs)
- Fixed DataChannel-open race (`wait_data_channels_open`; PC Connected ≠ DC open)
- Host/viewer demos: `--transport=webrtc` / `--webrtc-demo` with `--features webrtc-rs`

**Later same day (SessionManager factory):**
- `SessionManager::from_transport_config` / `from_mode` / `from_env` (host offerer factory)
- `ViewerSession::attach_transport_from_config` / `attach_transport_mode` (viewer answerer)
- `PeerTransport::wait_ready` (webrtc waits for DataChannel open)
- Live/webrtc agent demos pump synthetic A/V through **SessionManager** (not raw peer sends)

**Later same day (WSS SDP/ICE relay):**
- Server `SessionRegistry::relay_media_signal` for offer/answer/ICE/auth/media control
- `hello_ok.feature_flags.sdp_relay = true`
- Tests: `sdp_ice_relay_after_accept`, role/state guards, e2e `ws_media_signaling`

**Later same day (CLI WSS clients):**
- `packages/signaling` — register HTTP + `SignalingClient` WSS
- Host `--role=ws` (enroll + accept + SessionManager media)
- Viewer `--ws-connect` (intent + answer + media RX)
- e2e `ws_cli_live`: live TCP media over real WSS (video_tx/rx=3)

**Later same day (RTP SampleBuilder):**
- webrtc-rs offerer adds H.264 + Opus `TrackLocalStaticSample`
- answerer `on_track` + `SampleBuilder` → `IncomingTrackData`
- default interceptors for RTP; media still mirrored on DC during bind races

**Later same day (RTP-only + service loop):**
- Media send is RTP-primary when tracks bound (`REMOTELINK_WEBRTC_DUAL_MEDIA=1` for mirror)
- `wait_ready` only requires input/identity DCs
- Host `--role=service --server=…`: multi-session WSS + reconnect (`--sessions=0`)

**Later same day (creds + OTP mint):**
- `HostCredentialFile` save/load (`.remotelink-host.json`)
- Token refresh on restart; Mode A OTP mint + server hash post
- e2e `host_creds_otp` proves prefilter with real code

**Later same day (KD5 control IPC):**
- `ServiceAgentClient` + `run_agent_control_server` over TCP control framing
- Request/reply + outbound SignalForward drain (`drain_complete` sentinel)
- `--role=ipc-colocate` and `--role=agent --control-listen=tcp:PORT`
- Auto pump after answer/ICE when Connected (no media on the wire)

**Later same day (WSS service → agent IPC):**
- `WsHostConfig::agent_control` + `--agent-control=tcp:PORT` / `REMOTELINK_AGENT_CONTROL`
- `handle_one_session_agent`: offer/answer/ICE over WSS, media on agent PeerTransport
- Agent polls live peer after each control message; QueryStats pokes Connected+pump
- Multi-accept agent server rebuilds SessionManager after detach
- e2e `ws_agent_ipc`: WSS service + agent IPC + live TCP media (`media=agent`, video_rx>0)
- Re-export `ControlEndpoint` / `listen_control` from `remotelink-host`

**Later same day (named-pipe ACL control IPC):**
- Windows `CreateNamedPipeW` backend with SDDL DACL (SYSTEM + Admins + Owner)
- `PIPE_REJECT_REMOTE_CLIENTS` (no SMB remote open)
- Same framed codec as TCP; `ControlStream` backend-agnostic timeouts/shutdown
- CLI: `--control-listen=pipe` / `pipe:NAME` / `\\.\pipe\…` and matching `--agent-control`
- Unit test `named_pipe_send_recv_with_acl` green on Windows

**Later same day (host tray OTP + session chrome):**
- `HostTray` / `TrayState`: identity, Mode A OTP, G9 session chrome projection
- Console panel + atomic `.remotelink-host-status.json` for tooling
- Windows `Shell_NotifyIcon` tooltip + OTP balloon (message-only window thread)
- Wired into `run_ws_host_service` (session begin/active/end + mint OTP)
- Flags: `--tray` / `--no-tray`, `--os-tray` / `--no-os-tray`, `--status-path`

**Later same day (tray menu + package stage):**
- Tray right-click: Copy OTP (clipboard), End session, Exit host
- Double-click tray icon copies OTP when available
- `TrayCommands` polled by WSS service (kill current session / graceful exit)
- `scripts/package-release.ps1` stages unsigned `dist/remotelink-<ver>/` with SHA-256 manifest

**Later same day (boot-secret + WiX skeleton):**
- Agent requires matching `attach_session.boot_secret` when `--boot-secret` set
- Constant-time compare; `auth_failed` error code; service injects secret on attach
- CLI/env: `--boot-secret` / `REMOTELINK_BOOT_SECRET` for agent + WSS service
- `deploy/packaging/Product.wxs` WiX skeleton over package-release layout

**Later same day (DXGI capture integration):**
- Landed `packages/platform-windows/src/capture` (mock + DXGI Desktop Duplication)
- Host `VideoCaptureKind::WindowsMock` / `WindowsDxgi`; Windows default is mock BGRA
- `SessionManager::start_media` / `pump_media` open platform capture sources
- Capture backend names via `capture_backends()`; DXGI idle frames retried

**Later same day (WASAPI loopback integration):**
- Landed `packages/platform-windows/src/wasapi` (stub + native skeleton + exclusive-mode hooks)
- Host audio kinds: `WindowsWasapiStub` (default), PreferNative, NativeOnly
- SessionManager pumps WASAPI stub PCM through mock Opus encode
- Native COM path documents full IAudioClient sequence; PreferNative falls back today

**Later same day (H.264 encode integration):**
- Landed `packages/platform-windows/src/encode` (MockSoftwareEncoder + HardwareEncoderStub)
- SessionManager pumps capture frames through `open_encoder` → PeerTransport NALUs
- Keyframe request + bitrate feedback APIs for PLI/FIR/GCC
- Hardware path documents NVENC/QSV/AMF; always falls back to software today

**Later same day (native WASAPI COM loopback):**
- `NativeLoopbackCapture::try_open` uses real COM: MMDeviceEnumerator → IAudioClient
  shared loopback + AUTOCONVERTPCM → IAudioCaptureClient pump → 10 ms s16 packets
- `is_available()` true on Windows; PreferNative opens wasapi when a render endpoint exists
- Exclusive-mode near-silence detection on native PCM; device-change → ReopenRequired

**Later same day (Media Foundation H.264 + Windows CI):**
- `MediaFoundationEncoder` via Microsoft H.264 Encoder MFT (RGB32 → Annex-B)
- `open_encoder` prefers MF on Windows, falls back to software mock
- GitHub Actions: Linux + Windows test jobs; package-stage artifact on `main`
- Vendor NVENC/QSV/AMF remains a documented stub

**Later same day (core product packaging complete):**
- Portable zip + install/uninstall/lab-start scripts in package layout
- `scripts/build-msi.ps1` builds WiX MSI when candle/light present; otherwise portable-only
- CI uploads portable zip (+ MSI if WiX on runner)
- **Core product marked COMPLETE** — remaining work is optional codesign / vendor SDKs

**Later same day (Phase 1 product shell):**
- New `apps/desktop` → binary **`remotelink-app`** (egui home screen)
- **This PC**: Allow remote access, public ID, OTP, copy buttons, session chrome
- **Connect**: remote ID + OTP, recent hosts, background WSS viewer job
- **Advanced**: signaling URL, transport, auto-start, data folder
- Host runs in-process via `run_ws_host_blocking` (status JSON under `%LOCALAPPDATA%\RemoteLink`)
- Viewer lib export (`remotelink_viewer::ws_connect`) for shared connect path
- Packaging: app primary binary in zip/MSI inventory + Start Menu shortcut

**Earlier same day (README + release checklist):**
- Top-level README reflects core product complete + ship commands
- `deploy/packaging/RELEASE_CHECKLIST.md` for MSI/codesign/smoke steps
- WiX MSI remains optional (needs admin install of WiX Toolset)

**2026-08-12 (Phase 3 live session):**
- Host `--frames=0` keeps capturing until hangup (desktop host uses this)
- Mock encoder embeds full/downscaled pixels so the viewer can paint a real picture
- `remotelink-app` Connect opens a live remote-desktop window (mouse/keys + Disconnect)
- Hosting / STUN/TURN still deferred until the product is finished

**2026-08-12 (live OTP bind + lab port):**
- Live sessions run Mode A identity bind (OTP → DC challenge) instead of the test helper
- Host remints a fresh OTP after each live hangup
- Product default signaling port is **18080** (server + desktop + host + lab-start)

**2026-08-12 (live picture quality):**
- Mock preview is 1280×720 with box-filter downscale (was 960×540 nearest-neighbor)
- Desktop session window fills the view (no 480px height cap); mouse coords are 0..1
- Host logs `video=` capture backend so DXGI vs mock is visible

**2026-08-12 (Start with Windows):**
- Advanced checkbox writes HKCU Run → `remotelink-app --autostart`
- Login launch starts minimized with host allowed
- Portable install `-Startup`; uninstall removes the Run key

**2026-08-12 (lab parked — live connect succeeded):**
- User connected two `remotelink-app` windows; session stayed up
- Fixed: OTP retry bind, idle WASAPI not fatal, colliding ICE seq dropped
- Lab processes stopped; resume from `RESUME.md`

**2026-08-12 (GitHub CI green):**
- Linux fmt / clippy / test, Windows tests, and package stage all pass
- Unsigned MSI builds on the Windows runner (`RemoteLink-0.1.0.msi` in artifacts)
- Artifact: https://github.com/miguelsonantenor/remotelink/actions/runs/31637055102

**2026-08-12 (live session takes over the window):**
- Connect hides the home form so the picture fills the window
- Fullscreen / Esc; Copy ID + OTP pastes as one pair into Remote ID

**2026-08-12 (WebRTC is the product transport):**
- `remotelink-host` / `viewer` / `desktop` default-enable `webrtc-rs`
- App default transport is `webrtc` (ICE + DTLS); `live` TCP remains a LAN fallback
- Advanced STUN/TURN field → `REMOTELINK_WEBRTC_STUN` (empty = host candidates / same LAN)
- WSS `mock`/`auto` resolve to webrtc when compiled (no longer forced to live)

**Optional (post-core):** Authenticode EV signing, NVENC SDK, webrtc multi-process e2e.

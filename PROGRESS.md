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

**Still open for product:** named-pipe ACL, tray OTP, MSI/codesign, GitHub remote.

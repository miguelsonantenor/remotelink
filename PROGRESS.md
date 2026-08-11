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

**Still open for product:** host/viewer CLI WSS dialers, SampleBuilder RTP tracks, MSI/codesign, GitHub remote.

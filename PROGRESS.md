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

**Still open for product:** SampleBuilder RTP tracks, full WSS multi-process sessions, MSI/codesign, GitHub remote.

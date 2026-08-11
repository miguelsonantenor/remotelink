# Progress

**2026-08-11:** Integrated monorepo on `main` (`integrate/v1`). Workspace tests pass.
Plan PRs 1–27 done.

**Later same day:**
- Live TCP `PeerTransport` + docker-compose stack
- webrtc-rs `PeerTransport` behind feature `webrtc-rs` (ICE/DTLS + DataChannels; interim media on DCs)
- Fixed DataChannel-open race (`wait_data_channels_open`; PC Connected ≠ DC open)
- Host/viewer demos: `--transport=webrtc` / `--webrtc-demo` with `--features webrtc-rs`

**Still open for product:** SampleBuilder RTP tracks, full WSS multi-process sessions, MSI/codesign, GitHub remote.

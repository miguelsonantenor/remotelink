# RemoteLink progress snapshot

Last updated: 2026-08-07  
See **RESUME.md** for full handoff.

## Summary

- **Design:** complete (`DESIGN.md`)
- **Implementation:** ~25 of ~30 PRs done (~80–85% of plan)
- **Product readiness:** ~40–50% (mock codec + input path; not real WebRTC product)

## Done vs remaining

### Done
Workspace, protocol, auth, server (HTTP/WSS/SDP-ICE/security/TURN/OTP), media core, PeerTransport mock, host IPC, synthetic host A/V, viewer-core, identity bind, OTP/unattended, e2e synthetic, DXGI/WASAPI/encode stubs, **viewer decode+skew HUD (17)**, **host input inject (18)**, **viewer input send (19)**, unit-test agent, coverage gates, session chrome/kill-switch.

### Remaining
Metrics (21), Linux host (22), bug-hunt agent (24), packaging (26), runbooks (27), **real libwebrtc**.

## Quick resume

Read `RESUME.md` → use plan id `35709e22` → start at **PR 21** or WebRTC.

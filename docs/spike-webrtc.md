# Spike: WebRTC stack evaluation & PeerTransport

**Status:** mock-first + **live TCP** step; pure-Rust WebRTC deferred; Plan B libwebrtc tracked.  
**Date:** 2026-08  
**Related:** [DESIGN.md](../DESIGN.md) (KD1, KD18), [`packages/net`](../packages/net), [runbook.md](runbook.md)

---

## Goal

Host session agent and viewer must exchange:

- H.264 video (external encoder → packetizer)
- Opus audio
- DataChannel (input + identity bind)
- ICE (host / srflx / relay) + DTLS fingerprints for identity bind

without coupling the encode pipeline to a browser capturer.

---

## Decision summary

| Path | Verdict | Notes |
|------|---------|--------|
| **In-process mock** | **Default / CI** | `MockPeerTransport` — no sockets, no native deps; windows-gnu green |
| **Live TCP** | **Local demos** | `LivePeerTransport` — real TCP, length-prefixed frames; **not** DTLS-SRTP |
| **webrtc-rs / str0m** | Tracked | Pure Rust attractive; H.264 packetization + ICE maturity + windows-gnu cost TBD |
| **libwebrtc FFI** | **Plan B / v1 ship** | Browser-grade ICE/DTLS-SRTP; heavier build matrix |

**Ship posture:** keep CI on **mock**. Use **live TCP** for multi-process dogfood of signaling + media framing. Introduce real WebRTC behind the same [`PeerTransport`](../packages/net/src/transport.rs) trait when the spike for libwebrtc or pure-Rust clears windows-gnu / packaging.

---

## Transport factory

```text
REMOTELINK_TRANSPORT=mock|live|auto   # default: mock
```

| Value | Backend |
|-------|---------|
| `mock` (default) | `MockPeerTransport` — CI and unit tests |
| `live` | `LivePeerTransport` — TCP media plane (feature `live`) |
| `auto` | Prefer live when the feature is compiled; else mock |

CLI (host / viewer):

```bash
remotelink-host --role=agent --transport=mock
remotelink-host --role=agent --transport=live
remotelink-viewer --live-demo
remotelink-viewer --transport=live
```

API:

```rust
use remotelink_net::{create_peer_transport, PeerRole, TransportConfig};

let cfg = TransportConfig::from_env(); // or TransportConfig::parse("live")?
let mut peer = create_peer_transport_with_config(PeerRole::Offerer, &cfg)?;
```

---

## Live TCP design (pragmatic “real network” step)

Not WebRTC. Same trait surface for host/viewer session code.

| Concern | Live TCP behavior |
|---------|-------------------|
| SDP | JSON (`type=remotelink-live`) with `listen`, `fingerprint`, `ufrag`/`pwd` |
| ICE | Synthetic **host TCP** candidates (`tcptype passive|active`) |
| Fingerprint | SHA-256 of 32 ephemeral random bytes (identity bind practice) |
| Media | Length-prefixed frames: VideoNalu / Audio / Data / Control |
| Connect | Offerer listens; answerer dials; Hello control frame swaps digests |
| Delivery | Pull model: receiver `poll()` drains reader-thread queue |

### Frame layout

```text
[u32 BE body_len][u8 kind][payload…]
kind: 1=video 2=audio 3=data 4=control
```

### When to use live vs mock

| Scenario | Mode |
|----------|------|
| `cargo test`, PR CI | `mock` (default) |
| Single-process agent/viewer demos | either |
| Two processes on one machine | `live` |
| Production / WAN / NAT | **real WebRTC** (not live TCP) |

Env helpers for live:

- `REMOTELINK_LIVE_BIND` — bind addr (default `127.0.0.1:0`)
- `REMOTELINK_LIVE_ADVERTISE` — host string embedded in SDP/ICE

---

## Real WebRTC options (future)

### A. Pure Rust (`webrtc` crate / str0m)

**Pros:** no C++ toolchain; easier audit of packetization.  
**Cons:** windows-gnu CI risk; NVENC/external NALU path must stay outside the stack; maturity of DataChannel partial reliability (KD7).

Feature flag name only today: `webrtc-rs` (no crates.io deps wired).

### B. libwebrtc FFI (`packages/net-libwebrtc` plan)

**Pros:** production ICE/TURN/DTLS-SRTP; matches browser viewers if ever needed.  
**Cons:** build complexity; ship separate from default mock CI.

Encoder always produces Annex-B/AVCC into `send_video_nalu` — transport packetizes.

---

## Fingerprints & identity bind

- Canonical form: `sha-256` + space + uppercase colon-hex (32 bytes).  
- Mock and live both implement `local_fingerprint` / `remote_fingerprint`.  
- Live verifies Hello digest against SDP fingerprint.  
- Real DTLS: remote fingerprint must come from the **completed DTLS cert**, not SDP alone, before enabling input (PR 13).

---

## Compose stack

See [`deploy/docker-compose.yml`](../deploy/docker-compose.yml):

- **postgres** — device registry  
- **server** — `remotelink-server` image (`deploy/Dockerfile.server`)  
- **coturn-bridge** / **coturn** (profile `turn`) — STUN/TURN lab  

Live TCP does **not** use coturn; real WebRTC will.

---

## Open questions for the next spike PR

1. windows-gnu vs msvc builder for libwebrtc artifacts  
2. H.264 packetizer ownership (in-net vs media crate)  
3. DataChannel unordered/partial-reliability mapping for mouse moves  
4. TURN credential mint end-to-end against compose coturn  

---

## Conclusion

- **Do not block** host/viewer/session work on full WebRTC.  
- **Default mock** keeps CI deterministic.  
- **Live TCP** is the intentional middle step: real sockets, same trait, no DTLS-SRTP claim.  
- Document any “connected” beta HUD path as mock/live until a real PeerConnection backend lands.

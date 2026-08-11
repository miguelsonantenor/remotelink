# Spike: WebRTC stack evaluation & PeerTransport

**Status:** mock-first + live TCP + **webrtc-rs backend (feature-gated)**; Plan B libwebrtc tracked.  
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
| **webrtc-rs (`webrtc` 0.11)** | **Optional feature** | Real SDP/ICE/DTLS; media interim on DataChannels; **default-off** for CI speed |
| **libwebrtc FFI** | **Plan B / v1 ship risk** | Browser-grade ICE/DTLS-SRTP; heavier build matrix |

**Ship posture:** keep CI on **mock** (+ live TCP tests under default features). Enable `webrtc-rs` for local/dev builds that need real PeerConnection. Introduce SampleBuilder H.264 tracks as a follow-up; Plan B libwebrtc if packaging or packetization fails.

---

## Transport factory

```text
REMOTELINK_TRANSPORT=mock|live|webrtc|auto   # default: mock
```

| Value | Backend |
|-------|---------|
| `mock` (default) | `MockPeerTransport` — CI and unit tests |
| `live` | `LivePeerTransport` — TCP media plane (feature `live`) |
| `webrtc` | `WebrtcPeerTransport` — webrtc-rs (feature `webrtc-rs`) |
| `auto` | Prefer **webrtc** if feature on → **live** if feature on → **mock** |

CLI (host / viewer):

```bash
remotelink-host --role=agent --transport=mock
remotelink-host --role=agent --transport=live
# webrtc demos need the feature flag:
cargo run -p remotelink-host --features webrtc-rs -- --role=agent --transport=webrtc
remotelink-viewer --live-demo
cargo run -p remotelink-viewer --features webrtc-rs -- --webrtc-demo
```

**Note:** PeerConnection `Connected` does not imply DataChannels are open. Call
`wait_data_channels_open` (or use `webrtc_handshake`, which waits for DC open)
before `send_data` / media sends.

Build with webrtc-rs:

```bash
cargo test -p remotelink-net --features webrtc-rs
cargo run -p remotelink-host --features remotelink-net/webrtc-rs -- --role=agent --transport=webrtc
```

API:

```rust
use remotelink_net::{create_peer_transport, PeerRole, TransportConfig};

let cfg = TransportConfig::from_env(); // or TransportConfig::parse("webrtc")?
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
| Real ICE/DTLS lab | **`webrtc`** (`--features webrtc-rs`) |
| Production / WAN / NAT | **real WebRTC** (webrtc-rs path maturing; Plan B libwebrtc) |

Env helpers for live:

- `REMOTELINK_LIVE_BIND` — bind addr (default `127.0.0.1:0`)
- `REMOTELINK_LIVE_ADVERTISE` — host string embedded in SDP/ICE

---

## webrtc-rs backend (`WebrtcPeerTransport`)

Feature: **`webrtc-rs`** on `remotelink-net` (default **off**).

| Concern | Behavior |
|---------|----------|
| Stack | `webrtc` crate **0.11** — `RTCPeerConnection`, real SDP / ICE / DTLS |
| Runtime | Process-wide multi-thread tokio runtime; trait methods `block_on` |
| DataChannels | Offerer creates `input`, `identity`, `media-video`, `media-audio` |
| Media (interim) | H.264 NALUs / Opus on DCs — **same payload layout as live TCP** video/audio bodies; DC message boundary replaces TCP `[len][kind]`. **Not** SampleBuilder RTP tracks yet |
| Fingerprint | Local from DTLS cert DER SHA-256; remote from SDP then upgraded from completed DTLS cert |
| ICE | Trickle via `on_ice_candidate` → `poll`; `set_local_description` also waits for gather-complete so SDP embeds candidates |
| Delivery | Pull model: async handlers queue; app calls `poll()` |

Env:

- `REMOTELINK_WEBRTC_STUN` — optional comma-separated STUN/TURN URLs (empty = host candidates only)

### Interim media note

Until SampleBuilder H.264 / Opus RTP tracks land, encoders still call
`send_video_nalu` / `send_audio`; the webrtc-rs backend packs those units onto
`media-video` / `media-audio` DataChannels with the live-TCP-compatible payload
codec. Do not treat this as production SRTP media.

---

## Real WebRTC options (status)

### A. Pure Rust (`webrtc` crate) — **wired**

**Pros:** no C++ toolchain; windows-gnu compiles for 0.11 on this tree.  
**Cons:** H.264 RTP SampleBuilder path not finished; partial-reliability (KD7) TBD; feature stays optional for CI.

### B. libwebrtc FFI (`packages/net-libwebrtc` plan)

**Pros:** production ICE/TURN/DTLS-SRTP; matches browser viewers if ever needed.  
**Cons:** build complexity; ship separate from default mock CI.

Encoder always produces Annex-B/AVCC into `send_video_nalu` — transport packetizes.

---

## Fingerprints & identity bind

- Canonical form: `sha-256` + space + uppercase colon-hex (32 bytes).  
- Mock, live, and webrtc-rs implement `local_fingerprint` / `remote_fingerprint`.  
- Live verifies Hello digest against SDP fingerprint.  
- webrtc-rs: remote fingerprint from **completed DTLS cert** preferred over SDP alone before enabling input (PR 13).

---

## Compose stack

See [`deploy/docker-compose.yml`](../deploy/docker-compose.yml):

- **postgres** — device registry  
- **server** — `remotelink-server` image (`deploy/Dockerfile.server`)  
- **coturn-bridge** / **coturn** (profile `turn`) — STUN/TURN lab  

Live TCP does **not** use coturn. webrtc-rs can take STUN/TURN via `REMOTELINK_WEBRTC_STUN` against compose coturn.

---

## Open questions for the next spike PR

1. SampleBuilder H.264 / Opus RTP tracks replacing interim media DCs  
2. DataChannel unordered/partial-reliability mapping for mouse moves (KD7)  
3. windows-gnu vs msvc builder for libwebrtc artifacts (Plan B)  
4. TURN credential mint end-to-end against compose coturn  

---

## Conclusion

- **Do not block** host/viewer/session work on full RTP media.  
- **Default mock** keeps CI deterministic and fast.  
- **Live TCP** remains the intentional multi-process socket step without DTLS-SRTP claims.  
- **webrtc-rs** is available behind `--features webrtc-rs` for real PeerConnection dogfood; media DC path is explicitly interim.

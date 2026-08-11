# RemoteLink operator runbook

Operational guide for self-hosting and running **server**, **host**, and **viewer**. This runbook follows [DESIGN.md](../DESIGN.md). It does not contain secrets, default production passwords, or private keys—generate and store those yourself.

**Audience:** operators deploying a single-node or small cluster self-host; on-call for session incidents.

**Related docs:** [threat-model.md](threat-model.md) · [platform-limitations.md](platform-limitations.md) · [spike-webrtc.md](spike-webrtc.md)

---

## 1. Architecture at a glance

| Component | Binary / service | Role |
|-----------|------------------|------|
| **Server** | `remotelink-server` | Device registry, WSS signaling, session brokerage, rate limits, audit, blocklist, admin APIs, TURN credential mint |
| **TURN** | coturn (sidecar) | Media relay when P2P ICE fails; **opaque** SRTP only |
| **Postgres** | postgres | Devices, credentials, sessions, OTP hashes, audit, blocklist |
| **Redis** (scale-out / prod) | redis | Presence, rate-limit counters, short-lived TURN cache, WS node routing |
| **Host** | `remotelink-host` | Windows: **service** (enrollment, WS, policy, kill-switch) + **session agent** (capture, encode, WebRTC, input) |
| **Viewer** | `remotelink-viewer` | Connect UI, decode/playout, input capture, identity bind |

Media prefers **host ↔ viewer P2P**. The server is on the media path only via TURN when needed. Authorization for **input** is host-side (Modes A/B/C + identity bind)—the server is a broker and prefilter, not sole trust.

```text
Viewer --HTTPS/WSS--> Server <--HTTPS/WSS-- Host service
Viewer <---- ICE / DTLS-SRTP (+ TURN if needed) ----> Host session agent
```

---

## 2. Prerequisites

- Docker (or equivalent) for Postgres / coturn / optional Redis
- TLS termination in front of the server for any non-loopback deployment (TLS 1.3)
- Open UDP (and TCP if you enable TCP TURN) for coturn public endpoints
- Host: Windows primary (Linux host secondary per design); privileged install for service + interactive session agent
- Viewer: Windows / macOS / Linux
- Operator access to admin APIs (protect with network policy + auth; never expose admin anonymously)

---

## 3. Self-host server stack

### 3.1 Minimal local / lab (Postgres + server + coturn)

From the repository root:

```bash
# Postgres + remotelink-server + bridge-network coturn
docker compose -f deploy/docker-compose.yml up -d --build

# Linux host-network coturn (better ICE addressing) — optional profile:
# docker compose -f deploy/docker-compose.yml --profile turn up -d
```

| Service | Ports | Notes |
|---------|-------|--------|
| `postgres` | `5432` | User/db/password `remotelink` (lab only) |
| `server` | `8080` | Built from `deploy/Dockerfile.server` |
| `coturn-bridge` | `3478`, `5349`, relay UDP range | Docker Desktop / Windows-friendly |
| `coturn` (profile `turn`) | host network | Prefer on Linux for real STUN/TURN tests |

Set durable registry storage when running the server **on the host** (PowerShell):

```powershell
$env:DATABASE_URL = "postgres://remotelink:remotelink@127.0.0.1:5432/remotelink"
```

Compose already injects `DATABASE_URL` for the `server` service.

> **Warning:** Compose defaults in `deploy/docker-compose.yml` are for **local development** only. Change DB user/password, `TURN_SHARED_SECRET`, bind addresses, and add TLS before any shared or internet-facing use.

### 3.2 Server process

```bash
# Optional: LISTEN_ADDR (default 0.0.0.0:8080)
export LISTEN_ADDR=0.0.0.0:8080
export DATABASE_URL=postgres://USER:PASS@HOST:5432/remotelink
# Only when a reverse proxy overwrites X-Forwarded-For with the real client:
# export TRUST_PROXY=1
cargo run -p remotelink-server
# or: remotelink-server
```

| Variable | Purpose |
|----------|---------|
| `LISTEN_ADDR` | Bind address (default `0.0.0.0:8080`) |
| `DATABASE_URL` | Postgres URL; if unset, **in-memory** repo (not durable—lab only) |
| `TRUST_PROXY` | Set `1` **only** behind a reverse proxy that overwrites `X-Forwarded-For`; otherwise rate limits / lockout can be spoofed |
| `RUST_LOG` / `EnvFilter` | Logging (`info` default) |

### 3.3 Production-shaped single node

Design target (KD16): **docker-compose self-host first**—signaling + API + local coturn + Postgres (+ Redis when using presence TTL / multi-instance rate limits).

Recommended layout:

1. Reverse proxy (HTTPS → server HTTP; WSS → `/v1/ws`)
2. `remotelink-server` (not public without TLS)
3. Postgres (not public)
4. coturn with public STUN/TURN ports; long-term secret shared only with server for REST-style ephemeral creds
5. Redis (optional at tiny scale; required for multi-node presence / shared rate limits)
6. Prometheus scrapes `/metrics` (when enabled); alert on auth spikes and TURN errors

**Reference capacity (G8):** ~4 vCPU / 8 GB RAM signaling node → roughly **5–10k** concurrent WS **presence** connections (not media). Media cost is dominated by relayed TURN egress.

### 3.4 Health probes

| Endpoint | Use |
|----------|-----|
| `GET /healthz` | Liveness |
| `GET /readyz` | Readiness (DB / critical deps) |
| `GET /metrics` | Prometheus (design; enable in observability builds) |

Example:

```bash
curl -fsS https://signaling.example/healthz
curl -fsS https://signaling.example/readyz
```

### 3.5 Core HTTP / WS surface (operator-relevant)

| Method | Path | Notes |
|--------|------|--------|
| `POST` | `/v1/devices/register` | Host enrollment → device credential |
| `POST` | `/v1/devices/{id}/token/refresh` | Refresh host credential |
| `DELETE` | `/v1/devices/{id}` | Revoke / GDPR delete |
| `POST` | `/v1/devices/{id}/otp` | Host-authenticated OTP mint (Mode A) |
| `POST` | `/v1/sessions` | Viewer session intent (pre-step) |
| `POST` | `/v1/sessions/{id}/end` | Hangup |
| `GET` | `/v1/sessions/{id}/turn-credentials` | **Session-scoped** TURN creds |
| `GET` | `/v1/devices/{id}/audit` | Host-owner audit list |
| `POST` | `/v1/devices/{id}/blocklist` | Block viewer fingerprint / IP / device |
| `GET` | `/v1/config` | Feature flags (authenticated) |
| `POST` | `/v1/admin/sessions/{id}/force-disconnect` | Operator security hangup |
| `GET` | `/v1/ws` | Signaling WebSocket |

WS messages are session-scoped with monotonic `signal_seq` (stale seq dropped). See DESIGN.md for the full message set (`hello`, `session_intent`, SDP/ICE relay, `session_end`, etc.).

---

## 4. TURN (coturn)

### 4.1 Goals

- Prefer **P2P** (host / srflx candidates).
- Issue **time-limited, session-scoped** credentials (coturn REST style: username embeds `session_id` + expiry).
- Authorize credential fetch only for **parties to the session**.
- Apply **bandwidth / bitrate** quotas; TURN egress is the primary cost driver (~4 Mbps × N relayed 1080p30 sessions).

### 4.2 Operator checklist

1. Deploy coturn with a shared secret known only to the server process (not clients long-term).
2. Expose STUN/TURN on the public address clients will use in ICE (correct `external-ip` / realm).
3. Prefer UDP; document TCP TURN for UDP-blocked networks; expect higher failure if both ends are hard NAT without relay.
4. Server endpoint: `GET /v1/sessions/{id}/turn-credentials` after session membership is established.
5. Feature flag `force_relay` for chaos tests or strict privacy mode (all media via TURN).
6. Monitor relay share, TURN auth failures, and egress bandwidth.

### 4.3 Failure modes

| Symptom | Likely cause | Action |
|---------|--------------|--------|
| Sessions work on LAN, fail WAN | Missing STUN/TURN or wrong external IP | Fix coturn public config; check ICE path metrics |
| High cost / saturated uplink | Many relayed HD sessions | Cap bitrate flags; prefer P2P; scale TURN fleet |
| Creds rejected | Clock skew, wrong secret, expired session | NTP; rotate secret carefully; check session TTL |
| UDP blocked | Corporate firewall | TCP TURN if enabled; else document fail |

---

## 4b. PeerTransport modes (mock / live)

Media-plane backends are selected by env or CLI. **Default is mock** so CI never requires sockets or WebRTC.

| Mode | Env / flag | Behavior |
|------|------------|----------|
| **mock** (default) | `REMOTELINK_TRANSPORT=mock` or unset | In-process `MockPeerTransport` — unit tests, colocate CI |
| **live** | `REMOTELINK_TRANSPORT=live` or `--transport=live` | TCP length-prefixed frames between peers — local multi-process demos |
| **auto** | `REMOTELINK_TRANSPORT=auto` | Prefer live when compiled (`remotelink-net` feature `live`); else mock |

```powershell
# CI / default
cargo run -p remotelink-host -- --role=agent
cargo run -p remotelink-viewer -- --synthetic

# Live TCP demos (localhost client–server inside one process)
cargo run -p remotelink-host -- --role=agent --transport=live
cargo run -p remotelink-viewer -- --live-demo

# Env form
$env:REMOTELINK_TRANSPORT = "live"
$env:REMOTELINK_LIVE_BIND = "127.0.0.1:0"          # optional
$env:REMOTELINK_LIVE_ADVERTISE = "127.0.0.1"       # optional SDP/ICE host
```

Factory API (library):

```rust
remotelink_net::create_peer_transport(remotelink_net::PeerRole::Offerer)?;
// or
remotelink_net::create_peer_transport_with_config(
    remotelink_net::PeerRole::Answerer,
    &remotelink_net::TransportConfig::parse("live")?,
)?;
```

Live TCP is **not** DTLS-SRTP WebRTC. Production path remains spike-gated — see [spike-webrtc.md](spike-webrtc.md). coturn is for the future real ICE/TURN path, not for the live TCP demo.

---

## 5. Host agent

### 5.1 Process model (Windows)

| Process | Owns | Does not own |
|---------|------|--------------|
| **Host service** (Session 0 / elevated) | Enrollment, long-lived signaling WS, presence, feature flags, OTP mint API, **kill-switch orchestration**, session policy, spawn/attach agent, tray coordination | DXGI, WASAPI, encode, PeerTransport, input inject |
| **Session agent** (interactive desktop) | Capture, loopback audio, H.264/Opus, **WebRTC PeerTransport**, identity bind crypto, input after bind, in-desktop session chrome | Outbound registry WS (receives signaling via IPC) |

IPC is **control-plane only** (named pipe, ACL + per-boot shared secret). No media bytes on IPC.

### 5.2 Operator steps

1. Install host package (MSI/MSIX or dev build); ensure service starts at boot.
2. Complete enrollment against your signaling base URL → receive device **public ID** (numeric + check digits) and store device credential securely.
3. Confirm tray presence and server connectivity (presence heartbeat).
4. For ad-hoc support: use **Mode A OTP** (mint + display code; never log the plaintext OTP).
5. For unattended: enable **Mode B** explicitly; secret stays on host (DPAPI/keyring)—never upload reverseable unattended passwords to the server.
6. Verify mandatory **connection indicator** (tray + border/top bar) cannot be dismissed remotely.
7. Teach host users the **kill-switch** hotkey (default design: `Ctrl+Alt+Shift+End`, configurable).

### 5.3 Kill-switch

| Property | Behavior |
|----------|----------|
| Trigger | Global hotkey; tray action; IPC `KillSwitch` |
| Path | Service/agent handles **before** inject path |
| Effect | Immediate session disconnect; disable input; **optionally disable unattended** |
| Remote disable | **Must not** be disableable by the viewer or signaling server |

On secure desktop (UAC / Winlogon), capture and inject do not work in v1—kill-switch remains available on the **normal** desktop. See [platform-limitations.md](platform-limitations.md).

### 5.4 Local confirm / policy

- Mode A: local confirm default when accepting support sessions.
- Mode B: notify-only if configured; first enable of unattended should require local confirm.
- Single active controller: additional intents get `session_reject reason=busy`.

---

## 6. Viewer

1. Point viewer at the same signaling base URL (TLS).
2. Enter host **public ID** and auth material for the mode:
   - **A:** OTP shown on host screen  
   - **B:** unattended password / pairing secret (proved to host, not sent as sole server password for control)  
   - **C:** optional server password prefilter **plus** host bind  
3. Wait for identity bind before assuming control works (beta HUD should show bind status).
4. Disconnect via UI; if host is compromised or stuck, host kill-switch or operator force-disconnect is authoritative.
5. Beta: keep stats HUD available (RTT, bitrate, FPS, loss, **A/V skew**, bind status).

---

## 7. OTP (Mode A) operations

### 7.1 Happy path

1. Host user enables “Allow remote” → host generates OTP `c` (6–8 digits).
2. Host stores `hash(c)` + TTL via authenticated `POST /v1/devices/{id}/otp` (pepper+salt; never store bare digests of short codes without pepper).
3. Host shows `c` on screen only (UI string; do not write to logs).
4. Viewer creates `session_intent` with host public ID + OTP.
5. Server prefilters (unexpired, unconsumed, rate limits), binds pending to `session_intent_id`.
6. Host re-validates; on success OTP is **consumed once** (`consumed_at` with session active).
7. Double consume or expiry → reject.

### 7.2 Operator notes

- OTP is short-lived; lockout uses auth-fail counters (IP + host) with exponential backoff.
- Stolen OTP hash store without pepper is offline-brute-forceable—protect DB and use designed peppering.
- Do not reuse OTP endpoints without host authentication.

---

## 8. Unattended (Mode B) operations

1. Host stores long-term secret `K` **only locally**.
2. Viewer connect → host challenges with nonce → viewer proves MAC/PAKE over session material (password not sent to server as control authority).
3. Host verifies, then fingerprint-signed SDP and bind.
4. Ensure toast + mandatory chrome; kill-switch can disable unattended after abuse.

---

## 9. Force-disconnect (operator / security)

Use when a session must end regardless of client cooperation (security patch, abuse, stuck lock).

```http
POST /v1/admin/sessions/{session_id}/force-disconnect
Authorization: <operator credential>
```

Expected effects:

1. Server marks session ended / releases single-session lock on host device (`active_session_id`).
2. Broadcast / forward `session_end` with reason such as `security` to host and viewer signaling.
3. Host service/agent tears down PeerConnection, disables input, updates chrome.
4. Session-scoped TURN creds become invalid at expiry (do not rely on TURN alone to stop media—host teardown is required).

**Also consider:**

- Blocklist abusive viewer fingerprint / IP: `POST /v1/devices/{id}/blocklist`
- Revoke device credentials / `DELETE /v1/devices/{id}` if the host enrollment is compromised
- Feature flags: `force_protocol_min`, `kill_switch_session_region`, `disable_hw_encode` via `/v1/config` as applicable
- Signed update manifest + `force_update` on hello for bad client builds

Protect admin routes with network allowlists and strong auth. Log force-disconnects into audit.

---

## 10. Metrics & observability

### 10.1 Logging

- Prefer `tracing` JSON with **`session_id`** on session-scoped events.
- Never log OTP plaintext, unattended secrets, device refresh tokens, or TURN long-term secrets.

### 10.2 Prometheus-oriented signals (design)

| Area | Examples |
|------|----------|
| Auth | auth fail rate, lockouts, OTP consume failures |
| Sessions | setup time, setup success, busy rejects, force-disconnect count |
| ICE | path host / srflx / relay / mixed; setup failures |
| Media quality | bitrate, RTT, loss, FPS, **A/V skew**, `input_drop_rate` |
| TURN | relay %, auth errors, bytes relayed |

### 10.3 SLOs (design)

| SLO | Target |
|-----|--------|
| Setup success | ≥ 99% excluding offline hosts |
| Signaling availability | ≥ 99.9% |
| p95 setup (healthy nets) | < 3 s |
| G4 connectivity | ≥ 95% sessions without manual port-forward under NAT mix plan |

### 10.4 Alert ideas

- Spike in auth failures per host or global
- TURN error rate / egress saturation
- Setup p95 regression
- Force-disconnect rate (may indicate incident response or abuse)

---

## 11. Blocklist & audit

| Action | API |
|--------|-----|
| Add block | `POST /v1/devices/{id}/blocklist` (`ip` \| `viewer_fingerprint` \| `device`) |
| List | `GET /v1/devices/{id}/blocklist` |
| Remove | `DELETE /v1/devices/{id}/blocklist/{entry_id}` |
| Audit | `GET /v1/devices/{id}/audit` |

Retention design default: audit **90 days**. GDPR-style device delete soft-deletes device and credentials; no media content is stored on the server.

---

## 12. Incident playbooks (short)

### 12.1 Suspected unauthorized control

1. Host: kill-switch immediately; disable unattended if enabled.
2. Operator: force-disconnect session; blocklist viewer subject.
3. Rotate host device credentials; review audit for host device.
4. Confirm identity-bind and Mode A/B paths were in use (Mode C alone is not sufficient for input).

### 12.2 Signaling outage

- Existing P2P media may continue briefly; treat signaling loss as soft warning if media up, full teardown after grace (design).
- Hosts re-register presence on reconnect (Redis TTL ~30s presence model when used).

### 12.3 Compromised signaling server

- Assume ability to DoS, lie about presence, serve wrong enrolled keys on first connect—see [threat-model.md](threat-model.md).
- Input should remain protected **if** identity bind + host auth held; still force-disconnect sessions, pin/update clients, rotate keys as needed.
- TOFU: re-confirm host key fingerprint / SAS out-of-band after server rebuild.

### 12.4 Bad host build (encoder / security)

- Set feature flags (e.g. `disable_hw_encode`) for affected versions.
- Pin update channel; force update via signed manifest; host polls manifest on timer (**not** via the remote session alone).

---

## 13. Feature flags (reference)

| Flag | Intent |
|------|--------|
| `force_relay` | All ICE via TURN |
| `max_bitrate` | Cap encoder / relay cost |
| `enable_unattended` | Server-side policy gate (host still holds Mode B secret) |
| `codec_preference` | Codec selection hints |
| `disable_hw_encode` | Force software encode |
| `force_protocol_min` | Reject old protocol_version |
| `kill_switch_session_region` | Regional emergency controls |

---

## 14. Scaling notes

| Stage | Architecture |
|-------|----------------|
| Single node | Signaling + API + local coturn + Postgres |
| Growth | Stateless signaling + Redis pub/sub; shared Postgres; TURN pool |
| Large | Regional signaling + geo-DNS TURN |

Single-session lock in Postgres is source of truth (`active_session_id`). Pub/sub is best-effort; hosts ignore stale `signal_seq`.

---

## 15. Build & test (dev operators)

```bash
cargo build --workspace
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Binaries: `remotelink-host`, `remotelink-viewer`, `remotelink-server`.

---

## 16. What this runbook does not cover

- Clipboard, file transfer, multi-viewer control (non-goals v1)
- Winlogon/UAC secure desktop remote interaction (documented gap)
- Production secret generation procedures specific to your org (use your vault)

For residual trust and abuse, see [threat-model.md](threat-model.md). For OS capture/input quirks, see [platform-limitations.md](platform-limitations.md).

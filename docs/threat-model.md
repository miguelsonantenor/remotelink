# RemoteLink threat model

Security model for RemoteLink aligned with [DESIGN.md](../DESIGN.md). Operators and implementers should treat this as the authoritative product threat narrative; code must not weaken the rules below for convenience.

**Related docs:** [runbook.md](runbook.md) · [platform-limitations.md](platform-limitations.md)

---

## 1. Assets

| Asset | Sensitivity | Notes |
|-------|-------------|--------|
| Host interactive desktop | Critical | Screen content + input control |
| System audio loopback | High | May include calls, media, notifications |
| Unattended secret (Mode B) | Critical | Host-only; never server-reversible |
| OTP (Mode A) | High short-lived | Shown on host UI; consume-once |
| Device private key | Critical | Signs DTLS fingerprints / enrollment |
| Device credentials (tokens) | High | Host ↔ server auth |
| Mode C password hash | Medium–High | Server-side Argon2id prefilter only |
| Session metadata | Medium | IDs, timestamps, ICE path, audit |
| Viewer install / ephemeral keys | Medium | Per-session or pairing |
| TURN long-term secret | High | Server–coturn only |
| Enrolled public keys | Medium | Discovery; TOFU risk if wrong key served |

**Explicitly not stored on server:** media payloads, Mode B secrets, raw OTP digits (only hashes with proper peppering).

---

## 2. Trust boundaries

```text
┌─────────────┐   TLS 1.3    ┌──────────────────┐
│   Viewer    │◄────────────►│  Signaling server │
└──────┬──────┘              │  (broker only)    │
       │                     └────────┬─────────┘
       │  DTLS-SRTP + bind             │ TLS
       │  (identity_bound)             ▼
       │                     ┌──────────────────┐
       └────────────────────►│  Host service +  │
                             │  session agent   │
                             └──────────────────┘
         optional opaque relay via TURN
```

| Party | Trusted for | Not trusted for |
|-------|-------------|-----------------|
| **Host** | Authorizing control; holding Mode A/B secrets; enabling input only after bind | — (host compromise = full desktop compromise by definition) |
| **Viewer** | User intent; proving knowledge of OTP/unattended material | Controlling host without host verification |
| **Signaling server** | Presence, brokerage, rate limits, prefilters, audit, TURN mint | Sole authorization of input; plaintext media; Mode B secret |
| **TURN** | Relaying ciphertext | Inspecting SRTP; long-lived open relays |
| **Network path** | — | Confidentiality/integrity without DTLS-SRTP + bind |

---

## 3. Signaling MITM (DTLS fingerprint swap)

### 3.1 Classic WebRTC failure mode

A malicious or compromised signaling server can rewrite SDP `a=fingerprint` values so both peers complete DTLS with the attacker. SRTP then protects only the hop to the attacker—not end-to-end. Input over a compromised DataChannel would be attacker-controlled.

**Severity:** Critical for remote-control products.

### 3.2 RemoteLink mitigations

1. **Host signs** its DTLS certificate fingerprint (and `session_id`) with the **enrolled device private key** (`fingerprint_sig` on offer).
2. **Viewer verifies** the signature with the enrolled public key (from TLS server fetch and/or pinned prior pairing).
3. **Viewer binding** of its fingerprint is established toward the host.
4. After DTLS, optional/required **application DataChannel challenge**: nonce + proof of session auth material; channel-bind  
   `session_id || fingerprint_host || fingerprint_viewer`.
5. **Hard gate:**  
   `Host MUST NOT accept input until identity_bound && session_authorized`.  
   Default v1: no input; optional blank/preview video until bound (product choice)—control path never skips bind.

### 3.3 What binding does *not* fix

See [§6 Residual trust](#6-residual-trust). Binding closes **media/input MITM** when keys and host auth hold; it does not stop server DoS or first-connect key substitution without TOFU/SAS.

---

## 4. Identity bind & session authorization modes

### 4.1 Rules (normative)

| Rule | Statement |
|------|-----------|
| R1 | Server assertion alone **never** enables host input |
| R2 | Media confidentiality assumes DTLS-SRTP; authenticity vs signaling MITM requires **fingerprint binding** |
| R3 | Input enablement requires **both** session auth (Mode A/B/C path) **and** identity bind |
| R4 | Single active controller per host; busy reject |
| R5 | Mode B secret never leaves host in reverseable form to the server |

### 4.2 Mode A — Ad-hoc OTP

| | |
|--|--|
| **UX** | Support: host shows 6–8 digit code |
| **Secret** | Host-minted OTP; server may store **hash** + TTL |
| **Server** | Rate-limit; optional hash prefilter; bind to `session_intent_id`; consume-once |
| **Host** | Generates OTP; **re-validates**; preferred final verifier |
| **Threats** | Guessing → rate limit + short TTL + lockout; hash leak → offline brute without pepper → use pepper+salt; shoulder-surf → short TTL |

### 4.3 Mode B — Unattended

| | |
|--|--|
| **UX** | Silent or notify-only after explicit enable |
| **Secret** | Host-only long-term `K` (DPAPI/keyring) or PAKE verifier |
| **Server** | Forwards challenge/response only; **no** unattended password store |
| **Host** | Challenge-response / PAKE; policy + chrome + kill-switch |
| **Threats** | Weak password → strength UX; malware on host → full compromise (inherent); server cannot authorize unattended alone |

### 4.4 Mode C — Server-checked password (optional)

| | |
|--|--|
| **UX** | Familiar “password to server” |
| **Secret** | Argon2id hash on server |
| **Server** | Prefilter only |
| **Host** | Still requires fingerprint bind + preferably Mode A/B-class host auth for input |
| **Threats** | Treating Mode C as sufficient for control is a **design rejection**—implementations must not enable inject on server password alone |

### 4.5 Default product policy

- Unattended → **Mode B only**
- Ad-hoc support → **Mode A**
- Mode C optional enterprise experiment; **not** sufficient alone for input

---

## 5. Threat → mitigation matrix

| Threat | Severity | Mitigation |
|--------|----------|------------|
| Unauthorized remote control | Critical | Host Mode A/B auth; rate limits; single session; local confirm; bind before inject |
| Signaling MITM (fingerprint swap) | Critical | Fingerprint signing + DC bind; no input until bound |
| Server as sole authorizer | Critical | Server broker/prefilter only for control |
| Credential stuffing / OTP brute | High | Rate limit IP+device; lockout; audit; short OTP TTL; consume-once |
| Server compromise → media eavesdrop | High | Bound SRTP; TURN opaque |
| Compromised TURN | Medium–High | Opaque packets only; session-scoped creds; no open relay |
| Supply chain / bad update | High | Signed manifests; channel pin; force-update; host polls outside remote session |
| Silent / abusive control | High | Mandatory chrome; kill-switch; notifications; single controller |
| Candidate IP leak | Medium | Privacy mode / `force_relay` TURN-only option |
| DoS signaling or TURN | Medium | Rate limits; session-scoped TURN; quotas; force-disconnect |
| Agent CI / generated tests | Medium | Draft PRs only; sandbox; human review; CI runs checked-in tests only |
| Split-brain session messages | Medium | `signal_seq`; Postgres single-session lock |
| Secure desktop gap | High (availability / incomplete control) | Documented limitation; local host completes UAC; not a bypass of bind |

---

## 6. Residual trust

Even with correct identity binding, the **signaling server** (and first-connect key distribution) retain residual capabilities:

| Residual capability | Impact | Operator / product response |
|---------------------|--------|------------------------------|
| DoS / drop signaling | Cannot start or control sessions; may disrupt signaling-dependent teardown | Multi-node, health checks; host local kill-switch still works offline for local end |
| Lie about presence | Viewer thinks host online/offline incorrectly | UX honesty; retries; not a media MITM if bind holds |
| Serve **wrong enrolled public key** on first connect | Viewer may TOFU attacker key if user ignores confirmation | **TOFU + local confirm**; display host key fingerprint / short **SAS** in UI; optional out-of-band ID check; re-confirm after server reinstall |
| Learn session metadata | Who connected when, ICE path, auth success/fail | Minimize logs; audit retention policy; no media content stored |
| Prefilter abuse (Mode C) | Block legitimate users or accept wrong password as prefilter | Host still must bind; monitor auth_attempts |
| Force busy / session lock issues | Denial of new sessions | Admin force-disconnect; repair `active_session_id` |

**Password-only server-side gate without host verify is rejected for unattended control.**

**Host compromise** is out of scope as “defense in depth against the host OS owner”—malware with host privileges defeats any remote-desktop agent. Focus on making **remote** unauthorized control hard without host secrets.

---

## 7. Abuse prevention

| Control | Purpose |
|---------|---------|
| Rate limits + `auth_attempts` | Slow stuffing and OTP guessing |
| Blocklist API | Host owner or operator blocks IP / viewer fingerprint / device |
| Single session lock | Prevent second controller race |
| Session-scoped TURN | No open TURN; limits free relay abuse |
| Audit events | Forensics; retention default 90 days |
| Force-disconnect | Operator hangup on abuse or security patch |
| Mandatory chrome + kill-switch | Detectable control; local abort |
| No open registration without policy | Operators set enrollment posture for their deployment |

Report path for abused hosts: blocklist + audit + optional external incident process (product-specific).

---

## 8. Unattended access checklist

- [ ] Explicit user enable of Mode B  
- [ ] Secret host-only (DPAPI/keyring)  
- [ ] Session toast + non-dismissible chrome  
- [ ] Kill-switch disconnects and can disable unattended  
- [ ] Local + server audit of session start/end  
- [ ] No server-stored reverseable unattended password  

---

## 9. Data handling & privacy

| Topic | Policy |
|-------|--------|
| Media on server | Not stored |
| GDPR delete | Device + credentials; soft-delete device row |
| Audit retention | 90 days default |
| Aggregate quality metrics | Open question (privacy review) if geo-region attached |
| Clipboard | Non-goal v1 (sensitive surface deferred) |

---

## 10. Security UX (host)

| Control | Requirement |
|---------|-------------|
| Connection indicator | Non-dismissible tray + colored border/top bar while active; not remote-disableable |
| Kill-switch | Global hotkey before inject path; disconnect; optional disable unattended |
| Single controller | One active viewer; busy reject |
| Local confirm | Default first unattended enable and Mode A; Mode B may notify-only if configured |
| Secure desktop | Documented limitation—not a silent remote path through UAC |

---

## 11. Testing expectations (security-relevant)

- Hand-written tests on `protocol`, `auth`, bind paths  
- E2E: input accepted only after identity bind (mock input sink)  
- Chaos: force TURN, partition, reconnect; no bind bypass  
- Rate-limit / lockout unit tests  
- Fuzz parsers; proptest OTP expiry and clamping  

CI must not auto-merge agent-generated tests; treat draft code as untrusted.

---

## 12. Incident classification (quick)

| Class | Examples | First response |
|-------|----------|----------------|
| Critical | Auth bypass, inject without bind | Force-disconnect region/all; revoke credentials; patch; audit |
| High | Crash loops remote path, TURN secret leak | Rotate secrets; force-update; blocklist |
| Medium | Desync, presence lies | Mitigate UX; monitor |
| Low | Cosmetic chrome issues | Track; do not weaken kill-switch |

---

*This document summarizes and expands DESIGN.md “Trust model”, “Session authorization modes”, and “Security & Privacy Considerations”. When in conflict with informal notes, prefer DESIGN.md + this file and fix the other.*

# RemoteLink platform limitations

Known OS and stack limitations for operators, support staff, and users. Aligned with [DESIGN.md](../DESIGN.md). These are **expected behaviors** in v1, not silent security bypasses—except where noted as residual risk (see [threat-model.md](threat-model.md)).

**Related docs:** [runbook.md](runbook.md) · [threat-model.md](threat-model.md)

---

## 1. UAC and secure desktop (Windows)

### 1.1 What fails

Capture (DXGI) and input injection (**do not**) operate on **Winlogon / UAC secure desktop** without a separate signed path (credential provider, special driver, etc.). That path is **out of scope for v1**.

Consequences:

- Remote viewer **cannot** interact with UAC elevation prompts.
- Remote viewer **cannot** complete Ctrl+Alt+Del secure-desktop flows (password change, Task Manager from that path, etc.).
- When the host switches to secure desktop, the remote session may freeze on the last normal-desktop frame, show black/empty capture, and/or drop input—by design limitation, not full remote admin takeover of logon UI.

### 1.2 What still works

- Host user completes UAC / secure-desktop steps **locally**.
- Tray **kill-switch** remains available on the **normal** desktop (service/agent path); do not assume kill-switch UI is visible *on* the secure desktop itself.
- Session policy and signaling may still be alive in the host service while the agent cannot capture the secure surface.

### 1.3 Support guidance

| Situation | Guidance |
|-----------|----------|
| Viewer stuck at UAC | Ask host user to accept/deny locally; do not promise remote click-through |
| Need elevation for install | Schedule local admin or pre-stage with local rights |
| Security review | Document as **availability / control gap**, not as “agent bypasses UAC” |

### 1.4 Process model reminder

Interactive capture and inject run in the **session agent** (user session), not Session 0 service, so normal-desktop DXGI/WASAPI work under Fast User Switching—but secure desktop remains a separate Windows desktop the agent does not own in v1.

---

## 2. Exclusive-mode audio (Windows WASAPI)

### 2.1 Design intent

Host system audio uses **shared mode** loopback:

- `IAudioClient` + `AUDCLNT_STREAMFLAGS_LOOPBACK` on the selected render endpoint.
- Device-change notifications (`IMMNotificationClient`); on default device change: stop, reopen, `media_restart`, brief mute (&lt; ~200 ms).

### 2.2 Exclusive-mode games / apps

Some titles open the render device in **exclusive mode**. Loopback may then:

- Open-fail, or  
- Yield **near-silence** while the game still plays locally.

**Product behavior (required):**

- Do **not** crash the session.
- Surface a **host tray warning** and **viewer banner**.
- Detection heuristic: sustained near-zero energy while session active and Windows reports exclusive mode where observable.

### 2.3 Support guidance

| Symptom | Likely cause | Mitigation |
|---------|--------------|------------|
| No remote audio, video OK | Exclusive mode, wrong endpoint, muted loopback | Exit exclusive mode; select correct output device; check tray warning |
| Audio glitch on device switch | Default device change | Expected short mute + `media_restart` |
| Skew HUD bad after switch | Epoch reset | Confirm `media_restart` path; re-settle jitter |

Linux host (secondary): PipeWire/Pulse monitor has different exclusive/passthrough failure modes—treat as separate checklist when Linux host ships.

---

## 3. DXGI access lost / desktop duplication failures

### 3.1 Common causes

Desktop Duplication (`IDXGIOutputDuplication`) can return **access lost** / must-reinit when:

- Display mode or resolution changes  
- Monitor topology changes (cable, docking, projector)  
- Fullscreen exclusive mode transitions  
- GPU driver reset / TDR  
- Fast User Switching / session composition changes  
- Secure desktop transitions (see §1)  
- Remote Desktop / other capture conflicts in some configurations  

### 3.2 Expected agent behavior

Design lifecycle on display change:

1. **Pause input inject**  
2. Reinit capture / new offer with updated track params  
3. Force **keyframe**  
4. Resume inject after stable capture  

Operators should treat “black frame / frozen frame then recover” after resolution change as expected if renegotiation completes. Persistent failure → check GPU drivers, capture permissions, and whether another tool holds duplication.

### 3.3 Metrics / debugging

- Host stats: capture reinit count, encode queue, ICE still up vs media restart  
- Viewer: PLI/FIR, freeze duration, keyframe wait  
- Session chrome should still show **connected** until kill-switch or teardown  

### 3.4 Non-goals

- Multi-monitor advanced layouts in v1 (single selected display; `display_id` reserved)  
- Real DXGI inside agent CI sandboxes (manual dogfood / integration only)

---

## 4. Pause, Break, NumLock, and keyboard state

### 4.1 Why this is hard

Remote input uses **scan set 1 scancodes** (+ extended flag), not Unicode characters for key-down/up. The **host OS layout wins**. Viewer-generated key repeat is sent explicitly; the host does not auto-repeat.

Several keys and LED-backed toggles are historically fragile across remote-desktop stacks:

| Class | Examples | Issue |
|-------|----------|--------|
| Toggle locks | **NumLock**, CapsLock, ScrollLock | Viewer and host LED/state can **desync**; numpad keys then produce digits vs navigation differently than the viewer user expects |
| Pause / Break | **Pause**, Ctrl+Break | Often poorly exposed by windowing toolkits; may not generate the same scancode path as a physical keyboard; some APIs swallow or remap |
| Extended keys | arrows, Insert/Delete, numpad with NumLock off | Require correct **extended** flag; missing flag → wrong host inject |
| OS special chords | Ctrl+Alt+Del, Win+L, some accessibility chords | May be blocked or routed to secure desktop / OS; not fully injectable from user-mode `SendInput` in all cases |

### 4.2 v1 expectations

- Prefer documenting **NumLock**: match host NumLock state before relying on numpad; if digits are wrong, toggle NumLock on host (locally or via known-good inject) and retry.  
- **Pause/Break**: may be missing or inconsistent depending on viewer OS and egui/winit key delivery—treat as best-effort, not a certified control key.  
- Do not send character shortcuts assuming viewer locale; use scancode table in `packages/protocol`.  
- Focus policy: viewer sends input only when focused by default (“always capture” optional with warning).

### 4.3 Support guidance

| Symptom | Try |
|---------|-----|
| Numpad moves cursor instead of typing digits | Align NumLock on host with user intent |
| Shortcut triggers wrong action | Confirm host keyboard layout; avoid locale-dependent chars |
| Pause key does nothing | Expected limitation on some platforms; use alternate host-side control |
| Keys stick | Viewer lost focus mid-chord; host kill-switch; session end clears inject path |

---

## 5. Mock vs real WebRTC

RemoteLink stages connectivity and media behind a **PeerTransport** trait so CI and early dogfood do not require full browser-grade WebRTC + real DXGI.

### 5.1 Modes (conceptual)

| Mode | Media | Network | Input | Typical use |
|------|-------|---------|-------|-------------|
| **Synthetic / mock** | Bars, tone, fake PTS | In-process or loopback PeerTransport mock | Mock `InputSink` timestamps | Unit/e2e, bind tests, agents CI |
| **Real WebRTC** | H.264/Opus over DTLS-SRTP | ICE + STUN + optional TURN | DataChannel after bind | Closed beta / production path |
| **Hybrid dogfood** | Real capture + synthetic net (or reverse) | Spike / debug | Restricted | Engineering only |

### 5.2 Rules that apply to both

- **Identity bind before input** is mandatory on any path that can enable real inject (PR 13/15/18 dependency chain).  
- Synthetic e2e must assert bind before mock input is accepted so production cannot “forget” the gate.  
- Mock success **does not** prove NAT traversal, HW encode, or exclusive-audio behavior.

### 5.3 WebRTC stack

Implementation is **spike-gated** (webrtc-rs / str0m / libwebrtc FFI). Plan B: `PeerTransport` + libwebrtc feature if pure-Rust path fails H.264 packetization / ICE needs. Operators should not assume a particular crate in docs beyond “DTLS-SRTP + ICE + DataChannel.”

### 5.4 Feature flags affecting “realness”

| Flag | Effect |
|------|--------|
| `force_relay` | Real ICE but always TURN (chaos / privacy) |
| `disable_hw_encode` | Real path with software H.264 |
| `max_bitrate` | Caps real encode even when link allows more |

---

## 6. Other host / viewer constraints (quick list)

| Area | Limitation |
|------|------------|
| Controllers | **Single** active viewer controller per host |
| Displays | One selected display stream in v1 |
| Clipboard / files / chat | Non-goals v1 |
| macOS host | Deferred |
| Mobile / web viewer | Out of scope v1 |
| Session 0 capture | Not used; agent in interactive session |
| Corporate UDP block | May require TCP TURN or fail (document) |
| Screensaver | May inhibit while session active (configurable, default on) |
| Latency targets | See DESIGN appendix; relay path has looser budgets |

---

## 7. Operator communication template

When filing support notes or status pages, prefer precise language:

> RemoteLink cannot control or capture the Windows secure desktop (UAC / Ctrl+Alt+Del). Complete elevation prompts on the host machine. Remote audio may be silent if an application has exclusive WASAPI access—exit exclusive mode or accept local-only audio. After display changes, the session may briefly renegotiate video. Numpad behavior depends on host NumLock. Lab “synthetic WebRTC” tests do not validate production ICE/TURN.

---

*For operational steps (TURN, OTP, force-disconnect, kill-switch), see [runbook.md](runbook.md). For MITM and residual server trust, see [threat-model.md](threat-model.md).*

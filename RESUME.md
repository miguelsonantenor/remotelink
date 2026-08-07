# RemoteLink — Resume handoff

**Saved:** 2026-08-06  
**Plan ID:** `35709e22`  
**Repo:** `C:\Users\Linked\Documents\remotelink`  
**Design:** `DESIGN.md` (full architecture + PR plan)

This document is the checkpoint so work can continue later without losing context.

---

## Status snapshot

| Metric | Value |
|--------|--------|
| **PR plan progress** | **~22 / ~30 (~70–75%)** implemented + reviewed |
| **Usable as AnyDesk-like product** | **No** (~25–35% product readiness) |
| **`main` branch** | Scaffold only (README + DESIGN + this handoff) |
| **Real code** | Feature branches + worktrees (see below) |
| **Remote / GitHub** | None configured — all local |

---

## Environment (required to build)

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
```

- Rust: stable via rustup  
- MinGW: `C:\Users\Linked\tools\mingw64`  
- Prefer **GNU** toolchain (`stable-x86_64-pc-windows-gnu`) for linking on this machine  

---

## How to resume later

### Option A — Continue the plan with Grok

Say:

```text
Continue RemoteLink plan 35709e22 from RESUME.md
```

or:

```text
/execute-plan --resume 35709e22
```

Point at design: `C:\Users\Linked\Documents\remotelink\DESIGN.md`

### Option B — Manual continue

1. Read `DESIGN.md` § PR Plan  
2. Next PRs (not done): **17, 18, 19, 21, 22, 24, 26, 27** (8b optional)  
3. Check out a tip branch (below) or worktree and build on it  

### Recommended “richest” tips to start from

| Goal | Branch or worktree |
|------|---------------------|
| Identity + host bind | `progress/identity` or `remotelink-wt-pr-13` |
| OTP / unattended | `progress/otp` or `remotelink-wt-pr-14` |
| E2E synthetic tests | `progress/e2e` or `remotelink-wt-pr-15` |
| Session chrome / kill-switch | `progress/chrome` or `remotelink-wt-pr-20` |
| H.264 encode path | `progress/encode` or `remotelink-wt-pr-16b` |
| Server signaling path | `progress/server-path` or `remotelink-wt-pr-5b` |

```powershell
cd C:\Users\Linked\Documents\remotelink
git checkout progress/e2e
# or work in worktree:
cd C:\Users\Linked\Documents\remotelink-wt-pr-15
cargo test -p remotelink-e2e
```

---

## Completed PRs (tips)

Each tip is the reviewed tip of that PR branch (full SHA prefix).

| PR | Short SHA | Subject |
|----|-----------|---------|
| 1 | `034c0af` | Cargo workspace skeleton + CI |
| 2 | `1364ea0` | Protocol schemas + golden tests |
| 3 | `597a46e` | Auth: IDs, OTP, challenge-response |
| 4 | `508db88` | Server registration / credentials |
| 5a | `cec26c6` | WSS hello / session_intent / accept |
| 5b | `52d1146` | SDP / ICE relay |
| 6 | `8015bbe` | Rate limits, audit, blocklist |
| 7 | `122b0f3` | Session-scoped TURN credentials |
| 8 | `3f4d5f9` | PeerTransport spike (mock + Plan B libwebrtc) |
| 9 | `b375372` | Media: jitter, skew, synthetic A/V |
| 10 | `eef587a` | Host service/agent control IPC |
| 11 | `0f0f1c2` | Host session manager + synthetic A/V |
| 12 | `c5f33bb` | Viewer-core + CLI/egui shell |
| 13 | `a3e8006` | Identity binding (no input until bound) |
| 14 | `09b64bd` | OTP UX + unattended Mode B policy |
| 15 | `87eae18` | E2E identity + synthetic A/V + input mock |
| 16a | `cb38ffb` | DXGI capture |
| 16b | `7f9db21` | H.264 encode into PeerTransport |
| 16c | `27dff33` | WASAPI loopback → Opus path |
| 20 | `e913afe` | Session chrome + kill-switch |
| 23 | `a161b63` | Unit-test agent inventory |
| 25 | `03a988e` | Coverage / test-presence gates |

**Branch naming pattern:**

```text
execute-plan/35709e22-pr-<N>-<slug>
```

**Worktree paths:**

```text
C:\Users\Linked\Documents\remotelink-wt-pr-<N>
```

(Exceptions: `remotelink-wt-pr1` for PR1.)

---

## Progress pointer branches (local)

| Branch | Points at |
|--------|-----------|
| `progress/server-path` | PR 5b tip |
| `progress/security` | PR 6 |
| `progress/turn` | PR 7 |
| `progress/net` | PR 8 |
| `progress/media` | PR 9 |
| `progress/host-ipc` | PR 10 |
| `progress/host-session` | PR 11 |
| `progress/viewer` | PR 12 |
| `progress/identity` | PR 13 |
| `progress/otp` | PR 14 |
| `progress/e2e` | PR 15 |
| `progress/dxgi` | PR 16a |
| `progress/encode` | PR 16b |
| `progress/wasapi` | PR 16c |
| `progress/chrome` | PR 20 |
| `progress/agents` | PR 23 |
| `progress/coverage` | PR 25 |

---

## Not done (pick up here)

Priority order for a usable product:

1. **PR 17** — Viewer real H.264 decode + Opus playout + skew HUD  
2. **PR 18** — Windows input injection (after identity bind)  
3. **PR 19** — Viewer input capture → DataChannel  
4. **Real WebRTC** — libwebrtc (Plan B from PR 8), not only mock  
5. **PR 21** — Prometheus metrics / tracing  
6. **PR 22** — Linux host (secondary)  
7. **PR 24** — Bug-hunt agent + chaos  
8. **PR 26** — Packaging (MSI) + force-disconnect  
9. **PR 27** — Runbooks + threat model docs  
10. **PR 8b** — only if pure-Rust WebRTC is chosen later (currently optional)

Also remaining: merge all tips into one linear stack / `main`, add `origin` remote, full stack assembly.

---

## What works today (demos)

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"

# E2E synthetic (identity + A/V + input gate)
cd C:\Users\Linked\Documents\remotelink-wt-pr-15
cargo test -p remotelink-e2e

# Host synthetic agent
cd C:\Users\Linked\Documents\remotelink-wt-pr-11
cargo run -p remotelink-host -- --role=agent

# Viewer synthetic
cd C:\Users\Linked\Documents\remotelink-wt-pr-12
cargo run -p remotelink-viewer -- --synthetic
```

---

## Orchestrator state (scratch)

```text
C:\Users\Linked\AppData\Local\Temp\grok-S-1-5-21-2837964814-3470935208-3283404530-1001\grok-exec-plan-35709e22.json
```

May be cleaned by the OS; **this RESUME.md is the durable source of truth**.

---

## Important notes

- Branches are **not merged into `main`**. Code lives on PR branches / worktrees.  
- There is **no remote push** yet — back up this folder if the machine is wiped.  
- Diamond merges of packages were sometimes done by **copy + workspace fix** when git merge conflicted.  
- Do **not** delete worktrees or `execute-plan/35709e22-*` branches until code is merged or pushed.  

---

## Suggested next message to the agent

```text
Resume RemoteLink from C:\Users\Linked\Documents\remotelink\RESUME.md
Continue execute-plan 35709e22 starting with PR 17 (viewer decode).
```

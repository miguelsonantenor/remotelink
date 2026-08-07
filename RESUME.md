# RemoteLink — Resume handoff

**Saved:** 2026-08-07  
**Plan ID:** `35709e22`  
**Repo:** `C:\Users\Linked\Documents\remotelink`  
**Design:** `DESIGN.md`

---

## Status snapshot

| Metric | Value |
|--------|--------|
| **PR plan progress** | **~30 / ~30 plan PRs done** (~95%+ of planned list; **PR 8b optional** skipped) |
| **Usable as AnyDesk-like product** | **No** — mock PeerTransport/codec; **real WebRTC not integrated** (~50–60% product readiness) |
| **`main` branch** | Handoff docs + DESIGN only |
| **Real code** | Feature branches + worktrees |
| **Remote / GitHub** | None |

---

## Environment

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
```

---

## How to resume

```text
Resume RemoteLink from C:\Users\Linked\Documents\remotelink\RESUME.md
Continue with real WebRTC (libwebrtc PeerTransport) or merge all branches to main.
```

---

## Completed PRs (tips)

| PR | Short | Subject |
|----|-------|---------|
| 1 | `034c0af` | Workspace + CI |
| 2 | `1364ea0` | Protocol |
| 3 | `597a46e` | Auth |
| 4 | `508db88` | Server registry |
| 5a | `cec26c6` | WSS sessions |
| 5b | `52d1146` | SDP/ICE relay |
| 6 | `8015bbe` | Rate limits / audit / blocklist |
| 7 | `122b0f3` | TURN credentials |
| 8 | `3f4d5f9` | PeerTransport mock + spike GO libwebrtc |
| 9 | `b375372` | Media core |
| 10 | `eef587a` | Host IPC |
| 11 | `0f0f1c2` | Host synthetic session |
| 12 | `c5f33bb` | Viewer-core shell |
| 13 | `a3e8006` | Identity bind |
| 14 | `09b64bd` | OTP + unattended |
| 15 | `87eae18` | E2E synthetic |
| 16a | `cb38ffb` | DXGI capture |
| 16b | `7f9db21` | H.264 encode |
| 16c | `27dff33` | WASAPI loopback |
| 17 | `fab8061` | Viewer decode + skew HUD |
| 18 | `c7e1539` | Host input injection |
| 19 | `0f52a14` | Viewer input send |
| 20 | `e913afe` | Session chrome + kill-switch |
| 21 | `84239a7` | Prometheus metrics + tracing |
| 22 | `4671fc6` | Linux host platform (mock CI) |
| 23 | `a161b63` | Unit-test agent |
| 24 | `3e7778e` | Bug-hunt / chaos agent |
| 25 | `03a988e` | Coverage gates |
| 26 | `913ee07` | Packaging outline + force-disconnect |
| 27 | `3075be7` | Runbook + threat model + limitations |

Worktrees: `C:\Users\Linked\Documents\remotelink-wt-pr-*`  
Branches: `execute-plan/35709e22-pr-*` and `progress/*`

### Progress pointers

`progress/server-path`, `security`, `turn`, `net`, `media`, `host-ipc`, `host-session`, `viewer`, `identity`, `otp`, `e2e`, `dxgi`, `encode`, `wasapi`, `decode`, `inject`, `viewer-input`, `chrome`, `metrics`, `linux`, `agents`, `chaos`, `coverage`, `packaging`, `docs`

---

## Remaining for a real product

1. **Real WebRTC** (libwebrtc behind `PeerTransport`) — largest gap  
2. Merge all PR tips into one linear stack / `main`  
3. Add git remote and push  
4. Real MSI/codesign (outline only today)  
5. Optional PR **8b** if pure-Rust WebRTC is chosen instead  

---

## Demos

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"

cd C:\Users\Linked\Documents\remotelink-wt-pr-15
cargo test -p remotelink-e2e

cd C:\Users\Linked\Documents\remotelink-wt-pr-17
cargo run -p remotelink-viewer -- --mock-codec --hud-block

cd C:\Users\Linked\Documents\remotelink-wt-pr-24
cargo run -p bug-hunt-agent -- nightly --out target/chaos
```

---

## Docs for operators

On tip `progress/docs` / worktree `remotelink-wt-pr-27`:

- `docs/runbook.md`
- `docs/threat-model.md`
- `docs/platform-limitations.md`

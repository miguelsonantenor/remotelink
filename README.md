# RemoteLink

Low-latency remote desktop with **system audio**, host agent, viewer, and signaling server.

**Core product: complete.** Ship the portable zip from `scripts/package-release.ps1`.

| Layer | Status |
|-------|--------|
| Signaling (WSS, SDP/ICE, OTP) | Done |
| Host service + tray + KD5 agent IPC | Done |
| Live TCP media + DXGI / WASAPI / MF H.264 | Done |
| **Product shell** (`remotelink-app`) — This PC + Connect | Phase 1 |
| Portable package + lab scripts | Done |
| MSI (WiX) | Optional — `scripts/build-msi.ps1` |
| Authenticode signing | Optional — release pipeline only |

## Docs

| Doc | Path |
|-----|------|
| Design | [DESIGN.md](DESIGN.md) |
| Resume / handoff | [RESUME.md](RESUME.md) |
| Progress | [PROGRESS.md](PROGRESS.md) |
| Runbook | [docs/runbook.md](docs/runbook.md) |
| Threat model | [docs/threat-model.md](docs/threat-model.md) |
| Platform limits | [docs/platform-limitations.md](docs/platform-limitations.md) |
| Packaging | [deploy/packaging/README.md](deploy/packaging/README.md) |

## Environment (Windows)

```powershell
$env:Path = "C:\Users\Linked\tools\mingw64\bin;$env:USERPROFILE\.cargo\bin;" + $env:Path
$env:RUSTUP_TOOLCHAIN = "stable-x86_64-pc-windows-gnu"
cd C:\Users\Linked\Documents\remotelink
```

## Product shell (Phase 1)

One window, AnyDesk-style home screen:

```powershell
cargo run -p remotelink-server
cargo run -p remotelink-desktop
# binary name: remotelink-app
# • This PC — Your ID + OTP (Allow remote access)
# • Connect — remote ID + OTP
# • Advanced — signaling URL (default http://127.0.0.1:8080)
```

Settings live under `%LOCALAPPDATA%\RemoteLink` (`config.json`, host creds, status).

## Ship the product

```powershell
# Release binaries + portable zip (~14 MB)
.\scripts\package-release.ps1
# → dist\RemoteLink-0.1.0-portable.zip
# → dist\remotelink-0.1.0\  (QUICKSTART, install-portable, lab-start)
# includes remotelink-app.exe (product shell)

# Optional MSI if WiX v3 is installed
.\scripts\build-msi.ps1 -SkipStage
```

Portable install:

```powershell
Expand-Archive dist\RemoteLink-0.1.0-portable.zip -DestinationPath $env:TEMP\rl -Force
powershell -ExecutionPolicy Bypass -File $env:TEMP\rl\install-portable.ps1
```

One-machine lab from the package:

```powershell
powershell -ExecutionPolicy Bypass -File dist\remotelink-0.1.0\lab-start.ps1
# Use public_id + OTP printed by the host (also tray balloon)
# Or: .\bin\remotelink-app.exe after starting the server
```

## Develop & test

```powershell
cargo build --workspace
cargo test --workspace
cargo test -p remotelink-e2e --test ws_cli_live --test ws_agent_ipc

# Product shell
cargo run -p remotelink-server
cargo run -p remotelink-desktop

# CLI lab (advanced)
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 --transport=live
cargo run -p remotelink-viewer -- --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp CODE --transport=live
```

### KD5 split (service + agent)

```powershell
cargo run -p remotelink-host -- --role=agent --control-listen=pipe --boot-secret=SECRET --transport=live
cargo run -p remotelink-host -- --role=service --server=http://127.0.0.1:8080 `
  --transport=live --agent-control=pipe --boot-secret=SECRET
```

## Layout

- `apps/desktop` — **product shell** (`remotelink-app`: This PC + Connect)
- `apps/host` — service + session agent (tray, capture, encode, control IPC)
- `apps/viewer` — CLI viewer library + binary / optional egui shell
- `apps/server` — registry, WSS signaling, security, metrics, admin
- `packages/*` — protocol, auth, media, net, platforms, viewer-core, common
- `deploy/packaging` — WiX skeleton, portable install scripts
- `scripts/package-release.ps1` — build shippable zip
- `tests/e2e` — live WSS + agent IPC media tests

## License

MIT OR Apache-2.0

# RemoteLink

Low-latency remote desktop with **system audio**, host agent, viewer, and signaling server.

> **Integrated tree** (`integrate/v1` / `main`): monorepo assembled from plan PRs 1–27.
> Media path is still **mock PeerTransport / MH264** unless you wire real WebRTC.

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
```

## Build & test

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p remotelink-server
cargo run -p remotelink-host -- --role=colocate
cargo run -p remotelink-viewer -- --mock-codec --hud-block
cargo test -p remotelink-e2e
```

## Layout

- `apps/host` — service + session agent
- `apps/viewer` — CLI / optional egui shell
- `apps/server` — registry, WSS signaling, security, metrics, admin
- `packages/*` — protocol, auth, media, net, platforms, viewer-core, common
- `agents/*` — unit-test-agent, coverage-gate, bug-hunt-agent
- `tests/e2e` — synthetic identity + A/V + input tests

## License

MIT OR Apache-2.0

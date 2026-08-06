# RemoteLink

Low-latency remote desktop with system audio. See [DESIGN.md](DESIGN.md) for architecture, threat model, and PR plan.

## Status

PR 1 monorepo skeleton: compiling workspace stubs (`packages/common`, `apps/host`, `apps/viewer`, `apps/server`). Coverage fail-closed gates are **off** until a later PR; see `agents/shared/allowlist.toml`.

## Repository layout

```text
remotelink/
├── apps/host, viewer, server   # deployable binaries (stubs)
├── packages/common             # shared types / version
├── agents/shared/              # allowlists, agent config (later)
├── tests/{integration,e2e,fixtures}/
├── docs/
├── Cargo.toml                  # workspace root
└── .github/workflows/ci.yml
```

## Prerequisites

- [Rust](https://rustup.rs/) stable (see `rust-toolchain.toml`)
- On Windows GNU builds: a MinGW-w64 toolchain on `PATH` if using `stable-*-pc-windows-gnu`

## Build

```bash
# From repository root
cargo build --workspace
```

Binaries:

| Binary              | Crate               | Role                          |
|---------------------|---------------------|-------------------------------|
| `remotelink-host`   | `apps/host`         | Host agent                    |
| `remotelink-viewer` | `apps/viewer`       | Viewer client                 |
| `remotelink-server` | `apps/server`       | Signaling / registry server   |

```bash
cargo run -p remotelink-host
cargo run -p remotelink-viewer
cargo run -p remotelink-server
```

## Test & lint

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI (`.github/workflows/ci.yml`) runs `fmt --check`, `clippy -D warnings`, and `test` on `ubuntu-latest`.

## License

MIT OR Apache-2.0 (see workspace `Cargo.toml`).

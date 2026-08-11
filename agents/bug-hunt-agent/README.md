# Bug-Hunt Agent

Deterministic **chaos profiles**, **protocol random-byte fuzz**, and **nightly artifacts** for RemoteLink.

No LLM is required for the core nightly path (`cargo fuzz` / property tests / chaos scripts).

## Policy

| Rule | Detail |
|------|--------|
| **No real network** | Profiles use `MockPeerPair` and in-process skew injection only. |
| **No LLM for nightly** | Pure tooling; optional model hooks are out of scope for v1. |
| **Artifacts for humans** | JSON + markdown under `agents/shared/artifacts` or `target/chaos`. |
| **Never auto-merge** | Failures become issues / draft follow-ups after human triage. |

See `DESIGN.md` (Bug-Hunt Agent) and `agents/shared/bug_hunt_config.toml`.

## Chaos profiles

| Profile | What it does | Network? |
|---------|----------------|----------|
| `drop_packets` | Randomly **skips** mock-peer sends (flaky peer / simulated loss); checks delivered count matches non-dropped sends | Mock only |
| `reconnect` | Handshake → media → **close** → fresh pair (teardown/restart harness) + ICE restart | Mock only |
| `audio_desync` | Force large **skew inject** into `SkewController`; asserts delay steps + resample clamp | In-process |
| `protocol_fuzz` | Seeded random + mutated JSON against `decode_message` / `decode_input`; **never panic** | N/A |

Severity rubric (artifacts): Critical (auth bypass), High (crash), Medium (desync), Low (cosmetic), Info (clean pass).

## CLI

From the repository root:

```bash
# List profiles
cargo run -p bug-hunt-agent -- list

# Run full nightly (all profiles) → agents/shared/artifacts by default when
# agents/shared exists, else target/chaos
cargo run -p bug-hunt-agent -- nightly --out agents/shared/artifacts

# Single profile (config seed is used as-is — no per-profile derivation)
cargo run -p bug-hunt-agent -- run --profile drop_packets --out target/chaos
cargo run -p bug-hunt-agent -- run --profile all --config agents/shared/bug_hunt_config.toml
```

Binary name: `bug-hunt-agent`.

### Seed / repro contract

| Field | Meaning |
|-------|---------|
| `root_seed` | `[chaos].seed` from config (shared across a nightly run) |
| `seed` / `effective_seed` | Value actually used by the profile RNG |

**Policy (no double-derive):**

1. **`--seed N`** → effective seed is `N` for every profile; **never** re-derived.
2. **Multi-profile** (`nightly` / `--profile all`) → `effective = derive(root, profile)`.
3. **Single profile** → `effective = root` (config seed as-is).

**Repro a nightly failure** (paste artifact `seed` / `effective_seed`):

```bash
bug-hunt-agent run --profile protocol_fuzz --seed <effective_seed_from_artifact>
```

Do **not** put the artifact effective seed into `[chaos].seed` and re-run **nightly** — that would derive again. Use single-profile + `--seed` (or single-profile with config seed = effective).

### Artifacts

| Path | Purpose |
|------|---------|
| `<out>/drop_packets.json` | Metrics + `root_seed` + `effective_seed` + repro |
| `<out>/reconnect.json` | … |
| `<out>/audio_desync.json` | … |
| `<out>/protocol_fuzz.json` | … |
| `<out>/summary.json` | Aggregate pass/fail |
| `<out>/nightly-report.md` | Human-readable table + repro hint |

Repro key: `effective_seed` + profile name (and iteration for fuzz panics).

## Protocol fuzz (package-level)

Hand-fuzz / property-style tests also live in `remotelink-protocol` so CI catches decoder panics without running the agent:

```bash
cargo test -p remotelink-protocol -- random_bytes
```

Optional `cargo-fuzz` stubs: see [`packages/protocol/fuzz/README.md`](../../packages/protocol/fuzz/README.md) (not wired into default CI; requires nightly + libFuzzer).

## Nightly / CI

Local:

```bash
cargo test -p bug-hunt-agent
cargo run -p bug-hunt-agent -- nightly --out target/chaos
```

A **commented** optional GitHub Actions job is in `.github/workflows/ci.yml` (`bug-hunt-nightly`). Enable when ready; keep it non-blocking on PR until profiles stabilize.

## Shared config

| Path | Purpose |
|------|---------|
| `agents/shared/bug_hunt_config.toml` | Seeds, iterations, drop rate, skew inject, reconnect cycles |
| `agents/shared/artifacts/` | Default nightly output (gitignored recommended for large runs) |

## Non-goals

- Real TURN / packet capture chaos (follow-up once libwebrtc lands)
- Auto-filing GitHub issues without a bot + dedup review
- Running unreviewed generated code in CI
- Replacing hand-written unit tests for protocol / media / net

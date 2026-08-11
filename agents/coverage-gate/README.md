# coverage-gate

Per-package **coverage / test-presence** gates for RemoteLink.

## Policy

| Rule | Detail |
|------|--------|
| **Fail-closed allowlist only** | Only packages listed under `[[package]]` in `agents/shared/allowlist.toml` can fail CI. |
| **Portable presence gate** | Every gated package must have ≥ `min_tests` `#[test]` (etc.) under `src/` or `tests/`. |
| **Absolute line floors (llvm-cov)** | When `cargo-llvm-cov` runs, each gated package’s **own** sources must meet `min_line_coverage`. |
| **No PR delta yet** | This tool enforces **absolute** floors from the allowlist only. DESIGN’s “report delta on PRs” / “−1% abs regression vs base” is **deferred**; JSON artifacts are written so a later PR can compare to `main`. |
| **Windows / local** | Prefer `--mode tests` or `auto` (falls back when llvm-cov is missing). |

## CLI

From the repository root:

```bash
# Validate allowlist TOML
cargo run -p coverage-gate -- validate

# Test-presence gate (all platforms)
cargo run -p coverage-gate -- check --mode tests

# Auto: llvm-cov if installed, else tests
cargo run -p coverage-gate -- check --mode auto

# Force line coverage (requires cargo-llvm-cov + llvm-tools-preview)
cargo run -p coverage-gate -- check --mode llvm-cov --json-out target/coverage-gate.json
```

Binary name: `coverage-gate`.

### llvm-cov package isolation

For each gated package the tool runs:

```text
cargo llvm-cov --package <name> --json --summary-only \
  --exclude-from-report <every other workspace member> \
  --ignore-filename-regex '(^|/)(packages/(?!<dir>/)|apps/|agents/)'
```

Then it **requires** a parsed per-package line % (path aggregation or isolated `*` total). Missing parse data fails the package (fail-closed). Floors are checked in-process (not via a mixed-workspace `--fail-under-lines` total).

## Allowlist fields

```toml
schema_version = 1

[[package]]
name = "remotelink-auth"
min_tests = 50
min_line_coverage = 90.0   # optional; only checked in llvm-cov mode

# Optional intentional public-API gaps (documentation for unit-test agent)
# [[gap]]
# crate = "…"
# item = "…"
# reason = "…"
```

## CI

Both jobs on `ubuntu-latest` are **required** (no `continue-on-error`):

- **rust** job: `coverage-gate check --mode tests` after `cargo test` (portable presence floors).
- **coverage** job: installs `cargo-llvm-cov`, runs `check --mode llvm-cov --json-out …`, uploads the JSON summary as a workflow artifact (absolute floors today; delta compare later).

Local Windows: use `--mode tests` (llvm-cov install is optional).

Bootstrap gated packages: `remotelink-common` (min_tests=2), `remotelink-protocol` (30), `remotelink-auth` (50).

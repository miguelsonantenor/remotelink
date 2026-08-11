# Unit-Test Agent

Deterministic **public-API inventory** and **draft coverage reports** for RemoteLink.

## Policy (non-negotiable)

| Rule | Detail |
|------|--------|
| **NEVER auto-merge** | Agent output is **draft only**. Humans review and merge. |
| **CI runs checked-in tests only** | Do **not** execute model-generated tests until they are reviewed and committed via PR. |
| **Offline-first** | `inventory` and `draft-report` need no network or LLM. |
| **Label** | Draft PRs: `needs-human-review`. Treat generated code as untrusted. |

See `DESIGN.md` (Multi-Agent Testing Strategy) and `agents/shared/draft_pr_template.md`.

## CLI

From the repository root (after `cargo build -p unit-test-agent`):

```bash
# Scan library crates for pub items → agents/shared/inventory.json
cargo run -p unit-test-agent -- inventory --workspace . --out agents/shared/inventory.json

# Markdown report of packages lacking tests (stdout)
cargo run -p unit-test-agent -- draft-report --inventory agents/shared/inventory.json
```

Binary name: `unit-test-agent`.

## Shared artifacts

| Path | Purpose |
|------|---------|
| `agents/shared/inventory_schema.json` | JSON Schema for inventory documents |
| `agents/shared/inventory.json` | Checked-in snapshot (regenerate with `inventory`) |
| `agents/shared/draft_pr_template.md` | Body template for draft coverage PRs |
| `agents/shared/allowlist.toml` | Intentional coverage gaps (gates still report-only until PR 25) |

## Non-goals

- Auto-merge or silent push
- Running generated tests in CI without review
- GUI / DXGI pixel tests
- Replacing hand-written tests for `protocol` / `auth` / `media` (those remain merge gates)

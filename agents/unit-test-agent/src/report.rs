//! Draft coverage report markdown (human review only — never auto-merge).

use crate::types::{Inventory, InventoryItem, ItemKind};

/// Packages that look like they lack tests (lib crates without `#[test]` / `tests/`).
///
/// Note: `has_tests` is **presence-only** (any `#[test]` / `tests/`), not coverage.
pub fn packages_lacking_tests(inv: &Inventory) -> Vec<&str> {
    let mut names: Vec<&str> = inv
        .packages
        .iter()
        .filter(|p| p.is_lib && !p.has_tests)
        .map(|p| p.name.as_str())
        .collect();
    names.sort();
    names
}

/// Unrestricted `pub` items that are not pure `use` re-exports (sample surface for TODOs).
pub fn sample_declared_surface(inv: &Inventory, limit: usize) -> Vec<&InventoryItem> {
    inv.items
        .iter()
        .filter(|i| i.visibility.is_public_api() && i.kind != ItemKind::Use)
        .take(limit)
        .collect()
}

/// Render a markdown draft report for human reviewers / draft PR bodies.
pub fn draft_report_markdown(inv: &Inventory) -> String {
    let mut out = String::new();
    out.push_str("# Unit-Test Agent — Draft Coverage Report\n\n");
    out.push_str("## Policy (read me)\n\n");
    out.push_str("- **NEVER auto-merge.** This report and any follow-up PRs are **draft only**.\n");
    out.push_str(
        "- **CI runs only checked-in tests.** Do not execute model-generated tests until they are reviewed and committed.\n",
    );
    out.push_str(
        "- Label draft PRs with `needs-human-review`. Generated code is untrusted until review.\n\n",
    );

    let public_count = inv
        .items
        .iter()
        .filter(|i| i.visibility.is_public_api())
        .count();
    let declared_count = inv
        .items
        .iter()
        .filter(|i| i.visibility.is_public_api() && i.kind != ItemKind::Use)
        .count();

    out.push_str(&format!(
        "- Workspace: `{}`\n- Generated: `{}`\n- Schema version: {}\n- Public items scanned: {} ({} excluding pure `use` re-exports)\n\n",
        inv.workspace_root,
        inv.generated_at,
        inv.schema_version,
        public_count,
        declared_count,
    ));

    out.push_str("## Packages lacking tests\n\n");
    out.push_str(
        "_`has_tests` is presence-only (any `#[test]` / `tests/` tree), **not** line or item coverage._\n\n",
    );
    let lacking = packages_lacking_tests(inv);
    if lacking.is_empty() {
        out.push_str(
            "_All library packages appear to have at least one unit or integration test._\n\n",
        );
    } else {
        out.push_str("Library packages with **no** detected `#[test]` / `tests/` tree:\n\n");
        for name in &lacking {
            out.push_str(&format!("- `{name}`\n"));
        }
        out.push('\n');
    }

    out.push_str("## Package summary\n\n");
    out.push_str("| Package | Lib | Has tests | Public items |\n");
    out.push_str("|---------|-----|-----------|--------------|\n");
    for p in &inv.packages {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            p.name,
            if p.is_lib { "yes" } else { "no" },
            if p.has_tests { "yes" } else { "**no**" },
            p.public_item_count
        ));
    }
    out.push('\n');

    out.push_str("## Suggested draft PR titles\n\n");
    if lacking.is_empty() {
        // Still suggest per-lib draft titles for item-level work (offline mode).
        let libs: Vec<_> = inv
            .packages
            .iter()
            .filter(|p| p.is_lib && p.public_item_count > 0)
            .collect();
        if libs.is_empty() {
            out.push_str("_No library packages with public items._\n\n");
        } else {
            out.push_str(
                "Package-level presence checks passed; open **item-level** draft PRs as needed:\n\n",
            );
            for p in libs {
                let module = p.name.trim_start_matches("remotelink-");
                out.push_str(&format!(
                    "- `test(agent): cover {module}` — draft only, label `needs-human-review`\n"
                ));
            }
            out.push('\n');
        }
    } else {
        for name in &lacking {
            let module = name.trim_start_matches("remotelink-");
            out.push_str(&format!(
                "- `test(agent): cover {module}` — draft only, label `needs-human-review`\n"
            ));
        }
        out.push('\n');
    }

    // Highlight unrestricted pub items from packages without tests (actionable list).
    out.push_str("## Public items in packages without tests\n\n");
    let lacking_set: std::collections::HashSet<&str> = lacking.iter().copied().collect();
    let mut listed = 0usize;
    for item in inv
        .items
        .iter()
        .filter(|i| i.visibility.is_public_api() && lacking_set.contains(i.crate_name.as_str()))
    {
        out.push_str(&format!(
            "- `{}` :: `{}` (`{}`, {})\n",
            item.crate_name,
            item.item,
            item.path,
            format!("{:?}", item.kind).to_lowercase()
        ));
        listed += 1;
        if listed >= 100 {
            out.push_str("\n_…truncated (first 100)._\n");
            break;
        }
    }
    if listed == 0 {
        out.push_str("_None (no untested lib packages, or no public items)._\n");
    }
    out.push('\n');

    // Offline mode: stub TODO list over declared surface (excludes pure re-exports).
    out.push_str("## Sample public surface — offline TODO stubs\n\n");
    out.push_str(
        "Every unrestricted `pub` item should eventually be **tested** or **allowlisted** \
         (`agents/shared/allowlist.toml`). Gates remain report-only until PR 25.\n\n",
    );
    out.push_str(
        "Below is a bounded sample of **declared** surface (excludes pure `use` re-exports). \
         Treat as review checklist stubs — **not** executable tests.\n\n",
    );
    const SAMPLE_LIMIT: usize = 40;
    let sample = sample_declared_surface(inv, SAMPLE_LIMIT);
    if sample.is_empty() {
        out.push_str("_No declared public items found._\n\n");
    } else {
        for item in &sample {
            out.push_str(&format!(
                "- [ ] TODO test `{crate}::{item}` (`{path}`:{line})\n",
                crate = item.crate_name,
                item = item.item,
                path = item.path,
                line = item
                    .line
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
            ));
        }
        if declared_count > sample.len() {
            out.push_str(&format!(
                "\n_…and {} more declared pub items not listed (run inventory JSON for full list)._\n",
                declared_count - sample.len()
            ));
        }
        out.push('\n');
    }

    out.push_str("## Allowlist\n\n");
    out.push_str(
        "Intentional gaps go in `agents/shared/allowlist.toml` with a reason. \
         File currently bootstraps **empty** (no registered gaps).\n\n",
    );

    out.push_str("---\n\n");
    out.push_str(
        "*Generated by `unit-test-agent draft-report`. See `agents/shared/draft_pr_template.md`.*\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InventoryItem, ItemKind, PackageSummary, Visibility};

    fn sample_inv(has_tests: bool) -> Inventory {
        Inventory {
            schema_version: 1,
            workspace_root: "/tmp/ws".into(),
            generated_at: "t0".into(),
            items: vec![
                InventoryItem {
                    crate_name: "remotelink-demo".into(),
                    path: "packages/demo/src/lib.rs".into(),
                    item: "VERSION".into(),
                    kind: ItemKind::Const,
                    visibility: Visibility::Pub,
                    line: Some(1),
                },
                InventoryItem {
                    crate_name: "remotelink-demo".into(),
                    path: "packages/demo/src/lib.rs".into(),
                    item: "error::AuthError".into(),
                    kind: ItemKind::Use,
                    visibility: Visibility::Pub,
                    line: Some(2),
                },
            ],
            packages: vec![PackageSummary {
                name: "remotelink-demo".into(),
                manifest_path: "packages/demo/Cargo.toml".into(),
                is_lib: true,
                has_tests,
                public_item_count: 2,
            }],
        }
    }

    #[test]
    fn lacking_tests_lists_lib_without_tests() {
        let inv = sample_inv(false);
        assert_eq!(packages_lacking_tests(&inv), vec!["remotelink-demo"]);
        let md = draft_report_markdown(&inv);
        assert!(md.contains("NEVER auto-merge"));
        assert!(md.contains("remotelink-demo"));
        assert!(md.contains("test(agent): cover demo"));
        assert!(md.contains("TODO test"));
        assert!(md.contains("presence-only"));
    }

    #[test]
    fn no_lacking_when_has_tests_still_suggests_item_level() {
        let inv = sample_inv(true);
        assert!(packages_lacking_tests(&inv).is_empty());
        let md = draft_report_markdown(&inv);
        assert!(md.contains("appear to have at least one"));
        assert!(md.contains("item-level"));
        assert!(md.contains("test(agent): cover demo"));
        assert!(md.contains("TODO test `remotelink-demo::VERSION`"));
        // Pure use re-exports excluded from TODO sample.
        assert!(!md.contains("TODO test `remotelink-demo::error::AuthError`"));
        assert!(md.contains("allowlist.toml"));
    }

    #[test]
    fn sample_skips_use_reexports() {
        let inv = sample_inv(true);
        let sample = sample_declared_surface(&inv, 10);
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0].item, "VERSION");
    }
}

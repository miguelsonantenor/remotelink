//! Unit-Test Agent library: public-API inventory and draft coverage reports.
//!
//! # Policy (non-negotiable)
//!
//! - **Never auto-merge.** Outputs are draft PR material for human review only.
//! - **CI runs only checked-in tests.** Generated code must not execute in CI
//!   until it is committed via a reviewed PR.
//! - **Offline-first.** Inventory and draft-report need no network / LLM.

#![deny(missing_docs)]

mod inventory;
mod report;
mod types;

pub use inventory::{
    expand_use_imports, inventory_workspace, is_cfg_test_attr, package_has_tests,
    parse_pub_use_statement, parse_public_items, walk_rust_files, InventoryError,
};
pub use report::{draft_report_markdown, packages_lacking_tests, sample_declared_surface};
pub use types::{Inventory, InventoryItem, ItemKind, PackageSummary, Visibility};

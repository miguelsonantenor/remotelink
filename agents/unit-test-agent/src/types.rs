//! Inventory JSON types (see `agents/shared/inventory_schema.json`).

use serde::{Deserialize, Serialize};

/// Root document written to `agents/shared/inventory.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    /// Schema / format version for consumers.
    pub schema_version: u32,
    /// Absolute or workspace-relative root that was scanned.
    pub workspace_root: String,
    /// When the inventory was produced (RFC 3339 if available; else local).
    pub generated_at: String,
    /// One entry per public item across library crates.
    pub items: Vec<InventoryItem>,
    /// Per-package rollup (test presence, item counts).
    pub packages: Vec<PackageSummary>,
}

/// A single public API item discovered by source walk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryItem {
    /// Cargo package name (e.g. `remotelink-auth`).
    pub crate_name: String,
    /// Source path relative to workspace root (forward slashes).
    pub path: String,
    /// Fully-qualified-ish item name within the file (e.g. `DevicePublicId::parse`).
    pub item: String,
    /// Kind of item (`fn`, `struct`, `enum`, …).
    pub kind: ItemKind,
    /// Visibility string (`pub`, `pub(crate)`, …).
    pub visibility: Visibility,
    /// 1-based line number in `path`, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Classification of a public item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// `fn`
    Fn,
    /// `struct`
    Struct,
    /// `enum`
    Enum,
    /// `trait`
    Trait,
    /// `type` alias
    Type,
    /// `const`
    Const,
    /// `static`
    Static,
    /// `mod`
    Mod,
    /// `use` re-export
    Use,
    /// `macro_rules!` or `macro`
    Macro,
}

/// Visibility of a discovered item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Fully public (`pub`).
    Pub,
    /// Restricted (`pub(crate)`, `pub(super)`, `pub(in path)`).
    Restricted(String),
}

impl Visibility {
    /// Parse a visibility keyword prefix from source text.
    pub fn parse(vis: &str) -> Self {
        let v = vis.trim();
        if v == "pub" {
            Visibility::Pub
        } else {
            Visibility::Restricted(v.to_string())
        }
    }

    /// True only for unrestricted `pub`.
    pub fn is_public_api(&self) -> bool {
        matches!(self, Visibility::Pub)
    }
}

/// Rollup for one workspace package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSummary {
    /// Cargo package name.
    pub name: String,
    /// Manifest path relative to workspace root.
    pub manifest_path: String,
    /// Package is a library crate (has `lib` target).
    pub is_lib: bool,
    /// Package appears to ship unit or integration tests.
    pub has_tests: bool,
    /// Count of unrestricted `pub` items in library sources.
    pub public_item_count: usize,
}

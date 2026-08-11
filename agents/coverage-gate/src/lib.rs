//! Per-package coverage / test-presence gates.
//!
//! Fail-closed **only** for packages listed in `agents/shared/allowlist.toml`.
//! Packages not on the allowlist are ignored.
//!
//! # Modes
//!
//! - **tests** — require `min_tests` `#[test]` (etc.) under `src/` / `tests/`
//! - **llvm-cov** — also enforce `min_line_coverage` via `cargo-llvm-cov`
//! - **auto** — llvm-cov when installed, else tests
//!
//! Full line-coverage enforcement is intended for **Linux CI**. On Windows,
//! use the test-presence gate (`--mode tests` or auto fallback).

#![deny(missing_docs)]

mod allowlist;
mod gate;
mod test_count;

pub use allowlist::{Allowlist, AllowlistError, GapEntry, PackageGate};
pub use gate::{
    evaluate_package, llvm_cov_available, package_line_percent, parse_llvm_cov_json, resolve_mode,
    run_gates, workspace_packages, GateError, GateMode, GateReport, GateReportJson, PackageResult,
    PackageResultJson, WorkspacePackage,
};
pub use test_count::{
    count_package_tests, count_tests_in_source, package_has_test_surface, walk_rust_files,
};

//! Parse `agents/shared/allowlist.toml` coverage-gate configuration.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// Root allowlist / gate configuration document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Allowlist {
    /// Format version for forward compatibility.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Packages subject to fail-closed gates. Empty = no packages gated.
    #[serde(default)]
    pub package: Vec<PackageGate>,
    /// Documented intentional public-API gaps (informational).
    #[serde(default)]
    pub gap: Vec<GapEntry>,
}

fn default_schema_version() -> u32 {
    1
}

/// Fail-closed gate for one Cargo package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PackageGate {
    /// Cargo package name (e.g. `remotelink-auth`).
    pub name: String,
    /// Minimum number of test functions (`#[test]`, `#[tokio::test]`, …).
    pub min_tests: u32,
    /// Minimum line coverage percent when `cargo-llvm-cov` runs.
    ///
    /// Omitted / `None` skips the llvm-cov numeric check for this package
    /// (test-presence gate still applies).
    #[serde(default)]
    pub min_line_coverage: Option<f64>,
}

/// Intentional coverage gap for inventory / human review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapEntry {
    /// Cargo package / crate name.
    #[serde(rename = "crate")]
    pub crate_name: String,
    /// Item path or glob (e.g. `dxgi_capture::*`).
    pub item: String,
    /// Why this gap is accepted.
    pub reason: String,
}

/// Errors loading or validating the allowlist.
#[derive(Debug)]
pub enum AllowlistError {
    /// Filesystem I/O.
    Io(io::Error),
    /// TOML parse failure.
    Parse(String),
    /// Logical validation failure.
    Validate(String),
}

impl fmt::Display for AllowlistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllowlistError::Io(e) => write!(f, "io error: {e}"),
            AllowlistError::Parse(e) => write!(f, "parse error: {e}"),
            AllowlistError::Validate(e) => write!(f, "validation error: {e}"),
        }
    }
}

impl std::error::Error for AllowlistError {}

impl From<io::Error> for AllowlistError {
    fn from(e: io::Error) -> Self {
        AllowlistError::Io(e)
    }
}

impl Allowlist {
    /// Load and validate an allowlist from a TOML file.
    pub fn load(path: &Path) -> Result<Self, AllowlistError> {
        let text = fs::read_to_string(path)?;
        Self::parse_str(&text)
    }

    /// Parse and validate allowlist TOML text.
    pub fn parse_str(text: &str) -> Result<Self, AllowlistError> {
        let al: Allowlist =
            toml::from_str(text).map_err(|e| AllowlistError::Parse(e.to_string()))?;
        al.validate()?;
        Ok(al)
    }

    /// Validate package gates and gaps.
    pub fn validate(&self) -> Result<(), AllowlistError> {
        if self.schema_version == 0 {
            return Err(AllowlistError::Validate(
                "schema_version must be >= 1".into(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        for pkg in &self.package {
            if pkg.name.trim().is_empty() {
                return Err(AllowlistError::Validate(
                    "package.name must not be empty".into(),
                ));
            }
            if !seen.insert(pkg.name.clone()) {
                return Err(AllowlistError::Validate(format!(
                    "duplicate package gate: {}",
                    pkg.name
                )));
            }
            if let Some(cov) = pkg.min_line_coverage {
                if !(0.0..=100.0).contains(&cov) {
                    return Err(AllowlistError::Validate(format!(
                        "package {}: min_line_coverage must be in 0..=100, got {cov}",
                        pkg.name
                    )));
                }
            }
        }

        for (i, gap) in self.gap.iter().enumerate() {
            if gap.crate_name.trim().is_empty() {
                return Err(AllowlistError::Validate(format!(
                    "gap[{i}].crate must not be empty"
                )));
            }
            if gap.item.trim().is_empty() {
                return Err(AllowlistError::Validate(format!(
                    "gap[{i}].item must not be empty"
                )));
            }
            if gap.reason.trim().is_empty() {
                return Err(AllowlistError::Validate(format!(
                    "gap[{i}].reason must not be empty"
                )));
            }
        }

        Ok(())
    }

    /// Look up a package gate by Cargo package name.
    pub fn package_gate(&self, name: &str) -> Option<&PackageGate> {
        self.package.iter().find(|p| p.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const BOOTSTRAP: &str = r#"
schema_version = 1

[[package]]
name = "remotelink-common"
min_tests = 2
min_line_coverage = 90.0

[[package]]
name = "remotelink-protocol"
min_tests = 30
min_line_coverage = 90.0

[[package]]
name = "remotelink-auth"
min_tests = 50
min_line_coverage = 90.0
"#;

    #[test]
    fn parse_bootstrap_packages() {
        let al = Allowlist::parse_str(BOOTSTRAP).unwrap();
        assert_eq!(al.schema_version, 1);
        assert_eq!(al.package.len(), 3);
        assert!(al.gap.is_empty());

        let common = al.package_gate("remotelink-common").unwrap();
        assert_eq!(common.min_tests, 2);
        assert_eq!(common.min_line_coverage, Some(90.0));

        let protocol = al.package_gate("remotelink-protocol").unwrap();
        assert_eq!(protocol.min_tests, 30);

        let auth = al.package_gate("remotelink-auth").unwrap();
        assert_eq!(auth.min_tests, 50);
        assert!(al.package_gate("remotelink-host").is_none());
    }

    #[test]
    fn load_repo_allowlist_matches_bootstrap_floors() {
        // Keep fixture floors aligned with agents/shared/allowlist.toml when present.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../shared/allowlist.toml");
        let al = Allowlist::load(&path).unwrap();
        assert_eq!(al.package_gate("remotelink-common").unwrap().min_tests, 2);
        assert_eq!(
            al.package_gate("remotelink-protocol").unwrap().min_tests,
            30
        );
        assert_eq!(al.package_gate("remotelink-auth").unwrap().min_tests, 50);
    }

    #[test]
    fn parse_empty_is_valid_fail_open() {
        let al = Allowlist::parse_str("schema_version = 1\n").unwrap();
        assert!(al.package.is_empty());
        assert!(al.gap.is_empty());
    }

    #[test]
    fn parse_defaults_schema_version() {
        let al = Allowlist::parse_str("").unwrap();
        assert_eq!(al.schema_version, 1);
        assert!(al.package.is_empty());
    }

    #[test]
    fn parse_package_without_coverage() {
        let text = r#"
[[package]]
name = "remotelink-common"
min_tests = 2
"#;
        let al = Allowlist::parse_str(text).unwrap();
        let p = &al.package[0];
        assert_eq!(p.min_tests, 2);
        assert_eq!(p.min_line_coverage, None);
    }

    #[test]
    fn parse_gaps() {
        let text = r#"
[[gap]]
crate = "remotelink-common"
item = "foo::*"
reason = "FFI shim"
"#;
        let al = Allowlist::parse_str(text).unwrap();
        assert_eq!(al.gap.len(), 1);
        assert_eq!(al.gap[0].crate_name, "remotelink-common");
        assert_eq!(al.gap[0].item, "foo::*");
        assert_eq!(al.gap[0].reason, "FFI shim");
    }

    #[test]
    fn reject_duplicate_package() {
        let text = r#"
[[package]]
name = "remotelink-common"
min_tests = 1

[[package]]
name = "remotelink-common"
min_tests = 2
"#;
        let err = Allowlist::parse_str(text).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn reject_empty_package_name() {
        let text = r#"
[[package]]
name = ""
min_tests = 1
"#;
        let err = Allowlist::parse_str(text).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn reject_coverage_out_of_range() {
        let text = r#"
[[package]]
name = "remotelink-common"
min_tests = 1
min_line_coverage = 101.0
"#;
        let err = Allowlist::parse_str(text).unwrap_err();
        assert!(err.to_string().contains("min_line_coverage"), "{err}");
    }

    #[test]
    fn reject_gap_missing_reason() {
        let text = r#"
[[gap]]
crate = "c"
item = "i"
reason = ""
"#;
        let err = Allowlist::parse_str(text).unwrap_err();
        assert!(err.to_string().contains("reason"), "{err}");
    }

    #[test]
    fn reject_invalid_toml() {
        let err = Allowlist::parse_str("[[package]\n").unwrap_err();
        assert!(matches!(err, AllowlistError::Parse(_)));
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowlist.toml");
        let mut f = fs::File::create(&path).unwrap();
        write!(f, "{BOOTSTRAP}").unwrap();
        let al = Allowlist::load(&path).unwrap();
        assert_eq!(al.package.len(), 3);
    }

    #[test]
    fn load_missing_file() {
        let err = Allowlist::load(Path::new("/nonexistent/allowlist.toml")).unwrap_err();
        assert!(matches!(err, AllowlistError::Io(_)));
    }
}

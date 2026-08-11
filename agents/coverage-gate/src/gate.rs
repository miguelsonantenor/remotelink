//! Run per-package coverage / test-presence gates.

use crate::allowlist::{Allowlist, PackageGate};
use crate::test_count::count_package_tests;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// How the gate should measure compliance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Only enforce `min_tests` (works everywhere).
    Tests,
    /// Require `cargo-llvm-cov` and enforce `min_line_coverage`.
    LlvmCov,
    /// Prefer llvm-cov when the tool is available; else tests.
    Auto,
}

impl GateMode {
    /// Parse CLI mode string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tests" => Some(GateMode::Tests),
            "llvm-cov" | "llvm_cov" | "cov" => Some(GateMode::LlvmCov),
            "auto" => Some(GateMode::Auto),
            _ => None,
        }
    }
}

impl fmt::Display for GateMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateMode::Tests => write!(f, "tests"),
            GateMode::LlvmCov => write!(f, "llvm-cov"),
            GateMode::Auto => write!(f, "auto"),
        }
    }
}

/// One package's gate evaluation result.
#[derive(Debug, Clone)]
pub struct PackageResult {
    /// Package name.
    pub name: String,
    /// Tests found under the package tree.
    pub test_count: u32,
    /// Configured minimum tests.
    pub min_tests: u32,
    /// Line coverage percent when measured.
    pub line_coverage: Option<f64>,
    /// Configured minimum line coverage.
    pub min_line_coverage: Option<f64>,
    /// Human-readable failure reasons (empty = pass).
    pub failures: Vec<String>,
}

impl PackageResult {
    /// True when all configured checks passed.
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Full gate run summary.
#[derive(Debug, Clone)]
pub struct GateReport {
    /// Mode actually used (after auto resolution).
    pub effective_mode: GateMode,
    /// Whether cargo-llvm-cov was available.
    pub llvm_cov_available: bool,
    /// Per-package results (gated packages only).
    pub packages: Vec<PackageResult>,
}

/// Machine-readable package result for CI artifacts / future delta compares.
#[derive(Debug, Clone, Serialize)]
pub struct PackageResultJson {
    /// Package name.
    pub name: String,
    /// Tests found.
    pub test_count: u32,
    /// Configured minimum tests.
    pub min_tests: u32,
    /// Line coverage percent when measured.
    pub line_coverage: Option<f64>,
    /// Configured minimum line coverage.
    pub min_line_coverage: Option<f64>,
    /// Whether this package passed.
    pub passed: bool,
    /// Failure reasons.
    pub failures: Vec<String>,
}

/// Machine-readable gate report (absolute floors; PR delta deferred).
#[derive(Debug, Clone, Serialize)]
pub struct GateReportJson {
    /// Effective mode string (`tests` / `llvm-cov`).
    pub effective_mode: String,
    /// Whether cargo-llvm-cov was available.
    pub llvm_cov_available: bool,
    /// Overall pass.
    pub passed: bool,
    /// Policy note for consumers.
    pub policy: String,
    /// Per-package results.
    pub packages: Vec<PackageResultJson>,
}

impl GateReport {
    /// True when every gated package passed.
    pub fn passed(&self) -> bool {
        self.packages.iter().all(PackageResult::passed)
    }

    /// Serialize absolute-floor results for CI artifacts.
    pub fn to_json_report(&self) -> GateReportJson {
        GateReportJson {
            effective_mode: self.effective_mode.to_string(),
            llvm_cov_available: self.llvm_cov_available,
            passed: self.passed(),
            policy: "absolute floors from allowlist.toml only; PR coverage delta / −1% regression compare deferred".into(),
            packages: self
                .packages
                .iter()
                .map(|p| PackageResultJson {
                    name: p.name.clone(),
                    test_count: p.test_count,
                    min_tests: p.min_tests,
                    line_coverage: p.line_coverage,
                    min_line_coverage: p.min_line_coverage,
                    passed: p.passed(),
                    failures: p.failures.clone(),
                })
                .collect(),
        }
    }

    /// Render a human-readable summary for CI logs.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "coverage-gate: mode={} (llvm-cov available: {})\n",
            self.effective_mode, self.llvm_cov_available
        ));
        if self.packages.is_empty() {
            out.push_str("No packages listed in allowlist — nothing to enforce (fail-open).\n");
            return out;
        }
        for p in &self.packages {
            let status = if p.passed() { "PASS" } else { "FAIL" };
            out.push_str(&format!(
                "  [{status}] {}  tests={}/{}",
                p.name, p.test_count, p.min_tests
            ));
            if let (Some(got), Some(min)) = (p.line_coverage, p.min_line_coverage) {
                out.push_str(&format!("  lines={got:.2}% (min {min:.2}%)"));
            } else if let Some(min) = p.min_line_coverage {
                if matches!(self.effective_mode, GateMode::LlvmCov) {
                    out.push_str(&format!("  lines=n/a (min {min:.2}%)"));
                }
            }
            out.push('\n');
            for f in &p.failures {
                out.push_str(&format!("         - {f}\n"));
            }
        }
        if self.passed() {
            out.push_str("coverage-gate: all gated packages passed\n");
        } else {
            out.push_str("coverage-gate: one or more packages failed\n");
        }
        out
    }
}

/// Errors while running the gate.
#[derive(Debug)]
pub enum GateError {
    /// I/O.
    Io(io::Error),
    /// `cargo metadata` or external tool failure.
    Tool(String),
    /// JSON parse.
    Json(String),
    /// Allowlist package not found in workspace.
    MissingPackage(String),
}

impl fmt::Display for GateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateError::Io(e) => write!(f, "io error: {e}"),
            GateError::Tool(e) => write!(f, "{e}"),
            GateError::Json(e) => write!(f, "json error: {e}"),
            GateError::MissingPackage(n) => {
                write!(f, "gated package not found in workspace: {n}")
            }
        }
    }
}

impl std::error::Error for GateError {}

impl From<io::Error> for GateError {
    fn from(e: io::Error) -> Self {
        GateError::Io(e)
    }
}

/// Workspace package location from `cargo metadata`.
#[derive(Debug, Clone)]
pub struct WorkspacePackage {
    /// Cargo package name.
    pub name: String,
    /// Package root directory (parent of Cargo.toml).
    pub root_dir: PathBuf,
}

/// Run `cargo metadata` and return workspace members.
pub fn workspace_packages(workspace_root: &Path) -> Result<Vec<WorkspacePackage>, GateError> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(workspace_root.join("Cargo.toml"))
        .current_dir(workspace_root)
        .output()
        .map_err(|e| GateError::Tool(format!("failed to run cargo metadata: {e}")))?;

    if !output.status.success() {
        return Err(GateError::Tool(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let meta: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| GateError::Json(e.to_string()))?;

    let packages = meta
        .get("packages")
        .and_then(|p| p.as_array())
        .ok_or_else(|| GateError::Json("missing packages array".into()))?;

    let mut out = Vec::new();
    for pkg in packages {
        let name = pkg
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| GateError::Json("package missing name".into()))?
            .to_string();
        let manifest = pkg
            .get("manifest_path")
            .and_then(|m| m.as_str())
            .ok_or_else(|| GateError::Json("package missing manifest_path".into()))?;
        let root_dir = Path::new(manifest)
            .parent()
            .ok_or_else(|| GateError::Tool(format!("bad manifest_path for {name}")))?
            .to_path_buf();
        out.push(WorkspacePackage { name, root_dir });
    }
    Ok(out)
}

/// True if `cargo llvm-cov --version` succeeds.
pub fn llvm_cov_available() -> bool {
    Command::new("cargo")
        .args(["llvm-cov", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve Auto → Tests or LlvmCov.
pub fn resolve_mode(requested: GateMode, llvm_available: bool) -> GateMode {
    match requested {
        GateMode::Auto => {
            if llvm_available {
                GateMode::LlvmCov
            } else {
                GateMode::Tests
            }
        }
        other => other,
    }
}

/// Run gates for all allowlisted packages.
pub fn run_gates(
    workspace_root: &Path,
    allowlist: &Allowlist,
    mode: GateMode,
) -> Result<GateReport, GateError> {
    let llvm_available = llvm_cov_available();
    let effective = resolve_mode(mode, llvm_available);

    if matches!(mode, GateMode::LlvmCov) && !llvm_available {
        return Err(GateError::Tool(
            "cargo-llvm-cov is not available (install: cargo install cargo-llvm-cov; \
             rustup component add llvm-tools-preview). \
             On Windows, use --mode tests for the test-presence gate; \
             full line coverage is enforced on Linux CI."
                .into(),
        ));
    }

    let workspace = workspace_packages(workspace_root)?;
    let by_name: BTreeMap<&str, &WorkspacePackage> =
        workspace.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut results = Vec::new();
    for gate in &allowlist.package {
        let pkg = by_name
            .get(gate.name.as_str())
            .ok_or_else(|| GateError::MissingPackage(gate.name.clone()))?;

        let test_count = count_package_tests(&pkg.root_dir)?;
        let mut failures = Vec::new();

        if test_count < gate.min_tests {
            failures.push(format!(
                "test presence: found {test_count} test(s), require ≥ {}",
                gate.min_tests
            ));
        }

        let mut line_coverage = None;
        if matches!(effective, GateMode::LlvmCov) {
            if let Some(min) = gate.min_line_coverage {
                // Fail-closed: require a parsed per-package percent after an
                // isolated llvm-cov run (no silent Ok(None) greenwash).
                match run_llvm_cov_package(workspace_root, &gate.name, &workspace) {
                    Ok(got) => {
                        line_coverage = Some(got);
                        if got + f64::EPSILON < min {
                            failures.push(format!("line coverage: {got:.2}% < required {min:.2}%"));
                        }
                    }
                    Err(e) => {
                        failures.push(format!("line coverage: {e}"));
                    }
                }
            }
        }

        results.push(PackageResult {
            name: gate.name.clone(),
            test_count,
            min_tests: gate.min_tests,
            line_coverage,
            min_line_coverage: gate.min_line_coverage,
            failures,
        });
    }

    // Stable order matching allowlist.
    Ok(GateReport {
        effective_mode: effective,
        llvm_cov_available: llvm_available,
        packages: results,
    })
}

/// Evaluate a single package gate against known counts (unit-test helper).
pub fn evaluate_package(
    gate: &PackageGate,
    test_count: u32,
    line_coverage: Option<f64>,
    enforce_coverage: bool,
) -> PackageResult {
    let mut failures = Vec::new();
    if test_count < gate.min_tests {
        failures.push(format!(
            "test presence: found {test_count} test(s), require ≥ {}",
            gate.min_tests
        ));
    }
    if enforce_coverage {
        if let Some(min) = gate.min_line_coverage {
            match line_coverage {
                Some(got) if got + f64::EPSILON < min => {
                    failures.push(format!("line coverage: {got:.2}% < required {min:.2}%"));
                }
                None => {
                    failures.push(format!(
                        "line coverage: no data for package (required ≥ {min:.2}%)"
                    ));
                }
                _ => {}
            }
        }
    }
    PackageResult {
        name: gate.name.clone(),
        test_count,
        min_tests: gate.min_tests,
        line_coverage,
        min_line_coverage: gate.min_line_coverage,
        failures,
    }
}

/// Run `cargo llvm-cov` for one package with the report isolated to that package.
///
/// Isolation:
/// - `--exclude-from-report` for every other workspace member so path deps
///   (e.g. auth → common) do not inflate/deflate the measured total.
/// - `--ignore-filename-regex` further drops non-package source paths when the
///   monorepo layout is known (`packages/<dir>/`).
///
/// Returns the **parsed per-package** line coverage percent. Missing parse data
/// is an error (fail-closed); the numeric floor is enforced by the caller, not
/// by `--fail-under-lines` on a mixed workspace total.
fn run_llvm_cov_package(
    workspace_root: &Path,
    package_name: &str,
    workspace: &[WorkspacePackage],
) -> Result<f64, GateError> {
    let mut cmd = Command::new("cargo");
    cmd.arg("llvm-cov")
        .arg("--package")
        .arg(package_name)
        .arg("--json")
        .arg("--summary-only")
        .current_dir(workspace_root);

    for other in workspace {
        if other.name != package_name {
            cmd.arg("--exclude-from-report").arg(&other.name);
        }
    }

    if let Some(re) = ignore_filename_regex_for_package(package_name) {
        cmd.arg("--ignore-filename-regex").arg(re);
    }

    let output = cmd
        .output()
        .map_err(|e| GateError::Tool(format!("failed to run cargo llvm-cov: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        return Err(GateError::Tool(format!(
            "cargo llvm-cov --package {package_name} (isolated report) failed:\n{stderr}\n{stdout}"
        )));
    }

    let map = parse_llvm_cov_json(output.stdout.as_slice())?;
    package_line_percent(&map, package_name).ok_or_else(|| {
        GateError::Tool(format!(
            "no line coverage data for package {package_name} after isolated llvm-cov run \
             (required for min_line_coverage; refuse to greenwash on missing parse)"
        ))
    })
}

/// Prefer path-aggregated package key; accept workspace total (`*`) only when
/// the map has no other package keys (report already isolated).
pub fn package_line_percent(map: &BTreeMap<String, f64>, package_name: &str) -> Option<f64> {
    if let Some(p) = map.get(package_name) {
        return Some(*p);
    }
    let has_other_packages = map.keys().any(|k| k.as_str() != "*");
    if !has_other_packages {
        return map.get("*").copied();
    }
    None
}

/// Ignore monorepo paths that are not this package's `packages/<dir>/` tree.
///
/// `remotelink-auth` → ignore `packages/(?!auth/)`, `apps/`, `agents/`.
fn ignore_filename_regex_for_package(package_name: &str) -> Option<String> {
    let dir = package_name.strip_prefix("remotelink-")?;
    if dir.is_empty() || dir.contains(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
    {
        return None;
    }
    // Drop other packages/* plus apps/agents so only this package's sources count.
    Some(format!(r"(^|/)(packages/(?!{dir}/)|apps/|agents/)"))
}

/// Parse cargo-llvm-cov JSON summary into package → line % map.
pub fn parse_llvm_cov_json(bytes: &[u8]) -> Result<BTreeMap<String, f64>, GateError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| GateError::Json(e.to_string()))?;
    parse_llvm_cov_value(&v)
}

fn parse_llvm_cov_value(v: &Value) -> Result<BTreeMap<String, f64>, GateError> {
    let mut map = BTreeMap::new();

    // cargo-llvm-cov --json uses llvm-cov export format with "data" array, or
    // a simplified summary. Support several shapes.

    // Shape A: { "data": [ { "files": [...], "totals": { "lines": { "percent": N } } } ] }
    // Shape B: summary with per-crate keys under "dependencies" / custom.
    // Shape C: cargo-llvm-cov text-as-json with "files" at top level.

    if let Some(data) = v.get("data").and_then(|d| d.as_array()) {
        // Prefer file paths → package mapping is hard; use totals when single package
        // and also scan files for path segments matching package dirs.
        for entry in data {
            if let Some(files) = entry.get("files").and_then(|f| f.as_array()) {
                // Aggregate lines covered/count per path prefix packages/<name>/
                let mut agg: BTreeMap<String, (u64, u64)> = BTreeMap::new();
                for file in files {
                    let filename = file.get("filename").and_then(|s| s.as_str()).unwrap_or("");
                    let lines = file.get("summary").and_then(|s| s.get("lines"));
                    let count = lines
                        .and_then(|l| l.get("count"))
                        .and_then(|c| c.as_u64())
                        .unwrap_or(0);
                    let covered = lines
                        .and_then(|l| l.get("covered"))
                        .and_then(|c| c.as_u64())
                        .unwrap_or(0);
                    if let Some(pkg) = package_name_from_source_path(filename) {
                        let e = agg.entry(pkg).or_insert((0, 0));
                        e.0 = e.0.saturating_add(covered);
                        e.1 = e.1.saturating_add(count);
                    }
                }
                for (pkg, (covered, count)) in agg {
                    if count > 0 {
                        let pct = (covered as f64) * 100.0 / (count as f64);
                        map.insert(pkg, pct);
                    }
                }
            }

            // Workspace totals as fallback under key "*"
            if let Some(pct) = entry
                .get("totals")
                .and_then(|t| t.get("lines"))
                .and_then(|l| l.get("percent"))
                .and_then(|p| p.as_f64())
            {
                map.entry("*".into()).or_insert(pct);
            }
        }
    }

    // Shape D: { "cargo_llvm_cov": { "crates": { "name": { "lines": { "percent": N }}}}}
    if let Some(crates) = v
        .pointer("/cargo_llvm_cov/crates")
        .and_then(|c| c.as_object())
    {
        for (name, info) in crates {
            if let Some(pct) = info.pointer("/lines/percent").and_then(|p| p.as_f64()) {
                map.insert(name.clone(), pct);
            }
        }
    }

    // Shape E: top-level "files" with percentages (some versions)
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        let mut agg: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for file in files {
            let filename = file
                .get("filename")
                .or_else(|| file.get("name"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let percent = file
                .get("summary")
                .and_then(|s| s.get("lines"))
                .and_then(|l| l.get("percent"))
                .and_then(|p| p.as_f64());
            let count = file
                .get("summary")
                .and_then(|s| s.get("lines"))
                .and_then(|l| l.get("count"))
                .and_then(|c| c.as_u64());
            let covered = file
                .get("summary")
                .and_then(|s| s.get("lines"))
                .and_then(|l| l.get("covered"))
                .and_then(|c| c.as_u64());
            if let Some(pkg) = package_name_from_source_path(filename) {
                if let (Some(c), Some(n)) = (covered, count) {
                    let e = agg.entry(pkg).or_insert((0, 0));
                    e.0 = e.0.saturating_add(c);
                    e.1 = e.1.saturating_add(n);
                } else if let Some(pct) = percent {
                    // Single-file package approximation
                    map.entry(pkg).or_insert(pct);
                }
            }
        }
        for (pkg, (covered, count)) in agg {
            if count > 0 {
                map.insert(pkg, (covered as f64) * 100.0 / (count as f64));
            }
        }
    }

    Ok(map)
}

/// Heuristic: map a source path to a Cargo package name used in this monorepo.
fn package_name_from_source_path(path: &str) -> Option<String> {
    let norm = path.replace('\\', "/");
    // packages/<dir>/...
    if let Some(idx) = norm.find("packages/") {
        let rest = &norm[idx + "packages/".len()..];
        let dir = rest.split('/').next().unwrap_or("");
        if !dir.is_empty() {
            return Some(format!("remotelink-{dir}"));
        }
    }
    // agents/<dir>/...
    if let Some(idx) = norm.find("agents/") {
        let rest = &norm[idx + "agents/".len()..];
        let dir = rest.split('/').next().unwrap_or("");
        if !dir.is_empty() {
            return Some(dir.replace('_', "-"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::PackageGate;

    #[test]
    fn mode_parse() {
        assert_eq!(GateMode::parse("tests"), Some(GateMode::Tests));
        assert_eq!(GateMode::parse("llvm-cov"), Some(GateMode::LlvmCov));
        assert_eq!(GateMode::parse("auto"), Some(GateMode::Auto));
        assert_eq!(GateMode::parse("nope"), None);
    }

    #[test]
    fn resolve_auto() {
        assert_eq!(resolve_mode(GateMode::Auto, true), GateMode::LlvmCov);
        assert_eq!(resolve_mode(GateMode::Auto, false), GateMode::Tests);
        assert_eq!(resolve_mode(GateMode::Tests, true), GateMode::Tests);
    }

    #[test]
    fn evaluate_pass_tests_only() {
        let gate = PackageGate {
            name: "remotelink-common".into(),
            min_tests: 2,
            min_line_coverage: Some(90.0),
        };
        let r = evaluate_package(&gate, 2, None, false);
        assert!(r.passed(), "{:?}", r.failures);
    }

    #[test]
    fn evaluate_fail_low_tests() {
        let gate = PackageGate {
            name: "remotelink-common".into(),
            min_tests: 5,
            min_line_coverage: None,
        };
        let r = evaluate_package(&gate, 2, None, false);
        assert!(!r.passed());
        assert!(r.failures[0].contains("test presence"));
    }

    #[test]
    fn evaluate_fail_low_coverage() {
        let gate = PackageGate {
            name: "remotelink-common".into(),
            min_tests: 1,
            min_line_coverage: Some(90.0),
        };
        let r = evaluate_package(&gate, 10, Some(80.0), true);
        assert!(!r.passed());
        assert!(r.failures.iter().any(|f| f.contains("line coverage")));
    }

    #[test]
    fn evaluate_pass_coverage() {
        let gate = PackageGate {
            name: "remotelink-common".into(),
            min_tests: 1,
            min_line_coverage: Some(90.0),
        };
        let r = evaluate_package(&gate, 10, Some(95.0), true);
        assert!(r.passed(), "{:?}", r.failures);
    }

    #[test]
    fn package_name_from_path() {
        assert_eq!(
            package_name_from_source_path("/work/packages/auth/src/lib.rs").as_deref(),
            Some("remotelink-auth")
        );
        assert_eq!(
            package_name_from_source_path(r"C:\x\packages\protocol\src\m.rs").as_deref(),
            Some("remotelink-protocol")
        );
        assert_eq!(
            package_name_from_source_path("/work/agents/coverage-gate/src/lib.rs").as_deref(),
            Some("coverage-gate")
        );
    }

    #[test]
    fn parse_llvm_json_files_shape() {
        let json = r#"{
          "data": [{
            "files": [
              {
                "filename": "/repo/packages/common/src/lib.rs",
                "summary": { "lines": { "count": 100, "covered": 95, "percent": 95.0 } }
              },
              {
                "filename": "/repo/packages/auth/src/lib.rs",
                "summary": { "lines": { "count": 200, "covered": 180, "percent": 90.0 } }
              }
            ],
            "totals": { "lines": { "percent": 92.0 } }
          }]
        }"#;
        let map = parse_llvm_cov_json(json.as_bytes()).unwrap();
        assert!((map["remotelink-common"] - 95.0).abs() < 0.01);
        assert!((map["remotelink-auth"] - 90.0).abs() < 0.01);
        assert!((map["*"] - 92.0).abs() < 0.01);
    }

    #[test]
    fn package_line_percent_prefers_named_key() {
        let mut map = BTreeMap::new();
        map.insert("remotelink-auth".into(), 91.0);
        map.insert("*".into(), 50.0);
        map.insert("remotelink-common".into(), 99.0);
        assert_eq!(package_line_percent(&map, "remotelink-auth"), Some(91.0));
        // Mixed report without named key → refuse (would greenwash on "*")
        map.remove("remotelink-auth");
        assert_eq!(package_line_percent(&map, "remotelink-auth"), None);
    }

    #[test]
    fn package_line_percent_accepts_star_when_isolated() {
        let mut map = BTreeMap::new();
        map.insert("*".into(), 93.5);
        assert_eq!(package_line_percent(&map, "remotelink-auth"), Some(93.5));
    }

    #[test]
    fn ignore_regex_for_remotelink_packages() {
        let re = ignore_filename_regex_for_package("remotelink-auth").unwrap();
        assert!(re.contains("packages/(?!auth/)"), "{re}");
        assert!(re.contains("apps/"), "{re}");
        assert!(ignore_filename_regex_for_package("coverage-gate").is_none());
    }

    #[test]
    fn json_report_records_absolute_policy() {
        let r = GateReport {
            effective_mode: GateMode::LlvmCov,
            llvm_cov_available: true,
            packages: vec![PackageResult {
                name: "remotelink-common".into(),
                test_count: 2,
                min_tests: 2,
                line_coverage: Some(100.0),
                min_line_coverage: Some(90.0),
                failures: vec![],
            }],
        };
        let j = r.to_json_report();
        assert!(j.passed);
        assert!(j.policy.contains("absolute floors"));
        assert!(j.policy.contains("deferred"));
        assert_eq!(j.packages[0].line_coverage, Some(100.0));
    }

    #[test]
    fn report_render_empty() {
        let r = GateReport {
            effective_mode: GateMode::Tests,
            llvm_cov_available: false,
            packages: vec![],
        };
        let s = r.render();
        assert!(s.contains("fail-open"));
        assert!(r.passed());
    }

    #[test]
    fn report_render_fail() {
        let r = GateReport {
            effective_mode: GateMode::Tests,
            llvm_cov_available: false,
            packages: vec![PackageResult {
                name: "remotelink-common".into(),
                test_count: 0,
                min_tests: 1,
                line_coverage: None,
                min_line_coverage: Some(90.0),
                failures: vec!["test presence: found 0 test(s), require ≥ 1".into()],
            }],
        };
        assert!(!r.passed());
        let s = r.render();
        assert!(s.contains("FAIL"));
        assert!(s.contains("remotelink-common"));
    }
}

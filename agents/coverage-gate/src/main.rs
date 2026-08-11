//! CLI for per-package coverage gates.
//!
//! ```text
//! coverage-gate check [--workspace PATH] [--allowlist PATH] [--mode tests|llvm-cov|auto]
//!                     [--json-out PATH]
//! coverage-gate validate [--allowlist PATH]
//! ```
//!
//! Fail-closed only for packages listed in the allowlist.

use coverage_gate::{run_gates, Allowlist, GateMode};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return ExitCode::FAILURE;
    }

    let cmd = args.remove(0);
    match cmd.as_str() {
        "check" => match cmd_check(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        "validate" => match cmd_validate(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        "help" | "-h" | "--help" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "\
coverage-gate — per-package coverage / test-presence gates

Fail-closed only for packages listed in agents/shared/allowlist.toml.
Packages not listed are ignored.

MODES:
  tests     Test-presence gate (min_tests). Works on all platforms.
  llvm-cov  Line coverage via cargo-llvm-cov (Linux CI; needs llvm-tools).
  auto      llvm-cov when available, otherwise tests (default).

NOTE:
  Full llvm-cov line coverage is intended for Linux CI. On Windows, install
  is optional; the test-presence gate is the portable enforcement path.

USAGE:
  coverage-gate check [--workspace PATH] [--allowlist PATH] [--mode MODE] [--json-out PATH]
      Evaluate gates. Exit 0 if all gated packages pass; 1 otherwise.
      Default --workspace: current directory
      Default --allowlist: agents/shared/allowlist.toml (relative to workspace)
      Default --mode: auto
      --json-out: write machine-readable absolute-floor results (CI artifact)

  coverage-gate validate [--allowlist PATH]
      Parse and validate the allowlist only (no package scan).

  coverage-gate help
"
    );
}

fn cmd_check(args: &[String]) -> Result<ExitCode, String> {
    let workspace = match flag_value(args, "--workspace") {
        Some(p) => PathBuf::from(p),
        None => env::current_dir().map_err(|e| e.to_string())?,
    };

    let allowlist_path = flag_value(args, "--allowlist")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("agents/shared/allowlist.toml"));

    let mode = match flag_value(args, "--mode") {
        Some(s) => GateMode::parse(&s)
            .ok_or_else(|| format!("invalid --mode {s:?} (expected tests|llvm-cov|auto)"))?,
        None => GateMode::Auto,
    };

    let json_out = flag_value(args, "--json-out").map(PathBuf::from);

    let allowlist = Allowlist::load(&allowlist_path).map_err(|e| e.to_string())?;
    let report = run_gates(&workspace, &allowlist, mode).map_err(|e| e.to_string())?;
    print!("{}", report.render());

    if let Some(path) = json_out {
        let body = serde_json::to_string_pretty(&report.to_json_report())
            .map_err(|e| format!("json serialize: {e}"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
        }
        fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
        eprintln!("wrote {}", path.display());
    }

    if report.passed() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn cmd_validate(args: &[String]) -> Result<(), String> {
    let allowlist_path = flag_value(args, "--allowlist")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("agents/shared/allowlist.toml")
        });

    let al = Allowlist::load(&allowlist_path).map_err(|e| e.to_string())?;
    println!(
        "allowlist ok: schema_version={} packages={} gaps={}",
        al.schema_version,
        al.package.len(),
        al.gap.len()
    );
    for p in &al.package {
        match p.min_line_coverage {
            Some(c) => println!(
                "  - {}  min_tests={}  min_line_coverage={c:.1}",
                p.name, p.min_tests
            ),
            None => println!("  - {}  min_tests={}", p.name, p.min_tests),
        }
    }
    Ok(())
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag {
            return args.get(i + 1).cloned();
        }
        if let Some(rest) = args[i].strip_prefix(&format!("{flag}=")) {
            return Some(rest.to_string());
        }
        i += 1;
    }
    None
}

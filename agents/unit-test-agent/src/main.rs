//! CLI for the Unit-Test Agent.
//!
//! ```text
//! unit-test-agent inventory [--workspace PATH] [--out PATH]
//! unit-test-agent draft-report [--inventory PATH] [--out PATH]
//! ```
//!
//! **Never auto-merge.** Draft reports only. CI must run checked-in tests only.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use unit_test_agent::{draft_report_markdown, inventory_workspace, Inventory};

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return ExitCode::FAILURE;
    }

    let cmd = args.remove(0);
    match cmd.as_str() {
        "inventory" => match cmd_inventory(&args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        "draft-report" => match cmd_draft_report(&args) {
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
unit-test-agent — public-API inventory + draft test PR reports

POLICY:
  • NEVER auto-merge agent output
  • Draft PRs only (label needs-human-review)
  • CI runs checked-in tests only — never execute unreviewed generated code

USAGE:
  unit-test-agent inventory [--workspace PATH] [--out PATH]
      Scan workspace library crates for public items.
      Default --workspace: current directory
      Default --out: agents/shared/inventory.json

  unit-test-agent draft-report [--inventory PATH] [--out PATH]
      Emit markdown of packages lacking tests (and related gaps).
      Default --inventory: agents/shared/inventory.json
      Default --out: stdout (-)

  unit-test-agent help
"
    );
}

fn cmd_inventory(args: &[String]) -> Result<(), String> {
    let workspace = flag_value(args, "--workspace")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().expect("cwd"));
    let out = flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("agents/shared/inventory.json"));

    let inv = inventory_workspace(&workspace).map_err(|e| e.to_string())?;
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(&inv).map_err(|e| e.to_string())?;
    fs::write(&out, json + "\n").map_err(|e| e.to_string())?;

    eprintln!(
        "wrote {} ({} items, {} packages)",
        out.display(),
        inv.items.len(),
        inv.packages.len()
    );
    Ok(())
}

fn cmd_draft_report(args: &[String]) -> Result<(), String> {
    let inventory_path = flag_value(args, "--inventory")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("agents/shared/inventory.json"));
    let out = flag_value(args, "--out").unwrap_or_else(|| "-".into());

    let text = fs::read_to_string(&inventory_path).map_err(|e| {
        format!(
            "read {}: {e} (run `unit-test-agent inventory` first)",
            inventory_path.display()
        )
    })?;
    let inv: Inventory = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let md = draft_report_markdown(&inv);

    if out == "-" {
        print!("{md}");
    } else {
        let path = Path::new(&out);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, md).map_err(|e| e.to_string())?;
        eprintln!("wrote {}", path.display());
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

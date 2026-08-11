//! CLI for the Bug-Hunt Agent.
//!
//! ```text
//! bug-hunt-agent nightly [--out PATH] [--config PATH] [--seed N]
//! bug-hunt-agent run --profile NAME [--out PATH] [--config PATH] [--seed N]
//! bug-hunt-agent list
//! ```
//!
//! Profiles run **without real network** (mock peers + in-process skew).
//! Artifacts: JSON per profile + `summary.json` + `nightly-report.md`.
//!
//! # Seed / repro
//!
//! - Config `[chaos].seed` is the **root** seed.
//! - Multi-profile (`nightly` / `--profile all`) **derives** a per-profile
//!   effective seed; single-profile runs use the root seed as-is.
//! - `--seed N` always sets the **effective** seed and **never derives**
//!   (use this to replay an artifact: paste `seed` / `effective_seed`).

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use bug_hunt_agent::{default_config, load_config, run_profiles, ProfileName, RunOptions};

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return ExitCode::FAILURE;
    }

    let cmd = args.remove(0);
    match cmd.as_str() {
        "nightly" => match cmd_nightly(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        "run" => match cmd_run(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        "list" => {
            println!("Available chaos / fuzz profiles (no real network):\n");
            for p in ProfileName::all() {
                let desc = match p {
                    ProfileName::DropPackets => "simulate loss via random skip on mock peer sends",
                    ProfileName::Reconnect => "session teardown + restart unit harness (mock pair)",
                    ProfileName::AudioDesync => "force A/V skew injection through SkewController",
                    ProfileName::ProtocolFuzz => {
                        "hand-fuzz protocol decode with random/mutated bytes"
                    }
                };
                println!("  {:<16}  {desc}", p.as_str());
            }
            ExitCode::SUCCESS
        }
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
bug-hunt-agent — chaos profiles, protocol fuzz, nightly artifacts

POLICY:
  • No LLM required for core nightly
  • No real network (mock PeerTransport + in-process skew)
  • Artifacts for human review; never auto-merge

USAGE:
  bug-hunt-agent nightly [--out PATH] [--config PATH] [--seed N]
      Run all enabled profiles and write summary + report.
      Default --out: agents/shared/artifacts (if present) else target/chaos
      Default --config: agents/shared/bug_hunt_config.toml (optional)
      Multi-profile: derives effective seed per profile from [chaos].seed
      --seed N: force effective seed for all profiles (no derivation)

  bug-hunt-agent run --profile NAME [--out PATH] [--config PATH] [--seed N]
      Run one profile (or --profile all).
      Profiles: drop_packets | reconnect | audio_desync | protocol_fuzz | all
      Single profile: config seed is effective as-is (no derivation).
      --seed N: effective seed as-is (repro path for artifact seed field).

  bug-hunt-agent list
      List profiles.

  bug-hunt-agent help

REPRO (from artifact):
  # seed / effective_seed in <profile>.json is already derived for nightly
  bug-hunt-agent run --profile protocol_fuzz --seed <effective_seed>

ARTIFACTS:
  <out>/<profile>.json   root_seed + effective seed + metrics + repro
  <out>/summary.json     aggregate
  <out>/nightly-report.md
"
    );
}

fn cmd_nightly(args: &[String]) -> Result<ExitCode, String> {
    let out = out_dir(args);
    let config = load_config_from_args(args)?;
    let effective_seed = parse_seed_flag(args)?;
    eprintln!("bug-hunt-agent nightly → {}", out.display());
    let opts = RunOptions {
        out_dir: out.clone(),
        profiles: vec![], // resolve from config → all / enabled
        config,
        effective_seed,
    };
    // Empty profiles → resolve_profiles uses config enabled or all.
    // But RunOptions with empty profiles is correct via resolve_profiles.
    let report = run_profiles(&opts)?;
    eprintln!(
        "done: {}/{} passed, {} failed (artifacts in {})",
        report.passed,
        report.total,
        report.failed,
        out.display()
    );
    Ok(if report.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn cmd_run(args: &[String]) -> Result<ExitCode, String> {
    let profile_s = flag_value(args, "--profile").ok_or_else(|| {
        String::from(
            "missing --profile NAME (drop_packets|reconnect|audio_desync|protocol_fuzz|all)",
        )
    })?;
    let profiles = if profile_s == "all" {
        ProfileName::all().to_vec()
    } else {
        let p = ProfileName::parse(&profile_s)
            .ok_or_else(|| format!("unknown profile: {profile_s}"))?;
        vec![p]
    };
    let out = out_dir(args);
    let config = load_config_from_args(args)?;
    let effective_seed = parse_seed_flag(args)?;
    let opts = RunOptions {
        out_dir: out.clone(),
        profiles,
        config,
        effective_seed,
    };
    eprintln!(
        "bug-hunt-agent run → {} ({})",
        out.display(),
        opts.profiles
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let report = run_profiles(&opts)?;
    for o in &report.outcomes {
        eprintln!(
            "  {}  {}  effective_seed={}  {}",
            o.profile.as_str(),
            o.status.as_str(),
            o.seed,
            o.summary
        );
    }
    Ok(if report.failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn out_dir(args: &[String]) -> PathBuf {
    flag_value(args, "--out")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Prefer agents/shared/artifacts when run from repo root; else target/chaos.
            let shared = PathBuf::from("agents/shared/artifacts");
            if PathBuf::from("agents/shared").is_dir() {
                shared
            } else {
                PathBuf::from("target/chaos")
            }
        })
}

fn load_config_from_args(args: &[String]) -> Result<bug_hunt_agent::BugHuntConfig, String> {
    if let Some(p) = flag_value(args, "--config") {
        return load_config(&PathBuf::from(p));
    }
    let default = PathBuf::from("agents/shared/bug_hunt_config.toml");
    if default.exists() {
        load_config(&default)
    } else {
        Ok(default_config())
    }
}

fn parse_seed_flag(args: &[String]) -> Result<Option<u64>, String> {
    match flag_value(args, "--seed") {
        None => Ok(None),
        Some(s) => s
            .parse::<u64>()
            .map(Some)
            .map_err(|e| format!("invalid --seed {s}: {e}")),
    }
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

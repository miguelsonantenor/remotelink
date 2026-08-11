//! Orchestrate profile runs and produce nightly reports.
//!
//! # Seed model (repro contract)
//!
//! - **`root_seed`**: `[chaos].seed` from config (or default). Recorded on every
//!   artifact as `root_seed`.
//! - **`effective_seed`**: value actually fed to the profile RNG. Recorded as
//!   `seed` / `effective_seed` in artifacts and `repro`.
//!
//! Resolution ([`resolve_effective_seed`]):
//!
//! 1. If [`RunOptions::effective_seed`] is `Some` (CLI `--seed`), use it **as-is**
//!    for every profile — **never derive** (this is the repro path).
//! 2. Else if running **more than one** profile (`nightly` / `--profile all`),
//!    `effective = derive_seed(root, profile)` so streams stay independent.
//! 3. Else (**single** named profile), `effective = root` — config seed is used
//!    directly so pasting an artifact `seed` into config or `--seed` replays.
//!
//! **Repro a nightly profile:**  
//! `bug-hunt-agent run --profile protocol_fuzz --seed <artifact.seed>`

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::artifact::{write_profile_artifact, write_summary, ArtifactPaths};
use crate::config::{BugHuntConfig, ChaosProfileConfig, ProfileName};
use crate::profiles::{run_profile_catch_unwind, ProfileOutcome, ProfileStatus};

/// Options for a single agent invocation.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Output directory for artifacts.
    pub out_dir: PathBuf,
    /// Profiles to run (empty = all, or config-enabled).
    pub profiles: Vec<ProfileName>,
    /// Config (seed, iterations, …).
    pub config: BugHuntConfig,
    /// When set (CLI `--seed`), used as the **effective** seed for every profile
    /// with **no** derivation. Prefer this for artifact repro.
    pub effective_seed: Option<u64>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::from("target/chaos"),
            profiles: ProfileName::all().to_vec(),
            config: BugHuntConfig::default(),
            effective_seed: None,
        }
    }
}

/// Aggregate nightly report written to `summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NightlyReport {
    /// Unix seconds.
    pub generated_at_unix: u64,
    /// Root seed from config.
    pub seed: u64,
    /// Output directory.
    pub out_dir: String,
    /// Total profiles attempted.
    pub total: u32,
    /// Pass count.
    pub passed: u32,
    /// Fail count.
    pub failed: u32,
    /// Skip count.
    pub skipped: u32,
    /// Per-profile outcomes.
    pub outcomes: Vec<ProfileOutcome>,
}

/// Resolve which profiles to run from CLI list + config.
pub fn resolve_profiles(requested: &[ProfileName], config: &BugHuntConfig) -> Vec<ProfileName> {
    if !requested.is_empty() {
        return requested.to_vec();
    }
    if !config.enabled_profiles.is_empty() {
        return config
            .enabled_profiles
            .iter()
            .filter_map(|s| ProfileName::parse(s))
            .collect();
    }
    ProfileName::all().to_vec()
}

/// Resolve the effective seed for one profile under the run options.
///
/// See module docs for the three-way policy (explicit / multi-derive / single-as-is).
pub fn resolve_effective_seed(
    root_seed: u64,
    profile: ProfileName,
    profile_count: usize,
    explicit_effective: Option<u64>,
) -> u64 {
    if let Some(s) = explicit_effective {
        return s;
    }
    if profile_count > 1 {
        derive_seed(root_seed, profile)
    } else {
        // Single profile: do not derive — root is already the effective seed.
        root_seed
    }
}

/// Stable per-profile mix of a root seed (multi-profile / nightly only).
pub fn derive_seed(root: u64, name: ProfileName) -> u64 {
    let tag = name
        .as_str()
        .bytes()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)));
    root.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(tag)
}

/// Run selected profiles and write artifacts. Returns the report.
pub fn run_profiles(opts: &RunOptions) -> Result<NightlyReport, String> {
    let paths = ArtifactPaths::prepare(&opts.out_dir)?;
    let profiles = resolve_profiles(&opts.profiles, &opts.config);
    let root_seed = opts.config.chaos.seed;
    let chaos = &opts.config.chaos;
    let n = profiles.len();

    let mut outcomes = Vec::with_capacity(n);
    for name in profiles {
        let effective = resolve_effective_seed(root_seed, name, n, opts.effective_seed);
        let mut cfg = chaos.clone();
        cfg.seed = effective;
        let outcome = run_profile_catch_unwind(name, &cfg).with_seeds(root_seed, effective);
        write_profile_artifact(&paths, &outcome)?;
        outcomes.push(outcome);
    }

    let report = build_report(&opts.out_dir, chaos, outcomes);
    write_summary(&paths, &report)?;
    Ok(report)
}

/// Nightly alias: all enabled profiles + summary (always multi-profile derivation).
pub fn run_nightly(out_dir: &Path, config: BugHuntConfig) -> Result<NightlyReport, String> {
    let opts = RunOptions {
        out_dir: out_dir.to_path_buf(),
        profiles: resolve_profiles(&[], &config),
        config,
        effective_seed: None,
    };
    run_profiles(&opts)
}

fn build_report(
    out_dir: &Path,
    chaos: &ChaosProfileConfig,
    outcomes: Vec<ProfileOutcome>,
) -> NightlyReport {
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut skipped = 0u32;
    for o in &outcomes {
        match o.status {
            ProfileStatus::Pass => passed += 1,
            ProfileStatus::Fail => failed += 1,
            ProfileStatus::Skip => skipped += 1,
        }
    }
    let total = outcomes.len() as u32;
    NightlyReport {
        generated_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        seed: chaos.seed,
        out_dir: out_dir.display().to_string(),
        total,
        passed,
        failed,
        skipped,
        outcomes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::run_protocol_fuzz;

    #[test]
    fn nightly_writes_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = BugHuntConfig::default();
        config.chaos.iterations = 32;
        config.chaos.reconnect_cycles = 1;
        let report = run_nightly(dir.path(), config).unwrap();
        assert_eq!(report.total, ProfileName::all().len() as u32);
        assert_eq!(report.failed, 0, "failures: {:?}", report.outcomes);
        assert!(dir.path().join("summary.json").exists());
        assert!(dir.path().join("nightly-report.md").exists());
        assert!(dir.path().join("drop_packets.json").exists());
        assert!(dir.path().join("reconnect.json").exists());
        assert!(dir.path().join("audio_desync.json").exists());
        assert!(dir.path().join("protocol_fuzz.json").exists());
    }

    #[test]
    fn resolve_profiles_from_config() {
        let cfg = BugHuntConfig {
            enabled_profiles: vec!["reconnect".into(), "audio_desync".into()],
            ..Default::default()
        };
        let p = resolve_profiles(&[], &cfg);
        assert_eq!(p, vec![ProfileName::Reconnect, ProfileName::AudioDesync]);
    }

    #[test]
    fn multi_profile_derives_single_does_not() {
        let root = 184853989u64;
        let multi = resolve_effective_seed(root, ProfileName::ProtocolFuzz, 4, None);
        assert_ne!(multi, root);
        assert_eq!(multi, derive_seed(root, ProfileName::ProtocolFuzz));

        let single = resolve_effective_seed(root, ProfileName::ProtocolFuzz, 1, None);
        assert_eq!(single, root);

        // Explicit --seed never derives even in multi context.
        let explicit =
            resolve_effective_seed(root, ProfileName::ProtocolFuzz, 4, Some(0xDEAD_BEEF));
        assert_eq!(explicit, 0xDEAD_BEEF);
    }

    #[test]
    fn artifact_effective_seed_repros_without_double_derive() {
        let root = 184853989u64;
        let dir1 = tempfile::tempdir().unwrap();
        let mut config = BugHuntConfig::default();
        config.chaos.seed = root;
        config.chaos.iterations = 64;
        config.chaos.reconnect_cycles = 1;

        // Multi-profile nightly derives per profile.
        let report = run_nightly(dir1.path(), config.clone()).unwrap();
        let fuzz = report
            .outcomes
            .iter()
            .find(|o| o.profile == ProfileName::ProtocolFuzz)
            .expect("protocol_fuzz");
        let effective = fuzz.seed;
        assert_eq!(fuzz.root_seed, Some(root));
        assert_eq!(effective, derive_seed(root, ProfileName::ProtocolFuzz));
        assert_ne!(
            effective, root,
            "nightly must derive so multi-profile streams differ"
        );

        // Repro path: single profile + --seed effective (or config seed = effective).
        let dir2 = tempfile::tempdir().unwrap();
        let mut repro_cfg = config.clone();
        repro_cfg.chaos.seed = effective; // if someone pastes into config for single run
        let opts = RunOptions {
            out_dir: dir2.path().to_path_buf(),
            profiles: vec![ProfileName::ProtocolFuzz],
            config: repro_cfg,
            effective_seed: Some(effective), // CLI --seed; must not derive again
        };
        let report2 = run_profiles(&opts).unwrap();
        let fuzz2 = &report2.outcomes[0];
        assert_eq!(fuzz2.seed, effective, "must not double-derive");
        assert_eq!(
            fuzz2.metrics, fuzz.metrics,
            "RNG stream must match original"
        );

        // Also: single profile with config.seed = effective and no --seed.
        let dir3 = tempfile::tempdir().unwrap();
        let opts3 = RunOptions {
            out_dir: dir3.path().to_path_buf(),
            profiles: vec![ProfileName::ProtocolFuzz],
            config: BugHuntConfig {
                chaos: ChaosProfileConfig {
                    seed: effective,
                    iterations: 64,
                    ..ChaosProfileConfig::default()
                },
                ..Default::default()
            },
            effective_seed: None,
        };
        let report3 = run_profiles(&opts3).unwrap();
        assert_eq!(report3.outcomes[0].seed, effective);
        assert_eq!(report3.outcomes[0].metrics, fuzz.metrics);

        // Sanity: direct profile API with effective seed matches too.
        let direct = ChaosProfileConfig {
            seed: effective,
            iterations: 64,
            ..ChaosProfileConfig::default()
        };
        let o = run_protocol_fuzz(&direct);
        assert_eq!(o.metrics, fuzz.metrics);
    }

    #[test]
    fn double_derive_would_break_repro_documented() {
        // Guard: if someone pastes effective into multi-profile path without --seed,
        // derivation happens again — that is intentional for nightly isolation.
        let root = 184853989u64;
        let once = derive_seed(root, ProfileName::ProtocolFuzz);
        let twice = derive_seed(once, ProfileName::ProtocolFuzz);
        assert_ne!(once, twice);
        // The fix is: repro uses single-profile or --seed, which skips derive.
        assert_eq!(
            resolve_effective_seed(once, ProfileName::ProtocolFuzz, 1, None),
            once
        );
        assert_eq!(
            resolve_effective_seed(root, ProfileName::ProtocolFuzz, 4, Some(once)),
            once
        );
    }
}

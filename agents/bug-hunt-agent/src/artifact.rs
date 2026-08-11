//! Artifact writers for chaos / fuzz profile runs.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{ProfileName, Severity};
use crate::profiles::{ProfileOutcome, ProfileStatus};
use crate::runner::NightlyReport;

/// Paths used for a single run's outputs.
#[derive(Debug, Clone)]
pub struct ArtifactPaths {
    /// Root directory (e.g. `target/chaos` or `agents/shared/artifacts`).
    pub root: PathBuf,
}

impl ArtifactPaths {
    /// Create dirs under `root` if needed.
    pub fn prepare(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
        Ok(Self { root })
    }

    /// Path for a profile JSON artifact.
    pub fn profile_json(&self, name: ProfileName) -> PathBuf {
        self.root.join(format!("{}.json", name.as_str()))
    }

    /// Nightly summary JSON.
    pub fn summary_json(&self) -> PathBuf {
        self.root.join("summary.json")
    }

    /// Nightly markdown report.
    pub fn report_md(&self) -> PathBuf {
        self.root.join("nightly-report.md")
    }
}

/// Serialized per-profile artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileArtifact {
    /// Profile id.
    pub profile: String,
    /// Wall clock unix seconds when written.
    pub written_at_unix: u64,
    /// Config / nightly root seed (before derivation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_seed: Option<u64>,
    /// **Effective** seed used for this profile's RNG.
    ///
    /// Repro: `bug-hunt-agent run --profile <name> --seed <seed>`
    /// (do **not** re-derive; `--seed` is always as-is).
    pub seed: u64,
    /// Alias for `seed` (same value) so repro docs can say `effective_seed`.
    pub effective_seed: u64,
    /// Pass / fail / skip.
    pub status: String,
    /// Severity if a defect was found (or `info` on clean pass).
    pub severity: String,
    /// Human summary.
    pub summary: String,
    /// Structured metrics (profile-specific).
    pub metrics: serde_json::Value,
    /// Optional repro notes / fingerprint.
    pub repro: Option<String>,
}

impl ProfileArtifact {
    /// Build from a profile outcome.
    pub fn from_outcome(outcome: &ProfileOutcome) -> Self {
        let severity = match outcome.status {
            ProfileStatus::Pass => Severity::Info,
            ProfileStatus::Fail => outcome.severity.unwrap_or(Severity::High),
            ProfileStatus::Skip => Severity::Low,
        };
        Self {
            profile: outcome.profile.as_str().to_string(),
            written_at_unix: now_unix(),
            root_seed: outcome.root_seed,
            seed: outcome.seed,
            effective_seed: outcome.seed,
            status: outcome.status.as_str().to_string(),
            severity: severity.as_str().to_string(),
            summary: outcome.summary.clone(),
            metrics: outcome.metrics.clone(),
            repro: outcome.repro.clone(),
        }
    }
}

/// Write one profile JSON artifact.
pub fn write_profile_artifact(
    paths: &ArtifactPaths,
    outcome: &ProfileOutcome,
) -> Result<PathBuf, String> {
    let art = ProfileArtifact::from_outcome(outcome);
    let path = paths.profile_json(outcome.profile);
    let json = serde_json::to_string_pretty(&art).map_err(|e| e.to_string())?;
    fs::write(&path, json + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Write summary.json + nightly-report.md.
pub fn write_summary(paths: &ArtifactPaths, report: &NightlyReport) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report).map_err(|e| e.to_string())?;
    fs::write(paths.summary_json(), json + "\n").map_err(|e| format!("write summary: {e}"))?;

    let md = render_markdown(report);
    fs::write(paths.report_md(), md).map_err(|e| format!("write report: {e}"))?;
    Ok(())
}

fn render_markdown(report: &NightlyReport) -> String {
    let mut out = String::new();
    out.push_str("# Bug-hunt nightly report\n\n");
    out.push_str(&format!(
        "- **generated_at_unix**: {}\n",
        report.generated_at_unix
    ));
    out.push_str(&format!("- **root_seed**: {}\n", report.seed));
    out.push_str(&format!("- **out**: `{}`\n", report.out_dir));
    out.push_str(&format!(
        "- **passed**: {} / {}\n",
        report.passed, report.total
    ));
    out.push_str(&format!("- **failed**: {}\n\n", report.failed));
    out.push_str("| Profile | Status | Severity | Root seed | Effective seed | Summary |\n");
    out.push_str("|---------|--------|----------|-----------|----------------|---------|\n");
    for o in &report.outcomes {
        let sev = o
            .severity
            .map(|s| s.as_str())
            .unwrap_or(if o.status == ProfileStatus::Pass {
                "info"
            } else {
                "-"
            });
        let root = o
            .root_seed
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} | {} |\n",
            o.profile.as_str(),
            o.status.as_str(),
            sev,
            root,
            o.seed,
            o.summary.replace('|', "\\|")
        ));
    }
    out.push_str("\n## Repro\n\n");
    out.push_str("Use the **effective seed** with a **single** profile (no re-derivation):\n\n");
    out.push_str("```bash\n");
    out.push_str(
        "bug-hunt-agent run --profile <name> --seed <effective_seed> --out target/chaos-repro\n",
    );
    out.push_str("```\n\n");
    out.push_str("## Severity rubric\n\n");
    out.push_str("| Level | Meaning |\n|-------|--------|\n");
    out.push_str("| Critical | Auth bypass |\n");
    out.push_str("| High | Crash / remote inject without auth |\n");
    out.push_str("| Medium | A/V desync |\n");
    out.push_str("| Low | Cosmetic |\n");
    out.push_str("| Info | Clean pass |\n");
    out.push('\n');
    out
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::ProfileStatus;
    use serde_json::json;

    #[test]
    fn write_artifacts_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ArtifactPaths::prepare(dir.path()).unwrap();
        let outcome = ProfileOutcome {
            profile: ProfileName::DropPackets,
            root_seed: Some(9),
            seed: 1,
            status: ProfileStatus::Pass,
            severity: None,
            summary: "ok".into(),
            metrics: json!({"dropped": 3}),
            repro: Some("root_seed=9 effective_seed=1 profile=drop_packets".into()),
        };
        let p = write_profile_artifact(&paths, &outcome).unwrap();
        assert!(p.exists());
        let text = fs::read_to_string(p).unwrap();
        assert!(text.contains("drop_packets"));
        assert!(text.contains("\"effective_seed\": 1"));
        assert!(text.contains("\"root_seed\": 9"));
    }
}

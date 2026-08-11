//! Configuration for chaos profiles and severity rubric.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Named chaos / fuzz profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileName {
    /// Simulate packet loss via random skip on mock peer sends.
    DropPackets,
    /// Session teardown + restart on mock peer pair.
    Reconnect,
    /// Force A/V skew injection through [`remotelink_media::SkewController`].
    AudioDesync,
    /// Hand-fuzz protocol decode with random bytes (never panic).
    ProtocolFuzz,
}

impl ProfileName {
    /// Stable wire / CLI / filename id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DropPackets => "drop_packets",
            Self::Reconnect => "reconnect",
            Self::AudioDesync => "audio_desync",
            Self::ProtocolFuzz => "protocol_fuzz",
        }
    }

    /// Parse from CLI / config string.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "drop_packets" => Some(Self::DropPackets),
            "reconnect" => Some(Self::Reconnect),
            "audio_desync" => Some(Self::AudioDesync),
            "protocol_fuzz" => Some(Self::ProtocolFuzz),
            "all" => None, // handled by caller
            _ => None,
        }
    }

    /// All built-in profiles (order used by `nightly`).
    pub fn all() -> &'static [Self] {
        &[
            Self::DropPackets,
            Self::Reconnect,
            Self::AudioDesync,
            Self::ProtocolFuzz,
        ]
    }
}

impl std::fmt::Display for ProfileName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Severity rubric (DESIGN.md bug-hunt outputs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Auth bypass.
    Critical,
    /// Crash / remote inject without auth.
    High,
    /// Desync / media quality.
    Medium,
    /// Cosmetic.
    Low,
    /// Informational (profile completed, no defect).
    Info,
}

impl Severity {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
        }
    }
}

/// Per-profile knobs.
///
/// Unknown keys under `[chaos]` are rejected (`deny_unknown_fields`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChaosProfileConfig {
    /// Root RNG seed from config (`[chaos].seed`).
    ///
    /// See [`crate::runner::resolve_effective_seed`] for how this becomes the
    /// per-profile **effective** seed used by RNG profiles.
    pub seed: u64,
    /// Packets / iterations for drop and fuzz profiles.
    pub iterations: u32,
    /// Target drop rate for `drop_packets` (0.0–1.0).
    pub drop_rate: f64,
    /// Forced skew magnitude (ms) for `audio_desync`.
    pub skew_inject_ms: f64,
    /// Reconnect cycles for `reconnect`.
    pub reconnect_cycles: u32,
}

impl Default for ChaosProfileConfig {
    fn default() -> Self {
        Self {
            seed: 0xB_00B_1E5,
            iterations: 256,
            drop_rate: 0.25,
            skew_inject_ms: 80.0,
            reconnect_cycles: 3,
        }
    }
}

/// Top-level bug-hunt config (`agents/shared/bug_hunt_config.toml` shape).
///
/// File encoding: UTF-8 **without BOM**. A leading BOM is stripped if present.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BugHuntConfig {
    /// Default profile parameters.
    #[serde(default)]
    pub chaos: ChaosProfileConfig,
    /// Profiles enabled for `nightly` (empty = all).
    #[serde(default)]
    pub enabled_profiles: Vec<String>,
}

/// Built-in defaults (no file required).
pub fn default_config() -> BugHuntConfig {
    BugHuntConfig::default()
}

/// Load config from path.
///
/// - Missing file → defaults.
/// - `.json` → `serde_json` with the same schema.
/// - Otherwise → TOML via the `toml` crate (BOM-stripped UTF-8).
/// - Type errors (e.g. `seed = "nope"`) and unknown `[chaos]` keys are hard errors.
pub fn load_config(path: &Path) -> Result<BugHuntConfig, String> {
    if !path.exists() {
        return Ok(default_config());
    }
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let text = strip_bom(&text);
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
    {
        return serde_json::from_str(text)
            .map_err(|e| format!("parse json config {}: {e}", path.display()));
    }
    parse_toml(text).map_err(|e| format!("parse toml config {}: {e}", path.display()))
}

/// Parse TOML body (after BOM strip). Public for unit tests.
pub fn parse_toml(text: &str) -> Result<BugHuntConfig, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn profile_name_roundtrip() {
        for p in ProfileName::all() {
            assert_eq!(ProfileName::parse(p.as_str()), Some(*p));
        }
    }

    #[test]
    fn parse_toml_valid() {
        let text = r#"
# comment
enabled_profiles = ["drop_packets", "reconnect"]

[chaos]
seed = 42
iterations = 10
drop_rate = 0.5
skew_inject_ms = 40.0
reconnect_cycles = 2
"#;
        let cfg = parse_toml(text).expect("valid toml");
        assert_eq!(cfg.chaos.seed, 42);
        assert_eq!(cfg.chaos.iterations, 10);
        assert!((cfg.chaos.drop_rate - 0.5).abs() < 1e-9);
        assert_eq!(cfg.chaos.reconnect_cycles, 2);
        assert_eq!(
            cfg.enabled_profiles,
            vec!["drop_packets".to_string(), "reconnect".to_string()]
        );
    }

    #[test]
    fn parse_toml_bom_ok() {
        let text = "\u{feff}[chaos]\nseed = 7\n";
        let cfg = parse_toml(strip_bom(text)).expect("bom stripped");
        assert_eq!(cfg.chaos.seed, 7);
    }

    #[test]
    fn parse_toml_invalid_seed_type() {
        let text = "[chaos]\nseed = \"not-a-number\"\n";
        let err = parse_toml(text).unwrap_err();
        assert!(
            err.contains("seed") || err.contains("integer") || err.contains("invalid"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn parse_toml_unknown_chaos_key() {
        let text = "[chaos]\nseed = 1\nnot_a_real_key = 2\n";
        let err = parse_toml(text).unwrap_err();
        assert!(
            err.contains("not_a_real_key") || err.contains("unknown"),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn load_config_missing_defaults() {
        let cfg = load_config(Path::new("definitely/missing/bug_hunt_config.toml")).unwrap();
        assert_eq!(cfg.chaos.seed, ChaosProfileConfig::default().seed);
    }

    #[test]
    fn load_config_tempfile_valid() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "[chaos]\nseed = 99\niterations = 5\n").unwrap();
        let cfg = load_config(f.path()).unwrap();
        assert_eq!(cfg.chaos.seed, 99);
        assert_eq!(cfg.chaos.iterations, 5);
    }

    #[test]
    fn load_config_tempfile_invalid_numeric() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "[chaos]\niterations = true\n").unwrap();
        let err = load_config(f.path()).unwrap_err();
        assert!(err.contains("parse"), "err={err}");
    }
}

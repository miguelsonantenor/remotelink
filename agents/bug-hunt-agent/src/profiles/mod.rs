//! Chaos and fuzz profiles runnable without real network.

mod audio_desync;
mod drop_packets;
mod protocol_fuzz;
mod reconnect;

pub use audio_desync::run_audio_desync;
pub use drop_packets::run_drop_packets;
pub use protocol_fuzz::run_protocol_fuzz;
pub use reconnect::run_reconnect;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ChaosProfileConfig, ProfileName, Severity};

/// Outcome of a single profile run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileOutcome {
    /// Profile executed.
    pub profile: ProfileName,
    /// Config / nightly **root** seed (before per-profile derivation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_seed: Option<u64>,
    /// **Effective** seed used for this profile's RNG (repro with `--seed`).
    pub seed: u64,
    /// Pass / fail / skip.
    pub status: ProfileStatus,
    /// Defect severity when failed.
    pub severity: Option<Severity>,
    /// One-line summary.
    pub summary: String,
    /// Profile-specific metrics.
    pub metrics: Value,
    /// Optional repro fingerprint / steps.
    pub repro: Option<String>,
}

impl ProfileOutcome {
    /// Attach root seed and rewrite repro to include both seeds.
    pub fn with_seeds(mut self, root_seed: u64, effective_seed: u64) -> Self {
        self.root_seed = Some(root_seed);
        self.seed = effective_seed;
        let base = format!(
            "root_seed={root_seed} effective_seed={effective_seed} profile={}",
            self.profile.as_str()
        );
        self.repro = Some(match self.repro.take() {
            Some(extra) if !extra.is_empty() => format!("{base}; {extra}"),
            _ => base,
        });
        self
    }
}

/// Profile terminal status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// Completed with expected chaos behavior, no defect.
    Pass,
    /// Unexpected failure (panic caught as error, invariant broken).
    Fail,
    /// Not run (disabled / missing dependency).
    Skip,
}

impl ProfileStatus {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
        }
    }
}

/// Dispatch a profile by name.
pub fn run_profile(name: ProfileName, cfg: &ChaosProfileConfig) -> ProfileOutcome {
    match name {
        ProfileName::DropPackets => run_drop_packets(cfg),
        ProfileName::Reconnect => run_reconnect(cfg),
        ProfileName::AudioDesync => run_audio_desync(cfg),
        ProfileName::ProtocolFuzz => run_protocol_fuzz(cfg),
    }
}

/// Run a profile, converting panics into [`ProfileStatus::Fail`] (High severity).
pub fn run_profile_catch_unwind(name: ProfileName, cfg: &ChaosProfileConfig) -> ProfileOutcome {
    let seed = cfg.seed;
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_profile(name, cfg))) {
        Ok(outcome) => outcome,
        Err(payload) => {
            let msg = panic_payload_string(&payload);
            ProfileOutcome {
                profile: name,
                root_seed: None,
                seed,
                status: ProfileStatus::Fail,
                severity: Some(Severity::High),
                summary: format!("profile panicked: {msg}"),
                metrics: serde_json::json!({ "panic": true }),
                repro: Some(format!("effective_seed={seed} profile={}", name.as_str())),
            }
        }
    }
}

fn panic_payload_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".into()
    }
}

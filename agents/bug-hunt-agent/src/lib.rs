//! Bug-Hunt Agent: chaos profiles, protocol random-byte fuzz, nightly artifacts.
//!
//! # Policy
//!
//! - **No LLM required** for core nightly (`cargo fuzz` / proptest / chaos).
//! - **No real network** — profiles use mock peers and in-process skew injection.
//! - Outputs under `agents/shared/artifacts/` or `target/chaos/` for human review.
//! - Never auto-merge; never open production ports.

#![deny(missing_docs)]

mod artifact;
mod config;
mod profiles;
mod runner;

pub use artifact::{write_profile_artifact, write_summary, ArtifactPaths, ProfileArtifact};
pub use config::{
    default_config, load_config, BugHuntConfig, ChaosProfileConfig, ProfileName, Severity,
};
pub use profiles::{
    run_audio_desync, run_drop_packets, run_protocol_fuzz, run_reconnect, ProfileOutcome,
    ProfileStatus,
};
pub use runner::{
    derive_seed, resolve_effective_seed, resolve_profiles, run_nightly, run_profiles,
    NightlyReport, RunOptions,
};

//! `audio_desync` chaos: force skew injection into the media skew controller.
//!
//! No real audio devices — uses [`remotelink_media::SkewController`] with
//! synthetic samples that force audio-ahead / audio-behind conditions.

use remotelink_media::{SkewController, SkewSample};
use serde_json::json;

use crate::config::{ChaosProfileConfig, ProfileName, Severity};
use crate::profiles::{ProfileOutcome, ProfileStatus};

/// Run audio_desync profile: inject large skew and assert controller responds.
pub fn run_audio_desync(cfg: &ChaosProfileConfig) -> ProfileOutcome {
    let seed = cfg.seed;
    let inject = cfg.skew_inject_ms.abs().max(1.0);

    let mut ctrl = SkewController::with_defaults();
    let deadband = ctrl.config().deadband_ms;

    // 1) Inside deadband — no adjust.
    let inside = ctrl.update(
        SkewSample {
            audio_playout_host_equiv_ms: 100.0,
            video_present_host_equiv_ms: 100.0 + deadband * 0.5,
        },
        0.0,
    );
    if !inside.in_deadband || inside.delay_adjust_ms != 0.0 {
        return fail(
            seed,
            format!(
                "deadband violated: in_deadband={} delay={}",
                inside.in_deadband, inside.delay_adjust_ms
            ),
        );
    }

    // 2) Force audio-ahead by `inject` ms.
    let ahead = ctrl.update(
        SkewSample {
            audio_playout_host_equiv_ms: 1000.0 + inject,
            video_present_host_equiv_ms: 1000.0,
        },
        0.0,
    );
    if ahead.in_deadband {
        return fail(seed, "expected leave deadband on audio-ahead inject".into());
    }
    if ahead.skew_ms <= 0.0 {
        return fail(
            seed,
            format!("expected positive skew, got {}", ahead.skew_ms),
        );
    }
    if ahead.delay_adjust_ms <= 0.0 {
        return fail(
            seed,
            format!(
                "expected positive delay step on audio-ahead, got {}",
                ahead.delay_adjust_ms
            ),
        );
    }
    if ahead.resample_ratio <= 1.0 {
        return fail(
            seed,
            format!(
                "expected resample_ratio > 1 (slow audio), got {}",
                ahead.resample_ratio
            ),
        );
    }
    let max_stretch = ctrl.config().max_stretch;
    if ahead.resample_ratio > 1.0 + max_stretch + 1e-9 {
        return fail(
            seed,
            format!(
                "resample_ratio {} exceeds max_stretch {}",
                ahead.resample_ratio, max_stretch
            ),
        );
    }

    let offset_after_ahead = ctrl.delay_offset_ms();

    // 3) Force audio-behind (after rate-limit window).
    let behind = ctrl.update(
        SkewSample {
            audio_playout_host_equiv_ms: 2000.0,
            video_present_host_equiv_ms: 2000.0 + inject,
        },
        10_000.0, // far enough for another step
    );
    if behind.skew_ms >= 0.0 {
        return fail(
            seed,
            format!("expected negative skew, got {}", behind.skew_ms),
        );
    }
    if behind.delay_adjust_ms >= 0.0 {
        return fail(
            seed,
            format!(
                "expected negative delay step on audio-behind, got {}",
                behind.delay_adjust_ms
            ),
        );
    }
    if behind.resample_ratio >= 1.0 {
        return fail(
            seed,
            format!(
                "expected resample_ratio < 1 (speed audio), got {}",
                behind.resample_ratio
            ),
        );
    }

    // 4) Reset clears offset.
    ctrl.reset();
    if ctrl.delay_offset_ms() != 0.0 {
        return fail(
            seed,
            format!("reset left delay_offset_ms={}", ctrl.delay_offset_ms()),
        );
    }

    ProfileOutcome {
        profile: ProfileName::AudioDesync,
        root_seed: None,
        seed,
        status: ProfileStatus::Pass,
        severity: None,
        summary: format!(
            "skew inject ±{inject} ms: ahead delay={}, ratio={:.4}; behind delay={}, ratio={:.4}",
            ahead.delay_adjust_ms,
            ahead.resample_ratio,
            behind.delay_adjust_ms,
            behind.resample_ratio
        ),
        metrics: json!({
            "skew_inject_ms": inject,
            "deadband_ms": deadband,
            "ahead_skew_ms": ahead.skew_ms,
            "ahead_delay_adjust_ms": ahead.delay_adjust_ms,
            "ahead_resample_ratio": ahead.resample_ratio,
            "behind_skew_ms": behind.skew_ms,
            "behind_delay_adjust_ms": behind.delay_adjust_ms,
            "behind_resample_ratio": behind.resample_ratio,
            "offset_after_ahead_ms": offset_after_ahead,
            "offset_after_reset_ms": 0.0,
        }),
        repro: Some(format!("seed={seed} skew_inject_ms={inject}")),
    }
}

fn fail(seed: u64, summary: String) -> ProfileOutcome {
    ProfileOutcome {
        profile: ProfileName::AudioDesync,
        root_seed: None,
        seed,
        status: ProfileStatus::Fail,
        severity: Some(Severity::Medium),
        summary,
        metrics: json!({}),
        repro: Some(format!("seed={seed}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_desync_pass() {
        let cfg = ChaosProfileConfig {
            seed: 3,
            skew_inject_ms: 80.0,
            ..ChaosProfileConfig::default()
        };
        let o = run_audio_desync(&cfg);
        assert_eq!(o.status, ProfileStatus::Pass, "{}", o.summary);
    }

    #[test]
    fn large_inject_clamps_stretch() {
        let cfg = ChaosProfileConfig {
            seed: 1,
            skew_inject_ms: 500.0,
            ..ChaosProfileConfig::default()
        };
        let o = run_audio_desync(&cfg);
        assert_eq!(o.status, ProfileStatus::Pass, "{}", o.summary);
        let ratio = o.metrics["ahead_resample_ratio"].as_f64().unwrap();
        assert!(ratio <= 1.02 + 1e-9);
    }
}

//! A/V skew controller: slave audio playout to video.
//!
//! Contract (DESIGN.md):
//! - `skew_ms = audio_playout_host_equiv_ms - video_present_host_equiv_ms`
//!   (positive ⇒ audio ahead of video)
//! - Deadband ±15 ms (no adjust inside)
//! - Step-adjust playout delay by 5 ms toward zero at ≤ 10 ms/s
//! - Max audio time-stretch ±2% via linear resample (v1)
//!
//! # Stretch law (v1)
//!
//! Resample ratio is proportional to the **residual past the deadband**, not
//! raw `|skew| / 100` (which would always saturate ±2% immediately outside the
//! deadband). Residual of ~50 ms maps to full `max_stretch`; larger residuals
//! clamp at ±`max_stretch`.

/// Configuration for [`SkewController`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkewConfig {
    /// No correction when `|skew| <= deadband_ms`.
    pub deadband_ms: f64,
    /// Delay adjustment step size (ms).
    pub step_ms: f64,
    /// Maximum absolute rate of delay change (ms per second).
    pub max_adjust_rate_ms_per_s: f64,
    /// Maximum linear resample ratio deviation from 1.0 (0.02 = ±2%).
    pub max_stretch: f64,
}

impl Default for SkewConfig {
    fn default() -> Self {
        Self {
            deadband_ms: 15.0,
            step_ms: 5.0,
            max_adjust_rate_ms_per_s: 10.0,
            max_stretch: 0.02,
        }
    }
}

/// One observation of audio vs video playout times (host-equivalent ms).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkewSample {
    /// Host-equivalent time of audio playout (ms).
    pub audio_playout_host_equiv_ms: f64,
    /// Host-equivalent time of video present (ms).
    pub video_present_host_equiv_ms: f64,
}

impl SkewSample {
    /// `audio - video` in ms; positive ⇒ audio ahead.
    pub fn skew_ms(self) -> f64 {
        self.audio_playout_host_equiv_ms - self.video_present_host_equiv_ms
    }
}

/// Decision produced by the skew controller for the next audio interval.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkewDecision {
    /// Measured skew in ms (positive = audio ahead).
    pub skew_ms: f64,
    /// Additional audio playout delay to apply (ms). May be negative (pull earlier).
    pub delay_adjust_ms: f64,
    /// Resample ratio for audio (1.0 = unity). Clamped to `[1-max_stretch, 1+max_stretch]`.
    /// Ratio > 1 stretches (slows) audio, reducing audio-ahead skew.
    pub resample_ratio: f64,
    /// True when |skew| is inside the deadband.
    pub in_deadband: bool,
}

/// Step-based skew controller (not full PID in v1).
#[derive(Debug, Clone)]
pub struct SkewController {
    cfg: SkewConfig,
    /// Accumulated playout delay offset applied to audio (ms).
    delay_offset_ms: f64,
    /// Last wall time we applied a step (for rate limiting), in ms.
    last_step_wall_ms: Option<f64>,
}

impl SkewController {
    /// Create with the given config.
    pub fn new(cfg: SkewConfig) -> Self {
        Self {
            cfg,
            delay_offset_ms: 0.0,
            last_step_wall_ms: None,
        }
    }

    /// Create with DESIGN.md defaults.
    pub fn with_defaults() -> Self {
        Self::new(SkewConfig::default())
    }

    /// Current accumulated delay offset (ms).
    pub fn delay_offset_ms(&self) -> f64 {
        self.delay_offset_ms
    }

    /// Config.
    pub fn config(&self) -> SkewConfig {
        self.cfg
    }

    /// Observe a skew sample at viewer wall time `wall_ms` and produce a decision.
    ///
    /// `wall_ms` is used only for rate-limiting step adjustments.
    pub fn update(&mut self, sample: SkewSample, wall_ms: f64) -> SkewDecision {
        let skew_ms = sample.skew_ms();
        let abs = skew_ms.abs();

        if abs <= self.cfg.deadband_ms {
            return SkewDecision {
                skew_ms,
                delay_adjust_ms: 0.0,
                resample_ratio: 1.0,
                in_deadband: true,
            };
        }

        // Desired direction: if audio ahead (skew > 0), increase delay / slow audio.
        // if audio behind (skew < 0), decrease delay / speed audio.
        let mut step = 0.0;
        let can_step = match self.last_step_wall_ms {
            None => true,
            Some(last) => {
                let elapsed_s = (wall_ms - last).max(0.0) / 1000.0;
                // At most one step per (step_ms / max_rate) seconds.
                let min_interval_s = self.cfg.step_ms / self.cfg.max_adjust_rate_ms_per_s;
                elapsed_s >= min_interval_s
            }
        };

        if can_step {
            step = if skew_ms > 0.0 {
                self.cfg.step_ms
            } else {
                -self.cfg.step_ms
            };
            self.delay_offset_ms += step;
            self.last_step_wall_ms = Some(wall_ms);
        }

        // Continuous stretch from residual past deadband, capped at ±max_stretch.
        // Audio ahead → ratio > 1 (play slower); behind → ratio < 1.
        // Gain: ~50 ms residual → full max_stretch (see module docs).
        const RESIDUAL_FOR_FULL_STRETCH_MS: f64 = 50.0;
        let residual_ms = if skew_ms > 0.0 {
            skew_ms - self.cfg.deadband_ms
        } else {
            skew_ms + self.cfg.deadband_ms
        };
        let stretch = (residual_ms / RESIDUAL_FOR_FULL_STRETCH_MS * self.cfg.max_stretch)
            .clamp(-self.cfg.max_stretch, self.cfg.max_stretch);
        let resample_ratio =
            (1.0 + stretch).clamp(1.0 - self.cfg.max_stretch, 1.0 + self.cfg.max_stretch);

        SkewDecision {
            skew_ms,
            delay_adjust_ms: step,
            resample_ratio,
            in_deadband: false,
        }
    }

    /// Reset controller state (e.g. on media_restart).
    pub fn reset(&mut self) {
        self.delay_offset_ms = 0.0;
        self.last_step_wall_ms = None;
    }
}

/// Linear resample of interleaved i16 PCM by `ratio`.
///
/// - `ratio > 1.0`: output is longer (time-stretch / slow down)
/// - `ratio < 1.0`: output is shorter (speed up)
/// - `channels`: interleaved channel count
///
/// Output length is `round(input_frames * ratio)` frames.
pub fn linear_resample_i16(input: &[i16], channels: u16, ratio: f64) -> Vec<i16> {
    assert!(channels > 0, "channels must be > 0");
    if input.is_empty() {
        return Vec::new();
    }
    let ch = channels as usize;
    assert!(
        input.len().is_multiple_of(ch),
        "input length must be a multiple of channels"
    );
    let in_frames = input.len() / ch;
    if in_frames == 1 || (ratio - 1.0).abs() < 1e-12 {
        return input.to_vec();
    }

    let ratio = ratio.clamp(1e-6, 1e6);
    let out_frames = ((in_frames as f64) * ratio).round().max(1.0) as usize;
    let mut out = vec![0i16; out_frames * ch];

    for of in 0..out_frames {
        let src = of as f64 / ratio;
        let i0 = src.floor() as usize;
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (src - i0 as f64).clamp(0.0, 1.0);
        let i0 = i0.min(in_frames - 1);
        for c in 0..ch {
            let s0 = input[i0 * ch + c] as f64;
            let s1 = input[i1 * ch + c] as f64;
            let v = s0 + (s1 - s0) * frac;
            out[of * ch + c] = v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        }
    }
    out
}

/// Apply max-stretch clamp to a proposed ratio.
pub fn clamp_resample_ratio(ratio: f64, max_stretch: f64) -> f64 {
    ratio.clamp(1.0 - max_stretch, 1.0 + max_stretch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadband_no_adjust() {
        let mut c = SkewController::with_defaults();
        let d = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 100.0,
                video_present_host_equiv_ms: 105.0, // skew -5 ms
            },
            0.0,
        );
        assert!(d.in_deadband);
        assert_eq!(d.delay_adjust_ms, 0.0);
        assert_eq!(d.resample_ratio, 1.0);
        assert!((d.skew_ms - (-5.0)).abs() < 1e-9);
    }

    #[test]
    fn audio_ahead_steps_positive_delay() {
        let mut c = SkewController::with_defaults();
        let d = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 150.0,
                video_present_host_equiv_ms: 100.0, // +50 ms
            },
            0.0,
        );
        assert!(!d.in_deadband);
        assert_eq!(d.delay_adjust_ms, 5.0);
        assert!(d.resample_ratio > 1.0);
        assert!(d.resample_ratio <= 1.02);
        assert_eq!(c.delay_offset_ms(), 5.0);
    }

    #[test]
    fn audio_behind_steps_negative_delay() {
        let mut c = SkewController::with_defaults();
        let d = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 100.0,
                video_present_host_equiv_ms: 150.0, // -50 ms
            },
            0.0,
        );
        assert_eq!(d.delay_adjust_ms, -5.0);
        assert!(d.resample_ratio < 1.0);
        assert!(d.resample_ratio >= 0.98);
    }

    #[test]
    fn rate_limit_max_10ms_per_second() {
        let mut c = SkewController::with_defaults();
        // First step at t=0.
        let d0 = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 200.0,
                video_present_host_equiv_ms: 100.0,
            },
            0.0,
        );
        assert_eq!(d0.delay_adjust_ms, 5.0);

        // 5 ms step at ≤10 ms/s ⇒ min 0.5 s between steps.
        let d1 = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 200.0,
                video_present_host_equiv_ms: 100.0,
            },
            400.0, // 0.4 s later — too soon
        );
        assert_eq!(d1.delay_adjust_ms, 0.0);
        assert_eq!(c.delay_offset_ms(), 5.0);

        let d2 = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 200.0,
                video_present_host_equiv_ms: 100.0,
            },
            500.0, // 0.5 s — allowed
        );
        assert_eq!(d2.delay_adjust_ms, 5.0);
        assert_eq!(c.delay_offset_ms(), 10.0);
    }

    #[test]
    fn stretch_clamped_to_2_percent() {
        let mut c = SkewController::with_defaults();
        let d = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 1000.0,
                video_present_host_equiv_ms: 0.0, // huge skew
            },
            0.0,
        );
        assert!(d.resample_ratio <= 1.02 + 1e-12);
        assert!(d.resample_ratio >= 0.98 - 1e-12);
        // Saturates at max for large residual.
        assert!((d.resample_ratio - 1.02).abs() < 1e-12);
    }

    #[test]
    fn stretch_proportional_to_residual_past_deadband() {
        let mut c = SkewController::with_defaults();
        // skew = +25 ms → residual = 10 ms → stretch = 10/50 * 0.02 = 0.004
        let d = c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 125.0,
                video_present_host_equiv_ms: 100.0,
            },
            0.0,
        );
        assert!(!d.in_deadband);
        assert!((d.resample_ratio - 1.004).abs() < 1e-9);
    }

    #[test]
    fn linear_resample_unity() {
        let input: Vec<i16> = (0..100).collect();
        let out = linear_resample_i16(&input, 1, 1.0);
        assert_eq!(out, input);
    }

    #[test]
    fn linear_resample_stretch_lengthens() {
        let input: Vec<i16> = (0..100).map(|i| (i * 100) as i16).collect();
        let out = linear_resample_i16(&input, 1, 1.02);
        assert_eq!(out.len(), 102);
    }

    #[test]
    fn linear_resample_squeeze_shortens() {
        let input: Vec<i16> = (0..100).map(|i| (i * 100) as i16).collect();
        let out = linear_resample_i16(&input, 1, 0.98);
        assert_eq!(out.len(), 98);
    }

    #[test]
    fn linear_resample_stereo_interleaved() {
        // L=1,R=2 repeated
        let input: Vec<i16> = vec![1, 2, 1, 2, 1, 2, 1, 2];
        let out = linear_resample_i16(&input, 2, 1.0);
        assert_eq!(out, input);
        let stretched = linear_resample_i16(&input, 2, 1.02);
        assert_eq!(stretched.len() % 2, 0);
    }

    #[test]
    fn clamp_resample_ratio_helper() {
        assert_eq!(clamp_resample_ratio(1.05, 0.02), 1.02);
        assert_eq!(clamp_resample_ratio(0.90, 0.02), 0.98);
        assert_eq!(clamp_resample_ratio(1.01, 0.02), 1.01);
    }

    #[test]
    fn reset_clears_offset() {
        let mut c = SkewController::with_defaults();
        c.update(
            SkewSample {
                audio_playout_host_equiv_ms: 200.0,
                video_present_host_equiv_ms: 100.0,
            },
            0.0,
        );
        assert!(c.delay_offset_ms() != 0.0);
        c.reset();
        assert_eq!(c.delay_offset_ms(), 0.0);
    }

    #[test]
    fn skew_sample_sign_convention() {
        // positive => audio ahead of video
        let s = SkewSample {
            audio_playout_host_equiv_ms: 40.0,
            video_present_host_equiv_ms: 10.0,
        };
        assert!(s.skew_ms() > 0.0);
    }
}

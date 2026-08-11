//! Video freeze → audio policy state machine.
//!
//! Default (DESIGN.md): when video is frozen longer than 200 ms, **hold** last
//! audio for 100 ms then **fade to silence**. Configurable via
//! [`AudioOnVideoFreeze`].

use std::time::Duration;

/// Policy when video stops updating while audio continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioOnVideoFreeze {
    /// Hold last audio 100 ms (default hold), then fade to silence.
    #[default]
    HoldFade,
    /// Immediately duck (attenuate) audio while frozen.
    Duck,
    /// Keep playing audio unchanged.
    Continue,
}

/// Configuration for [`FreezePolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreezeConfig {
    /// Video considered frozen after this gap without a new frame.
    pub freeze_threshold: Duration,
    /// How long to hold/repeat last audio after freeze is declared.
    pub hold_duration: Duration,
    /// Fade-to-silence duration after hold (HoldFade only).
    pub fade_duration: Duration,
    /// Policy variant.
    pub mode: AudioOnVideoFreeze,
    /// Duck attenuation factor in Q15 (0..32768); used for Duck mode.
    /// 8192 ≈ 0.25 amplitude.
    pub duck_q15: i16,
}

impl Default for FreezeConfig {
    fn default() -> Self {
        Self {
            freeze_threshold: Duration::from_millis(200),
            hold_duration: Duration::from_millis(100),
            fade_duration: Duration::from_millis(100),
            mode: AudioOnVideoFreeze::HoldFade,
            duck_q15: 8192,
        }
    }
}

/// State of the freeze audio policy machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeState {
    /// Video is updating normally; pass audio through.
    Normal,
    /// Video gap exceeded threshold; holding/repeating last audio.
    Holding {
        /// Elapsed time since freeze declared.
        elapsed: Duration,
    },
    /// Fading amplitude to silence after hold.
    Fading {
        /// Elapsed time in the fade phase.
        elapsed: Duration,
    },
    /// Fully silent while video remains frozen.
    Silent,
    /// Duck mode: attenuated audio while frozen.
    Ducked,
}

/// Gain to apply to the current audio block (0.0 = silence, 1.0 = unity).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreezeOutput {
    /// Current state.
    pub state: FreezeState,
    /// Linear gain for this audio interval.
    pub gain: f64,
    /// True if the caller should reuse the last audio buffer (hold).
    pub hold_last_audio: bool,
}

/// Unit-testable freeze policy state machine.
#[derive(Debug, Clone)]
pub struct FreezePolicy {
    cfg: FreezeConfig,
    state: FreezeState,
    /// Time of last video frame present (viewer clock).
    last_video_pts: Option<Duration>,
}

impl FreezePolicy {
    /// Create with config.
    pub fn new(cfg: FreezeConfig) -> Self {
        Self {
            cfg,
            state: FreezeState::Normal,
            last_video_pts: None,
        }
    }

    /// Defaults: hold 100 ms then fade after 200 ms freeze.
    pub fn with_defaults() -> Self {
        Self::new(FreezeConfig::default())
    }

    /// Current state.
    pub fn state(&self) -> FreezeState {
        self.state
    }

    /// Notify that a video frame was presented at `now`.
    pub fn on_video_frame(&mut self, now: Duration) {
        self.last_video_pts = Some(now);
        self.state = FreezeState::Normal;
    }

    /// Advance the policy given current viewer time `now` (no new video assumed).
    ///
    /// Call once per audio quantum while polling video.
    pub fn tick(&mut self, now: Duration) -> FreezeOutput {
        if self.cfg.mode == AudioOnVideoFreeze::Continue {
            return FreezeOutput {
                state: FreezeState::Normal,
                gain: 1.0,
                hold_last_audio: false,
            };
        }

        let frozen_for = match self.last_video_pts {
            None => {
                // No video yet — treat as normal (startup).
                return FreezeOutput {
                    state: FreezeState::Normal,
                    gain: 1.0,
                    hold_last_audio: false,
                };
            }
            Some(last) => now.saturating_sub(last),
        };

        // DESIGN.md: freeze when video gap is *greater than* threshold (not >=).
        if frozen_for <= self.cfg.freeze_threshold {
            self.state = FreezeState::Normal;
            return FreezeOutput {
                state: FreezeState::Normal,
                gain: 1.0,
                hold_last_audio: false,
            };
        }

        // Frozen (frozen_for > freeze_threshold).
        let since_freeze = frozen_for.saturating_sub(self.cfg.freeze_threshold);

        match self.cfg.mode {
            AudioOnVideoFreeze::Continue => unreachable!(),
            AudioOnVideoFreeze::Duck => {
                self.state = FreezeState::Ducked;
                let gain = f64::from(self.cfg.duck_q15) / 32768.0;
                FreezeOutput {
                    state: FreezeState::Ducked,
                    gain,
                    hold_last_audio: false,
                }
            }
            AudioOnVideoFreeze::HoldFade => {
                if since_freeze < self.cfg.hold_duration {
                    self.state = FreezeState::Holding {
                        elapsed: since_freeze,
                    };
                    FreezeOutput {
                        state: self.state,
                        gain: 1.0,
                        hold_last_audio: true,
                    }
                } else {
                    let fade_elapsed = since_freeze.saturating_sub(self.cfg.hold_duration);
                    if fade_elapsed < self.cfg.fade_duration {
                        let t = fade_elapsed.as_secs_f64()
                            / self.cfg.fade_duration.as_secs_f64().max(1e-9);
                        let gain = (1.0 - t).clamp(0.0, 1.0);
                        self.state = FreezeState::Fading {
                            elapsed: fade_elapsed,
                        };
                        FreezeOutput {
                            state: self.state,
                            gain,
                            hold_last_audio: true,
                        }
                    } else {
                        self.state = FreezeState::Silent;
                        FreezeOutput {
                            state: FreezeState::Silent,
                            gain: 0.0,
                            hold_last_audio: false,
                        }
                    }
                }
            }
        }
    }

    /// Apply `gain` to interleaved i16 PCM (in place).
    pub fn apply_gain(pcm: &mut [i16], gain: f64) {
        let g = gain.clamp(0.0, 1.0);
        if (g - 1.0).abs() < 1e-12 {
            return;
        }
        if g <= 0.0 {
            pcm.fill(0);
            return;
        }
        for s in pcm.iter_mut() {
            *s = (*s as f64 * g)
                .round()
                .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_until_threshold() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        let o = p.tick(Duration::from_millis(199));
        assert_eq!(o.state, FreezeState::Normal);
        assert_eq!(o.gain, 1.0);
        assert!(!o.hold_last_audio);
    }

    #[test]
    fn exactly_threshold_still_normal() {
        // DESIGN: freeze only when gap *>* 200 ms.
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        let o = p.tick(Duration::from_millis(200));
        assert_eq!(o.state, FreezeState::Normal);
        assert_eq!(o.gain, 1.0);
    }

    #[test]
    fn hold_after_200ms_freeze() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        // freeze_threshold 200 + 50 into hold
        let o = p.tick(Duration::from_millis(250));
        assert!(matches!(o.state, FreezeState::Holding { .. }));
        assert_eq!(o.gain, 1.0);
        assert!(o.hold_last_audio);
    }

    #[test]
    fn fade_after_hold() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        // 200 freeze + 100 hold + 50 fade = 350
        let o = p.tick(Duration::from_millis(350));
        assert!(matches!(o.state, FreezeState::Fading { .. }));
        assert!(o.gain > 0.0 && o.gain < 1.0);
        assert!(o.hold_last_audio);
    }

    #[test]
    fn silent_after_fade() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        // 200 + 100 + 100 = 400
        let o = p.tick(Duration::from_millis(400));
        assert_eq!(o.state, FreezeState::Silent);
        assert_eq!(o.gain, 0.0);
    }

    #[test]
    fn video_recovery_returns_normal() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::from_millis(0));
        let _ = p.tick(Duration::from_millis(500));
        assert_eq!(p.state(), FreezeState::Silent);
        p.on_video_frame(Duration::from_millis(500));
        assert_eq!(p.state(), FreezeState::Normal);
        let o = p.tick(Duration::from_millis(510));
        assert_eq!(o.state, FreezeState::Normal);
        assert_eq!(o.gain, 1.0);
    }

    #[test]
    fn duck_mode() {
        let mut p = FreezePolicy::new(FreezeConfig {
            mode: AudioOnVideoFreeze::Duck,
            ..FreezeConfig::default()
        });
        p.on_video_frame(Duration::ZERO);
        let o = p.tick(Duration::from_millis(300));
        assert_eq!(o.state, FreezeState::Ducked);
        assert!((o.gain - 8192.0 / 32768.0).abs() < 1e-9);
    }

    #[test]
    fn continue_mode_never_freezes_audio() {
        let mut p = FreezePolicy::new(FreezeConfig {
            mode: AudioOnVideoFreeze::Continue,
            ..FreezeConfig::default()
        });
        p.on_video_frame(Duration::ZERO);
        let o = p.tick(Duration::from_secs(10));
        assert_eq!(o.state, FreezeState::Normal);
        assert_eq!(o.gain, 1.0);
    }

    #[test]
    fn apply_gain_silence_and_unity() {
        let mut pcm = vec![1000i16, -1000];
        FreezePolicy::apply_gain(&mut pcm, 1.0);
        assert_eq!(pcm, vec![1000, -1000]);
        FreezePolicy::apply_gain(&mut pcm, 0.0);
        assert_eq!(pcm, vec![0, 0]);
    }

    #[test]
    fn apply_gain_half() {
        let mut pcm = vec![1000i16];
        FreezePolicy::apply_gain(&mut pcm, 0.5);
        assert_eq!(pcm[0], 500);
    }

    #[test]
    fn full_state_sequence_hold_fade_silent() {
        let mut p = FreezePolicy::with_defaults();
        p.on_video_frame(Duration::ZERO);

        assert_eq!(
            p.tick(Duration::from_millis(100)).state,
            FreezeState::Normal
        );
        assert!(matches!(
            p.tick(Duration::from_millis(250)).state,
            FreezeState::Holding { .. }
        ));
        assert!(matches!(
            p.tick(Duration::from_millis(350)).state,
            FreezeState::Fading { .. }
        ));
        assert_eq!(
            p.tick(Duration::from_millis(450)).state,
            FreezeState::Silent
        );
    }
}

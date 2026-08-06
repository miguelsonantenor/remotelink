//! RTP clock mapping from host monotonic time with a shared session epoch `t0`.
//!
//! Per RemoteLink A/V timing contract:
//! - Video RTP clock rate: 90_000 Hz
//! - Audio RTP clock rate: 48_000 Hz
//! - Both share the same `t0` so initial A/V offset is zero at media start
//!
//! ```text
//! rtp_ts = floor((host_mono - t0) * clock_rate_hz)
//! ```
//!
//! Timestamps wrap at 2^32 (standard RTP).

use std::time::Duration;

/// Video RTP clock rate (Hz).
pub const VIDEO_CLOCK_HZ: u32 = 90_000;

/// Audio RTP clock rate (Hz) — matches 48 kHz PCM / Opus.
pub const AUDIO_CLOCK_HZ: u32 = 48_000;

/// Which media stream an RTP clock applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtpClockRate {
    /// 90 kHz video.
    Video,
    /// 48 kHz audio.
    Audio,
}

impl RtpClockRate {
    /// Clock rate in Hz.
    pub fn hz(self) -> u32 {
        match self {
            RtpClockRate::Video => VIDEO_CLOCK_HZ,
            RtpClockRate::Audio => AUDIO_CLOCK_HZ,
        }
    }
}

/// Session epoch for RTP timestamp derivation.
///
/// Created once at media start; reset on `media_restart` (device change, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpEpoch {
    /// Host-monotonic time of session media start (`t0`).
    t0: Duration,
}

impl RtpEpoch {
    /// Create an epoch anchored at `t0` (host mono of media start).
    pub fn new(t0: Duration) -> Self {
        Self { t0 }
    }

    /// Epoch origin.
    pub fn t0(self) -> Duration {
        self.t0
    }

    /// Map host mono → RTP timestamp for the given clock rate.
    ///
    /// Returns 0 if `host_mono < t0` (clamped; should not happen in normal use).
    pub fn host_mono_to_rtp(self, host_mono: Duration, rate: RtpClockRate) -> u32 {
        host_mono_to_rtp_ts(host_mono, self.t0, rate.hz())
    }

    /// Convenience: video 90 kHz mapping.
    pub fn video_ts(self, host_mono: Duration) -> u32 {
        self.host_mono_to_rtp(host_mono, RtpClockRate::Video)
    }

    /// Convenience: audio 48 kHz mapping.
    pub fn audio_ts(self, host_mono: Duration) -> u32 {
        self.host_mono_to_rtp(host_mono, RtpClockRate::Audio)
    }

    /// Convert an RTP timestamp back to host-mono duration relative to `t0`
    /// (best-effort; loses sub-tick precision and does not unwrap RTP wraps).
    pub fn rtp_to_host_mono(self, rtp_ts: u32, rate: RtpClockRate) -> Duration {
        let ms = rtp_ts_to_host_mono_ms(rtp_ts, rate.hz());
        self.t0 + Duration::from_nanos((ms * 1_000_000.0) as u64)
    }
}

/// Map `(host_mono - t0)` to an RTP timestamp at `clock_rate_hz`.
///
/// Uses integer nanosecond math to avoid floating-point drift for typical
/// session durations. Result is `u32` with natural wrap.
pub fn host_mono_to_rtp_ts(host_mono: Duration, t0: Duration, clock_rate_hz: u32) -> u32 {
    let elapsed = host_mono.saturating_sub(t0);
    // rtp_ts = elapsed_ns * rate / 1e9
    let ns = elapsed.as_nanos();
    let rate = u128::from(clock_rate_hz);
    let ticks = ns.saturating_mul(rate) / 1_000_000_000u128;
    (ticks & 0xFFFF_FFFF) as u32
}

/// Convert RTP ticks to milliseconds since epoch (f64 for skew math).
pub fn rtp_ts_to_host_mono_ms(rtp_ts: u32, clock_rate_hz: u32) -> f64 {
    if clock_rate_hz == 0 {
        return 0.0;
    }
    (f64::from(rtp_ts) * 1000.0) / f64::from(clock_rate_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_t0_zero_offset_at_start() {
        let t0 = Duration::from_secs(100);
        let epoch = RtpEpoch::new(t0);
        assert_eq!(epoch.video_ts(t0), 0);
        assert_eq!(epoch.audio_ts(t0), 0);
    }

    #[test]
    fn one_second_maps_to_clock_rates() {
        let t0 = Duration::from_millis(50);
        let epoch = RtpEpoch::new(t0);
        let t = t0 + Duration::from_secs(1);
        assert_eq!(epoch.video_ts(t), VIDEO_CLOCK_HZ);
        assert_eq!(epoch.audio_ts(t), AUDIO_CLOCK_HZ);
    }

    #[test]
    fn ten_ms_audio_and_video_pts_contract() {
        // 10 ms Opus frame and a video frame captured at the same host mono
        // must map to consistent wall time via their respective clocks.
        let t0 = Duration::ZERO;
        let epoch = RtpEpoch::new(t0);
        let capture = Duration::from_millis(10);

        let v = epoch.video_ts(capture);
        let a = epoch.audio_ts(capture);

        assert_eq!(v, 900); // 10ms * 90kHz
        assert_eq!(a, 480); // 10ms * 48kHz

        let v_ms = rtp_ts_to_host_mono_ms(v, VIDEO_CLOCK_HZ);
        let a_ms = rtp_ts_to_host_mono_ms(a, AUDIO_CLOCK_HZ);
        assert!((v_ms - 10.0).abs() < 1e-9);
        assert!((a_ms - 10.0).abs() < 1e-9);
        assert!(
            (v_ms - a_ms).abs() < 1e-9,
            "shared t0 implies equal wall time"
        );
    }

    #[test]
    fn host_mono_before_t0_clamps_to_zero() {
        let epoch = RtpEpoch::new(Duration::from_secs(10));
        assert_eq!(epoch.video_ts(Duration::from_secs(1)), 0);
    }

    #[test]
    fn free_function_matches_epoch() {
        let t0 = Duration::from_millis(7);
        let host = t0 + Duration::from_millis(33);
        let epoch = RtpEpoch::new(t0);
        assert_eq!(
            epoch.video_ts(host),
            host_mono_to_rtp_ts(host, t0, VIDEO_CLOCK_HZ)
        );
        assert_eq!(
            epoch.audio_ts(host),
            host_mono_to_rtp_ts(host, t0, AUDIO_CLOCK_HZ)
        );
    }

    #[test]
    fn media_restart_resets_epoch() {
        let old = RtpEpoch::new(Duration::from_secs(1));
        let later = Duration::from_secs(5);
        assert!(old.video_ts(later) > 0);

        let restarted = RtpEpoch::new(later);
        assert_eq!(restarted.video_ts(later), 0);
        assert_eq!(restarted.audio_ts(later), 0);
    }

    #[test]
    fn long_session_does_not_panic_on_wrap_math() {
        // ~13.2 hours at 90 kHz wraps u32; ensure we only take low 32 bits.
        let t0 = Duration::ZERO;
        let host = Duration::from_secs(50_000);
        let ts = host_mono_to_rtp_ts(host, t0, VIDEO_CLOCK_HZ);
        // Just ensure computation completes and is deterministic.
        let ts2 = host_mono_to_rtp_ts(host, t0, VIDEO_CLOCK_HZ);
        assert_eq!(ts, ts2);
    }
}

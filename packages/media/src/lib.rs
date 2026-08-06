//! RemoteLink media core: capture traits, synthetic sources, RTP timing,
//! jitter buffer, A/V skew control, video-freeze audio policy, and Opus framing.
//!
//! # A/V timing contract
//!
//! - Shared session epoch `t0` (host monotonic).
//! - Video RTP clock: 90 kHz; audio RTP clock: 48 kHz.
//! - Skew is measured in playout/wall domain; audio is slaved to video.

#![deny(missing_docs)]

pub mod freeze;
pub mod jitter;
pub mod opus;
pub mod rtp_clock;
pub mod skew;
pub mod source;
pub mod synthetic;

pub use freeze::{AudioOnVideoFreeze, FreezeConfig, FreezePolicy, FreezeState};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats};
pub use opus::{MockOpusDecoder, MockOpusEncoder, OpusDecoder, OpusEncoder, OpusError, OpusFrame};
pub use rtp_clock::{
    host_mono_to_rtp_ts, rtp_ts_to_host_mono_ms, RtpClockRate, RtpEpoch, AUDIO_CLOCK_HZ,
    VIDEO_CLOCK_HZ,
};
pub use skew::{linear_resample_i16, SkewConfig, SkewController, SkewDecision, SkewSample};
pub use source::{AudioFrame, AudioSource, PixelFormat, SampleFormat, VideoFrame, VideoSource};
pub use synthetic::{SyntheticAudioTone, SyntheticVideoBars};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod contract_tests {
    //! End-to-end A/V timing contract tests (PR 9 acceptance).

    use std::time::Duration;

    use crate::opus::{MockOpusDecoder, MockOpusEncoder, OpusDecoder, OpusEncoder};
    use crate::rtp_clock::{
        host_mono_to_rtp_ts, rtp_ts_to_host_mono_ms, RtpEpoch, AUDIO_CLOCK_HZ, VIDEO_CLOCK_HZ,
    };
    use crate::skew::{SkewController, SkewSample};
    use crate::source::{AudioSource, VideoSource};
    use crate::synthetic::{SyntheticAudioTone, SyntheticVideoBars};

    #[test]
    fn shared_t0_epoch_maps_av_to_same_wall_time() {
        let t0 = Duration::from_millis(1_000);
        let epoch = RtpEpoch::new(t0);

        // Capture A/V at identical host mono offsets.
        for ms in [0u64, 10, 33, 100, 1000] {
            let host = t0 + Duration::from_millis(ms);
            let v_ts = epoch.video_ts(host);
            let a_ts = epoch.audio_ts(host);
            let v_ms = rtp_ts_to_host_mono_ms(v_ts, VIDEO_CLOCK_HZ);
            let a_ms = rtp_ts_to_host_mono_ms(a_ts, AUDIO_CLOCK_HZ);
            assert!(
                (v_ms - a_ms).abs() < 1e-6,
                "ms={ms}: video wall {v_ms} != audio wall {a_ms}"
            );
            assert!((v_ms - ms as f64).abs() < 1e-6);
        }
    }

    #[test]
    fn pts_contract_synthetic_sources_with_rtp() {
        let t0 = Duration::from_millis(250);
        let epoch = RtpEpoch::new(t0);
        let mut video = SyntheticVideoBars::new(64, 36, 30, t0).with_max_frames(5);
        let mut audio = SyntheticAudioTone::default_a440(t0).with_max_packets(5);

        // Epoch-bound mock so OpusFrame.rtp_ts follows shared t0.
        let mut enc = MockOpusEncoder::with_epoch(epoch);
        let mut dec = MockOpusDecoder::new();

        for i in 0..5u64 {
            let vf = video.next_frame().unwrap().unwrap();
            let af = audio.next_frame().unwrap().unwrap();

            let v_rtp = epoch.video_ts(vf.pts_host_mono);
            let a_rtp = epoch.audio_ts(af.pts_host_mono);

            // First frames share t0 → both RTP timestamps 0.
            if i == 0 {
                assert_eq!(v_rtp, 0);
                assert_eq!(a_rtp, 0);
            }

            // Free function matches epoch helper.
            assert_eq!(
                v_rtp,
                host_mono_to_rtp_ts(vf.pts_host_mono, t0, VIDEO_CLOCK_HZ)
            );
            assert_eq!(
                a_rtp,
                host_mono_to_rtp_ts(af.pts_host_mono, t0, AUDIO_CLOCK_HZ)
            );

            // Mock Opus preserves PTS and stamps RTP from the shared epoch.
            let pkt = enc.encode(&af).unwrap();
            assert_eq!(pkt.pts_host_mono, af.pts_host_mono);
            assert_eq!(pkt.rtp_ts, a_rtp);
            let decoded = dec.decode(&pkt).unwrap();
            assert_eq!(decoded.pts_host_mono, af.pts_host_mono);
            assert_eq!(decoded.pcm_s16, af.pcm_s16);

            // Audio packets are 10 ms; video 30 fps ≈ 33.333 ms — clocks stay aligned
            // when evaluated at the same host mono.
            let same = t0 + Duration::from_millis(i * 10);
            let skew_ms = rtp_ts_to_host_mono_ms(epoch.audio_ts(same), AUDIO_CLOCK_HZ)
                - rtp_ts_to_host_mono_ms(epoch.video_ts(same), VIDEO_CLOCK_HZ);
            assert!(skew_ms.abs() < 1e-6, "shared t0 skew at {i}: {skew_ms}");
        }
    }

    #[test]
    fn skew_controller_drives_audio_toward_video() {
        let mut ctrl = SkewController::with_defaults();
        // Audio 40 ms ahead of video — outside ±15 ms deadband.
        let d = ctrl.update(
            SkewSample {
                audio_playout_host_equiv_ms: 140.0,
                video_present_host_equiv_ms: 100.0,
            },
            0.0,
        );
        assert!(!d.in_deadband);
        assert_eq!(d.skew_ms, 40.0);
        // Slave audio to video: add delay / stretch when audio is ahead.
        assert!(d.delay_adjust_ms > 0.0);
        assert!(d.resample_ratio > 1.0);
        assert!(d.resample_ratio <= 1.02);
    }

    #[test]
    fn version_nonempty() {
        assert!(!crate::VERSION.is_empty());
    }
}

//! Exportable beta session stats (G3: skew metric required).
//!
//! Beta builds must surface A/V skew (HUD or export). This module defines the
//! snapshot shape printed by the CLI HUD and optional egui overlay.

use remotelink_media::{JitterConfig, SkewConfig, SkewDecision};

/// Identity-bind status for HUD / export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BindStatus {
    /// No bind key / not started.
    #[default]
    Unbound,
    /// Session authorized but DC identity not yet complete.
    AuthorizedPending,
    /// Post-DTLS identity bind completed.
    Bound,
    /// Bind failed (fingerprint or challenge).
    Failed,
}

impl BindStatus {
    /// Stable label for HUD lines.
    pub fn as_str(self) -> &'static str {
        match self {
            BindStatus::Unbound => "unbound",
            BindStatus::AuthorizedPending => "authorized_pending",
            BindStatus::Bound => "bound",
            BindStatus::Failed => "failed",
        }
    }
}

/// Snapshot of session stats for UI / CLI / tests (PR 17 / DESIGN G3).
///
/// **Required beta fields:** [`Self::skew_ms`], jitter targets, RTT placeholder,
/// identity-bind status.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionStats {
    /// Decoded (or synthetic) video frames produced.
    pub video_frames: u64,
    /// Audio packets enqueued for playout.
    pub audio_packets: u64,
    /// Input events sent toward the host.
    pub input_events: u64,
    /// ICE candidates emitted locally.
    pub local_ice: u64,
    /// DataChannel messages received (non-input / control).
    pub data_rx: u64,
    /// Identity DataChannel messages handled.
    pub identity_messages: u64,
    /// Measured A/V skew in ms (`audio - video`; positive ⇒ audio ahead).
    pub skew_ms: f64,
    /// True when |skew| is inside the controller deadband.
    pub skew_in_deadband: bool,
    /// Last resample ratio applied to audio (1.0 = unity).
    pub resample_ratio: f64,
    /// Accumulated audio delay offset from skew controller (ms).
    pub delay_offset_ms: f64,
    /// Video jitter buffer target (ms).
    pub video_jitter_target_ms: f64,
    /// Audio jitter buffer target (ms).
    pub audio_jitter_target_ms: f64,
    /// RTT estimate in ms when known; `None` is a valid placeholder until
    /// transport RTCP / ICE RTT is wired.
    pub rtt_ms: Option<f64>,
    /// Identity-bind status for beta HUD.
    pub bind_status: BindStatus,
    /// True when identity_bound on the session.
    pub identity_bound: bool,
    /// Frames decoded via mock MH264 (vs synthetic fill).
    pub mock_h264_frames: u64,
    /// MOPU / mock Opus packets decoded.
    pub mock_opus_packets: u64,
    /// Audio packets pushed to the playout sink.
    pub audio_played: u64,
    /// Video encode bitrate estimate (bps) when known; 0 = unknown.
    pub video_bitrate_bps: u64,
    /// Estimated video FPS from last PTS deltas (0 = unknown).
    pub video_fps: f64,
}

impl Default for SessionStats {
    fn default() -> Self {
        let video_j = JitterConfig::wan_default();
        let audio_j = JitterConfig::wan_default();
        Self {
            video_frames: 0,
            audio_packets: 0,
            input_events: 0,
            local_ice: 0,
            data_rx: 0,
            identity_messages: 0,
            skew_ms: 0.0,
            skew_in_deadband: true,
            resample_ratio: 1.0,
            delay_offset_ms: 0.0,
            video_jitter_target_ms: video_j.initial_target.as_secs_f64() * 1000.0,
            audio_jitter_target_ms: audio_j.initial_target.as_secs_f64() * 1000.0,
            rtt_ms: None,
            bind_status: BindStatus::Unbound,
            identity_bound: false,
            mock_h264_frames: 0,
            mock_opus_packets: 0,
            audio_played: 0,
            video_bitrate_bps: 0,
            video_fps: 0.0,
        }
    }
}

impl SessionStats {
    /// Apply defaults from WAN jitter profiles.
    pub fn with_jitter_targets(mut self, video: JitterConfig, audio: JitterConfig) -> Self {
        self.video_jitter_target_ms = video.initial_target.as_secs_f64() * 1000.0;
        self.audio_jitter_target_ms = audio.initial_target.as_secs_f64() * 1000.0;
        self
    }

    /// Format a single-line beta HUD string (CLI / log).
    pub fn hud_line(&self) -> String {
        let rtt = self
            .rtt_ms
            .map(|r| format!("{r:.1}"))
            .unwrap_or_else(|| "n/a".into());
        format!(
            "skew_ms={:.2} deadband={} rtt_ms={} v_jit={:.0}ms a_jit={:.0}ms \
             bind={} frames={} audio={} fps={:.1} bitrate_bps={} \
             mock_h264={} mopu={} played={} resample={:.4} delay_off={:.1}",
            self.skew_ms,
            if self.skew_in_deadband { "y" } else { "n" },
            rtt,
            self.video_jitter_target_ms,
            self.audio_jitter_target_ms,
            self.bind_status.as_str(),
            self.video_frames,
            self.audio_packets,
            self.video_fps,
            self.video_bitrate_bps,
            self.mock_h264_frames,
            self.mock_opus_packets,
            self.audio_played,
            self.resample_ratio,
            self.delay_offset_ms,
        )
    }

    /// Multi-line HUD block for CLI verbose mode / egui overlay.
    pub fn hud_block(&self) -> String {
        let rtt = self
            .rtt_ms
            .map(|r| format!("{r:.1} ms"))
            .unwrap_or_else(|| "n/a (placeholder)".into());
        format!(
            "=== RemoteLink beta stats (G3) ===\n\
             A/V skew:     {skew:.2} ms ({band})\n\
             Video jitter: {vj:.0} ms target\n\
             Audio jitter: {aj:.0} ms target\n\
             RTT:          {rtt}\n\
             Bind:         {bind} (identity_bound={ib})\n\
             Video:        {vf} frames @ {fps:.1} fps, {br} bps\n\
             Audio:        {ap} pkts enqueued, {played} played (mopu={mopu})\n\
             Decode:       mock_h264={mh} resample={rr:.4} delay_off={d:.1} ms\n",
            skew = self.skew_ms,
            band = if self.skew_in_deadband {
                "in deadband"
            } else {
                "correcting"
            },
            vj = self.video_jitter_target_ms,
            aj = self.audio_jitter_target_ms,
            rtt = rtt,
            bind = self.bind_status.as_str(),
            ib = self.identity_bound,
            vf = self.video_frames,
            fps = self.video_fps,
            br = self.video_bitrate_bps,
            ap = self.audio_packets,
            played = self.audio_played,
            mopu = self.mock_opus_packets,
            mh = self.mock_h264_frames,
            rr = self.resample_ratio,
            d = self.delay_offset_ms,
        )
    }

    /// Update skew fields from a controller decision + accumulated delay.
    pub fn apply_skew_decision(&mut self, decision: &SkewDecision, delay_offset_ms: f64) {
        self.skew_ms = decision.skew_ms;
        self.skew_in_deadband = decision.in_deadband;
        self.resample_ratio = decision.resample_ratio;
        self.delay_offset_ms = delay_offset_ms;
    }

    /// True when required G3 export fields are present (always for this type).
    pub fn has_required_skew_metric(&self) -> bool {
        // Field is always populated; this helper is for tests / HUD gating.
        true
    }

    /// Deadband from media defaults (for HUD legend).
    pub fn default_deadband_ms() -> f64 {
        SkewConfig::default().deadband_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_media::{SkewController, SkewSample};

    #[test]
    fn hud_line_includes_required_fields() {
        let s = SessionStats {
            skew_ms: 12.5,
            bind_status: BindStatus::Bound,
            identity_bound: true,
            video_frames: 10,
            ..Default::default()
        };
        let line = s.hud_line();
        assert!(line.contains("skew_ms=12.50"), "{line}");
        assert!(line.contains("bind=bound"), "{line}");
        assert!(line.contains("v_jit="), "{line}");
        assert!(line.contains("rtt_ms="), "{line}");
        assert!(s.has_required_skew_metric());
    }

    #[test]
    fn apply_skew_decision_updates_snapshot() {
        let mut ctrl = SkewController::with_defaults();
        let d = ctrl.update(
            SkewSample {
                audio_playout_host_equiv_ms: 140.0,
                video_present_host_equiv_ms: 100.0,
            },
            0.0,
        );
        let mut s = SessionStats::default();
        s.apply_skew_decision(&d, ctrl.delay_offset_ms());
        assert!((s.skew_ms - 40.0).abs() < 1e-9);
        assert!(!s.skew_in_deadband);
        assert!(s.resample_ratio > 1.0);
        assert!(s.delay_offset_ms > 0.0);
    }
}

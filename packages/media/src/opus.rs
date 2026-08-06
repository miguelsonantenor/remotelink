//! Opus encode/decode wrappers.
//!
//! Native `libopus` is **not** linked on windows-gnu by default (painful
//! sysdeps). This module provides:
//! - Traits [`OpusEncoder`] / [`OpusDecoder`] for real or mock backends
//! - [`MockOpusEncoder`] / [`MockOpusDecoder`]: length-prefixed PCM packer for
//!   unit tests and CI (not RFC 6716 bitstream — TODO wire real Opus)
//!
//! TODO: enable optional `opus` crate feature when a MinGW-friendly libopus
//! is available in the CI image.
//!
//! # RTP timestamps and shared `t0`
//!
//! RemoteLink wire audio RTP timestamps **must** be derived from
//! [`crate::rtp_clock::RtpEpoch`] (shared audio/video epoch), not from a
//! free-running encoder counter alone. Prefer binding an epoch via
//! [`MockOpusEncoder::with_epoch`] / [`MockOpusEncoder::set_epoch`] so that
//! `OpusFrame.rtp_ts = epoch.audio_ts(pts_host_mono)`. On `media_restart`,
//! install the new epoch (or call [`MockOpusEncoder::reset_rtp_ts`]).

use crate::rtp_clock::RtpEpoch;
use crate::source::AudioFrame;
use std::time::Duration;

/// Errors from Opus-like encode/decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpusError {
    /// Invalid PCM length or channel layout.
    InvalidPcm(&'static str),
    /// Invalid compressed frame.
    InvalidPacket(&'static str),
    /// Unsupported configuration.
    Unsupported(&'static str),
}

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpusError::InvalidPcm(m) => write!(f, "invalid PCM: {m}"),
            OpusError::InvalidPacket(m) => write!(f, "invalid packet: {m}"),
            OpusError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for OpusError {}

/// One compressed Opus (or mock-Opus) packet with timing metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusFrame {
    /// Host-mono PTS of the first sample in this packet.
    pub pts_host_mono: Duration,
    /// RTP timestamp at 48 kHz. When the encoder is bound to an [`RtpEpoch`],
    /// this equals `epoch.audio_ts(pts_host_mono)` (shared-`t0` contract).
    pub rtp_ts: u32,
    /// Duration of PCM covered by this packet.
    pub duration: Duration,
    /// Channel count.
    pub channels: u16,
    /// Encoded payload bytes.
    pub data: Vec<u8>,
}

/// Encode PCM → compressed frames.
pub trait OpusEncoder {
    /// Encode one audio frame into a compressed packet.
    fn encode(&mut self, frame: &AudioFrame) -> Result<OpusFrame, OpusError>;
}

/// Decode compressed frames → PCM.
pub trait OpusDecoder {
    /// Decode one packet into an [`AudioFrame`].
    fn decode(&mut self, packet: &OpusFrame) -> Result<AudioFrame, OpusError>;
}

/// Magic for mock "Opus" packets so we never confuse them with real bitstreams.
const MOCK_MAGIC: &[u8; 4] = b"MOPU"; // Mock OPUs

/// Mock encoder: packs i16 PCM with a small header (not real Opus).
///
/// Header layout (little-endian):
/// - magic: b"MOPU"
/// - sample_rate: u32
/// - channels: u16
/// - frame_count: u32
/// - pcm i16le samples...
///
/// # RTP / epoch
///
/// Bind a session [`RtpEpoch`] with [`Self::with_epoch`] or [`Self::set_epoch`]
/// so payload timestamps follow the shared-`t0` contract. Without an epoch,
/// `rtp_ts` is a local free-running counter (packer unit tests only — **do not**
/// use that counter as the wire RTP timestamp in host/viewer paths).
#[derive(Debug, Clone, Default)]
pub struct MockOpusEncoder {
    /// Free-running fallback counter when no epoch is bound.
    next_rtp_ts: u32,
    /// Session epoch for shared-`t0` RTP mapping.
    epoch: Option<RtpEpoch>,
}

impl MockOpusEncoder {
    /// Create a new mock encoder (no epoch; free-running `rtp_ts` only).
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a mock encoder that stamps RTP from `epoch.audio_ts(pts)`.
    pub fn with_epoch(epoch: RtpEpoch) -> Self {
        Self {
            next_rtp_ts: 0,
            epoch: Some(epoch),
        }
    }

    /// Bind or replace the session epoch (e.g. after `media_restart`).
    pub fn set_epoch(&mut self, epoch: RtpEpoch) {
        self.epoch = Some(epoch);
        self.next_rtp_ts = 0;
    }

    /// Clear the bound epoch; subsequent encodes use the free-running counter.
    pub fn clear_epoch(&mut self) {
        self.epoch = None;
    }

    /// Bound epoch, if any.
    pub fn epoch(&self) -> Option<RtpEpoch> {
        self.epoch
    }

    /// Seed/reset the free-running fallback counter (and after `clear_epoch`).
    pub fn reset_rtp_ts(&mut self, ts: u32) {
        self.next_rtp_ts = ts;
    }

    /// Seed the free-running counter from `epoch.audio_ts(pts)` when bound.
    pub fn seed_rtp_ts_from_pts(&mut self, pts: Duration) {
        if let Some(epoch) = self.epoch {
            self.next_rtp_ts = epoch.audio_ts(pts);
        }
    }
}

impl OpusEncoder for MockOpusEncoder {
    fn encode(&mut self, frame: &AudioFrame) -> Result<OpusFrame, OpusError> {
        if frame.channels == 0 {
            return Err(OpusError::InvalidPcm("channels == 0"));
        }
        if !frame.pcm_s16.len().is_multiple_of(frame.channels as usize) {
            return Err(OpusError::InvalidPcm(
                "pcm length not divisible by channels",
            ));
        }
        if frame.sample_rate == 0 {
            return Err(OpusError::InvalidPcm("sample_rate == 0"));
        }

        let frame_count = frame.frame_count() as u32;
        let mut data = Vec::with_capacity(14 + frame.pcm_s16.len() * 2);
        data.extend_from_slice(MOCK_MAGIC);
        data.extend_from_slice(&frame.sample_rate.to_le_bytes());
        data.extend_from_slice(&frame.channels.to_le_bytes());
        data.extend_from_slice(&frame_count.to_le_bytes());
        for s in &frame.pcm_s16 {
            data.extend_from_slice(&s.to_le_bytes());
        }

        // Prefer shared-t0 epoch mapping for wire-correct RTP timestamps.
        let (rtp_ts, advance) = if let Some(epoch) = self.epoch {
            let ts = epoch.audio_ts(frame.pts_host_mono);
            let next_pts = frame.pts_host_mono + frame.duration();
            let next_ts = epoch.audio_ts(next_pts);
            let adv = next_ts.wrapping_sub(ts);
            (ts, adv)
        } else {
            let advance = if frame.sample_rate == 48_000 {
                frame_count
            } else {
                let dur = frame.duration();
                (dur.as_secs_f64() * 48_000.0).round() as u32
            };
            let ts = self.next_rtp_ts;
            (ts, advance)
        };
        self.next_rtp_ts = rtp_ts.wrapping_add(advance);

        Ok(OpusFrame {
            pts_host_mono: frame.pts_host_mono,
            rtp_ts,
            duration: frame.duration(),
            channels: frame.channels,
            data,
        })
    }
}

/// Mock decoder for [`MockOpusEncoder`] packets.
#[derive(Debug, Clone, Default)]
pub struct MockOpusDecoder;

impl MockOpusDecoder {
    /// Create a new mock decoder.
    pub fn new() -> Self {
        Self
    }
}

impl OpusDecoder for MockOpusDecoder {
    fn decode(&mut self, packet: &OpusFrame) -> Result<AudioFrame, OpusError> {
        if packet.data.len() < 14 {
            return Err(OpusError::InvalidPacket("too short"));
        }
        if &packet.data[0..4] != MOCK_MAGIC {
            return Err(OpusError::InvalidPacket(
                "not a mock Opus packet (real Opus TODO)",
            ));
        }
        let sample_rate = u32::from_le_bytes(packet.data[4..8].try_into().unwrap());
        let channels = u16::from_le_bytes(packet.data[8..10].try_into().unwrap());
        let frame_count = u32::from_le_bytes(packet.data[10..14].try_into().unwrap());
        let expected = 14 + (frame_count as usize) * (channels as usize) * 2;
        if packet.data.len() != expected {
            return Err(OpusError::InvalidPacket("pcm size mismatch"));
        }
        let mut pcm = Vec::with_capacity(frame_count as usize * channels as usize);
        let mut off = 14;
        while off + 2 <= packet.data.len() {
            let s = i16::from_le_bytes([packet.data[off], packet.data[off + 1]]);
            pcm.push(s);
            off += 2;
        }
        Ok(AudioFrame::from_s16(
            packet.pts_host_mono,
            sample_rate,
            channels,
            pcm,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp_clock::RtpEpoch;

    #[test]
    fn mock_roundtrip_pcm() {
        let pcm: Vec<i16> = (0..480).map(|i| (i * 13) as i16).collect();
        let frame = AudioFrame::from_s16(Duration::from_millis(20), 48_000, 1, pcm.clone());
        let mut enc = MockOpusEncoder::new();
        let mut dec = MockOpusDecoder::new();
        let pkt = enc.encode(&frame).unwrap();
        assert_eq!(pkt.duration, Duration::from_millis(10));
        assert_eq!(pkt.pts_host_mono, Duration::from_millis(20));
        let out = dec.decode(&pkt).unwrap();
        assert_eq!(out.pcm_s16, pcm);
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(out.pts_host_mono, Duration::from_millis(20));
    }

    #[test]
    fn mock_rtp_ts_advances_by_480_per_10ms() {
        let mut enc = MockOpusEncoder::new();
        let frame = AudioFrame::from_s16(Duration::ZERO, 48_000, 1, vec![0i16; 480]);
        let p0 = enc.encode(&frame).unwrap();
        let p1 = enc.encode(&frame).unwrap();
        assert_eq!(p0.rtp_ts, 0);
        assert_eq!(p1.rtp_ts, 480);
    }

    #[test]
    fn with_epoch_matches_shared_t0() {
        let t0 = Duration::from_millis(500);
        let epoch = RtpEpoch::new(t0);
        let mut enc = MockOpusEncoder::with_epoch(epoch);
        let pts = t0 + Duration::from_millis(30);
        let frame = AudioFrame::from_s16(pts, 48_000, 1, vec![0i16; 480]);
        let pkt = enc.encode(&frame).unwrap();
        assert_eq!(pkt.rtp_ts, epoch.audio_ts(pts));
        assert_eq!(pkt.rtp_ts, 480 * 3); // 30 ms @ 48 kHz
    }

    #[test]
    fn set_epoch_on_media_restart_resets_mapping() {
        let t0 = Duration::from_secs(1);
        let mut enc = MockOpusEncoder::with_epoch(RtpEpoch::new(t0));
        let f0 = AudioFrame::from_s16(t0 + Duration::from_millis(10), 48_000, 1, vec![0i16; 480]);
        assert_eq!(enc.encode(&f0).unwrap().rtp_ts, 480);

        // media_restart: new epoch at later host mono
        let t1 = t0 + Duration::from_secs(5);
        enc.set_epoch(RtpEpoch::new(t1));
        let f1 = AudioFrame::from_s16(t1, 48_000, 1, vec![0i16; 480]);
        assert_eq!(enc.encode(&f1).unwrap().rtp_ts, 0);
    }

    #[test]
    fn reset_rtp_ts_seeds_free_running_counter() {
        let mut enc = MockOpusEncoder::new();
        enc.reset_rtp_ts(960);
        let frame = AudioFrame::from_s16(Duration::ZERO, 48_000, 1, vec![0i16; 480]);
        assert_eq!(enc.encode(&frame).unwrap().rtp_ts, 960);
    }

    #[test]
    fn reject_bad_magic() {
        let mut dec = MockOpusDecoder::new();
        let pkt = OpusFrame {
            pts_host_mono: Duration::ZERO,
            rtp_ts: 0,
            duration: Duration::from_millis(10),
            channels: 1,
            data: vec![0u8; 20],
        };
        assert!(matches!(dec.decode(&pkt), Err(OpusError::InvalidPacket(_))));
    }

    #[test]
    fn stereo_roundtrip() {
        let pcm = vec![1i16, 2, 3, 4, 5, 6]; // 3 frames stereo
        let frame = AudioFrame::from_s16(Duration::ZERO, 48_000, 2, pcm.clone());
        let mut enc = MockOpusEncoder::new();
        let mut dec = MockOpusDecoder::new();
        let out = dec.decode(&enc.encode(&frame).unwrap()).unwrap();
        assert_eq!(out.pcm_s16, pcm);
        assert_eq!(out.channels, 2);
    }
}

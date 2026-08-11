//! Audio playout queue + sink trait (jitter-ready; mock sink for CI).

use std::collections::VecDeque;
use std::time::Duration;

use remotelink_media::{
    AudioFrame, MockOpusDecoder, OpusDecoder, OpusFrame, SampleFormat, AUDIO_CLOCK_HZ,
};
use remotelink_net::AudioPacket;

use crate::error::{Result, ViewerError};

/// One audio unit ready for device playout or test drain.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayoutPacket {
    /// Host-mono PTS of the first sample.
    pub pts_host_mono: Duration,
    /// Decoded (or pass-through) PCM frame.
    pub frame: AudioFrame,
}

/// Sink that consumes decoded PCM for device playout or test capture.
///
/// Real CPAL output is optional and feature-gated in `apps/viewer`; CI and
/// unit tests use [`MockAudioPlayoutSink`].
pub trait AudioPlayoutSink: Send {
    /// Push one decoded packet for playout.
    fn push(&mut self, packet: &PlayoutPacket) -> Result<()>;

    /// Flush any buffered samples (device underrun / session close).
    fn flush(&mut self) {}

    /// Human-readable sink name for stats / HUD.
    fn name(&self) -> &'static str {
        "unknown"
    }
}

/// Records playout packets in memory (headless / CI).
#[derive(Debug, Default, Clone)]
pub struct MockAudioPlayoutSink {
    /// Packets accepted for "playout".
    pub packets: Vec<PlayoutPacket>,
    /// Total PCM samples (all channels) accepted.
    pub samples_played: u64,
}

impl MockAudioPlayoutSink {
    /// Create an empty mock sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain recorded packets.
    pub fn drain(&mut self) -> Vec<PlayoutPacket> {
        std::mem::take(&mut self.packets)
    }

    /// Number of packets accepted.
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// True when no packets recorded.
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

impl AudioPlayoutSink for MockAudioPlayoutSink {
    fn push(&mut self, packet: &PlayoutPacket) -> Result<()> {
        self.samples_played = self
            .samples_played
            .saturating_add(packet.frame.pcm_s16.len() as u64);
        self.packets.push(packet.clone());
        Ok(())
    }

    fn flush(&mut self) {
        // no-op for mock
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

/// Discarding sink (no device, no record).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullAudioPlayoutSink;

impl AudioPlayoutSink for NullAudioPlayoutSink {
    fn push(&mut self, _packet: &PlayoutPacket) -> Result<()> {
        Ok(())
    }

    fn name(&self) -> &'static str {
        "null"
    }
}

/// Bounded queue of decoded audio for playout.
///
/// Uses [`MockOpusDecoder`] when the packet carries the mock MOPU payload;
/// otherwise treats `AudioPacket.data` as raw interleaved i16 PCM (test path).
#[derive(Debug)]
pub struct AudioPlayoutQueue {
    queue: VecDeque<PlayoutPacket>,
    capacity: usize,
    decoder: MockOpusDecoder,
    /// Packets dropped due to capacity.
    dropped: u64,
    /// Packets successfully enqueued after decode.
    enqueued: u64,
    /// Packets successfully decoded from MOPU.
    mock_opus_decoded: u64,
}

impl Default for AudioPlayoutQueue {
    fn default() -> Self {
        Self::new(64)
    }
}

impl AudioPlayoutQueue {
    /// Create a queue that holds at most `capacity` playout packets.
    pub fn new(capacity: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(capacity.clamp(1, 256)),
            capacity: capacity.max(1),
            decoder: MockOpusDecoder::new(),
            dropped: 0,
            enqueued: 0,
            mock_opus_decoded: 0,
        }
    }

    /// Current depth.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Packets dropped on overflow.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Packets enqueued.
    pub fn enqueued(&self) -> u64 {
        self.enqueued
    }

    /// MOPU packets decoded.
    pub fn mock_opus_decoded(&self) -> u64 {
        self.mock_opus_decoded
    }

    /// Push an inbound transport audio packet (decode + enqueue).
    pub fn push_packet(&mut self, packet: &AudioPacket) -> Result<()> {
        let (frame, was_mopu) = decode_audio_packet(&mut self.decoder, packet)?;
        if was_mopu {
            self.mock_opus_decoded = self.mock_opus_decoded.saturating_add(1);
        }
        self.push_frame(frame);
        Ok(())
    }

    /// Enqueue an already-decoded frame (tests / alternate decoders).
    pub fn push_frame(&mut self, frame: AudioFrame) {
        while self.queue.len() >= self.capacity {
            self.queue.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        let pts = frame.pts_host_mono;
        self.queue.push_back(PlayoutPacket {
            pts_host_mono: pts,
            frame,
        });
        self.enqueued = self.enqueued.saturating_add(1);
    }

    /// Pop the next packet for playout (FIFO).
    pub fn pop(&mut self) -> Option<PlayoutPacket> {
        self.queue.pop_front()
    }

    /// Drain all queued packets (headless tests).
    pub fn drain(&mut self) -> Vec<PlayoutPacket> {
        self.queue.drain(..).collect()
    }

    /// Peek PTS of the next packet without removing it.
    pub fn peek_pts(&self) -> Option<Duration> {
        self.queue.front().map(|p| p.pts_host_mono)
    }

    /// Pop up to `max` packets into `sink`.
    pub fn pump_to_sink(&mut self, sink: &mut dyn AudioPlayoutSink, max: usize) -> Result<usize> {
        let mut n = 0;
        while n < max {
            let Some(pkt) = self.pop() else {
                break;
            };
            sink.push(&pkt)?;
            n += 1;
        }
        Ok(n)
    }
}

fn decode_audio_packet(
    decoder: &mut MockOpusDecoder,
    packet: &AudioPacket,
) -> Result<(AudioFrame, bool)> {
    if packet.data.starts_with(b"MOPU") {
        let opus = OpusFrame {
            pts_host_mono: packet.pts_host_mono,
            rtp_ts: packet.rtp_ts.unwrap_or(0),
            duration: Duration::from_millis(10),
            channels: packet.channels,
            data: packet.data.clone(),
        };
        let frame = decoder
            .decode(&opus)
            .map_err(|e| ViewerError::Media(e.to_string()))?;
        Ok((frame, true))
    } else {
        // Raw i16le PCM passthrough for simple mock AudioPacket payloads.
        if !packet.data.len().is_multiple_of(2) {
            return Err(ViewerError::Media(
                "raw audio payload length not multiple of 2".into(),
            ));
        }
        if packet.channels == 0 {
            return Err(ViewerError::Media("audio channels == 0".into()));
        }
        let mut pcm = Vec::with_capacity(packet.data.len() / 2);
        for chunk in packet.data.chunks_exact(2) {
            pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
        }
        let _ = AUDIO_CLOCK_HZ; // document default clock domain
        Ok((
            AudioFrame {
                pts_host_mono: packet.pts_host_mono,
                sample_rate: if packet.sample_rate == 0 {
                    48_000
                } else {
                    packet.sample_rate
                },
                channels: packet.channels,
                format: SampleFormat::S16Le,
                pcm_s16: pcm,
            },
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_media::{AudioSource, RtpEpoch};
    use remotelink_media::{MockOpusEncoder, OpusEncoder, SyntheticAudioTone};

    #[test]
    fn queue_raw_pcm_packet() {
        let mut q = AudioPlayoutQueue::new(8);
        let pcm: Vec<u8> = [100i16, -100i16]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        q.push_packet(&AudioPacket {
            pts_host_mono: Duration::from_millis(10),
            rtp_ts: Some(480),
            sample_rate: 48_000,
            channels: 1,
            data: pcm,
        })
        .unwrap();
        assert_eq!(q.len(), 1);
        let p = q.pop().unwrap();
        assert_eq!(p.frame.pcm_s16, vec![100, -100]);
    }

    #[test]
    fn queue_mock_opus_roundtrip() {
        let t0 = Duration::from_millis(0);
        let mut tone = SyntheticAudioTone::default_a440(t0).with_max_packets(1);
        let af = tone.next_frame().unwrap().unwrap();
        let mut enc = MockOpusEncoder::with_epoch(RtpEpoch::new(t0));
        let pkt = enc.encode(&af).unwrap();

        let mut q = AudioPlayoutQueue::new(4);
        q.push_packet(&AudioPacket {
            pts_host_mono: pkt.pts_host_mono,
            rtp_ts: Some(pkt.rtp_ts),
            sample_rate: af.sample_rate,
            channels: af.channels,
            data: pkt.data,
        })
        .unwrap();
        let out = q.pop().unwrap();
        assert_eq!(out.frame.pcm_s16, af.pcm_s16);
        assert_eq!(q.mock_opus_decoded(), 1);
    }

    #[test]
    fn capacity_drops_oldest() {
        let mut q = AudioPlayoutQueue::new(2);
        for i in 0..3 {
            q.push_frame(AudioFrame {
                pts_host_mono: Duration::from_millis(i * 10),
                sample_rate: 48_000,
                channels: 1,
                format: SampleFormat::S16Le,
                pcm_s16: vec![i as i16],
            });
        }
        assert_eq!(q.len(), 2);
        assert_eq!(q.dropped(), 1);
        assert_eq!(q.pop().unwrap().frame.pcm_s16, vec![1]);
    }

    #[test]
    fn mock_sink_receives_pcm() {
        let mut q = AudioPlayoutQueue::new(4);
        q.push_frame(AudioFrame::from_s16(
            Duration::from_millis(5),
            48_000,
            1,
            vec![7i16; 48],
        ));
        let mut sink = MockAudioPlayoutSink::new();
        assert_eq!(q.pump_to_sink(&mut sink, 8).unwrap(), 1);
        assert_eq!(sink.len(), 1);
        assert_eq!(sink.samples_played, 48);
        assert_eq!(sink.name(), "mock");
    }
}

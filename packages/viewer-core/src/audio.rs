//! Audio playout queue (jitter-ready; headless drain for tests).

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
            queue: VecDeque::with_capacity(capacity.clamp(1, 16)),
            capacity: capacity.max(1),
            decoder: MockOpusDecoder::new(),
            dropped: 0,
            enqueued: 0,
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

    /// Push an inbound transport audio packet (decode + enqueue).
    pub fn push_packet(&mut self, packet: &AudioPacket) -> Result<()> {
        let frame = decode_audio_packet(&mut self.decoder, packet)?;
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
}

fn decode_audio_packet(decoder: &mut MockOpusDecoder, packet: &AudioPacket) -> Result<AudioFrame> {
    if packet.data.starts_with(b"MOPU") {
        let opus = OpusFrame {
            pts_host_mono: packet.pts_host_mono,
            rtp_ts: packet.rtp_ts.unwrap_or(0),
            duration: Duration::from_millis(10),
            channels: packet.channels,
            data: packet.data.clone(),
        };
        decoder
            .decode(&opus)
            .map_err(|e| ViewerError::Media(e.to_string()))
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
        Ok(AudioFrame {
            pts_host_mono: packet.pts_host_mono,
            sample_rate: if packet.sample_rate == 0 {
                48_000
            } else {
                packet.sample_rate
            },
            channels: packet.channels,
            format: SampleFormat::S16Le,
            pcm_s16: pcm,
        })
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
}

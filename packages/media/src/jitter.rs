//! Simple adaptive jitter buffer abstraction.
//!
//! Default WAN targets: 20–40 ms. Packets are ordered by PTS for playout; the
//! target delay adapts slowly toward observed inter-arrival jitter within
//! `[min_target, max_target]`.
//!
//! # Adaptation and reordering
//!
//! Jitter adaptation uses **consecutive RTP sequence numbers** when provided
//! via [`JitterBuffer::push_with_seq`] (RFC 3550-style transit). Without a
//! sequence, adaptation runs only when PTS is strictly increasing so reordered
//! pushes cannot collapse `d_pts` via `saturating_sub` and inflate the target.
//! Prefer `push_with_seq` once real RTP is wired.

use std::collections::VecDeque;
use std::time::Duration;

/// Configuration for [`JitterBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitterConfig {
    /// Lower bound on adaptive target delay.
    pub min_target: Duration,
    /// Upper bound on adaptive target delay.
    pub max_target: Duration,
    /// Initial target delay (clamped into `[min_target, max_target]`).
    pub initial_target: Duration,
}

impl JitterConfig {
    /// WAN defaults: 20–40 ms target range, start at 30 ms.
    pub fn wan_default() -> Self {
        Self {
            min_target: Duration::from_millis(20),
            max_target: Duration::from_millis(40),
            initial_target: Duration::from_millis(30),
        }
    }

    /// LAN low-latency video profile: 10–15 ms.
    pub fn lan_video() -> Self {
        Self {
            min_target: Duration::from_millis(10),
            max_target: Duration::from_millis(15),
            initial_target: Duration::from_millis(12),
        }
    }

    /// LAN audio profile: 15–25 ms.
    pub fn lan_audio() -> Self {
        Self {
            min_target: Duration::from_millis(15),
            max_target: Duration::from_millis(25),
            initial_target: Duration::from_millis(20),
        }
    }
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self::wan_default()
    }
}

/// Snapshot of buffer statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JitterStats {
    /// Packets currently queued.
    pub depth: usize,
    /// Packets dropped as too late or overflow.
    pub dropped: u64,
    /// Packets successfully released for playout.
    pub released: u64,
    /// Packets pushed.
    pub received: u64,
}

/// A timestamped packet held in the jitter buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedPacket<T> {
    /// Optional RTP sequence number.
    pub seq: Option<u16>,
    /// Playout-relevant timestamp (e.g. host-equivalent of RTP PTS).
    pub pts: Duration,
    /// Receive time on the local (viewer) clock.
    pub recv_time: Duration,
    /// Payload.
    pub payload: T,
}

/// Adaptive jitter buffer parameterized by payload type `T`.
#[derive(Debug, Clone)]
pub struct JitterBuffer<T> {
    cfg: JitterConfig,
    /// Current adaptive target delay.
    target: Duration,
    queue: VecDeque<BufferedPacket<T>>,
    stats: JitterStats,
    /// Last packet used for adaptation (receive time).
    last_recv: Option<Duration>,
    /// Last packet used for adaptation (PTS).
    last_pts: Option<Duration>,
    /// Last RTP sequence used for adaptation.
    last_seq: Option<u16>,
    /// EWMA of |transit| variation in microseconds.
    jitter_us_ewma: u64,
}

impl<T> JitterBuffer<T> {
    /// Create a buffer with the given config.
    pub fn new(cfg: JitterConfig) -> Self {
        let target = clamp_duration(cfg.initial_target, cfg.min_target, cfg.max_target);
        Self {
            cfg,
            target,
            queue: VecDeque::new(),
            stats: JitterStats::default(),
            last_recv: None,
            last_pts: None,
            last_seq: None,
            jitter_us_ewma: 0,
        }
    }

    /// Current target delay.
    pub fn target(&self) -> Duration {
        self.target
    }

    /// Statistics snapshot.
    pub fn stats(&self) -> JitterStats {
        let mut s = self.stats;
        s.depth = self.queue.len();
        s
    }

    /// Number of queued packets.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Push a packet without an RTP sequence number.
    ///
    /// Adaptation updates only when `pts` is strictly after the last adapted
    /// PTS (skips reorders). Prefer [`Self::push_with_seq`] for real RTP.
    pub fn push(&mut self, pts: Duration, recv_time: Duration, payload: T) {
        self.push_inner(None, pts, recv_time, payload);
    }

    /// Push a packet with an RTP sequence number (preferred for adaptation).
    ///
    /// Adaptation updates only for consecutive sequences
    /// (`seq == last_seq.wrapping_add(1)`). Reordered or gapped packets are
    /// still queued by PTS but do not corrupt the jitter EWMA.
    pub fn push_with_seq(&mut self, seq: u16, pts: Duration, recv_time: Duration, payload: T) {
        self.push_inner(Some(seq), pts, recv_time, payload);
    }

    fn push_inner(&mut self, seq: Option<u16>, pts: Duration, recv_time: Duration, payload: T) {
        self.stats.received += 1;
        self.adapt(seq, pts, recv_time);

        let pkt = BufferedPacket {
            seq,
            pts,
            recv_time,
            payload,
        };
        // Insert sorted by pts ascending.
        let pos = self
            .queue
            .iter()
            .position(|p| p.pts > pts)
            .unwrap_or(self.queue.len());
        self.queue.insert(pos, pkt);
    }

    /// Pop the next packet if it has been held at least `target` beyond its
    /// receive time (viewer-clock hold).
    ///
    /// `now` is the local viewer clock and must be comparable to `recv_time`.
    pub fn pop_ready(&mut self, now: Duration) -> Option<BufferedPacket<T>> {
        let front = self.queue.front()?;
        let held = now.saturating_sub(front.recv_time);
        if held >= self.target {
            self.stats.released += 1;
            return self.queue.pop_front();
        }
        None
    }

    /// Drop packets that have been waiting longer than `target + late_threshold`
    /// past receive (too late to play).
    pub fn drop_late(&mut self, now: Duration, late_threshold: Duration) {
        while let Some(front) = self.queue.front() {
            let held = now.saturating_sub(front.recv_time);
            if held > self.target.saturating_add(late_threshold) {
                self.queue.pop_front();
                self.stats.dropped += 1;
            } else {
                break;
            }
        }
    }

    /// Force-pop the front packet regardless of readiness (underrun recovery).
    pub fn pop_front(&mut self) -> Option<BufferedPacket<T>> {
        let p = self.queue.pop_front()?;
        self.stats.released += 1;
        Some(p)
    }

    fn adapt(&mut self, seq: Option<u16>, pts: Duration, recv_time: Duration) {
        match seq {
            Some(s) => {
                if let Some(last) = self.last_seq {
                    if s != last.wrapping_add(1) {
                        // Reorder or gap: do not update EWMA from non-consecutive seq.
                        // Still remember the latest *seen* seq only when it advances
                        // wrap-aware? Keep last_seq as last *adapted* consecutive chain
                        // head — on gap, resync so future consecutive pairs work.
                        self.last_seq = Some(s);
                        self.last_recv = Some(recv_time);
                        self.last_pts = Some(pts);
                        return;
                    }
                }
                self.apply_transit(pts, recv_time);
                self.last_seq = Some(s);
                self.last_recv = Some(recv_time);
                self.last_pts = Some(pts);
            }
            None => {
                // No seq: require strictly increasing PTS to avoid reorder inflation.
                if let Some(lp) = self.last_pts {
                    if pts <= lp {
                        return;
                    }
                }
                self.apply_transit(pts, recv_time);
                self.last_recv = Some(recv_time);
                self.last_pts = Some(pts);
            }
        }
    }

    fn apply_transit(&mut self, pts: Duration, recv_time: Duration) {
        if let (Some(lr), Some(lp)) = (self.last_recv, self.last_pts) {
            // transit ≈ (recv - prev_recv) - (pts - prev_pts)
            let d_recv = recv_time.saturating_sub(lr).as_micros() as i64;
            let d_pts = pts.saturating_sub(lp).as_micros() as i64;
            let transit = d_recv - d_pts;
            let abs = transit.unsigned_abs();
            // RFC 3550-ish EWMA: j = j + (|D| - j) / 16
            self.jitter_us_ewma = self
                .jitter_us_ewma
                .saturating_add(abs.saturating_sub(self.jitter_us_ewma) / 16);

            // Map jitter EWMA to a target: ~2× jitter, clamped.
            let desired_us = self.jitter_us_ewma.saturating_mul(2);
            let desired = Duration::from_micros(desired_us);
            // Slow adapt: move target 1/8 of the way toward desired each packet.
            let t_us = self.target.as_micros() as i64;
            let d_us = desired.as_micros() as i64;
            let next = t_us + (d_us - t_us) / 8;
            self.target = clamp_duration(
                Duration::from_micros(next.clamp(0, i64::MAX) as u64),
                self.cfg.min_target,
                self.cfg.max_target,
            );
        }
    }
}

fn clamp_duration(v: Duration, min: Duration, max: Duration) -> Duration {
    if v < min {
        min
    } else if v > max {
        max
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wan_defaults_in_20_40ms() {
        let cfg = JitterConfig::wan_default();
        assert_eq!(cfg.min_target, Duration::from_millis(20));
        assert_eq!(cfg.max_target, Duration::from_millis(40));
        let jb: JitterBuffer<u8> = JitterBuffer::new(cfg);
        assert!(jb.target() >= Duration::from_millis(20));
        assert!(jb.target() <= Duration::from_millis(40));
    }

    #[test]
    fn packets_ordered_by_pts() {
        let mut jb = JitterBuffer::new(JitterConfig::wan_default());
        jb.push(Duration::from_millis(30), Duration::from_millis(100), 3u8);
        jb.push(Duration::from_millis(10), Duration::from_millis(101), 1u8);
        jb.push(Duration::from_millis(20), Duration::from_millis(102), 2u8);
        assert_eq!(jb.pop_front().unwrap().payload, 1);
        assert_eq!(jb.pop_front().unwrap().payload, 2);
        assert_eq!(jb.pop_front().unwrap().payload, 3);
    }

    #[test]
    fn pop_ready_respects_target_hold() {
        let mut jb = JitterBuffer::new(JitterConfig {
            min_target: Duration::from_millis(20),
            max_target: Duration::from_millis(40),
            initial_target: Duration::from_millis(20),
        });
        jb.push(Duration::from_millis(0), Duration::from_millis(1000), 1u8);
        // Only held 10 ms — not ready.
        assert!(jb.pop_ready(Duration::from_millis(1010)).is_none());
        // Held 20 ms — ready.
        assert_eq!(
            jb.pop_ready(Duration::from_millis(1020)).unwrap().payload,
            1
        );
    }

    #[test]
    fn drop_late_increments_stats() {
        let mut jb = JitterBuffer::new(JitterConfig {
            min_target: Duration::from_millis(20),
            max_target: Duration::from_millis(40),
            initial_target: Duration::from_millis(20),
        });
        jb.push(Duration::from_millis(0), Duration::from_millis(0), 9u8);
        // now far past pts+target
        jb.drop_late(Duration::from_millis(500), Duration::from_millis(50));
        assert_eq!(jb.stats().dropped, 1);
        assert!(jb.is_empty());
    }

    #[test]
    fn target_stays_within_bounds_under_jitter() {
        let mut jb = JitterBuffer::new(JitterConfig::wan_default());
        // Inject irregular arrivals.
        let mut recv = Duration::from_millis(0);
        for i in 0..50u64 {
            let pts = Duration::from_millis(i * 10);
            // Alternate early/late arrivals.
            let jitter = if i % 2 == 0 { 0 } else { 25 };
            recv = recv + Duration::from_millis(10 + jitter);
            jb.push(pts, recv, i);
        }
        assert!(jb.target() >= Duration::from_millis(20));
        assert!(jb.target() <= Duration::from_millis(40));
    }

    #[test]
    fn reordered_pts_does_not_inflate_target_without_seq() {
        let mut jb = JitterBuffer::new(JitterConfig {
            min_target: Duration::from_millis(20),
            max_target: Duration::from_millis(40),
            initial_target: Duration::from_millis(20),
        });
        let baseline = jb.target();
        // In-order pair to arm last_pts/last_recv.
        jb.push(Duration::from_millis(0), Duration::from_millis(1000), 0u8);
        jb.push(Duration::from_millis(10), Duration::from_millis(1010), 1u8);
        // Reordered late arrival of an older PTS — must not adapt.
        jb.push(Duration::from_millis(5), Duration::from_millis(2000), 2u8);
        // Target should stay near baseline (min clamp), not jump from bogus transit.
        assert_eq!(jb.target(), baseline);
    }

    #[test]
    fn seq_gap_skips_adapt_but_still_queues() {
        let mut jb = JitterBuffer::new(JitterConfig {
            min_target: Duration::from_millis(20),
            max_target: Duration::from_millis(40),
            initial_target: Duration::from_millis(20),
        });
        jb.push_with_seq(
            10,
            Duration::from_millis(0),
            Duration::from_millis(100),
            0u8,
        );
        jb.push_with_seq(
            11,
            Duration::from_millis(10),
            Duration::from_millis(110),
            1u8,
        );
        let after_consec = jb.target();
        // Gap: seq 13 after 11 — no EWMA update from this pair.
        jb.push_with_seq(
            13,
            Duration::from_millis(30),
            Duration::from_millis(500),
            2u8,
        );
        assert_eq!(jb.target(), after_consec);
        assert_eq!(jb.len(), 3);
        // Next consecutive after resync (13 → 14) may adapt again.
        jb.push_with_seq(
            14,
            Duration::from_millis(40),
            Duration::from_millis(510),
            3u8,
        );
        assert!(jb.target() >= Duration::from_millis(20));
        assert!(jb.target() <= Duration::from_millis(40));
    }
}

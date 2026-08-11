//! `drop_packets` chaos: simulate loss by randomly skipping mock-peer sends.
//!
//! No real network — uses [`remotelink_net::MockPeerPair`].

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use remotelink_net::{
    AudioPacket, ConnectionState, MockPeerPair, NaluFormat, PeerTransport, SharedRecording,
    VideoNalu,
};
use serde_json::json;

use crate::config::{ChaosProfileConfig, ProfileName, Severity};
use crate::profiles::{ProfileOutcome, ProfileStatus};

/// Run drop_packets profile.
pub fn run_drop_packets(cfg: &ChaosProfileConfig) -> ProfileOutcome {
    let seed = cfg.seed;
    let mut rng = StdRng::seed_from_u64(seed);
    let drop_rate = cfg.drop_rate.clamp(0.0, 1.0);
    let n = cfg.iterations.max(1);

    let mut pair = MockPeerPair::new();
    let rec = SharedRecording::new();
    pair.peer_b.set_callbacks(Box::new(rec.clone()));

    if let Err(e) = pair.handshake() {
        return fail(seed, format!("handshake failed: {e}"));
    }
    if pair.peer_a.connection_state() != ConnectionState::Connected {
        return fail(seed, "host not connected after handshake".into());
    }

    let mut attempted = 0u32;
    let mut dropped = 0u32;
    let mut sent = 0u32;
    let mut send_errors = 0u32;

    for i in 0..n {
        attempted += 1;
        // Random skip = simulated packet drop before wire.
        if rng.gen::<f64>() < drop_rate {
            dropped += 1;
            continue;
        }
        let is_video = i % 2 == 0;
        let result = if is_video {
            pair.peer_a.send_video_nalu(VideoNalu {
                pts_host_mono: Duration::from_millis(u64::from(i) * 33),
                rtp_ts: Some(i * 3000),
                keyframe: i % 30 == 0,
                format: NaluFormat::AnnexB,
                data: vec![0, 0, 0, 1, 0x65, (i % 256) as u8],
            })
        } else {
            pair.peer_a.send_audio(AudioPacket {
                pts_host_mono: Duration::from_millis(u64::from(i) * 20),
                rtp_ts: Some(i * 960),
                sample_rate: 48_000,
                channels: 2,
                data: vec![0xde, 0xad, (i % 256) as u8, 0xbe],
            })
        };
        match result {
            Ok(()) => sent += 1,
            Err(_) => send_errors += 1,
        }
    }

    // Deliver whatever made it onto the wire.
    if let Err(e) = pair.flush() {
        return fail(seed, format!("flush failed: {e}"));
    }

    let received = rec.snapshot().tracks.len() as u32;
    // Receiver should see roughly `sent` tracks (mock is reliable once sent).
    let ok = send_errors == 0 && received == sent && dropped + sent == attempted;

    let metrics = json!({
        "attempted": attempted,
        "dropped": dropped,
        "sent": sent,
        "received": received,
        "send_errors": send_errors,
        "drop_rate_target": drop_rate,
        "drop_rate_actual": if attempted > 0 {
            f64::from(dropped) / f64::from(attempted)
        } else {
            0.0
        },
    });

    if ok {
        ProfileOutcome {
            profile: ProfileName::DropPackets,
            root_seed: None,
            seed,
            status: ProfileStatus::Pass,
            severity: None,
            summary: format!(
                "flaky send: dropped {dropped}/{attempted}, delivered {received}/{sent}"
            ),
            metrics,
            repro: Some(format!("seed={seed} drop_rate={drop_rate} iterations={n}")),
        }
    } else {
        ProfileOutcome {
            profile: ProfileName::DropPackets,
            root_seed: None,
            seed,
            status: ProfileStatus::Fail,
            severity: Some(Severity::High),
            summary: format!(
                "invariant broken: sent={sent} received={received} errors={send_errors} dropped={dropped}"
            ),
            metrics,
            repro: Some(format!("seed={seed} drop_rate={drop_rate} iterations={n}")),
        }
    }
}

fn fail(seed: u64, summary: String) -> ProfileOutcome {
    ProfileOutcome {
        profile: ProfileName::DropPackets,
        root_seed: None,
        seed,
        status: ProfileStatus::Fail,
        severity: Some(Severity::High),
        summary,
        metrics: json!({}),
        repro: Some(format!("seed={seed}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_packets_is_deterministic() {
        let cfg = ChaosProfileConfig {
            seed: 99,
            iterations: 64,
            drop_rate: 0.3,
            ..ChaosProfileConfig::default()
        };
        let a = run_drop_packets(&cfg);
        let b = run_drop_packets(&cfg);
        assert_eq!(a.status, ProfileStatus::Pass);
        assert_eq!(a.metrics, b.metrics);
        assert_eq!(a.summary, b.summary);
    }

    #[test]
    fn zero_drop_delivers_all() {
        let cfg = ChaosProfileConfig {
            seed: 1,
            iterations: 32,
            drop_rate: 0.0,
            ..ChaosProfileConfig::default()
        };
        let o = run_drop_packets(&cfg);
        assert_eq!(o.status, ProfileStatus::Pass);
        assert_eq!(o.metrics["dropped"], 0);
        assert_eq!(o.metrics["sent"], 32);
        assert_eq!(o.metrics["received"], 32);
    }
}

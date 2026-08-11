//! `reconnect` chaos: session teardown + restart on mock peer pair.
//!
//! Simulates host/viewer hangup then a fresh handshake (unit harness, no network).

use std::time::Duration;

use remotelink_net::{
    AudioPacket, ConnectionState, DataMessage, MockPeerPair, PeerTransport, SharedRecording,
};
use serde_json::json;

use crate::config::{ChaosProfileConfig, ProfileName, Severity};
use crate::profiles::{ProfileOutcome, ProfileStatus};

/// Run reconnect profile for `cfg.reconnect_cycles` cycles.
pub fn run_reconnect(cfg: &ChaosProfileConfig) -> ProfileOutcome {
    let seed = cfg.seed;
    let cycles = cfg.reconnect_cycles.max(1);
    let mut completed = 0u32;
    let mut media_ok = 0u32;

    for cycle in 0..cycles {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec.clone()));

        if let Err(e) = pair.handshake() {
            return fail(
                seed,
                format!("cycle {cycle}: handshake failed: {e}"),
                completed,
            );
        }
        if pair.peer_a.connection_state() != ConnectionState::Connected
            || pair.peer_b.connection_state() != ConnectionState::Connected
        {
            return fail(
                seed,
                format!("cycle {cycle}: not connected after handshake"),
                completed,
            );
        }

        // Exchange a data + audio frame to prove the session works.
        if let Err(e) = pair.peer_a.send_data(DataMessage {
            label: "input".into(),
            data: format!(r#"{{"cycle":{cycle}}}"#).into_bytes(),
            unordered: true,
        }) {
            return fail(seed, format!("cycle {cycle}: send_data: {e}"), completed);
        }
        if let Err(e) = pair.peer_a.send_audio(AudioPacket {
            pts_host_mono: Duration::from_millis(20),
            rtp_ts: Some(960),
            sample_rate: 48_000,
            channels: 2,
            data: vec![1, 2, 3, 4],
        }) {
            return fail(seed, format!("cycle {cycle}: send_audio: {e}"), completed);
        }
        if let Err(e) = pair.flush() {
            return fail(seed, format!("cycle {cycle}: flush: {e}"), completed);
        }
        let snap = rec.snapshot();
        if snap.data.is_empty() || snap.tracks.is_empty() {
            return fail(
                seed,
                format!("cycle {cycle}: media not delivered before teardown"),
                completed,
            );
        }
        media_ok += 1;

        // Teardown: close host; viewer should observe disconnect on poll.
        if let Err(e) = pair.peer_a.close() {
            return fail(seed, format!("cycle {cycle}: close host: {e}"), completed);
        }
        if pair.peer_a.connection_state() != ConnectionState::Closed {
            return fail(
                seed,
                format!("cycle {cycle}: host not Closed after close()"),
                completed,
            );
        }
        // Viewer poll after remote close.
        let _ = pair.peer_b.poll();
        if pair.peer_b.connection_state() != ConnectionState::Disconnected
            && pair.peer_b.connection_state() != ConnectionState::Closed
        {
            return fail(
                seed,
                format!(
                    "cycle {cycle}: viewer expected Disconnected/Closed, got {:?}",
                    pair.peer_b.connection_state()
                ),
                completed,
            );
        }

        // Drop pair; next loop constructs a fresh session (restart harness).
        completed += 1;
    }

    // Also exercise ICE restart within a single session (soft reconnect).
    let mut pair = MockPeerPair::new();
    if let Err(e) = pair.handshake() {
        return fail(seed, format!("ice-restart setup handshake: {e}"), completed);
    }
    if let Err(e) = pair.peer_a.restart_ice() {
        return fail(seed, format!("restart_ice: {e}"), completed);
    }
    if pair.peer_a.ice_restart_count() != 1 {
        return fail(
            seed,
            format!(
                "ice_restart_count={}, expected 1",
                pair.peer_a.ice_restart_count()
            ),
            completed,
        );
    }
    if pair.peer_a.connection_state() != ConnectionState::Connected {
        return fail(
            seed,
            "host not Connected after ICE restart".into(),
            completed,
        );
    }

    ProfileOutcome {
        profile: ProfileName::Reconnect,
        root_seed: None,
        seed,
        status: ProfileStatus::Pass,
        severity: None,
        summary: format!(
            "completed {completed} teardown/restart cycles; media_ok={media_ok}; ice restart ok"
        ),
        metrics: json!({
            "cycles": cycles,
            "completed": completed,
            "media_ok": media_ok,
            "ice_restart_count": 1,
        }),
        repro: Some(format!("seed={seed} reconnect_cycles={cycles}")),
    }
}

fn fail(seed: u64, summary: String, completed: u32) -> ProfileOutcome {
    ProfileOutcome {
        profile: ProfileName::Reconnect,
        root_seed: None,
        seed,
        status: ProfileStatus::Fail,
        severity: Some(Severity::High),
        summary,
        metrics: json!({ "completed": completed }),
        repro: Some(format!("seed={seed}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_cycles_pass() {
        let cfg = ChaosProfileConfig {
            seed: 7,
            reconnect_cycles: 2,
            ..ChaosProfileConfig::default()
        };
        let o = run_reconnect(&cfg);
        assert_eq!(o.status, ProfileStatus::Pass, "{}", o.summary);
        assert_eq!(o.metrics["completed"], 2);
    }
}

//! Host **service** role stubs (KD5).
//!
//! Owns (eventually): device enrollment, long-lived signaling WebSocket,
//! presence/heartbeat, session policy, kill-switch orchestration, and
//! spawning/attaching the session agent over control IPC.
//!
//! Does **not** own: DXGI/WASAPI, encode, PeerTransport, input inject.
//!
//! Signaling from the server WS is forwarded into the agent as
//! [`ControlMessage::SignalForward`] (SDP/ICE only on the control plane).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use remotelink_platform_windows::ipc::message::{
    AttachSession, ControlMessage, FeatureFlags, KillSwitch, KillSwitchSource, SetPolicy,
    SignalForward, SignalHop, StartMedia,
};
use remotelink_platform_windows::kill_switch::KillSwitchRegistrar;
use remotelink_platform_windows::{decode_control, encode_control};

use crate::session::signal_kind;

/// Run the service role skeleton (stdout-oriented for now).
pub fn run() {
    println!(
        "remotelink-host {} role=service",
        remotelink_common::VERSION
    );
    println!("service: enrollment/signaling/policy stubs (not connected)");

    let registrar = KillSwitchRegistrar::new();
    let kill_fired = Arc::new(AtomicBool::new(false));
    let kill_flag = Arc::clone(&kill_fired);

    let handle = match registrar.register(move |ev: KillSwitch| {
        kill_flag.store(true, Ordering::SeqCst);
        println!(
            "service: kill-switch armed event source={:?} disable_unattended={}",
            ev.source, ev.disable_unattended
        );
        // Production: forward ControlMessage::KillSwitch to agent + drop WS session.
        if let Err(e) = encode_control(&ControlMessage::KillSwitch(ev)) {
            eprintln!("service: encode kill-switch failed: {e}");
        }
    }) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("service: kill-switch registration failed: {e}");
            return;
        }
    };

    println!(
        "service: kill-switch registration stub armed={}",
        handle.is_armed()
    );

    // Demonstrate control message construction the service will send later.
    let session_id = "00000000-0000-0000-0000-000000000000";
    match smoke_encode_session_frames(session_id) {
        Ok((count, total_bytes)) => {
            println!(
                "service: control IPC skeleton frames ok ({count} messages, {total_bytes} bytes)"
            );
        }
        Err(e) => {
            eprintln!("service: control IPC smoke failed: {e}");
            return;
        }
    }

    // Skeleton exits after printing status; real service would block on WS + pipe.
    println!("service: skeleton idle exit (wire WS + named pipe in later PRs)");
    let _ = handle;
    let _ = kill_fired;
}

/// Colocate service control plane with an in-process agent for CI / synthetic demos.
///
/// Builds the attach → policy → start sequence, drives the agent, completes
/// mock offer/answer via `SignalForward`, and pumps synthetic A/V. No real
/// display capture and no media bytes on IPC.
pub fn run_colocate_synthetic(session_id: &str) -> Result<String, String> {
    use crate::agent::AgentSession;
    use crate::session::{parse_ice_payload, parse_sdp_payload, SdpPayload, SessionManager};
    use remotelink_net::{MockPeerPair, PeerTransport, SessionDescription, SharedRecording};
    use remotelink_platform_windows::ipc::message::FORBIDDEN_MEDIA_METHODS;

    let mut pair = MockPeerPair::new();
    let rec = SharedRecording::new();
    pair.peer_b.set_callbacks(Box::new(rec.clone()));
    let MockPeerPair { peer_a, mut peer_b } = pair;

    let mut agent = AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));

    // Service builds control sequence and encodes each frame (control-only).
    let sequence = build_session_start_sequence(session_id, false);
    for msg in &sequence {
        let frame = encode_control(msg).map_err(|e| format!("encode: {e}"))?;
        let json = String::from_utf8_lossy(&frame);
        for forbidden in FORBIDDEN_MEDIA_METHODS {
            if json.contains(forbidden) {
                return Err(format!("media method `{forbidden}` on IPC"));
            }
        }
        let (decoded, _) = decode_control(&frame).map_err(|e| format!("decode: {e}"))?;
        let reply = agent.handle(&decoded);
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!(
                "agent rejected {}: {reply:?}",
                decoded.method_name()
            ));
        }
    }

    // Agent produced offer + ICE as A→S SignalForward; service would relay to
    // viewer WS. Here we apply them to the mock viewer peer directly.
    let outbound = agent.take_outbound_signals();
    let mut host_ice = Vec::new();
    let mut offer_sdp = None;
    for msg in outbound {
        if let ControlMessage::SignalForward(s) = msg {
            match s.kind.as_str() {
                signal_kind::SESSION_OFFER => {
                    offer_sdp = Some(parse_sdp_payload(&s.payload).map_err(|e| e.to_string())?);
                }
                signal_kind::ICE_CANDIDATE => {
                    host_ice.push(parse_ice_payload(&s.payload).map_err(|e| e.to_string())?);
                }
                _ => {}
            }
        }
    }
    let offer = offer_sdp.ok_or_else(|| "agent did not emit session_offer".to_string())?;
    peer_b
        .set_remote_description(SessionDescription::offer(offer.sdp))
        .map_err(|e| e.to_string())?;
    let answer = peer_b.create_answer().map_err(|e| e.to_string())?;
    peer_b
        .set_local_description(answer.clone())
        .map_err(|e| e.to_string())?;

    // Service forwards viewer answer + ICE into agent via SignalForward.
    let answer_msg = signal_to_agent(
        session_id,
        signal_kind::SESSION_ANSWER,
        &serde_json::to_string(&SdpPayload {
            sdp: answer.sdp,
            fingerprint_sig: None,
        })
        .map_err(|e| e.to_string())?,
    );
    let frame = encode_control(&answer_msg).map_err(|e| e.to_string())?;
    let (decoded, _) = decode_control(&frame).map_err(|e| e.to_string())?;
    let reply = agent.handle(&decoded);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("answer forward failed: {reply:?}"));
    }

    for ice in host_ice {
        peer_b.add_ice_candidate(ice).map_err(|e| e.to_string())?;
    }
    if let Some(ice) = peer_b.last_local_ice().cloned() {
        let ice_msg = signal_to_agent(
            session_id,
            signal_kind::ICE_CANDIDATE,
            &serde_json::to_string(&ice).map_err(|e| e.to_string())?,
        );
        let frame = encode_control(&ice_msg).map_err(|e| e.to_string())?;
        let (decoded, _) = decode_control(&frame).map_err(|e| e.to_string())?;
        let reply = agent.handle(&decoded);
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!("ice forward failed: {reply:?}"));
        }
    }

    // Drain any further agent ICE after answer.
    for msg in agent.take_outbound_signals() {
        if let ControlMessage::SignalForward(s) = msg {
            if s.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&s.payload).map_err(|e| e.to_string())?;
                peer_b.add_ice_candidate(c).map_err(|e| e.to_string())?;
            }
        }
    }

    let stats = agent.pump_media(4).map_err(|e| e.to_string())?;
    peer_b.poll().map_err(|e| e.to_string())?;
    let snap = rec.snapshot();
    let videos = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, remotelink_net::IncomingTrackData::Video(_)))
        .count();
    let audios = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, remotelink_net::IncomingTrackData::Audio(_)))
        .count();

    if videos == 0 || audios == 0 {
        return Err(format!(
            "colocate: viewer got video={videos} audio={audios}"
        ));
    }

    Ok(format!(
        "colocate ok session={session_id} video_sent={} audio_sent={} viewer_video={videos} viewer_audio={audios}",
        stats.video_sent, stats.audio_sent
    ))
}

/// Encode the standard session-start control sequence (no panics).
fn smoke_encode_session_frames(session_id: &str) -> Result<(usize, usize), String> {
    let sequence = build_session_start_sequence(session_id, false);
    let mut total_bytes = 0usize;
    let mut count = 0usize;
    for msg in &sequence {
        let frame =
            encode_control(msg).map_err(|e| format!("encode {}: {e}", msg.method_name()))?;
        let (decoded, _) =
            decode_control(&frame).map_err(|e| format!("decode {}: {e}", msg.method_name()))?;
        if decoded.method_name() != msg.method_name() {
            return Err(format!(
                "method mismatch: encoded {} decoded {}",
                msg.method_name(),
                decoded.method_name()
            ));
        }
        total_bytes += frame.len();
        count += 1;
    }
    let signal = signal_to_agent(session_id, signal_kind::SESSION_OFFER, r#"{"sdp":"v=0"}"#);
    let kill = service_kill_switch(Some(session_id));
    for msg in [&signal, &kill] {
        let frame =
            encode_control(msg).map_err(|e| format!("encode {}: {e}", msg.method_name()))?;
        total_bytes += frame.len();
        count += 1;
    }
    Ok((count, total_bytes))
}

/// Build the standard attach + policy + start sequence (unit-tested helper).
pub fn build_session_start_sequence(session_id: &str, enable_input: bool) -> Vec<ControlMessage> {
    vec![
        ControlMessage::AttachSession(AttachSession {
            session_id: session_id.into(),
            viewer_label: None,
            feature_flags: FeatureFlags::default(),
            turn_uris: vec![],
            boot_secret: None,
        }),
        ControlMessage::SetPolicy(SetPolicy {
            session_id: session_id.into(),
            enable_input,
            unattended: false,
            max_bitrate_bps: 0,
            disable_hw_encode: false,
        }),
        ControlMessage::StartMedia(StartMedia {
            session_id: session_id.into(),
            display_id: None,
        }),
    ]
}

/// Forward an opaque signaling payload toward the agent.
///
/// Used for SDP (`session_offer` / `session_answer`) and `ice_candidate` from
/// the server WS path; the service does not interpret media.
pub fn signal_to_agent(session_id: &str, kind: &str, payload: &str) -> ControlMessage {
    ControlMessage::SignalForward(SignalForward {
        session_id: session_id.into(),
        kind: kind.into(),
        payload: payload.into(),
        from: SignalHop::Service,
    })
}

/// Construct a service-originated kill-switch message.
pub fn service_kill_switch(session_id: Option<&str>) -> ControlMessage {
    ControlMessage::KillSwitch(KillSwitch {
        session_id: session_id.map(str::to_string),
        disable_unattended: true,
        source: KillSwitchSource::Tray,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_platform_windows::ipc::message::FORBIDDEN_MEDIA_METHODS;
    use remotelink_platform_windows::{decode_control, encode_control};

    #[test]
    fn session_start_sequence_roundtrips() {
        let msgs = build_session_start_sequence("sess-xyz", false);
        assert_eq!(msgs.len(), 3);
        for msg in msgs {
            let frame = encode_control(&msg).unwrap();
            let (back, n) = decode_control(&frame).unwrap();
            assert_eq!(n, frame.len());
            assert_eq!(back.method_name(), msg.method_name());
            for forbidden in FORBIDDEN_MEDIA_METHODS {
                assert_ne!(back.method_name(), *forbidden);
            }
        }
    }

    #[test]
    fn signal_forward_from_service() {
        let msg = signal_to_agent("s1", signal_kind::ICE_CANDIDATE, r#"{"c":1}"#);
        match msg {
            ControlMessage::SignalForward(s) => {
                assert_eq!(s.from, SignalHop::Service);
                assert_eq!(s.kind, signal_kind::ICE_CANDIDATE);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn smoke_encode_ok() {
        let (count, bytes) = smoke_encode_session_frames("s-test").unwrap();
        assert_eq!(count, 5);
        assert!(bytes > 0);
    }

    #[test]
    fn colocate_synthetic_session_delivers_av() {
        let summary = run_colocate_synthetic("colocate-s1").unwrap();
        assert!(summary.contains("viewer_video="), "{summary}");
        assert!(summary.contains("colocate ok"), "{summary}");
    }
}

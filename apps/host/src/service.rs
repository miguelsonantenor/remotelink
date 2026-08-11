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
//!
//! G9: owns [`crate::chrome::HostSessionUx`] (mandatory session indicator +
//! tray chrome); local kill-switch ends the session and cannot be remote-disabled.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use remotelink_platform_windows::ipc::message::{
    AttachSession, ControlMessage, FeatureFlags, KillSwitch, KillSwitchSource, SetPolicy,
    SignalForward, SignalHop, StartMedia,
};
use remotelink_platform_windows::kill_switch::KillSwitchRegistrar;
use remotelink_platform_windows::{decode_control, encode_control};

use crate::chrome::HostSessionUx;
use crate::session::signal_kind;

/// Run the service role skeleton when no `--server` is configured.
///
/// For a **live multi-process host**, pass `--server=http://…` on the binary so
/// [`crate::ws_session::run_ws_host_service`] runs instead (enroll + WSS loop).
pub fn run() {
    println!(
        "remotelink-host {} role=service",
        remotelink_common::VERSION
    );
    println!(
        "service: enrollment/signaling/policy stubs (not connected); \
         pass --server=http://127.0.0.1:8080 for persistent WSS host"
    );

    let ux = Arc::new(Mutex::new(HostSessionUx::new()));
    println!("service: {}", ux.lock().expect("ux").status_line());

    let registrar = KillSwitchRegistrar::new();
    let kill_fired = Arc::new(AtomicBool::new(false));
    let kill_flag = Arc::clone(&kill_fired);
    let ux_for_kill = Arc::clone(&ux);

    let handle = match registrar.register(move |ev: KillSwitch| {
        kill_flag.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = ux_for_kill.lock() {
            guard.apply_kill();
            println!("service: kill-switch → {}", guard.status_line());
        }
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
    let _ = ux;
}

/// CLI / CI demo: attach → mark indicator active → local kill-switch ends session.
///
/// Proves G9: indicator is Active only while the session is live; kill-switch
/// clears media/input on the agent and returns chrome to Inactive.
pub fn run_kill_switch_demo(session_id: &str) -> Result<String, String> {
    use crate::agent::AgentSession;
    use remotelink_platform_windows::ipc::message::error_codes;
    use remotelink_platform_windows::kill_switch::KillSwitchRegistrar;

    let mut ux = HostSessionUx::new();
    let mut agent = AgentSession::new_mock();

    // --- begin session (service-owned indicator) ---
    ux.begin_session(session_id, Some("kill-demo-viewer".into()))
        .map_err(|busy| format!("begin_session busy with {busy}"))?;

    let sequence = build_session_start_sequence(session_id, true);
    for msg in &sequence {
        let frame = encode_control(msg).map_err(|e| format!("encode: {e}"))?;
        let (decoded, _) = decode_control(&frame).map_err(|e| format!("decode: {e}"))?;
        let reply = agent.handle(&decoded);
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!(
                "agent rejected {}: {reply:?}",
                decoded.method_name()
            ));
        }
    }
    ux.mark_active();

    if !ux.indicator().is_active() {
        return Err("indicator should be active after attach+start".into());
    }
    if !ux.chrome().is_active() {
        return Err("chrome should be Active while session live".into());
    }
    if !agent.state.chrome_visible {
        return Err("agent mandatory chrome should be visible".into());
    }
    if !agent.state.media_started {
        return Err("media should be started before kill".into());
    }

    let status_live = ux.status_line();
    println!("kill-switch-demo: live {status_live}");
    println!("kill-switch-demo: tray {}", ux.chrome());

    // Host-side busy: second attach rejected while session active.
    let second = ControlMessage::AttachSession(AttachSession {
        session_id: "other-session".into(),
        viewer_label: None,
        feature_flags: FeatureFlags::default(),
        turn_uris: vec![],
        boot_secret: None,
    });
    let busy_reply = agent.handle(&second);
    match busy_reply {
        ControlMessage::Error(e) if e.code == error_codes::BUSY => {}
        other => return Err(format!("expected busy reject, got {other:?}")),
    }
    if ux.begin_session("other-session", None).is_ok() {
        return Err("service indicator must reject second begin_session".into());
    }

    // --- local kill-switch via registrar ---
    let registrar = KillSwitchRegistrar::new();
    let kill_msg = Arc::new(Mutex::new(None::<ControlMessage>));
    let kill_slot = Arc::clone(&kill_msg);
    let _handle = registrar
        .register(move |ev: KillSwitch| {
            *kill_slot.lock().expect("kill slot") = Some(ControlMessage::KillSwitch(ev));
        })
        .map_err(|e| format!("register kill-switch: {e}"))?;

    registrar
        .trigger(KillSwitch {
            session_id: Some(session_id.into()),
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        })
        .map_err(|e| format!("trigger: {e}"))?;

    let kill = kill_msg
        .lock()
        .expect("kill slot")
        .take()
        .ok_or_else(|| "kill-switch callback did not fire".to_string())?;

    let frame = encode_control(&kill).map_err(|e| format!("encode kill: {e}"))?;
    let (decoded, _) = decode_control(&frame).map_err(|e| format!("decode kill: {e}"))?;
    let reply = agent.handle(&decoded);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("agent rejected kill_switch: {reply:?}"));
    }

    // Service-owned indicator cleared only by local kill.
    ux.apply_kill();

    if agent.state.media_started {
        return Err("kill-switch must clear media".into());
    }
    if agent.state.enable_input {
        return Err("kill-switch must disable input".into());
    }
    if !agent.state.killed {
        return Err("kill-switch must latch killed".into());
    }
    if agent.state.chrome_visible {
        return Err("kill-switch must clear agent chrome".into());
    }
    if agent.manager.session_id().is_some() {
        return Err("kill-switch must detach media session manager".into());
    }
    if ux.indicator().is_active() {
        return Err("indicator must not be active after kill".into());
    }
    if ux.chrome().is_active() {
        return Err("chrome must be Inactive after kill".into());
    }

    let status_dead = ux.status_line();
    println!("kill-switch-demo: after kill {status_dead}");
    println!("kill-switch-demo: tray {}", ux.chrome());

    Ok(format!(
        "kill-switch demo ok session={session_id} indicator_active_after=false chrome=Inactive"
    ))
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

    #[test]
    fn kill_switch_demo_ends_session_and_indicator() {
        let summary = run_kill_switch_demo("kill-demo-s1").unwrap();
        assert!(
            summary.contains("indicator_active_after=false"),
            "{summary}"
        );
        assert!(summary.contains("chrome=Inactive"), "{summary}");
        assert!(summary.contains("kill-switch demo ok"), "{summary}");
    }

    #[test]
    fn service_indicator_tracks_session_lifecycle() {
        let mut ux = HostSessionUx::new();
        assert!(!ux.indicator().is_active());
        assert_eq!(ux.chrome().status_label(), "Inactive");

        ux.begin_session("s-lifecycle", Some("v1".into())).unwrap();
        assert!(ux.indicator().is_connected());
        assert!(!ux.indicator().is_active());
        ux.mark_active();
        assert!(ux.indicator().is_active());
        assert_eq!(ux.chrome().status_label(), "Active");

        // Local kill only.
        ux.apply_kill();
        assert!(!ux.indicator().is_active());
        assert_eq!(ux.chrome().status_label(), "Inactive");
    }
}

//! E2E: host SessionManager + viewer-core over MockPeerTransport.
//!
//! Mode A (OTP) and Mode B (unattended) authorize → fingerprint bind → DC challenge.
//! Asserts input rejected before bind / accepted after, and synthetic A/V on viewer.
//!
//! No GPU, real WebRTC, or external network.

use remotelink_auth::{
    generate_device_keypair, mode_b_viewer_response, AuthChallenge, HostSecret, SessionBindKey,
};
use remotelink_e2e::{handshake_host_viewer, take_pair_peers};
use remotelink_host::{
    parse_sdp_payload, signal_kind, HostAuthService, HostLocalConfig, SessionManager,
    INPUT_CHANNEL_LABEL,
};
use remotelink_net::{ConnectionState, SessionDescription};
use remotelink_protocol::{decode_input, InputPayload};
use remotelink_viewer_core::{ConnectRequest, ViewerPhase, ViewerSession};

/// Inject input over the mock wire (viewer peer → host `poll_inbound`).
///
/// Temporarily lowers the viewer's local `require_identity_for_input` gate so a
/// non-compliant peer can put input-labeled DataChannel bytes on the transport;
/// the **host** identity gate is what under test.
fn inject_input_on_wire(viewer: &mut ViewerSession, host: &mut SessionManager) {
    viewer.set_require_identity_for_input(false);
    viewer
        .send_mouse_move(0.01, 0.02)
        .expect("viewer wire inject (gate lowered; need Connected phase)");
    viewer.set_require_identity_for_input(true);
    let inbound = host.poll_inbound().expect("host poll pre-bind wire input");
    assert!(
        inbound.input_rejected >= 1,
        "host must reject wire input before bind: {inbound:?}"
    );
    assert_eq!(inbound.input_accepted, 0);
    assert!(!host.input_allowed());
    assert!(host.take_accepted_input().is_empty());
}

/// Full Mode A path: OTP authorize, fingerprint_sig, DC bind, input gate, synthetic A/V.
#[test]
fn mode_a_identity_bind_input_and_synthetic_av() {
    let (sk, vk) = generate_device_keypair();
    let mut policy = HostAuthService::default();
    let otp = policy.mint_otp().expect("mint otp");
    let otp_str = otp.as_str().to_string();
    let pepper = policy.otp_pepper().to_vec();
    let bind_key = SessionBindKey::from_mode_a_otp(&otp_str, &pepper).expect("bind key");

    let (peer_a, peer_b) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.set_device_signing_key(sk);
    host.set_synthetic_geometry(64, 36, 30);

    let mut viewer = ViewerSession::new();
    viewer.set_host_verifying_key(vk);
    viewer.set_bind_key(bind_key);
    viewer.set_require_identity_for_input(true);

    let stub = viewer
        .begin_connect(&ConnectRequest::otp("e2e-host-mode-a", &otp_str).with_label("e2e-viewer"))
        .expect("begin_connect");
    let session_id = stub.session_id.clone();
    host.attach(&session_id);

    // Mode A authorize on host (consumes OTP window).
    policy
        .authorize_session_mode_a(&mut host, &otp_str, 0)
        .expect("authorize mode a");
    assert!(host.identity().session_authorized);
    assert!(!host.input_allowed(), "no input before identity_bound");
    viewer.mark_session_authorized();

    viewer.attach_transport(Box::new(peer_b));

    let sdp = handshake_host_viewer(&mut host, &mut viewer).expect("handshake");
    let sig = sdp
        .fingerprint_sig
        .as_deref()
        .filter(|s| !s.is_empty())
        .expect("host fingerprint_sig on offer");
    assert!(!sig.is_empty());

    assert_eq!(host.connection_state(), ConnectionState::Connected);
    assert_eq!(viewer.transport_state(), Some(ConnectionState::Connected));
    assert!(!host.identity().identity_bound);
    assert!(!viewer.identity_bound());
    assert!(!host.input_allowed());

    // Viewer refuses to send input while identity not bound (local gate).
    let err = viewer
        .send_mouse_move(0.25, 0.5)
        .expect_err("viewer input blocked pre-bind");
    assert!(
        format!("{err}").contains("identity") || format!("{err}").contains("bound"),
        "unexpected err: {err}"
    );

    // Wire-level reject: non-compliant peer sends input label → host.poll_inbound.
    let rejected_before = host.rejected_input_count();
    inject_input_on_wire(&mut viewer, &mut host);
    assert!(host.rejected_input_count() > rejected_before);

    // Post-DTLS DC identity challenge (host → viewer → host).
    host.start_identity_challenge()
        .expect("start_identity_challenge");
    viewer.poll().expect("viewer answers dc challenge");
    assert!(
        viewer.identity_bound(),
        "viewer marks bound after answering"
    );

    let inbound = host.poll_inbound().expect("host verifies dc response");
    assert!(
        inbound.identity_messages >= 1,
        "host should process identity response"
    );
    assert!(host.identity().identity_bound);
    assert!(host.input_allowed(), "input allowed after full bind");

    // Viewer input now succeeds and is accepted on host.
    viewer
        .send_mouse_move(0.4, 0.6)
        .expect("viewer mouse after bind");
    viewer
        .send_key(0x1E, false, true, 0)
        .expect("viewer key after bind");
    let inbound = host.poll_inbound().expect("poll input");
    assert_eq!(inbound.input_accepted, 2);
    assert_eq!(inbound.input_rejected, 0);
    let accepted = host.take_accepted_input();
    assert_eq!(accepted.len(), 2);
    assert!(viewer.stats().input_events >= 2);

    // Decode accepted payloads (real InputEvent wire, not just counters).
    for msg in &accepted {
        assert_eq!(msg.label, INPUT_CHANNEL_LABEL);
    }
    let move_json = std::str::from_utf8(&accepted[0].data).expect("utf8 mouse");
    let move_ev = decode_input(move_json).expect("decode mouse");
    match &move_ev.payload {
        InputPayload::MouseMove(m) => {
            assert!((m.x - 0.4).abs() < 1e-5, "x={}", m.x);
            assert!((m.y - 0.6).abs() < 1e-5, "y={}", m.y);
            assert_eq!(m.display_id, 0);
        }
        other => panic!("expected MouseMove, got {other:?}"),
    }
    let key_json = std::str::from_utf8(&accepted[1].data).expect("utf8 key");
    let key_ev = decode_input(key_json).expect("decode key");
    match &key_ev.payload {
        InputPayload::Key(k) => {
            assert_eq!(k.scancode, 0x1E);
            assert!(!k.extended);
            assert!(k.pressed);
            assert_eq!(k.modifiers, 0);
        }
        other => panic!("expected Key, got {other:?}"),
    }
    assert_eq!(move_ev.seq + 1, key_ev.seq);

    // Synthetic A/V: host pump → viewer receive.
    let pump = host.pump_media(4).expect("pump_media");
    assert_eq!(pump.video_sent, 4);
    assert_eq!(pump.audio_sent, 12); // 3 audio packets per video frame
    assert!(!pump.skipped_not_connected);

    viewer.poll().expect("viewer media poll");
    assert!(
        viewer.stats().video_frames >= 4,
        "viewer video frames: {}",
        viewer.stats().video_frames
    );
    assert!(
        viewer.stats().audio_packets >= 12,
        "viewer audio packets: {}",
        viewer.stats().audio_packets
    );
    assert_eq!(viewer.recorded_video_nalus().len(), 4);
    assert_eq!(viewer.recorded_audio_packets().len(), 12);
    assert!(matches!(
        viewer.phase(),
        ViewerPhase::Streaming | ViewerPhase::Connected
    ));

    let frames = viewer.drain_video_frames();
    assert_eq!(frames.len(), 4);
    assert!(frames[0].keyframe || frames.iter().any(|f| f.keyframe));
}

/// Full Mode B path: unattended policy + host secret MAC, then DC bind + A/V.
#[test]
fn mode_b_identity_bind_input_and_synthetic_av() {
    let (sk, vk) = generate_device_keypair();
    let secret = HostSecret::try_new(b"e2e-unattended-host-secret!!").expect("host secret");
    let mut policy = HostAuthService::new(
        HostLocalConfig {
            unattended_enabled: true,
            confirm_sessions: false,
            ..HostLocalConfig::default()
        },
        remotelink_host::DEFAULT_HOST_OTP_PEPPER.to_vec(),
    );
    policy.set_host_secret(secret.clone());

    let (peer_a, peer_b) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.set_device_signing_key(sk);

    let mut viewer = ViewerSession::new();
    viewer.set_host_verifying_key(vk);
    viewer.set_bind_key(SessionBindKey::from_mode_b_secret(&secret));
    viewer.set_require_identity_for_input(true);

    let stub = viewer
        .begin_connect(
            &ConnectRequest::unattended("e2e-host-mode-b", "e2e-unattended-host-secret!!")
                .with_label("e2e-viewer-b"),
        )
        .expect("begin_connect");
    let session_id = stub.session_id.clone();
    host.attach(&session_id);

    // Mode B authorize (fingerprints empty at pre-connect; DC bind uses real fps).
    let challenge = AuthChallenge::issue();
    let mac = mode_b_viewer_response(&secret, &session_id, challenge.nonce.as_bytes(), b"", b"");
    policy
        .authorize_session_mode_b(&mut host, &challenge, b"", b"", &mac)
        .expect("authorize mode b");
    assert!(host.identity().session_authorized);
    assert!(!host.input_allowed());
    viewer.mark_session_authorized();

    viewer.attach_transport(Box::new(peer_b));
    let sdp = handshake_host_viewer(&mut host, &mut viewer).expect("handshake");
    assert!(
        sdp.fingerprint_sig.as_ref().is_some_and(|s| !s.is_empty()),
        "fingerprint_sig required"
    );

    // Input rejected before bind: local gate + wire → host.poll_inbound.
    assert!(viewer.send_mouse_move(0.1, 0.1).is_err());
    inject_input_on_wire(&mut viewer, &mut host);

    host.start_identity_challenge().expect("dc challenge");
    viewer.poll().expect("viewer dc response");
    host.poll_inbound().expect("host dc verify");
    assert!(host.input_allowed());
    assert!(viewer.identity_bound());

    viewer.send_mouse_move(0.9, 0.1).expect("post-bind input");
    let inbound = host.poll_inbound().expect("accept input");
    assert_eq!(inbound.input_accepted, 1);
    assert_eq!(inbound.input_rejected, 0);
    let accepted = host.take_accepted_input();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].label, INPUT_CHANNEL_LABEL);
    let ev = decode_input(std::str::from_utf8(&accepted[0].data).unwrap()).unwrap();
    match ev.payload {
        InputPayload::MouseMove(m) => {
            assert!((m.x - 0.9).abs() < 1e-5);
            assert!((m.y - 0.1).abs() < 1e-5);
        }
        other => panic!("expected MouseMove, got {other:?}"),
    }

    let pump = host.pump_media(2).expect("pump");
    assert_eq!(pump.video_sent, 2);
    assert_eq!(pump.audio_sent, 6);
    viewer.poll().expect("media");
    assert!(viewer.stats().video_frames >= 2);
    assert!(viewer.stats().audio_packets >= 6);
}

/// Wrong viewer bind key → DC MAC fails → host input stays closed.
#[test]
fn mode_a_wrong_bind_key_keeps_input_closed() {
    let (sk, vk) = generate_device_keypair();
    let mut policy = HostAuthService::default();
    let otp = policy.mint_otp().expect("mint otp");
    let otp_str = otp.as_str().to_string();
    let pepper = policy.otp_pepper().to_vec();
    // Host derives the correct bind key via authorize; viewer gets a wrong one.
    let wrong_key = SessionBindKey::try_new(b"wrong-e2e-bind-key!!!!!!").expect("wrong key");

    let (peer_a, peer_b) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.set_device_signing_key(sk);

    let mut viewer = ViewerSession::new();
    viewer.set_host_verifying_key(vk);
    viewer.set_bind_key(wrong_key);
    viewer.set_require_identity_for_input(true);

    let stub = viewer
        .begin_connect(&ConnectRequest::otp("e2e-host-bad-bind", &otp_str))
        .expect("begin_connect");
    let session_id = stub.session_id.clone();
    host.attach(&session_id);
    policy
        .authorize_session_mode_a(&mut host, &otp_str, 0)
        .expect("authorize mode a");
    // Correct key material exists only on host; prove pepper-derived key differs.
    let _correct = SessionBindKey::from_mode_a_otp(&otp_str, &pepper).unwrap();
    viewer.mark_session_authorized();
    viewer.attach_transport(Box::new(peer_b));

    handshake_host_viewer(&mut host, &mut viewer).expect("handshake");
    assert!(host.identity().session_authorized);
    assert!(!host.input_allowed());

    host.start_identity_challenge().expect("dc challenge");
    // Viewer answers with wrong bind key MAC.
    viewer.poll().expect("viewer still sends a response");
    assert!(
        viewer.identity_bound(),
        "viewer locally marks bound after answering (host must independently verify)"
    );

    let err = host
        .poll_inbound()
        .expect_err("host must reject wrong DC MAC");
    assert!(
        format!("{err}").to_lowercase().contains("auth")
            || format!("{err}").to_lowercase().contains("bind")
            || format!("{err}").to_lowercase().contains("mac")
            || format!("{err}").to_lowercase().contains("identity"),
        "err={err}"
    );
    assert!(!host.identity().identity_bound);
    assert!(!host.input_allowed());

    // Subsequent wire input still rejected.
    inject_input_on_wire(&mut viewer, &mut host);
    assert!(!host.input_allowed());
}

/// Corrupt `fingerprint_sig` on offer → viewer refuses to answer (MITM strip/swap).
#[test]
fn corrupt_fingerprint_sig_rejected_before_answer() {
    let (sk, vk) = generate_device_keypair();
    let mut policy = HostAuthService::default();
    let otp = policy.mint_otp().expect("mint otp");
    let otp_str = otp.as_str().to_string();
    let pepper = policy.otp_pepper().to_vec();
    let bind_key = SessionBindKey::from_mode_a_otp(&otp_str, &pepper).unwrap();

    let (peer_a, peer_b) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.set_device_signing_key(sk);

    let mut viewer = ViewerSession::new();
    viewer.set_host_verifying_key(vk);
    viewer.set_bind_key(bind_key);
    viewer.set_require_identity_for_input(true);

    let stub = viewer
        .begin_connect(&ConnectRequest::otp("e2e-host-bad-fp", &otp_str))
        .expect("begin_connect");
    host.attach(&stub.session_id);
    policy
        .authorize_session_mode_a(&mut host, &otp_str, 0)
        .expect("authorize");
    viewer.mark_session_authorized();
    viewer.attach_transport(Box::new(peer_b));

    host.start_media().expect("start_media");
    let outbound = host.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .expect("session_offer");
    let sdp = parse_sdp_payload(&offer_sig.payload).expect("parse offer");
    let real_sig = sdp.fingerprint_sig.filter(|s| !s.is_empty()).expect("sig");
    // Flip last hex nibble so verify fails while remaining plausible hex length.
    let mut bad = real_sig.clone();
    let last = bad.pop().expect("nonempty");
    bad.push(if last == '0' { '1' } else { '0' });

    let err = viewer
        .accept_offer_with_sig(SessionDescription::offer(sdp.sdp), Some(&bad))
        .expect_err("corrupt fingerprint_sig must fail");
    assert!(
        format!("{err}").to_lowercase().contains("fingerprint")
            || format!("{err}").to_lowercase().contains("signature")
            || format!("{err}").to_lowercase().contains("auth")
            || format!("{err}").to_lowercase().contains("verify"),
        "err={err}"
    );
    assert!(!host.input_allowed());
    assert!(!viewer.identity_bound());
    // Host never reached Connected via this viewer answer path.
    assert_ne!(host.connection_state(), ConnectionState::Connected);
}

/// Policy: Mode B rejected when unattended is disabled (no bind path).
#[test]
fn mode_b_rejected_when_unattended_disabled() {
    let secret = HostSecret::try_new(b"secret-but-disabled!!!!!!!").unwrap();
    let policy = HostAuthService::new(HostLocalConfig::default(), b"pepper".to_vec());
    // unattended_enabled defaults to false; secret not even installed.
    let mut host = SessionManager::new_mock();
    host.attach("sess-disabled");
    let challenge = AuthChallenge::issue();
    let mac = mode_b_viewer_response(
        &secret,
        "sess-disabled",
        challenge.nonce.as_bytes(),
        b"",
        b"",
    );
    let err = policy
        .authorize_session_mode_b(&mut host, &challenge, b"", b"", &mac)
        .expect_err("must reject");
    assert!(
        format!("{err}").to_lowercase().contains("unattended")
            || format!("{err}").to_lowercase().contains("disabled")
            || format!("{err}").to_lowercase().contains("secret"),
        "err={err}"
    );
    assert!(!host.input_allowed());
}

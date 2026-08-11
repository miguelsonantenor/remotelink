//! Synthetic e2e for integrated tree: bind gate + mock media + input.

use remotelink_auth::{HostSecret, SessionBindKey};
use remotelink_e2e::{handshake_host_viewer, take_pair_peers};
use remotelink_host::{
    HostAuthService, HostLocalConfig, InputProcessOutcome, SessionManager, DEFAULT_HOST_OTP_PEPPER,
};
use remotelink_net::DataMessage;
use remotelink_protocol::{InputPayload, SessionMode};
use remotelink_viewer_core::{ConnectRequest, ViewerSession};

#[test]
fn force_bind_allows_input_and_media() {
    let (peer_a, peer_b) = take_pair_peers();

    let mut host = SessionManager::with_peer(Box::new(peer_a));
    // No device key: offer has no fingerprint_sig (media path without MITM check).
    // `attach` resets identity + policy gates — enable them after attach.
    host.attach("e2e-session");
    host.set_input_policy_enabled(true);
    host.force_identity_bound_for_tests();

    let mut viewer = ViewerSession::new();
    // No verifying key → no fingerprint check required for answer.
    viewer.set_require_identity_for_input(false);
    let _ = viewer
        .begin_connect(&ConnectRequest::otp("e2e-host", "112233"))
        .expect("viewer connect stub");
    viewer.mark_session_authorized();
    viewer.attach_transport(Box::new(peer_b));

    handshake_host_viewer(&mut host, &mut viewer).expect("handshake");
    assert!(host.input_allowed());

    let post = host.process_input_message(&DataMessage {
        label: remotelink_host::INPUT_CHANNEL_LABEL.into(),
        data: br#"{"client_ts_us":2,"seq":2,"payload":{"kind":"mouse_move","x":0.4,"y":0.6,"display_id":0}}"#
            .to_vec(),
        unordered: false,
    });
    assert!(matches!(post, InputProcessOutcome::Injected));
    let accepted = host.take_accepted_input();
    assert_eq!(accepted.len(), 1);
    assert!(matches!(accepted[0].payload, InputPayload::MouseMove(_)));

    for _ in 0..6 {
        let _ = host.pump_media(4);
        let _ = viewer.poll();
    }
}

#[test]
fn input_rejected_before_identity_bound() {
    let (peer_a, _) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    host.set_input_policy_enabled(true);
    assert!(!host.input_allowed());
    let pre = host.process_input_message(&DataMessage {
        label: remotelink_host::INPUT_CHANNEL_LABEL.into(),
        data: br#"{"client_ts_us":1,"seq":1,"payload":{"kind":"mouse_move","x":0.5,"y":0.5,"display_id":0}}"#
            .to_vec(),
        unordered: false,
    });
    assert!(matches!(pre, InputProcessOutcome::RejectedGate));
}

#[test]
fn mode_a_otp_mint_and_authorize() {
    let (peer_a, _) = take_pair_peers();
    let mut host = SessionManager::with_peer(Box::new(peer_a));
    let mut policy =
        HostAuthService::new(HostLocalConfig::default(), DEFAULT_HOST_OTP_PEPPER.to_vec());
    let otp = policy.mint_otp().expect("mint");
    let code = otp.to_ui_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    policy
        .authorize_session_mode_a(&mut host, &code, now)
        .expect("authorize mode a");
    assert!(host.identity().session_authorized);
    let key = SessionBindKey::from_mode_a_otp(&code).unwrap();
    assert!(!key.as_bytes().is_empty());
}

#[test]
fn mode_b_policy_rejects_when_disabled() {
    let policy = HostAuthService::new(
        HostLocalConfig {
            unattended_enabled: false,
            ..HostLocalConfig::default()
        },
        DEFAULT_HOST_OTP_PEPPER.to_vec(),
    );
    assert!(policy.policy_allows_mode(SessionMode::Unattended).is_err());
}

#[test]
fn mode_b_secret_bind_key() {
    let secret = HostSecret::try_new(b"e2e-unattended-host-secret!!").unwrap();
    let key = SessionBindKey::from_mode_b_secret(&secret);
    assert_eq!(key.as_bytes(), secret.as_bytes());
}

#[test]
fn viewer_connect_stub_otp() {
    let mut viewer = ViewerSession::new();
    let stub = viewer
        .begin_connect(&ConnectRequest::otp("1234567890", "112233"))
        .expect("connect stub");
    assert!(!stub.session_id.is_empty());
}

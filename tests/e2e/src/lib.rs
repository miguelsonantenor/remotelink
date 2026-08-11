//! Synthetic end-to-end tests for RemoteLink (PR 15).
//!
//! # What this crate covers
//!
//! - Host [`SessionManager`](remotelink_host::SessionManager) + viewer
//!   [`ViewerSession`](remotelink_viewer_core::ViewerSession) over a
//!   [`MockPeerPair`](remotelink_net::MockPeerPair) (no real ICE/DTLS/network)
//! - Mode A (OTP) or Mode B (unattended) authorize → fingerprint_sig → DC identity bind
//! - Input rejected before bind, accepted after
//! - Synthetic video/audio frames received on the viewer
//! - Optional in-process server WS `session_intent` + accept smoke
//! - WSS SDP/ICE relay + [`SessionManager`] media (`tests/ws_media_signaling.rs`)
//!
//! # Run
//!
//! ```bash
//! cargo test -p remotelink-e2e
//! ```
//!
//! No GPU, DXGI, WASAPI, or external network is required. Server tests
//! bind `127.0.0.1:0` only.

#![deny(missing_docs)]

use remotelink_host::{
    parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager,
};
use remotelink_net::{
    ConnectionState, MockPeerConfig, MockPeerPair, MockPeerTransport, SessionDescription,
};
use remotelink_viewer_core::ViewerSession;

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Handshake host (offerer / SessionManager) with viewer (answerer / ViewerSession).
///
/// Expects the host to already own peer A of a pair and the viewer to own peer B.
/// Drains host outbound `session_offer` + ICE, applies answer + ICE both ways.
///
/// Returns the host's [`SdpPayload`] (includes `fingerprint_sig` when a device key is set).
pub fn handshake_host_viewer(
    host: &mut SessionManager,
    viewer: &mut ViewerSession,
) -> Result<SdpPayload, String> {
    host.start_media()
        .map_err(|e| format!("host start_media: {e}"))?;

    let outbound = host.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .ok_or_else(|| "missing session_offer from host".to_string())?;
    let sdp = parse_sdp_payload(&offer_sig.payload).map_err(|e| format!("parse offer: {e}"))?;

    let offer = SessionDescription::offer(sdp.sdp.clone());
    let answer = viewer
        .accept_offer_with_sig(offer, sdp.fingerprint_sig.as_deref())
        .map_err(|e| format!("viewer accept_offer: {e}"))?;

    host.apply_signal(
        signal_kind::SESSION_ANSWER,
        &serde_json::to_string(&SdpPayload {
            sdp: answer.sdp,
            fingerprint_sig: None,
        })
        .map_err(|e| format!("encode answer: {e}"))?,
    )
    .map_err(|e| format!("host apply answer: {e}"))?;

    // Host ICE → viewer
    for sig in host.take_outbound_signals() {
        if sig.kind == signal_kind::ICE_CANDIDATE {
            let c = parse_ice_payload(&sig.payload).map_err(|e| format!("parse host ice: {e}"))?;
            viewer
                .add_remote_ice(c)
                .map_err(|e| format!("viewer add ice: {e}"))?;
        }
    }
    // Also apply any offer-time ICE that was queued with the offer drain above.
    for sig in outbound
        .iter()
        .filter(|s| s.kind == signal_kind::ICE_CANDIDATE)
    {
        let c = parse_ice_payload(&sig.payload).map_err(|e| format!("parse offer ice: {e}"))?;
        viewer
            .add_remote_ice(c)
            .map_err(|e| format!("viewer add offer ice: {e}"))?;
    }

    // Viewer ICE → host
    for ice in viewer.take_pending_local_ice() {
        host.apply_signal(
            signal_kind::ICE_CANDIDATE,
            &serde_json::to_string(&ice).map_err(|e| format!("encode viewer ice: {e}"))?,
        )
        .map_err(|e| format!("host apply viewer ice: {e}"))?;
    }

    viewer.poll().map_err(|e| format!("viewer poll: {e}"))?;

    if host.connection_state() != ConnectionState::Connected {
        return Err(format!("host not connected: {:?}", host.connection_state()));
    }
    if viewer.transport_state() != Some(ConnectionState::Connected) {
        return Err(format!(
            "viewer not connected: {:?}",
            viewer.transport_state()
        ));
    }

    Ok(sdp)
}

/// Split a fresh [`MockPeerPair`] into host-owned peer A and a standalone peer B.
///
/// Peer B is replaced with a disconnected placeholder so the pair drop is safe.
pub fn take_pair_peers() -> (MockPeerTransport, MockPeerTransport) {
    let mut pair = MockPeerPair::new();
    let peer_b = std::mem::replace(
        &mut pair.peer_b,
        MockPeerTransport::new(MockPeerConfig {
            label: "e2e-placeholder".into(),
            fingerprint: None,
        }),
    );
    let MockPeerPair { peer_a, peer_b: _ } = pair;
    (peer_a, peer_b)
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn version_nonempty() {
        assert!(!VERSION.is_empty());
    }
}

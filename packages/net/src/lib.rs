//! RemoteLink network / media-plane transport boundary.
//!
//! # PeerTransport
//!
//! Host session agent and viewer talk to ICE/DTLS-SRTP (or a CI mock) only
//! through [`PeerTransport`]. External encoders push H.264 NALUs and Opus via
//! [`PeerTransport::send_video_nalu`] / [`PeerTransport::send_audio`]; input and
//! identity challenges use DataChannel [`PeerTransport::send_data`].
//! Install sinks with [`PeerTransport::set_callbacks`] (works on `dyn` /
//! [`BoxPeerTransport`]). Pump mock inbound with [`PeerTransport::poll`].
//!
//! # Mock (default)
//!
//! Feature `mock` (default) provides [`mock::MockPeerTransport`] and
//! [`mock::MockPeerPair`] — in-process loopback with no native WebRTC deps so
//! CI and unit tests stay green on `windows-gnu`.
//!
//! # Real backends
//!
//! - Feature `webrtc-rs`: **name-only placeholder** (no crates.io deps wired).
//!   Pure-Rust remains a tracked option, not the v1 default
//!   (see `docs/spike-webrtc.md`).
//! - Plan B: libwebrtc FFI in a follow-up crate (`packages/net-libwebrtc`)
//!   behind the same trait.
//!
//! Spike decision: **GO for v1 with mock-first + Plan B libwebrtc**; pure-Rust
//! remains a tracked option, not the v1 ship path (see `docs/spike-webrtc.md`).

#![deny(missing_docs)]

pub mod error;
pub mod transport;
pub mod types;

#[cfg(feature = "mock")]
pub mod mock;

pub use error::{NetError, Result};
pub use transport::{
    BoxPeerTransport, NullCallbacks, PeerTransport, PeerTransportCallbacks, RecordingCallbacks,
};
pub use types::{
    AudioPacket, ConnectionState, DataMessage, DtlsFingerprint, IncomingTrackData,
    LocalIceCandidate, NaluFormat, ReceiverFeedback, SdpType, SessionDescription, TrackKind,
    TransportIceCandidate, VideoNalu,
};

#[cfg(feature = "mock")]
pub use mock::{MockPeerConfig, MockPeerPair, MockPeerTransport, SharedRecording};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which media backend this build enables (for diagnostics / stats HUD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBackend {
    /// In-process mock loopback.
    Mock,
    /// Pure-Rust webrtc crate (feature `webrtc-rs` placeholder).
    WebrtcRs,
    /// Placeholder for future libwebrtc FFI.
    Libwebrtc,
}

/// Backends compiled into this binary.
pub fn available_backends() -> Vec<TransportBackend> {
    // Libwebrtc is never in-tree for this spike (Plan B follow-up crate).
    let _ = TransportBackend::Libwebrtc;
    #[cfg(feature = "webrtc-rs")]
    let _ = TransportBackend::WebrtcRs;

    #[cfg(feature = "mock")]
    {
        vec![TransportBackend::Mock]
    }
    #[cfg(not(feature = "mock"))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn version_nonempty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn mock_backend_listed_by_default() {
        let backends = available_backends();
        assert!(backends.contains(&TransportBackend::Mock));
    }

    #[test]
    fn trait_object_handshake_and_media() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        {
            let viewer: &mut dyn PeerTransport = &mut pair.peer_b;
            viewer.set_callbacks(Box::new(rec.clone()));
        }

        {
            let host: &mut dyn PeerTransport = &mut pair.peer_a;
            let viewer: &mut dyn PeerTransport = &mut pair.peer_b;

            // Offer/answer via trait object (host session manager style).
            let offer = host.create_offer().unwrap();
            host.set_local_description(offer.clone()).unwrap();
            viewer.set_remote_description(offer).unwrap();
            let answer = viewer.create_answer().unwrap();
            viewer.set_local_description(answer.clone()).unwrap();
            host.set_remote_description(answer).unwrap();

            assert_eq!(host.connection_state(), ConnectionState::Connected);
            assert_eq!(viewer.connection_state(), ConnectionState::Connected);

            // Fingerprint export for identity bind path.
            let fp = host.local_fingerprint().unwrap();
            assert_eq!(fp.algorithm, "sha-256");
            assert!(!fp.value.is_empty());
            assert_eq!(fp.digest_bytes().unwrap().len(), 32);
            assert!(fp.as_sign_material().starts_with("sha-256 "));
            assert_eq!(viewer.remote_fingerprint().unwrap().as_ref(), Some(&fp));

            // Trait-object send from host.
            host.send_video_nalu(VideoNalu {
                pts_host_mono: Duration::from_millis(33),
                rtp_ts: Some(2970),
                keyframe: true,
                format: NaluFormat::AnnexB,
                data: vec![0, 0, 0, 1, 0x67],
            })
            .unwrap();
            host.send_data(DataMessage {
                label: "input".into(),
                data: b"{}".to_vec(),
                unordered: false,
            })
            .unwrap();
        }

        pair.peer_b.poll().unwrap();
        let snap = rec.snapshot();
        assert_eq!(snap.tracks.len(), 1);
        assert_eq!(snap.data.len(), 1);
    }

    #[test]
    fn boxed_peer_transport_usage() {
        let mut pair = MockPeerPair::new();
        pair.handshake().unwrap();

        let mut boxed: BoxPeerTransport = Box::new(pair.peer_a);
        assert_eq!(boxed.connection_state(), ConnectionState::Connected);
        boxed
            .send_audio(AudioPacket {
                pts_host_mono: Duration::from_millis(0),
                rtp_ts: Some(0),
                sample_rate: 48_000,
                channels: 2,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        boxed.close().unwrap();
        assert_eq!(boxed.connection_state(), ConnectionState::Closed);

        // Peer observes hangup on poll.
        pair.peer_b.poll().unwrap();
        assert_eq!(
            pair.peer_b.connection_state(),
            ConnectionState::Disconnected
        );
    }

    #[test]
    fn session_description_helpers() {
        let o = SessionDescription::offer("v=0");
        assert_eq!(o.sdp_type, SdpType::Offer);
        let a = SessionDescription::answer("v=0");
        assert_eq!(a.sdp_type.as_str(), "answer");
    }

    #[test]
    fn dtls_fingerprint_sdp_attribute_and_sign_material() {
        // 32 bytes = 64 hex digits (with optional colons).
        let fp = DtlsFingerprint::sha256(
            "0123ab0123ab0123ab0123ab0123ab0123ab0123ab0123ab0123ab0123ab0123",
        )
        .unwrap();
        assert_eq!(
            fp.sdp_attribute(),
            fp.as_sign_material(),
            "sdp attribute matches sign material"
        );
        assert!(fp.as_sign_material().starts_with("sha-256 01:23:AB:"));
        assert_eq!(fp.digest_bytes().unwrap()[0], 0x01);
        assert_eq!(fp.digest_bytes().unwrap()[1], 0x23);
    }
}

//! RemoteLink network / media-plane transport boundary.
//!
//! # PeerTransport
//!
//! Host session agent and viewer talk to ICE/DTLS-SRTP (or a CI mock / live TCP)
//! only through [`PeerTransport`]. External encoders push H.264 NALUs and Opus via
//! [`PeerTransport::send_video_nalu`] / [`PeerTransport::send_audio`]; input and
//! identity challenges use DataChannel [`PeerTransport::send_data`].
//! Install sinks with [`PeerTransport::set_callbacks`] (works on `dyn` /
//! [`BoxPeerTransport`]). Pump inbound with [`PeerTransport::poll`].
//!
//! # Transport factory
//!
//! [`create_peer_transport`] / [`TransportConfig::from_env`] select a backend:
//!
//! | `REMOTELINK_TRANSPORT` | Backend |
//! |------------------------|---------|
//! | unset / `mock` (default) | In-process mock — **CI path** |
//! | `live` | TCP length-prefixed frames (feature `live`) |
//! | `webrtc` | webrtc-rs PeerConnection (feature `webrtc-rs`; on by default in host/viewer/app) |
//! | `auto` | Prefer webrtc (if feature on) → live → mock |
//!
//! # Mock (default mode)
//!
//! Feature `mock` (default) provides [`mock::MockPeerTransport`] and
//! [`mock::MockPeerPair`] — in-process loopback with no sockets so CI and unit
//! tests stay green on `windows-gnu`.
//!
//! # Live TCP (feature `live`, default on)
//!
//! [`live_loopback::LivePeerTransport`] carries media/data over real TCP for
//! local multi-process demos. Not DTLS-SRTP / WebRTC — see `docs/spike-webrtc.md`.
//!
//! # webrtc-rs (feature `webrtc-rs`)
//!
//! [`webrtc_rs::WebrtcPeerTransport`] uses the pure-Rust `webrtc` crate (0.11)
//! for real SDP / ICE / DTLS. Media prefers **RTP H.264 + Opus tracks**
//! (SampleBuilder on receive) and mirrors on DataChannels `media-video` /
//! `media-audio` during track bind races. CI keeps default features (mock+live only).
//!
//! # Plan B
//!
//! libwebrtc FFI in a follow-up crate (`packages/net-libwebrtc`) behind the
//! same trait if pure-Rust path fails packaging / packetization needs.

#![deny(missing_docs)]

pub mod error;
pub mod factory;
pub mod transport;
pub mod types;

#[cfg(feature = "mock")]
pub mod mock;

#[cfg(feature = "live")]
pub mod live_loopback;

#[cfg(feature = "webrtc-rs")]
pub mod webrtc_rs;

pub use error::{NetError, Result};
pub use factory::{
    create_peer_transport, create_peer_transport_with_config, PeerRole, TransportConfig,
    TransportMode,
};
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

#[cfg(feature = "live")]
pub use live_loopback::{live_handshake, LivePeerConfig, LivePeerTransport, LiveSdp};

#[cfg(feature = "webrtc-rs")]
pub use webrtc_rs::{
    webrtc_handshake, WebrtcPeerConfig, WebrtcPeerTransport, LABEL_IDENTITY, LABEL_INPUT,
    LABEL_MEDIA_AUDIO, LABEL_MEDIA_VIDEO,
};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which media backend this build enables (for diagnostics / stats HUD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportBackend {
    /// In-process mock loopback.
    Mock,
    /// Length-prefixed TCP live path (feature `live`).
    Live,
    /// Pure-Rust webrtc crate PeerTransport (feature `webrtc-rs`).
    WebrtcRs,
    /// Placeholder for future libwebrtc FFI.
    Libwebrtc,
}

/// Backends compiled into this binary.
pub fn available_backends() -> Vec<TransportBackend> {
    let _ = TransportBackend::Libwebrtc;
    [
        #[cfg(feature = "mock")]
        TransportBackend::Mock,
        #[cfg(feature = "live")]
        TransportBackend::Live,
        #[cfg(feature = "webrtc-rs")]
        TransportBackend::WebrtcRs,
    ]
    .into()
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

    #[cfg(feature = "live")]
    #[test]
    fn live_backend_listed_by_default() {
        let backends = available_backends();
        assert!(backends.contains(&TransportBackend::Live));
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

    #[test]
    fn factory_default_is_mock() {
        let cfg = TransportConfig::default();
        assert_eq!(cfg.mode, TransportMode::Mock);
        let mut t = create_peer_transport_with_config(PeerRole::Offerer, &cfg).unwrap();
        assert_eq!(t.connection_state(), ConnectionState::New);
        t.close().unwrap();
    }
}

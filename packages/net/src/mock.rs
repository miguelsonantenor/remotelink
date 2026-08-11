//! Synthetic loopback [`PeerTransport`] for unit tests and CI.
//!
//! No real ICE/DTLS/SRTP — two paired peers exchange media and data in-process
//! via [`std::sync::mpsc`] channels. Signaling methods update state machines
//! enough for host/viewer session code to exercise offer/answer/ICE wiring.
//!
//! # Event delivery (pull)
//!
//! `send_*` only enqueues on the wire. The **receiver** must call
//! [`PeerTransport::poll`] (or [`MockPeerPair::flush`]) to fire `on_track` /
//! `on_data`. ICE and connection-state callbacks fire on the peer that mutates
//! state. Closing one peer drops the shared senders so the other observes
//! [`ConnectionState::Disconnected`] on the next `poll`.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use remotelink_protocol::IceCandidate;

use crate::error::{NetError, Result};
use crate::transport::{PeerTransport, PeerTransportCallbacks, RecordingCallbacks};
use crate::types::{
    AudioPacket, ConnectionState, DataMessage, DtlsFingerprint, IncomingTrackData,
    LocalIceCandidate, ReceiverFeedback, SdpType, SessionDescription, TransportIceCandidate,
    VideoNalu,
};

/// Shared loopback fabric between two mock peers.
struct LoopbackWire {
    /// Peer A → Peer B media/data.
    a_to_b: Mutex<Option<std::sync::mpsc::Sender<WireMsg>>>,
    /// Peer B → Peer A media/data.
    b_to_a: Mutex<Option<std::sync::mpsc::Sender<WireMsg>>>,
}

#[derive(Debug, Clone)]
enum WireMsg {
    Video(VideoNalu),
    Audio(AudioPacket),
    Data(DataMessage),
}

/// Which side of a [`MockPeerPair`] this transport is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairSide {
    A,
    B,
}

/// Configuration for a mock peer (deterministic fingerprints for tests).
#[derive(Debug, Clone)]
pub struct MockPeerConfig {
    /// Fixed local fingerprint; if `None`, a synthetic unique value is generated.
    pub fingerprint: Option<DtlsFingerprint>,
    /// Label embedded in mock SDP for debugging.
    pub label: String,
}

impl Default for MockPeerConfig {
    fn default() -> Self {
        Self {
            fingerprint: None,
            label: "mock".into(),
        }
    }
}

/// In-process mock peer connection.
pub struct MockPeerTransport {
    config: MockPeerConfig,
    local_fp: DtlsFingerprint,
    remote_fp: Option<DtlsFingerprint>,
    state: ConnectionState,
    local_desc: Option<SessionDescription>,
    remote_desc: Option<SessionDescription>,
    side: Option<PairSide>,
    wire: Option<Arc<LoopbackWire>>,
    /// Receiver for inbound wire messages (drained on [`PeerTransport::poll`]).
    inbound: Option<std::sync::mpsc::Receiver<WireMsg>>,
    /// Callbacks; held as trait object for flexibility in tests.
    callbacks: Box<dyn PeerTransportCallbacks>,
    closed: bool,
    ice_restart_count: u32,
    /// Last local ICE candidate emitted (for handshake / tests).
    last_local_ice: Option<IceCandidate>,
}

impl MockPeerTransport {
    /// Standalone mock peer (not yet paired). Useful for fingerprint/SDP tests.
    pub fn new(config: MockPeerConfig) -> Self {
        let local_fp = config
            .fingerprint
            .clone()
            .unwrap_or_else(|| generate_synthetic_fingerprint().expect("synthetic fp"));
        Self {
            config,
            local_fp,
            remote_fp: None,
            state: ConnectionState::New,
            local_desc: None,
            remote_desc: None,
            side: None,
            wire: None,
            inbound: None,
            callbacks: Box::new(crate::transport::NullCallbacks),
            closed: false,
            ice_restart_count: 0,
            last_local_ice: None,
        }
    }

    /// Take ownership of the current callbacks (tests inspect recordings).
    pub fn take_callbacks(&mut self) -> Box<dyn PeerTransportCallbacks> {
        std::mem::replace(
            &mut self.callbacks,
            Box::new(crate::transport::NullCallbacks),
        )
    }

    /// Number of successful [`PeerTransport::restart_ice`] calls.
    pub fn ice_restart_count(&self) -> u32 {
        self.ice_restart_count
    }

    /// Last ICE candidate emitted by this peer (if any).
    pub fn last_local_ice(&self) -> Option<&IceCandidate> {
        self.last_local_ice.as_ref()
    }

    /// Test helper: fire [`PeerTransportCallbacks::on_receiver_feedback`].
    pub fn inject_receiver_feedback(&mut self, feedback: ReceiverFeedback) {
        self.callbacks.on_receiver_feedback(feedback);
    }

    /// Drain any pending inbound messages into callbacks.
    ///
    /// Prefer [`PeerTransport::poll`] through the trait.
    pub fn poll_inbound(&mut self) -> Result<()> {
        self.poll()
    }

    fn ensure_open(&self) -> Result<()> {
        if self.closed || self.state == ConnectionState::Closed {
            return Err(NetError::Closed);
        }
        Ok(())
    }

    fn emit_state(&mut self, state: ConnectionState) {
        self.state = state;
        self.callbacks.on_connection_state(state);
    }

    fn mock_sdp(&self, sdp_type: SdpType) -> SessionDescription {
        let fp = self.local_fp.sdp_attribute();
        let sdp = format!(
            "v=0\r\n\
             o=- 0 0 IN IP4 127.0.0.1\r\n\
             s=RemoteLinkMock-{label}\r\n\
             t=0 0\r\n\
             a=fingerprint:{fp}\r\n\
             a=setup:actpass\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
             a=rtpmap:96 H264/90000\r\n\
             m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
             a=rtpmap:111 opus/48000/2\r\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
             a=sctp-port:5000\r\n",
            label = self.config.label,
            fp = fp,
        );
        SessionDescription { sdp_type, sdp }
    }

    fn emit_local_host_candidate(&mut self) {
        let ice = IceCandidate {
            candidate: format!(
                "candidate:1 1 UDP 2122252543 127.0.0.1 {} typ host",
                40000 + self.ice_restart_count
            ),
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(0),
            username_fragment: Some(format!("mock{}", self.ice_restart_count)),
        };
        self.last_local_ice = Some(ice.clone());
        self.callbacks
            .on_ice_candidate(LocalIceCandidate { candidate: ice });
    }

    fn try_connect(&mut self) {
        if self.local_desc.is_some() && self.remote_desc.is_some() && self.wire.is_some() {
            // Parse remote fingerprint from mock SDP if present.
            if let Some(ref remote) = self.remote_desc {
                if let Some(fp) = parse_fingerprint_from_sdp(&remote.sdp) {
                    self.remote_fp = Some(fp);
                }
            }
            if self.state != ConnectionState::Connected
                && self.state != ConnectionState::Closed
                && self.state != ConnectionState::Disconnected
            {
                self.emit_state(ConnectionState::Connected);
            }
        }
    }

    fn outbound_tx(&self) -> Result<std::sync::mpsc::Sender<WireMsg>> {
        let wire = self.wire.as_ref().ok_or_else(|| NetError::InvalidState {
            expected: "paired mock peer",
            actual: "standalone".into(),
        })?;
        let side = self.side.ok_or_else(|| NetError::InvalidState {
            expected: "paired mock peer",
            actual: "no side".into(),
        })?;
        let guard = match side {
            PairSide::A => wire.a_to_b.lock(),
            PairSide::B => wire.b_to_a.lock(),
        }
        .map_err(|e| NetError::Internal(format!("wire lock: {e}")))?;
        guard
            .clone()
            .ok_or_else(|| NetError::SendFailed("peer disconnected".into()))
    }

    fn send_wire(&mut self, msg: WireMsg) -> Result<()> {
        self.ensure_open()?;
        if self.state != ConnectionState::Connected {
            return Err(NetError::InvalidState {
                expected: "connected",
                actual: self.state.as_str().into(),
            });
        }
        self.outbound_tx()?
            .send(msg)
            .map_err(|_| NetError::SendFailed("channel closed".into()))?;
        Ok(())
    }

    fn drop_wire_senders(&self) -> Result<()> {
        let Some(wire) = &self.wire else {
            return Ok(());
        };
        // Tear down both directions so the remote observes disconnect on poll.
        if let Ok(mut g) = wire.a_to_b.lock() {
            *g = None;
        }
        if let Ok(mut g) = wire.b_to_a.lock() {
            *g = None;
        }
        Ok(())
    }
}

impl PeerTransport for MockPeerTransport {
    fn set_callbacks(&mut self, callbacks: Box<dyn PeerTransportCallbacks>) {
        self.callbacks = callbacks;
    }

    fn connection_state(&self) -> ConnectionState {
        self.state
    }

    fn create_offer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        Ok(self.mock_sdp(SdpType::Offer))
    }

    fn create_answer(&mut self) -> Result<SessionDescription> {
        self.ensure_open()?;
        if self.remote_desc.is_none() {
            return Err(NetError::InvalidState {
                expected: "remote offer set",
                actual: "no remote description".into(),
            });
        }
        Ok(self.mock_sdp(SdpType::Answer))
    }

    fn set_local_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        self.local_desc = Some(desc);
        if self.state == ConnectionState::New {
            self.emit_state(ConnectionState::Connecting);
        }
        self.emit_local_host_candidate();
        self.try_connect();
        Ok(())
    }

    fn set_remote_description(&mut self, desc: SessionDescription) -> Result<()> {
        self.ensure_open()?;
        if desc.sdp.is_empty() {
            return Err(NetError::InvalidDescription("empty sdp".into()));
        }
        self.remote_desc = Some(desc);
        if self.state == ConnectionState::New {
            self.emit_state(ConnectionState::Connecting);
        }
        self.try_connect();
        Ok(())
    }

    fn add_ice_candidate(&mut self, candidate: TransportIceCandidate) -> Result<()> {
        self.ensure_open()?;
        if candidate.candidate.is_empty() {
            return Err(NetError::InvalidCandidate("empty candidate".into()));
        }
        // Mock accepts any non-empty candidate; connection still driven by descriptions.
        self.try_connect();
        Ok(())
    }

    fn restart_ice(&mut self) -> Result<()> {
        self.ensure_open()?;
        self.ice_restart_count = self.ice_restart_count.saturating_add(1);
        if self.state == ConnectionState::Connected {
            self.emit_state(ConnectionState::Connecting);
            self.emit_local_host_candidate();
            // Immediate re-connect on loopback.
            self.emit_state(ConnectionState::Connected);
        } else {
            self.emit_local_host_candidate();
        }
        Ok(())
    }

    fn local_fingerprint(&self) -> Result<DtlsFingerprint> {
        Ok(self.local_fp.clone())
    }

    fn remote_fingerprint(&self) -> Result<Option<DtlsFingerprint>> {
        Ok(self.remote_fp.clone())
    }

    fn send_video_nalu(&mut self, nalu: VideoNalu) -> Result<()> {
        self.send_wire(WireMsg::Video(nalu))
    }

    fn send_audio(&mut self, packet: AudioPacket) -> Result<()> {
        self.send_wire(WireMsg::Audio(packet))
    }

    fn send_data(&mut self, message: DataMessage) -> Result<()> {
        self.send_wire(WireMsg::Data(message))
    }

    fn poll(&mut self) -> Result<()> {
        if self.closed || self.state == ConnectionState::Closed {
            return Err(NetError::Closed);
        }
        let Some(rx) = self.inbound.as_ref() else {
            return Ok(());
        };
        loop {
            match rx.try_recv() {
                Ok(WireMsg::Video(v)) => {
                    self.callbacks.on_track(IncomingTrackData::Video(v));
                }
                Ok(WireMsg::Audio(a)) => {
                    self.callbacks.on_track(IncomingTrackData::Audio(a));
                }
                Ok(WireMsg::Data(d)) => {
                    self.callbacks.on_data(d);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if self.state == ConnectionState::Connected
                        || self.state == ConnectionState::Connecting
                    {
                        self.emit_state(ConnectionState::Disconnected);
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.drop_wire_senders()?;
        self.emit_state(ConnectionState::Closed);
        Ok(())
    }
}

/// Paired host/viewer mock transports sharing a loopback wire.
pub struct MockPeerPair {
    /// Side A (typically host / offerer).
    pub peer_a: MockPeerTransport,
    /// Side B (typically viewer / answerer).
    pub peer_b: MockPeerTransport,
}

impl MockPeerPair {
    /// Create a connected loopback pair with default configs.
    pub fn new() -> Self {
        Self::with_configs(
            MockPeerConfig {
                label: "host".into(),
                fingerprint: Some(
                    DtlsFingerprint::sha256(
                        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    )
                    .expect("host fp"),
                ),
            },
            MockPeerConfig {
                label: "viewer".into(),
                fingerprint: Some(
                    DtlsFingerprint::sha256(
                        "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                    )
                    .expect("viewer fp"),
                ),
            },
        )
    }

    /// Create a pair with explicit configs (fingerprints, labels).
    pub fn with_configs(config_a: MockPeerConfig, config_b: MockPeerConfig) -> Self {
        let (tx_ab, rx_b) = std::sync::mpsc::channel();
        let (tx_ba, rx_a) = std::sync::mpsc::channel();
        let wire = Arc::new(LoopbackWire {
            a_to_b: Mutex::new(Some(tx_ab)),
            b_to_a: Mutex::new(Some(tx_ba)),
        });

        let mut peer_a = MockPeerTransport::new(config_a);
        peer_a.side = Some(PairSide::A);
        peer_a.wire = Some(Arc::clone(&wire));
        peer_a.inbound = Some(rx_a);

        let mut peer_b = MockPeerTransport::new(config_b);
        peer_b.side = Some(PairSide::B);
        peer_b.wire = Some(wire);
        peer_b.inbound = Some(rx_b);

        Self { peer_a, peer_b }
    }

    /// Run offer/answer + exchange of **emitted** local ICE candidates.
    ///
    /// Candidates come from `on_ice_candidate` / `last_local_ice` produced by
    /// `set_local_description` (not hardcoded strings).
    pub fn handshake(&mut self) -> Result<()> {
        let offer = self.peer_a.create_offer()?;
        self.peer_a.set_local_description(offer.clone())?;
        self.peer_b.set_remote_description(offer)?;

        let answer = self.peer_b.create_answer()?;
        self.peer_b.set_local_description(answer.clone())?;
        self.peer_a.set_remote_description(answer)?;

        let cand_a = self
            .peer_a
            .last_local_ice
            .clone()
            .ok_or_else(|| NetError::Internal("host emitted no ICE".into()))?;
        let cand_b = self
            .peer_b
            .last_local_ice
            .clone()
            .ok_or_else(|| NetError::Internal("viewer emitted no ICE".into()))?;
        self.peer_a.add_ice_candidate(cand_b)?;
        self.peer_b.add_ice_candidate(cand_a)?;
        Ok(())
    }

    /// Deliver all pending loopback messages on both peers (`poll` each side).
    pub fn flush(&mut self) -> Result<()> {
        self.peer_a.poll()?;
        self.peer_b.poll()?;
        Ok(())
    }
}

impl Default for MockPeerPair {
    fn default() -> Self {
        Self::new()
    }
}

fn generate_synthetic_fingerprint() -> Result<DtlsFingerprint> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 32 bytes hex = 64 hex chars for sha-256.
    let hex = format!("{nanos:032x}{nanos:032x}");
    DtlsFingerprint::sha256(&hex[..64])
}

fn parse_fingerprint_from_sdp(sdp: &str) -> Option<DtlsFingerprint> {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("a=fingerprint:") {
            let mut parts = rest.split_whitespace();
            if let (Some(_algo), Some(value)) = (parts.next(), parts.next()) {
                return DtlsFingerprint::sha256(value).ok();
            }
        }
    }
    None
}

/// Shared recording sink for tests (cheap to clone; callbacks hold one handle).
#[derive(Debug, Default, Clone)]
pub struct SharedRecording {
    inner: std::sync::Arc<std::sync::Mutex<RecordingCallbacks>>,
}

impl SharedRecording {
    /// Create an empty recording sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of collected events.
    pub fn snapshot(&self) -> RecordingCallbacks {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl PeerTransportCallbacks for SharedRecording {
    fn on_ice_candidate(&mut self, candidate: LocalIceCandidate) {
        if let Ok(mut g) = self.inner.lock() {
            g.on_ice_candidate(candidate);
        }
    }

    fn on_connection_state(&mut self, state: ConnectionState) {
        if let Ok(mut g) = self.inner.lock() {
            g.on_connection_state(state);
        }
    }

    fn on_track(&mut self, data: IncomingTrackData) {
        if let Ok(mut g) = self.inner.lock() {
            g.on_track(data);
        }
    }

    fn on_data(&mut self, message: DataMessage) {
        if let Ok(mut g) = self.inner.lock() {
            g.on_data(message);
        }
    }

    fn on_receiver_feedback(&mut self, feedback: ReceiverFeedback) {
        if let Ok(mut g) = self.inner.lock() {
            g.on_receiver_feedback(feedback);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NaluFormat;
    use std::time::Duration;

    fn fp_aa() -> DtlsFingerprint {
        DtlsFingerprint::sha256("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
            .unwrap()
    }

    #[test]
    fn fingerprint_sha256_canonical() {
        let fp = DtlsFingerprint::sha256(
            "aabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccddaabbccdd",
        )
        .unwrap();
        assert_eq!(fp.algorithm, "sha-256");
        assert!(fp.value.starts_with("AA:BB:CC:DD:"));
        assert_eq!(fp.digest_bytes().unwrap().len(), 32);
        assert_eq!(
            fp.as_sign_material(),
            format!("sha-256 {}", fp.value.to_ascii_uppercase())
        );
    }

    #[test]
    fn fingerprint_rejects_short_hex() {
        let err = DtlsFingerprint::sha256("aabbccdd").unwrap_err();
        assert!(matches!(err, NetError::InvalidFingerprint(_)));
    }

    #[test]
    fn parse_fingerprint_from_mock_sdp() {
        let mut peer = MockPeerTransport::new(MockPeerConfig {
            label: "t".into(),
            fingerprint: Some(
                DtlsFingerprint::sha256(
                    "0102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20",
                )
                .unwrap(),
            ),
        });
        let offer = peer.create_offer().unwrap();
        let fp = parse_fingerprint_from_sdp(&offer.sdp).expect("fp in sdp");
        assert_eq!(fp.algorithm, "sha-256");
        assert!(fp.value.contains(':'));
        assert_eq!(fp, peer.local_fingerprint().unwrap());
        assert_eq!(
            fp.as_sign_material(),
            peer.local_fingerprint().unwrap().as_sign_material()
        );
    }

    #[test]
    fn pair_handshake_connects_and_exports_fingerprints() {
        let mut pair = MockPeerPair::new();
        let rec_a = SharedRecording::new();
        let rec_b = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec_a.clone()));
        pair.peer_b.set_callbacks(Box::new(rec_b.clone()));
        pair.handshake().unwrap();

        assert_eq!(pair.peer_a.connection_state(), ConnectionState::Connected);
        assert_eq!(pair.peer_b.connection_state(), ConnectionState::Connected);

        let local_a = pair.peer_a.local_fingerprint().unwrap();
        let remote_b = pair.peer_b.remote_fingerprint().unwrap().unwrap();
        assert_eq!(local_a, remote_b);

        let local_b = pair.peer_b.local_fingerprint().unwrap();
        let remote_a = pair.peer_a.remote_fingerprint().unwrap().unwrap();
        assert_eq!(local_b, remote_a);

        // Local ICE candidates were signaled during set_local_description.
        assert!(!rec_a.snapshot().ice.is_empty());
        assert!(!rec_b.snapshot().ice.is_empty());
        assert!(rec_a
            .snapshot()
            .states
            .contains(&ConnectionState::Connected));

        // Handshake forwarded the emitted candidates (ports match last_local_ice).
        let emitted_a = pair.peer_a.last_local_ice().unwrap();
        assert!(emitted_a.candidate.contains("40000"));
    }

    #[test]
    fn loopback_video_audio_data() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec.clone()));
        pair.handshake().unwrap();

        pair.peer_a
            .send_video_nalu(VideoNalu {
                pts_host_mono: Duration::from_millis(10),
                rtp_ts: Some(900),
                keyframe: true,
                format: NaluFormat::AnnexB,
                data: vec![0, 0, 0, 1, 0x65, 1, 2, 3],
            })
            .unwrap();
        pair.peer_a
            .send_audio(AudioPacket {
                pts_host_mono: Duration::from_millis(10),
                rtp_ts: Some(480),
                sample_rate: 48_000,
                channels: 2,
                data: vec![0xde, 0xad],
            })
            .unwrap();
        pair.peer_a
            .send_data(DataMessage {
                label: "input".into(),
                data: br#"{"seq":1}"#.to_vec(),
                unordered: true,
            })
            .unwrap();

        // Pull model: receiver must poll.
        pair.peer_b.poll().unwrap();
        let snap = rec.snapshot();
        assert_eq!(snap.tracks.len(), 2);
        assert_eq!(snap.data.len(), 1);
        assert_eq!(snap.data[0].label, "input");
        match &snap.tracks[0] {
            IncomingTrackData::Video(v) => assert!(v.keyframe),
            other => panic!("expected video first, got {other:?}"),
        }
    }

    #[test]
    fn ice_restart_increments() {
        let mut pair = MockPeerPair::new();
        pair.handshake().unwrap();
        assert_eq!(pair.peer_a.ice_restart_count(), 0);
        pair.peer_a.restart_ice().unwrap();
        assert_eq!(pair.peer_a.ice_restart_count(), 1);
        assert_eq!(pair.peer_a.connection_state(), ConnectionState::Connected);
        assert!(pair
            .peer_a
            .last_local_ice()
            .unwrap()
            .candidate
            .contains("40001"));
    }

    #[test]
    fn send_before_connect_fails() {
        let mut peer = MockPeerTransport::new(MockPeerConfig::default());
        let err = peer
            .send_audio(AudioPacket {
                pts_host_mono: Duration::ZERO,
                rtp_ts: None,
                sample_rate: 48_000,
                channels: 1,
                data: vec![0],
            })
            .unwrap_err();
        assert!(matches!(err, NetError::InvalidState { .. }));
    }

    #[test]
    fn close_rejects_further_sends_and_notifies_peer() {
        let mut pair = MockPeerPair::new();
        let rec_b = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec_b.clone()));
        pair.handshake().unwrap();

        pair.peer_a.close().unwrap();
        let err = pair
            .peer_a
            .send_data(DataMessage {
                label: "input".into(),
                data: vec![],
                unordered: false,
            })
            .unwrap_err();
        assert!(matches!(err, NetError::Closed));

        // Remote observes disconnect when it polls (wire senders dropped).
        pair.peer_b.poll().unwrap();
        assert_eq!(
            pair.peer_b.connection_state(),
            ConnectionState::Disconnected
        );
        assert!(rec_b
            .snapshot()
            .states
            .contains(&ConnectionState::Disconnected));
    }

    #[test]
    fn inject_receiver_feedback_reaches_callbacks() {
        let mut peer = MockPeerTransport::new(MockPeerConfig {
            fingerprint: Some(fp_aa()),
            label: "fb".into(),
        });
        let rec = SharedRecording::new();
        peer.set_callbacks(Box::new(rec.clone()));
        peer.inject_receiver_feedback(ReceiverFeedback {
            pli: true,
            fir: false,
            nack_count: 2,
            target_bitrate_bps: Some(4_000_000),
        });
        let snap = rec.snapshot();
        assert_eq!(snap.feedback.len(), 1);
        assert!(snap.feedback[0].pli);
        assert_eq!(snap.feedback[0].target_bitrate_bps, Some(4_000_000));
    }

    #[test]
    fn set_callbacks_via_trait_object() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        {
            let t: &mut dyn PeerTransport = &mut pair.peer_a;
            t.set_callbacks(Box::new(rec.clone()));
        }
        pair.handshake().unwrap();
        assert!(!rec.snapshot().ice.is_empty());
    }
}

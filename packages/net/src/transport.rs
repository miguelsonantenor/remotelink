//! [`PeerTransport`] trait — WebRTC (or mock) peer boundary for host/viewer.

use crate::error::Result;
use crate::types::{
    AudioPacket, ConnectionState, DataMessage, DtlsFingerprint, IncomingTrackData,
    LocalIceCandidate, ReceiverFeedback, SessionDescription, TransportIceCandidate, VideoNalu,
};

/// Callbacks the application registers to receive peer events.
///
/// # Delivery model
///
/// - **Mock (`MockPeerTransport`):** media/data callbacks fire only when the
///   **receiving** peer calls [`PeerTransport::poll`] (pull). ICE and connection
///   state callbacks fire synchronously on the peer that mutates state
///   (`set_local_description`, `close`, etc.). Session loops and tests must
///   `poll` / `MockPeerPair::flush` after sends.
/// - **Real backends (libwebrtc, …):** may invoke callbacks from an internal
///   network thread (push). Handlers should be cheap or re-dispatch to an app
///   executor. [`PeerTransport::poll`] is typically a no-op.
pub trait PeerTransportCallbacks: Send {
    /// Local ICE candidate ready to signal (`ice_candidate` WS message).
    fn on_ice_candidate(&mut self, candidate: LocalIceCandidate);

    /// Connection state changed.
    fn on_connection_state(&mut self, state: ConnectionState);

    /// Remote media track data (video NALU or audio packet).
    fn on_track(&mut self, data: IncomingTrackData);

    /// DataChannel message (input / control / identity challenge).
    fn on_data(&mut self, message: DataMessage);

    /// RTCP / congestion feedback for the external encoder (PLI, FIR, NACK, bitrate).
    ///
    /// Default: ignore. Host encode path (PR 16b) must handle this to request
    /// keyframes and adapt bitrate. Mock does not emit unless tests inject.
    fn on_receiver_feedback(&mut self, _feedback: ReceiverFeedback) {}
}

/// No-op callbacks for tests that only exercise send/signaling paths.
#[derive(Debug, Default)]
pub struct NullCallbacks;

impl PeerTransportCallbacks for NullCallbacks {
    fn on_ice_candidate(&mut self, _candidate: LocalIceCandidate) {}
    fn on_connection_state(&mut self, _state: ConnectionState) {}
    fn on_track(&mut self, _data: IncomingTrackData) {}
    fn on_data(&mut self, _message: DataMessage) {}
}

/// Collecting callbacks for unit tests.
#[derive(Debug, Default, Clone)]
pub struct RecordingCallbacks {
    /// ICE candidates emitted.
    pub ice: Vec<LocalIceCandidate>,
    /// Connection state history.
    pub states: Vec<ConnectionState>,
    /// Received track data.
    pub tracks: Vec<IncomingTrackData>,
    /// Received DataChannel messages.
    pub data: Vec<DataMessage>,
    /// Receiver feedback events.
    pub feedback: Vec<ReceiverFeedback>,
}

impl PeerTransportCallbacks for RecordingCallbacks {
    fn on_ice_candidate(&mut self, candidate: LocalIceCandidate) {
        self.ice.push(candidate);
    }

    fn on_connection_state(&mut self, state: ConnectionState) {
        self.states.push(state);
    }

    fn on_track(&mut self, data: IncomingTrackData) {
        self.tracks.push(data);
    }

    fn on_data(&mut self, message: DataMessage) {
        self.data.push(message);
    }

    fn on_receiver_feedback(&mut self, feedback: ReceiverFeedback) {
        self.feedback.push(feedback);
    }
}

/// Peer connection transport used by host session agent and viewer.
///
/// Real backends (libwebrtc, optional pure-Rust) and the CI mock all implement
/// this trait. Media path: external encoder → [`Self::send_video_nalu`] /
/// [`Self::send_audio`]; input path: [`Self::send_data`] on a DataChannel
/// (partial reliability for moves is backend-specific; see KD7).
///
/// Identity: export [`Self::local_fingerprint`] for SDP signing (`fingerprint_sig`)
/// and read [`Self::remote_fingerprint`] after connect for bind checks (see
/// [`DtlsFingerprint`] canonical form).
///
/// Event sinks: install via [`Self::set_callbacks`] so `dyn PeerTransport` /
/// [`BoxPeerTransport`] session maps work without downcasting.
///
/// Encoder feedback: [`PeerTransportCallbacks::on_receiver_feedback`] carries
/// PLI/FIR/NACK/target bitrate. The mock never fires it unless tests inject;
/// real backends map RTCP + GCC here so PR 16b does not need a breaking trait add.
pub trait PeerTransport: Send {
    /// Install application event callbacks (ICE, state, tracks, data, feedback).
    ///
    /// Replaces any previous sink. Works through `dyn PeerTransport`.
    fn set_callbacks(&mut self, callbacks: Box<dyn PeerTransportCallbacks>);

    /// Current connection state.
    fn connection_state(&self) -> ConnectionState;

    /// Create a local SDP offer (host is offerer per DESIGN.md).
    fn create_offer(&mut self) -> Result<SessionDescription>;

    /// Create a local SDP answer (viewer is answerer).
    fn create_answer(&mut self) -> Result<SessionDescription>;

    /// Apply a local description (after create_offer/answer).
    fn set_local_description(&mut self, desc: SessionDescription) -> Result<()>;

    /// Apply a remote description from signaling.
    fn set_remote_description(&mut self, desc: SessionDescription) -> Result<()>;

    /// Add a remote ICE candidate from signaling.
    fn add_ice_candidate(&mut self, candidate: TransportIceCandidate) -> Result<()>;

    /// Restart ICE (new ufrag/pwd) while keeping the DTLS association if possible.
    ///
    /// Mock implements a state flip + new local candidate on the loopback path;
    /// real stacks map to `RTCPeerConnection.restartIce` / equivalent.
    fn restart_ice(&mut self) -> Result<()>;

    /// Local DTLS certificate fingerprint for identity binding / SDP.
    fn local_fingerprint(&self) -> Result<DtlsFingerprint>;

    /// Remote peer DTLS fingerprint once known; `None` if not yet.
    ///
    /// Mock: from remote SDP after both descriptions. Real backends: after DTLS.
    fn remote_fingerprint(&self) -> Result<Option<DtlsFingerprint>>;

    /// Push an encoded H.264 NAL unit / AU from HW or SW encoder.
    fn send_video_nalu(&mut self, nalu: VideoNalu) -> Result<()>;

    /// Push an Opus (or mock) audio packet.
    fn send_audio(&mut self, packet: AudioPacket) -> Result<()>;

    /// Send a DataChannel application message (input events, bind challenge).
    ///
    /// Partial reliability for mouse-move labels is a backend concern; the mock
    /// always delivers reliably on the loopback channel when the receiver polls.
    fn send_data(&mut self, message: DataMessage) -> Result<()>;

    /// Pump inbound events into callbacks.
    ///
    /// **Mock:** drains the loopback queue (`on_track` / `on_data`) and observes
    /// peer hangup (`Disconnected`). **Real backends:** default no-op (push model).
    fn poll(&mut self) -> Result<()> {
        Ok(())
    }

    /// Close the peer connection and release resources.
    fn close(&mut self) -> Result<()>;
}

/// Object-safe helper: box a transport for host/viewer session maps.
pub type BoxPeerTransport = Box<dyn PeerTransport>;

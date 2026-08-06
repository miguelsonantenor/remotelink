//! Viewer session: PeerTransport answerer + decode/playout/input.

use std::sync::{Arc, Mutex};

use remotelink_net::{
    AudioPacket, BoxPeerTransport, ConnectionState, DataMessage, IncomingTrackData,
    LocalIceCandidate, PeerTransport, PeerTransportCallbacks, SessionDescription, VideoNalu,
};
use remotelink_protocol::IceCandidate;

use crate::audio::{AudioPlayoutQueue, PlayoutPacket};
use crate::connect::{connect_stub, ConnectRequest, ConnectStubResult};
use crate::decode::{DecodedVideoFrame, SyntheticVideoDecoder, VideoDecodeHook};
use crate::error::{Result, ViewerError};
use crate::input::InputEmitter;
use crate::state::{ConnectionMachine, ViewerPhase};

/// Events collected from the peer transport for the session to process on `poll`.
#[derive(Debug, Default)]
struct EventBuf {
    ice: Vec<LocalIceCandidate>,
    states: Vec<ConnectionState>,
    tracks: Vec<IncomingTrackData>,
    data: Vec<DataMessage>,
}

/// Shared callback sink installed on the transport.
#[derive(Clone, Default)]
struct SessionCallbacks {
    inner: Arc<Mutex<EventBuf>>,
}

impl PeerTransportCallbacks for SessionCallbacks {
    fn on_ice_candidate(&mut self, candidate: LocalIceCandidate) {
        if let Ok(mut g) = self.inner.lock() {
            g.ice.push(candidate);
        }
    }

    fn on_connection_state(&mut self, state: ConnectionState) {
        if let Ok(mut g) = self.inner.lock() {
            g.states.push(state);
        }
    }

    fn on_track(&mut self, data: IncomingTrackData) {
        if let Ok(mut g) = self.inner.lock() {
            g.tracks.push(data);
        }
    }

    fn on_data(&mut self, message: DataMessage) {
        if let Ok(mut g) = self.inner.lock() {
            g.data.push(message);
        }
    }
}

/// Snapshot of session stats for UI / CLI / tests.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionStats {
    /// Decoded (or synthetic) video frames produced.
    pub video_frames: u64,
    /// Audio packets enqueued for playout.
    pub audio_packets: u64,
    /// Input events sent toward the host.
    pub input_events: u64,
    /// ICE candidates emitted locally.
    pub local_ice: u64,
    /// DataChannel messages received (non-input / control).
    pub data_rx: u64,
}

/// Toolkit-agnostic viewer session (answerer side).
///
/// Owns a [`PeerTransport`], connection state machine, synthetic video decode
/// hook, audio playout queue, and input emitter. Call [`Self::poll`] regularly
/// (mock pull model; real backends may push into the same queues via callbacks).
pub struct ViewerSession {
    machine: ConnectionMachine,
    transport: Option<BoxPeerTransport>,
    callbacks: SessionCallbacks,
    video: Box<dyn VideoDecodeHook>,
    /// Latest presentable video frames (bounded).
    video_out: Vec<DecodedVideoFrame>,
    video_out_cap: usize,
    audio: AudioPlayoutQueue,
    input: InputEmitter,
    /// Local ICE candidates waiting for the signaling path to forward.
    pending_local_ice: Vec<IceCandidate>,
    /// Remote ICE not yet applied (rare with mock; useful for async signaling).
    session_id: Option<String>,
    stats: SessionStats,
    /// All inbound video NALUs (encoded) for synthetic tests.
    recorded_video_nalus: Vec<VideoNalu>,
    /// All inbound audio packets (encoded) for synthetic tests.
    recorded_audio_packets: Vec<AudioPacket>,
}

impl ViewerSession {
    /// Create a session with the default synthetic video decoder.
    pub fn new() -> Self {
        Self::with_video_hook(Box::new(SyntheticVideoDecoder::default()))
    }

    /// Create a session with a custom video decode hook.
    pub fn with_video_hook(video: Box<dyn VideoDecodeHook>) -> Self {
        let callbacks = SessionCallbacks::default();
        Self {
            machine: ConnectionMachine::new(),
            transport: None,
            callbacks,
            video,
            video_out: Vec::new(),
            video_out_cap: 32,
            audio: AudioPlayoutQueue::new(64),
            input: InputEmitter::new(),
            pending_local_ice: Vec::new(),
            session_id: None,
            stats: SessionStats::default(),
            recorded_video_nalus: Vec::new(),
            recorded_audio_packets: Vec::new(),
        }
    }

    /// Current phase.
    pub fn phase(&self) -> &ViewerPhase {
        self.machine.phase()
    }

    /// Connection machine (for UI).
    pub fn machine(&self) -> &ConnectionMachine {
        &self.machine
    }

    /// Session stats snapshot.
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Active session id after a successful connect stub / attach.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Recorded inbound video NALUs (synthetic test path).
    pub fn recorded_video_nalus(&self) -> &[VideoNalu] {
        &self.recorded_video_nalus
    }

    /// Recorded inbound audio packets (synthetic test path).
    pub fn recorded_audio_packets(&self) -> &[AudioPacket] {
        &self.recorded_audio_packets
    }

    /// Local ICE candidates that the signaling layer should forward (drain).
    pub fn take_pending_local_ice(&mut self) -> Vec<IceCandidate> {
        std::mem::take(&mut self.pending_local_ice)
    }

    /// Drain presentable video frames produced since last drain.
    pub fn drain_video_frames(&mut self) -> Vec<DecodedVideoFrame> {
        std::mem::take(&mut self.video_out)
    }

    /// Drain audio playout queue.
    pub fn drain_audio(&mut self) -> Vec<PlayoutPacket> {
        self.audio.drain()
    }

    /// Attach a peer transport (viewer is answerer). Replaces any previous.
    pub fn attach_transport(&mut self, mut transport: BoxPeerTransport) {
        transport.set_callbacks(Box::new(self.callbacks.clone()));
        self.transport = Some(transport);
    }

    /// Whether a transport is attached.
    pub fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    /// Validate credentials and mark the session as connecting (server stub).
    ///
    /// Does not open WebSocket; returns the stub session id for later signaling.
    pub fn begin_connect(&mut self, req: &ConnectRequest) -> Result<ConnectStubResult> {
        let stub = connect_stub(req)?;
        self.machine.begin_connect(req.host_public_id.clone());
        self.session_id = Some(stub.session_id.clone());
        self.stats = SessionStats::default();
        self.recorded_video_nalus.clear();
        self.recorded_audio_packets.clear();
        self.video_out.clear();
        Ok(stub)
    }

    /// Apply a remote SDP offer and create+set a local answer (answerer path).
    pub fn accept_offer(&mut self, offer: SessionDescription) -> Result<SessionDescription> {
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| ViewerError::InvalidState {
                expected: "attached transport",
                actual: "no transport".into(),
            })?;
        self.machine.begin_answer();
        transport.set_remote_description(offer)?;
        let answer = transport.create_answer()?;
        transport.set_local_description(answer.clone())?;
        // Drain any ICE/state emitted synchronously into pending.
        self.drain_callback_buf();
        Ok(answer)
    }

    /// Add a remote ICE candidate from signaling.
    pub fn add_remote_ice(&mut self, candidate: IceCandidate) -> Result<()> {
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| ViewerError::InvalidState {
                expected: "attached transport",
                actual: "no transport".into(),
            })?;
        transport.add_ice_candidate(candidate)?;
        self.drain_callback_buf();
        Ok(())
    }

    /// Pump transport + process inbound media/data into decode/playout queues.
    pub fn poll(&mut self) -> Result<()> {
        if let Some(t) = self.transport.as_mut() {
            t.poll()?;
        }
        self.drain_callback_buf();
        Ok(())
    }

    /// Emit a normalized mouse-move input event to the host.
    pub fn send_mouse_move(&mut self, x: f32, y: f32) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.mouse_move(x, y)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    /// Emit a mouse button event.
    pub fn send_mouse_button(
        &mut self,
        button: remotelink_protocol::MouseButtonKind,
        pressed: bool,
        x: f32,
        y: f32,
    ) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.mouse_button(button, pressed, x, y)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    /// Emit a key event.
    pub fn send_key(
        &mut self,
        scancode: u32,
        extended: bool,
        pressed: bool,
        modifiers: u32,
    ) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.key(scancode, extended, pressed, modifiers)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    /// Current transport connection state, if attached.
    pub fn transport_state(&self) -> Option<ConnectionState> {
        self.transport.as_ref().map(|t| t.connection_state())
    }

    /// Close transport and mark session closed.
    pub fn close(&mut self) -> Result<()> {
        if let Some(t) = self.transport.as_mut() {
            t.close()?;
        }
        self.machine.close();
        Ok(())
    }

    fn ensure_can_send_input(&self) -> Result<()> {
        if !self.machine.phase().can_send_input() {
            return Err(ViewerError::InvalidState {
                expected: "connected or streaming",
                actual: self.machine.phase().as_str().into(),
            });
        }
        Ok(())
    }

    fn send_data(&mut self, msg: DataMessage) -> Result<()> {
        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| ViewerError::InvalidState {
                expected: "attached transport",
                actual: "no transport".into(),
            })?;
        transport.send_data(msg)?;
        Ok(())
    }

    fn drain_callback_buf(&mut self) {
        let mut buf = match self.callbacks.inner.lock() {
            Ok(mut g) => EventBuf {
                ice: std::mem::take(&mut g.ice),
                states: std::mem::take(&mut g.states),
                tracks: std::mem::take(&mut g.tracks),
                data: std::mem::take(&mut g.data),
            },
            Err(_) => return,
        };

        for c in buf.ice.drain(..) {
            self.pending_local_ice.push(c.candidate);
            self.stats.local_ice = self.stats.local_ice.saturating_add(1);
        }
        for s in buf.states.drain(..) {
            self.machine.on_transport_state(s);
        }
        for t in buf.tracks.drain(..) {
            self.handle_track(t);
        }
        for d in buf.data.drain(..) {
            self.stats.data_rx = self.stats.data_rx.saturating_add(1);
            let _ = d; // control/identity handled in later PRs
        }
    }

    fn handle_track(&mut self, data: IncomingTrackData) {
        match data {
            IncomingTrackData::Video(nalu) => {
                self.recorded_video_nalus.push(nalu.clone());
                if let Some(decoded) = self.video.decode(&nalu) {
                    while self.video_out.len() >= self.video_out_cap {
                        self.video_out.remove(0);
                    }
                    self.video_out.push(decoded);
                    self.stats.video_frames = self.stats.video_frames.saturating_add(1);
                    self.machine.on_media_received();
                }
            }
            IncomingTrackData::Audio(packet) => {
                self.recorded_audio_packets.push(packet.clone());
                if self.audio.push_packet(&packet).is_ok() {
                    self.stats.audio_packets = self.audio.enqueued();
                    self.machine.on_media_received();
                }
            }
        }
    }
}

impl Default for ViewerSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Drive a complete mock loopback: host (A) offerer + viewer session as answerer (B).
///
/// Sends `video_frames` synthetic NALUs and `audio_packets` raw PCM packets from
/// the host, polls the viewer, and returns final [`SessionStats`].
pub fn run_synthetic_loopback(
    video_frames: usize,
    audio_packets: usize,
) -> Result<(ViewerSession, SessionStats)> {
    use remotelink_net::{MockPeerPair, NaluFormat};
    use std::time::Duration;

    let mut pair = MockPeerPair::new();
    let mut session = ViewerSession::new();

    let req = ConnectRequest::otp("synthetic-host", "123456").with_label("viewer-core-test");
    session.begin_connect(&req)?;

    // Answerer is peer_b.
    let viewer_transport = std::mem::replace(
        &mut pair.peer_b,
        remotelink_net::MockPeerTransport::new(remotelink_net::MockPeerConfig {
            label: "viewer-placeholder".into(),
            fingerprint: None,
        }),
    );
    session.attach_transport(Box::new(viewer_transport));

    // Handshake manually so ICE from both sides is applied.
    let offer = pair.peer_a.create_offer()?;
    pair.peer_a.set_local_description(offer.clone())?;
    let answer = session.accept_offer(offer)?;
    pair.peer_a.set_remote_description(answer)?;

    // Exchange ICE: host last local → viewer; viewer pending → host.
    if let Some(host_ice) = pair.peer_a.last_local_ice().cloned() {
        session.add_remote_ice(host_ice)?;
    }
    for ice in session.take_pending_local_ice() {
        pair.peer_a.add_ice_candidate(ice)?;
    }

    session.poll()?;
    assert_connected(&session)?;

    for i in 0..video_frames {
        pair.peer_a.send_video_nalu(VideoNalu {
            pts_host_mono: Duration::from_millis((i as u64) * 33),
            rtp_ts: Some((i as u32).saturating_mul(2970)),
            keyframe: i == 0,
            format: NaluFormat::AnnexB,
            data: vec![0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }, i as u8],
        })?;
    }
    for i in 0..audio_packets {
        let sample = (i as i16).wrapping_mul(10);
        let mut data = Vec::new();
        data.extend_from_slice(&sample.to_le_bytes());
        data.extend_from_slice(&(-sample).to_le_bytes());
        pair.peer_a.send_audio(AudioPacket {
            pts_host_mono: Duration::from_millis((i as u64) * 10),
            rtp_ts: Some((i as u32).saturating_mul(480)),
            sample_rate: 48_000,
            channels: 1,
            data,
        })?;
    }

    session.poll()?;

    // Put host back so we can still use peer_a for input receive if needed —
    // peer_b was moved into the session.

    let stats = session.stats().clone();
    Ok((session, stats))
}

fn assert_connected(session: &ViewerSession) -> Result<()> {
    match session.transport_state() {
        Some(ConnectionState::Connected) => Ok(()),
        other => Err(ViewerError::InvalidState {
            expected: "connected",
            actual: format!("{other:?}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_net::{MockPeerPair, NaluFormat, SharedRecording};
    use remotelink_protocol::MouseButtonKind;
    use std::time::Duration;

    #[test]
    fn synthetic_loopback_records_media() {
        let (session, stats) = run_synthetic_loopback(3, 2).unwrap();
        assert_eq!(stats.video_frames, 3);
        assert_eq!(stats.audio_packets, 2);
        assert_eq!(session.recorded_video_nalus().len(), 3);
        assert_eq!(session.recorded_audio_packets().len(), 2);
        assert!(matches!(
            session.phase(),
            ViewerPhase::Streaming | ViewerPhase::Connected
        ));
        assert_eq!(session.phase(), &ViewerPhase::Streaming);
    }

    #[test]
    fn input_reaches_host_via_mock() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec.clone()));

        let mut session = ViewerSession::new();
        let req = ConnectRequest::password("host-1", "s3cret");
        session.begin_connect(&req).unwrap();

        let viewer = std::mem::replace(
            &mut pair.peer_b,
            remotelink_net::MockPeerTransport::new(Default::default()),
        );
        session.attach_transport(Box::new(viewer));

        let offer = pair.peer_a.create_offer().unwrap();
        pair.peer_a.set_local_description(offer.clone()).unwrap();
        let answer = session.accept_offer(offer).unwrap();
        pair.peer_a.set_remote_description(answer).unwrap();
        if let Some(ice) = pair.peer_a.last_local_ice().cloned() {
            session.add_remote_ice(ice).unwrap();
        }
        for ice in session.take_pending_local_ice() {
            pair.peer_a.add_ice_candidate(ice).unwrap();
        }
        session.poll().unwrap();

        // Need at least connected for input.
        assert_eq!(session.transport_state(), Some(ConnectionState::Connected));

        // Media optional; phase Connected allows input.
        session.send_mouse_move(0.5, 0.25).unwrap();
        session
            .send_mouse_button(MouseButtonKind::Left, true, 0.5, 0.25)
            .unwrap();
        session.send_key(0x1C, false, true, 0).unwrap();

        pair.peer_a.poll().unwrap();
        let snap = rec.snapshot();
        assert_eq!(snap.data.len(), 3);
        assert!(snap.data.iter().all(|d| d.label == "input"));
        assert_eq!(session.stats().input_events, 3);
    }

    #[test]
    fn begin_connect_rejects_empty_host() {
        let mut session = ViewerSession::new();
        let err = session
            .begin_connect(&ConnectRequest::otp("", "123456"))
            .unwrap_err();
        assert!(matches!(err, ViewerError::InvalidConnect(_)));
    }

    #[test]
    fn input_before_connect_fails() {
        let mut session = ViewerSession::new();
        let err = session.send_mouse_move(0.0, 0.0).unwrap_err();
        assert!(matches!(err, ViewerError::InvalidState { .. }));
    }

    #[test]
    fn drain_video_after_loopback() {
        let (mut session, _) = run_synthetic_loopback(2, 0).unwrap();
        let frames = session.drain_video_frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].keyframe);
        assert!(!frames[1].keyframe);
        assert!(frames[0].frame.is_well_formed());
    }

    #[test]
    fn host_sends_nalu_viewer_records() {
        // Explicit path: Mock PeerTransport answerer receives media and records.
        let mut pair = MockPeerPair::new();
        let mut session = ViewerSession::new();
        session
            .begin_connect(&ConnectRequest::otp("h", "999999"))
            .unwrap();
        let viewer = std::mem::replace(
            &mut pair.peer_b,
            remotelink_net::MockPeerTransport::new(Default::default()),
        );
        session.attach_transport(Box::new(viewer));

        let offer = pair.peer_a.create_offer().unwrap();
        pair.peer_a.set_local_description(offer.clone()).unwrap();
        let answer = session.accept_offer(offer).unwrap();
        pair.peer_a.set_remote_description(answer).unwrap();
        if let Some(ice) = pair.peer_a.last_local_ice().cloned() {
            session.add_remote_ice(ice).unwrap();
        }
        for ice in session.take_pending_local_ice() {
            pair.peer_a.add_ice_candidate(ice).unwrap();
        }

        pair.peer_a
            .send_video_nalu(VideoNalu {
                pts_host_mono: Duration::from_millis(0),
                rtp_ts: Some(0),
                keyframe: true,
                format: NaluFormat::AnnexB,
                data: vec![0, 0, 0, 1, 0x67, 0x42],
            })
            .unwrap();
        session.poll().unwrap();
        assert_eq!(session.recorded_video_nalus().len(), 1);
        assert_eq!(session.recorded_video_nalus()[0].data.len(), 6);
        assert_eq!(session.stats().video_frames, 1);
    }
}

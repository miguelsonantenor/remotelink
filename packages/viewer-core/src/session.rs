//! Viewer session: PeerTransport answerer + decode/playout/input/skew.
//!
//! # Identity binding (PR 13 / KD17)
//!
//! After the peer is connected, the host issues a DataChannel identity
//! challenge. The viewer proves session auth material bound to
//! `session_id || fp_host || fp_viewer`. [`Self::identity_bound`] tracks
//! completion. Real DTLS certs come later; mocks use
//! [`remotelink_net::DtlsFingerprint::sha256`].
//!
//! # A/V skew (PR 17 / G3)
//!
//! On each media unit the session updates [`remotelink_media::SkewController`]
//! from last video present PTS and audio playout PTS. Required beta stats
//! (skew_ms, jitter targets, RTT placeholder, bind status) are exported via
//! [`crate::SessionStats`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use remotelink_auth::{
    respond_dc_challenge, verify_session_fingerprint, DcIdentityMessage, IdentityBindState,
    SessionBindKey, IDENTITY_CHANNEL_LABEL,
};
use remotelink_media::{JitterConfig, SkewController, SkewSample, AUDIO_CLOCK_HZ, VIDEO_CLOCK_HZ};
use remotelink_net::{
    AudioPacket, BoxPeerTransport, ConnectionState, DataMessage, IncomingTrackData,
    LocalIceCandidate, PeerTransport, PeerTransportCallbacks, SessionDescription, VideoNalu,
};
use remotelink_protocol::IceCandidate;

use crate::audio::{AudioPlayoutQueue, AudioPlayoutSink, MockAudioPlayoutSink, PlayoutPacket};
use crate::connect::{connect_stub, ConnectRequest, ConnectStubResult};
use crate::decode::{DecodedVideoFrame, MockOrSyntheticDecoder, VideoDecodeHook};
use crate::error::{Result, ViewerError};
use crate::input::{
    CaptureRect, CapturedInput, InputCapture, InputCaptureConfig, InputEmitter, RawInput,
};
use crate::state::{ConnectionMachine, ViewerPhase};
use crate::stats::{BindStatus, SessionStats};

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

/// Toolkit-agnostic viewer session (answerer side).
///
/// Owns a [`PeerTransport`], connection state machine, video decode hook,
/// audio playout queue + sink, skew controller, and input emitter. Call
/// [`Self::poll`] regularly (mock pull model; real backends may push into the
/// same queues via callbacks).
///
/// # Reconnect
///
/// [`Self::begin_connect`] and [`Self::attach_transport`] tear down any previous
/// peer (`close` + clear) so phase and transport stay aligned for a second
/// Connect from the shell. Prefer a fresh [`ViewerSession`] when identity state
/// must not carry over.
///
/// # Identity (PR 13)
///
/// Set host verifying key + session bind key before/during connect. Verify
/// `fingerprint_sig` on offer; auto-respond to DC identity challenges on poll.
pub struct ViewerSession {
    machine: ConnectionMachine,
    transport: Option<BoxPeerTransport>,
    callbacks: SessionCallbacks,
    video: Box<dyn VideoDecodeHook>,
    /// Latest presentable video frames (bounded FIFO).
    video_out: VecDeque<DecodedVideoFrame>,
    video_out_cap: usize,
    audio: AudioPlayoutQueue,
    /// Playout sink (mock by default for CI).
    playout_sink: Box<dyn AudioPlayoutSink>,
    input: InputEmitter,
    /// Focus / coalesce / normalize capture stage (toolkit feeds [`RawInput`]).
    capture: InputCapture,
    /// Local ICE candidates waiting for the signaling path to forward.
    pending_local_ice: Vec<IceCandidate>,
    /// Active / last stub session id.
    session_id: Option<String>,
    stats: SessionStats,
    /// All inbound video NALUs (encoded) for synthetic tests.
    recorded_video_nalus: Vec<VideoNalu>,
    /// All inbound audio packets (encoded) for synthetic tests.
    recorded_audio_packets: Vec<AudioPacket>,
    /// Host enrolled public key for `fingerprint_sig` verification.
    host_verifying_key: Option<VerifyingKey>,
    /// Session bind key from Mode A OTP or Mode B secret (DC challenge proof).
    bind_key: Option<SessionBindKey>,
    /// Local identity bind tracking (`identity_bound` after DC success).
    identity: IdentityBindState,
    /// Last verified host fingerprint sign material (from offer SDP).
    host_fp_sign_material: Option<String>,
    /// When true, input send requires identity_bound (default true for KD17).
    require_identity_for_input: bool,
    /// A/V skew controller (slave audio to video).
    skew: SkewController,
    /// Last video present host-equivalent ms (from PTS).
    last_video_present_ms: Option<f64>,
    /// Last audio playout host-equivalent ms (from PTS).
    last_audio_playout_ms: Option<f64>,
    /// Viewer wall clock origin for skew rate limiting (first media).
    wall_origin: Option<std::time::Instant>,
    /// Last video PTS for FPS estimate.
    last_video_pts: Option<Duration>,
    /// Video jitter config (targets exported in stats).
    video_jitter_cfg: JitterConfig,
    /// Audio jitter config (targets exported in stats).
    audio_jitter_cfg: JitterConfig,
    /// Auto-pump decoded audio into the playout sink on each track.
    auto_playout: bool,
}

impl ViewerSession {
    /// Create a session with the default mock-or-synthetic video decoder and mock sink.
    pub fn new() -> Self {
        Self::with_video_hook(Box::new(MockOrSyntheticDecoder::new()))
    }

    /// Create a session with a custom video decode hook.
    pub fn with_video_hook(video: Box<dyn VideoDecodeHook>) -> Self {
        let video_jitter_cfg = JitterConfig::wan_default();
        let audio_jitter_cfg = JitterConfig::wan_default();
        let callbacks = SessionCallbacks::default();
        let mut stats =
            SessionStats::default().with_jitter_targets(video_jitter_cfg, audio_jitter_cfg);
        stats.rtt_ms = None; // placeholder until RTCP RTT is wired
        Self {
            machine: ConnectionMachine::new(),
            transport: None,
            callbacks,
            video,
            video_out: VecDeque::new(),
            video_out_cap: 32,
            audio: AudioPlayoutQueue::new(64),
            playout_sink: Box::new(MockAudioPlayoutSink::new()),
            input: InputEmitter::new(),
            // DESIGN: input only when focused by default. Headless demos call
            // `set_focused(true)` (see `inject_demo_input`) or
            // `set_always_capture(true)` for CLI `--always-capture`.
            capture: InputCapture::new(InputCaptureConfig::default()),
            pending_local_ice: Vec::new(),
            session_id: None,
            stats,
            recorded_video_nalus: Vec::new(),
            recorded_audio_packets: Vec::new(),
            host_verifying_key: None,
            bind_key: None,
            identity: IdentityBindState::default(),
            host_fp_sign_material: None,
            // Existing synthetic tests send input without a full bind; keep
            // false for backward-compatible loopback. Callers enabling real
            // security should set require_identity_for_input(true).
            require_identity_for_input: false,
            skew: SkewController::with_defaults(),
            last_video_present_ms: None,
            last_audio_playout_ms: None,
            wall_origin: None,
            last_video_pts: None,
            video_jitter_cfg,
            audio_jitter_cfg,
            auto_playout: true,
        }
    }

    /// Replace the playout sink (e.g. null sink when only draining via API).
    pub fn set_playout_sink(&mut self, sink: Box<dyn AudioPlayoutSink>) {
        self.playout_sink = sink;
    }

    /// When true (default), each decoded audio packet is also pushed to the sink.
    pub fn set_auto_playout(&mut self, enabled: bool) {
        self.auto_playout = enabled;
    }

    /// Use LAN jitter targets (10â€“15 ms video, 15â€“25 ms audio).
    pub fn use_lan_jitter_profile(&mut self) {
        self.video_jitter_cfg = JitterConfig::lan_video();
        self.audio_jitter_cfg = JitterConfig::lan_audio();
        self.stats.video_jitter_target_ms =
            self.video_jitter_cfg.initial_target.as_secs_f64() * 1000.0;
        self.stats.audio_jitter_target_ms =
            self.audio_jitter_cfg.initial_target.as_secs_f64() * 1000.0;
    }

    /// Install the enrolled host public key used to verify `fingerprint_sig`.
    pub fn set_host_verifying_key(&mut self, key: VerifyingKey) {
        self.host_verifying_key = Some(key);
    }

    /// Install session bind key (Mode A OTP-derived or Mode B secret).
    pub fn set_bind_key(&mut self, key: SessionBindKey) {
        self.bind_key = Some(key);
    }

    /// When true, [`Self::send_mouse_move`] etc. require identity bind.
    pub fn set_require_identity_for_input(&mut self, require: bool) {
        self.require_identity_for_input = require;
    }

    /// Identity bind state (viewer side).
    pub fn identity(&self) -> &IdentityBindState {
        &self.identity
    }

    /// Whether the post-DTLS identity bind completed successfully.
    pub fn identity_bound(&self) -> bool {
        self.identity.identity_bound
    }

    /// Current phase.
    pub fn phase(&self) -> &ViewerPhase {
        self.machine.phase()
    }

    /// Connection machine (for UI).
    pub fn machine(&self) -> &ConnectionMachine {
        &self.machine
    }

    /// Session stats snapshot (includes required G3 skew metric).
    pub fn stats(&self) -> &SessionStats {
        &self.stats
    }

    /// Mutable stats (HUD drivers / tests).
    pub fn stats_mut(&mut self) -> &mut SessionStats {
        &mut self.stats
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
        self.video_out.drain(..).collect()
    }

    /// Drain audio playout queue (does not go through sink).
    pub fn drain_audio(&mut self) -> Vec<PlayoutPacket> {
        self.audio.drain()
    }

    /// Pump queued audio into the configured playout sink.
    pub fn pump_playout(&mut self, max: usize) -> Result<usize> {
        let n = self.audio.pump_to_sink(self.playout_sink.as_mut(), max)?;
        self.stats.audio_played = self.stats.audio_played.saturating_add(n as u64);
        Ok(n)
    }

    /// Attach a peer transport (viewer is answerer). Replaces any previous.
    ///
    /// Closes and drops the prior transport (if any) and clears pending local ICE
    /// / callback backlog so state cannot desync across reconnects.
    pub fn attach_transport(&mut self, mut transport: BoxPeerTransport) {
        self.teardown_transport();
        transport.set_callbacks(Box::new(self.callbacks.clone()));
        self.transport = Some(transport);
    }

    /// Whether a transport is attached.
    pub fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    /// Validate credentials and mark the session as connecting (server stub).
    ///
    /// Tears down any previous peer first so a second Connect from the UI does
    /// not leave a live `Connected` transport under a `Connecting` phase.
    /// Does not open WebSocket; returns the stub session id for later signaling.
    pub fn begin_connect(&mut self, req: &ConnectRequest) -> Result<ConnectStubResult> {
        let stub = connect_stub(req)?;
        self.teardown_transport();
        self.machine.begin_connect(req.host_public_id.clone());
        self.session_id = Some(stub.session_id.clone());
        self.identity = IdentityBindState::new(stub.session_id.clone());
        self.host_fp_sign_material = None;
        // Bind key / host key intentionally retained across reconnect only if
        // caller re-installs; clear bind flags but keep keys for same host.
        self.stats = SessionStats::default()
            .with_jitter_targets(self.video_jitter_cfg, self.audio_jitter_cfg);
        self.recorded_video_nalus.clear();
        self.recorded_audio_packets.clear();
        self.video_out.clear();
        self.audio = AudioPlayoutQueue::new(64);
        self.input = InputEmitter::new();
        // Preserve always_capture / coalesce rate across reconnect; reset focus.
        let cfg = *self.capture.config();
        self.capture = InputCapture::new(cfg);
        self.skew.reset();
        self.last_video_present_ms = None;
        self.last_audio_playout_ms = None;
        self.wall_origin = None;
        self.last_video_pts = None;
        self.sync_bind_status();
        Ok(stub)
    }

    /// Apply a remote SDP offer and create+set a local answer (answerer path).
    ///
    /// When `fingerprint_sig` and a host verifying key are present, verifies the
    /// host DTLS fingerprint binding before answering.
    pub fn accept_offer(&mut self, offer: SessionDescription) -> Result<SessionDescription> {
        self.accept_offer_with_sig(offer, None)
    }

    /// Like [`Self::accept_offer`] but verifies `fingerprint_sig` when provided.
    pub fn accept_offer_with_sig(
        &mut self,
        offer: SessionDescription,
        fingerprint_sig: Option<&str>,
    ) -> Result<SessionDescription> {
        // Parse host fingerprint from offer SDP *before* answering so MITM
        // substitutions fail even when the transport has not yet completed
        // DTLS (mock only sets remote_fp after both descriptions are set).
        if let Some(fp_mat) = fingerprint_sign_material_from_sdp(&offer.sdp) {
            self.host_fp_sign_material = Some(fp_mat);
        }

        if let Some(sig) = fingerprint_sig.filter(|s| !s.is_empty()) {
            let vk = self
                .host_verifying_key
                .as_ref()
                .ok_or_else(|| ViewerError::InvalidState {
                    expected: "host verifying key",
                    actual: "none (set_host_verifying_key)".into(),
                })?;
            let sid = self
                .session_id
                .as_deref()
                .ok_or_else(|| ViewerError::InvalidState {
                    expected: "session id",
                    actual: "none".into(),
                })?;
            let fp_mat =
                self.host_fp_sign_material
                    .as_deref()
                    .ok_or_else(|| ViewerError::InvalidState {
                        expected: "host fingerprint in SDP",
                        actual: "missing a=fingerprint".into(),
                    })?;
            verify_session_fingerprint(vk, sid, fp_mat, sig)?;
        }

        let transport = self
            .transport
            .as_mut()
            .ok_or_else(|| ViewerError::InvalidState {
                expected: "attached transport",
                actual: "no transport".into(),
            })?;
        self.machine.begin_answer();
        transport.set_remote_description(offer)?;
        // Prefer transport-exported remote fingerprint when available (real DTLS).
        if let Some(fp) = transport.remote_fingerprint()? {
            self.host_fp_sign_material = Some(fp.as_sign_material());
        }

        let answer = transport.create_answer()?;
        transport.set_local_description(answer.clone())?;
        // Drain any ICE/state emitted synchronously into pending.
        self.drain_callback_buf()?;
        Ok(answer)
    }

    /// Mark the viewer side as session-authorized (after Mode A/B success).
    pub fn mark_session_authorized(&mut self) {
        self.identity.mark_authorized();
        self.sync_bind_status();
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
        self.drain_callback_buf()?;
        Ok(())
    }

    /// Pump transport + process inbound media/data into decode/playout queues.
    ///
    /// Identity DataChannel challenges are answered automatically when a
    /// [`SessionBindKey`] is installed. Skew stats refresh when both A/V PTS
    /// have been observed.
    pub fn poll(&mut self) -> Result<()> {
        if let Some(t) = self.transport.as_mut() {
            t.poll()?;
        }
        self.drain_callback_buf()?;
        self.refresh_skew_stats();
        self.sync_bind_status();
        Ok(())
    }

    /// Input capture stage (focus policy, coalesce, normalize).
    pub fn capture(&self) -> &InputCapture {
        &self.capture
    }

    /// Mutable capture stage (tests / toolkit wiring).
    pub fn capture_mut(&mut self) -> &mut InputCapture {
        &mut self.capture
    }

    /// Set window focus for the input focus policy.
    ///
    /// Losing focus (when not always-capture) **discards** any coalesced pending
    /// mouse move so a stale position is not sent after re-focus.
    pub fn set_focused(&mut self, focused: bool) {
        self.capture.set_focused(focused);
    }

    /// When true, send input even if unfocused (CLI: `--always-capture`).
    ///
    /// Default is **false** (DESIGN: focused-only). Enabling this is an explicit
    /// product choice and should surface a UI warning.
    pub fn set_always_capture(&mut self, always: bool) {
        self.capture.set_always_capture(always);
    }

    /// Whether always-capture is enabled.
    pub fn always_capture(&self) -> bool {
        self.capture.always_capture()
    }

    /// Update the rectangle used to normalize pointer coordinates.
    pub fn set_capture_rect(&mut self, rect: CaptureRect) {
        self.capture.set_rect(rect);
    }

    /// Earliest instant at which a deferred coalesced move may emit, if any.
    ///
    /// UI frame loops can schedule the next [`Self::poll_input_capture`] from this
    /// (or simply poll every frame).
    pub fn input_next_poll_deadline(&self) -> Option<std::time::Instant> {
        self.capture.next_poll_deadline()
    }

    /// Feed a raw platform sample through capture â†’ encode â†’ DataChannel.
    ///
    /// Returns the number of wire events sent (0 when focus policy blocks).
    ///
    /// # Continuous mouse moves
    ///
    /// After the first move in a coalesce window, further moves only update
    /// pending state. Call [`Self::poll_input_capture`] on each UI frame (or at
    /// ~`coalesce_hz`) so deferred moves are delivered. Button/key/wheel events
    /// flush pending automatically and do not require a separate poll.
    pub fn push_raw_input(&mut self, raw: RawInput) -> Result<usize> {
        self.ensure_can_send_input()?;
        let now = std::time::Instant::now();
        let captured = self.capture.push(raw, now);
        let mut sent = 0usize;
        for c in captured {
            self.send_captured(&c)?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
    }

    /// Poll coalesced mouse moves that became due since the last push.
    ///
    /// **Required for continuous pointer streams:** without this (or a
    /// button/key/wheel flush), only the first move in each coalesce interval is
    /// sent from [`Self::push_raw_input`]. Call from the UI frame loop or a timer
    /// near [`Self::input_next_poll_deadline`].
    ///
    /// Uses the same phase/identity gate as [`Self::push_raw_input`] (hard `Err`
    /// when not connected / identity not bound), so frame loops can distinguish
    /// â€œnot allowedâ€ from â€œnothing dueâ€ (`Ok(0)`).
    pub fn poll_input_capture(&mut self) -> Result<usize> {
        self.ensure_can_send_input()?;
        let now = std::time::Instant::now();
        let mut sent = 0usize;
        while let Some(c) = self.capture.poll(now) {
            self.send_captured(&c)?;
            sent = sent.saturating_add(1);
        }
        Ok(sent)
    }

    /// Force-send a normalized mouse-move, **bypassing** focus policy and coalesce.
    ///
    /// Prefer [`Self::push_raw_input`] from UI paths. This API is for tests,
    /// harnesses, and synthetic demos that intentionally skip capture policy.
    /// DESIGN â€œfocused-onlyâ€ is enforced only on the capture path.
    pub fn send_mouse_move(&mut self, x: f32, y: f32) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.mouse_move(x, y)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    /// Force-send a mouse button event (**bypasses** focus policy and coalesce).
    ///
    /// See [`Self::send_mouse_move`] for policy notes; UI should use
    /// [`Self::push_raw_input`].
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

    /// Force-send a mouse wheel event (**bypasses** focus policy and coalesce).
    ///
    /// See [`Self::send_mouse_move`] for policy notes; UI should use
    /// [`Self::push_raw_input`].
    pub fn send_mouse_wheel(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        precise: bool,
        x: f32,
        y: f32,
    ) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.mouse_wheel(delta_x, delta_y, precise, x, y)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    /// Force-send a key event (**bypasses** focus policy and coalesce).
    ///
    /// See [`Self::send_mouse_move`] for policy notes; UI should use
    /// [`Self::push_raw_input`].
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

    /// Force-send a named key via the scan-set-1 table (**bypasses** focus policy).
    ///
    /// See [`Self::send_mouse_move`] for policy notes; UI should use
    /// [`Self::push_raw_input`].
    pub fn send_key_named(
        &mut self,
        key: remotelink_protocol::NamedKey,
        pressed: bool,
        modifiers: u32,
    ) -> Result<()> {
        self.ensure_can_send_input()?;
        let (_ev, msg) = self.input.key_named(key, pressed, modifiers)?;
        self.send_data(msg)?;
        self.stats.input_events = self.input.emitted();
        Ok(())
    }

    fn send_captured(&mut self, captured: &CapturedInput) -> Result<()> {
        let (_ev, msg) = self.input.encode_captured(captured)?;
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
        self.teardown_transport();
        self.machine.close();
        self.playout_sink.flush();
        Ok(())
    }

    /// Close and drop the peer transport; clear ICE and callback backlog.
    fn teardown_transport(&mut self) {
        if let Some(mut t) = self.transport.take() {
            let _ = t.close();
        }
        self.pending_local_ice.clear();
        if let Ok(mut g) = self.callbacks.inner.lock() {
            *g = EventBuf::default();
        }
    }

    fn ensure_can_send_input(&self) -> Result<()> {
        if !self.machine.phase().can_send_input() {
            return Err(ViewerError::InvalidState {
                expected: "connected or streaming",
                actual: self.machine.phase().as_str().into(),
            });
        }
        if self.require_identity_for_input && !self.identity.identity_bound {
            return Err(ViewerError::InvalidState {
                expected: "identity_bound",
                actual: "identity not bound".into(),
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

    fn drain_callback_buf(&mut self) -> Result<()> {
        let mut buf = match self.callbacks.inner.lock() {
            Ok(mut g) => EventBuf {
                ice: std::mem::take(&mut g.ice),
                states: std::mem::take(&mut g.states),
                tracks: std::mem::take(&mut g.tracks),
                data: std::mem::take(&mut g.data),
            },
            Err(_) => return Ok(()),
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
        // Collect identity work so we can mutably use transport after the loop.
        let mut identity_msgs = Vec::new();
        for d in buf.data.drain(..) {
            self.stats.data_rx = self.stats.data_rx.saturating_add(1);
            if d.label == IDENTITY_CHANNEL_LABEL {
                identity_msgs.push(d.data);
            }
        }
        for data in identity_msgs {
            self.handle_identity_message(&data)?;
        }
        Ok(())
    }

    fn handle_identity_message(&mut self, data: &[u8]) -> Result<()> {
        let parsed = DcIdentityMessage::parse(data)?;
        self.stats.identity_messages = self.stats.identity_messages.saturating_add(1);
        match parsed {
            DcIdentityMessage::Challenge(challenge) => {
                let bind_key = self
                    .bind_key
                    .as_ref()
                    .ok_or_else(|| ViewerError::InvalidState {
                        expected: "session bind key",
                        actual: "no bind key for dc challenge".into(),
                    })?;
                let sid = self
                    .session_id
                    .as_deref()
                    .ok_or_else(|| ViewerError::InvalidState {
                        expected: "session id",
                        actual: "none".into(),
                    })?;
                let transport =
                    self.transport
                        .as_mut()
                        .ok_or_else(|| ViewerError::InvalidState {
                            expected: "attached transport",
                            actual: "no transport".into(),
                        })?;
                let fp_host = transport
                    .remote_fingerprint()?
                    .ok_or_else(|| ViewerError::InvalidState {
                        expected: "remote host fingerprint",
                        actual: "unknown".into(),
                    })?
                    .as_sign_material();
                let fp_viewer = transport.local_fingerprint()?.as_sign_material();
                let response =
                    respond_dc_challenge(bind_key, sid, &challenge, &fp_host, &fp_viewer);
                transport.send_data(DataMessage {
                    label: IDENTITY_CHANNEL_LABEL.into(),
                    data: response.encode(),
                    unordered: false,
                })?;
                // Viewer marks local identity_bound after successfully answering;
                // host independently verifies the MAC before accepting input.
                self.identity.mark_identity_bound();
                self.sync_bind_status();
                Ok(())
            }
            DcIdentityMessage::Response(_) => Err(ViewerError::InvalidState {
                expected: "dc_challenge from host",
                actual: "dc_response".into(),
            }),
        }
    }

    fn handle_track(&mut self, data: IncomingTrackData) {
        match data {
            IncomingTrackData::Video(nalu) => {
                self.recorded_video_nalus.push(nalu.clone());
                if let Some(decoded) = self.video.decode(&nalu) {
                    if decoded.from_mock_h264 {
                        self.stats.mock_h264_frames = self.stats.mock_h264_frames.saturating_add(1);
                    }
                    // FPS estimate from PTS deltas.
                    if let Some(prev) = self.last_video_pts {
                        let dt = decoded.pts_host_mono.saturating_sub(prev).as_secs_f64();
                        if dt > 1e-6 {
                            self.stats.video_fps = 1.0 / dt;
                        }
                    }
                    self.last_video_pts = Some(decoded.pts_host_mono);
                    self.last_video_present_ms = Some(duration_to_host_ms(decoded.pts_host_mono));
                    // Crude bitrate estimate from encoded size * fps.
                    if self.stats.video_fps > 0.0 {
                        self.stats.video_bitrate_bps =
                            (decoded.encoded_len as f64 * 8.0 * self.stats.video_fps).round()
                                as u64;
                    }
                    while self.video_out.len() >= self.video_out_cap {
                        self.video_out.pop_front();
                    }
                    self.video_out.push_back(decoded);
                    self.stats.video_frames = self.stats.video_frames.saturating_add(1);
                    self.machine.on_media_received();
                    self.note_wall();
                    self.refresh_skew_stats();
                }
            }
            IncomingTrackData::Audio(packet) => {
                self.recorded_audio_packets.push(packet.clone());
                if self.audio.push_packet(&packet).is_ok() {
                    self.stats.audio_packets = self.audio.enqueued();
                    self.stats.mock_opus_packets = self.audio.mock_opus_decoded();
                    // Playout PTS = packet PTS (+ jitter target as host-equiv delay).
                    let playout_ms = duration_to_host_ms(packet.pts_host_mono)
                        + self.stats.audio_jitter_target_ms;
                    self.last_audio_playout_ms = Some(playout_ms);
                    if self.auto_playout {
                        if let Some(pkt) = self.audio.pop() {
                            if self.playout_sink.push(&pkt).is_ok() {
                                self.stats.audio_played = self.stats.audio_played.saturating_add(1);
                            }
                        }
                    }
                    self.machine.on_media_received();
                    self.note_wall();
                    self.refresh_skew_stats();
                }
            }
        }
    }

    fn note_wall(&mut self) {
        if self.wall_origin.is_none() {
            self.wall_origin = Some(std::time::Instant::now());
        }
    }

    fn wall_ms(&self) -> f64 {
        self.wall_origin
            .map(|t| t.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }

    fn refresh_skew_stats(&mut self) {
        let (Some(audio_ms), Some(video_ms)) =
            (self.last_audio_playout_ms, self.last_video_present_ms)
        else {
            return;
        };
        // Video present also includes video jitter target as host-equiv delay.
        let video_present = video_ms + self.stats.video_jitter_target_ms;
        let decision = self.skew.update(
            SkewSample {
                audio_playout_host_equiv_ms: audio_ms,
                video_present_host_equiv_ms: video_present,
            },
            self.wall_ms(),
        );
        self.stats
            .apply_skew_decision(&decision, self.skew.delay_offset_ms());
    }

    fn sync_bind_status(&mut self) {
        self.stats.identity_bound = self.identity.identity_bound;
        self.stats.bind_status = if self.identity.identity_bound {
            BindStatus::Bound
        } else if self.identity.session_authorized {
            BindStatus::AuthorizedPending
        } else {
            BindStatus::Unbound
        };
    }
}

impl Default for ViewerSession {
    fn default() -> Self {
        Self::new()
    }
}

fn duration_to_host_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Drive a complete mock loopback: host (A) offerer + viewer session as answerer (B).
///
/// Sends `video_frames` synthetic NALUs and `audio_packets` raw PCM packets from
/// the host, polls the viewer, and returns final [`SessionStats`].
pub fn run_synthetic_loopback(
    video_frames: usize,
    audio_packets: usize,
) -> Result<(ViewerSession, SessionStats)> {
    let (session, stats, _) = run_synthetic_loopback_ex(video_frames, audio_packets, false)?;
    Ok((session, stats))
}

/// Synthetic loopback with optional demo input inject while the host peer is live.
///
/// Returns `(session, stats, demo_input_events_sent)`. Input must be injected
/// before the host side of the mock pair is dropped, or `send_data` fails with
/// a closed channel.
pub fn run_synthetic_loopback_ex(
    video_frames: usize,
    audio_packets: usize,
    inject_demo: bool,
) -> Result<(ViewerSession, SessionStats, usize)> {
    use remotelink_net::{MockPeerPair, NaluFormat};

    let mut pair = MockPeerPair::new();
    let mut session = ViewerSession::new();
    // Loopback drains via sink; keep mock sink so audio_played increments.
    session.set_playout_sink(Box::new(MockAudioPlayoutSink::new()));

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

    // Exchange ICE: host last local â†’ viewer; viewer pending â†’ host.
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

    let mut demo_sent = 0usize;
    if inject_demo {
        demo_sent = inject_demo_input(&mut session, true)?;
        // Drain host so DataChannel delivery is exercised before drop.
        pair.peer_a.poll()?;
    }

    let stats = session.stats().clone();
    Ok((session, stats, demo_sent))
}

/// Loopback using mock MH264 + mock Opus (MOPU) for PR 17 acceptance.
pub fn run_mock_codec_loopback(
    video_frames: usize,
    audio_packets: usize,
) -> Result<(ViewerSession, SessionStats)> {
    let (session, stats, _) = run_mock_codec_loopback_ex(video_frames, audio_packets, false)?;
    Ok((session, stats))
}

/// Mock-codec loopback with optional demo input inject while the host peer is live.
pub fn run_mock_codec_loopback_ex(
    video_frames: usize,
    audio_packets: usize,
    inject_demo: bool,
) -> Result<(ViewerSession, SessionStats, usize)> {
    use remotelink_media::{
        AudioSource, H264Encoder, H264EncoderConfig, MockOpusEncoder, MockSoftwareEncoder,
        OpusEncoder, PixelFormat, RtpEpoch, SyntheticAudioTone, VideoFrame,
    };
    use remotelink_net::{MockPeerPair, NaluFormat};

    let t0 = Duration::from_millis(0);
    let epoch = RtpEpoch::new(t0);

    let mut pair = MockPeerPair::new();
    let mut session = ViewerSession::new();
    session.set_playout_sink(Box::new(MockAudioPlayoutSink::new()));

    let req = ConnectRequest::otp("mock-codec-host", "123456").with_label("pr17");
    session.begin_connect(&req)?;

    let viewer_transport = std::mem::replace(
        &mut pair.peer_b,
        remotelink_net::MockPeerTransport::new(Default::default()),
    );
    session.attach_transport(Box::new(viewer_transport));

    let offer = pair.peer_a.create_offer()?;
    pair.peer_a.set_local_description(offer.clone())?;
    let answer = session.accept_offer(offer)?;
    pair.peer_a.set_remote_description(answer)?;
    if let Some(host_ice) = pair.peer_a.last_local_ice().cloned() {
        session.add_remote_ice(host_ice)?;
    }
    for ice in session.take_pending_local_ice() {
        pair.peer_a.add_ice_candidate(ice)?;
    }
    session.poll()?;
    assert_connected(&session)?;

    let mut enc = MockSoftwareEncoder::new(&H264EncoderConfig {
        width: 16,
        height: 9,
        fps: 30,
        target_bitrate_bps: 2_000_000,
    })
    .with_keyframe_interval(0);
    let mut opus_enc = MockOpusEncoder::with_epoch(epoch);
    let mut tone = SyntheticAudioTone::default_a440(t0).with_max_packets(audio_packets as u64);

    for i in 0..video_frames {
        let pts = t0 + Duration::from_millis((i as u64) * 33);
        let mut pixels = vec![0u8; 16 * 9 * 3];
        pixels[0] = i as u8;
        pixels[1] = 0x80;
        pixels[2] = 0x40;
        let frame = VideoFrame {
            pts_host_mono: pts,
            width: 16,
            height: 9,
            format: PixelFormat::Rgb24,
            data: pixels,
        };
        let au = enc.encode(&frame, i == 0)?;
        pair.peer_a.send_video_nalu(VideoNalu {
            pts_host_mono: au.pts_host_mono,
            rtp_ts: Some(epoch.video_ts(pts)),
            keyframe: au.keyframe,
            format: NaluFormat::AnnexB,
            data: au.data,
        })?;
        let _ = VIDEO_CLOCK_HZ;
    }

    for _ in 0..audio_packets {
        let af = tone
            .next_frame()
            .map_err(|e| ViewerError::Media(e.to_string()))?
            .ok_or_else(|| ViewerError::Media("tone eos".into()))?;
        let pkt = opus_enc
            .encode(&af)
            .map_err(|e| ViewerError::Media(e.to_string()))?;
        pair.peer_a.send_audio(AudioPacket {
            pts_host_mono: pkt.pts_host_mono,
            rtp_ts: Some(pkt.rtp_ts),
            sample_rate: af.sample_rate,
            channels: af.channels,
            data: pkt.data,
        })?;
        let _ = AUDIO_CLOCK_HZ;
    }

    session.poll()?;

    let mut demo_sent = 0usize;
    if inject_demo {
        demo_sent = inject_demo_input(&mut session, true)?;
        pair.peer_a.poll()?;
    }

    let stats = session.stats().clone();
    Ok((session, stats, demo_sent))
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

/// Inject a small fixed set of test input events (mouse move/button/wheel + key).
///
/// Used by the CLI synthetic/mock path (`--inject-input`) and unit tests.
/// Events go through the capture stage when `via_capture` is true; otherwise
/// they use the direct `send_*` APIs (normalized 0..1 coords).
pub fn inject_demo_input(session: &mut ViewerSession, via_capture: bool) -> Result<usize> {
    use remotelink_protocol::{MouseButtonKind, NamedKey};

    if via_capture {
        // Ensure focus policy allows capture for the demo burst.
        session.set_focused(true);
        let mut n = 0usize;
        n += session.push_raw_input(RawInput::MouseMove { px: 0.5, py: 0.25 })?;
        n += session.push_raw_input(RawInput::MouseButton {
            button: MouseButtonKind::Left,
            pressed: true,
            px: 0.5,
            py: 0.25,
        })?;
        n += session.push_raw_input(RawInput::MouseButton {
            button: MouseButtonKind::Left,
            pressed: false,
            px: 0.5,
            py: 0.25,
        })?;
        n += session.push_raw_input(RawInput::MouseWheel {
            delta_x: 0.0,
            delta_y: -1.0,
            precise: false,
            px: 0.5,
            py: 0.25,
        })?;
        n += session.push_raw_input(RawInput::KeyNamed {
            key: NamedKey::A,
            pressed: true,
            modifiers: 0,
        })?;
        n += session.push_raw_input(RawInput::KeyNamed {
            key: NamedKey::A,
            pressed: false,
            modifiers: 0,
        })?;
        Ok(n)
    } else {
        session.send_mouse_move(0.5, 0.25)?;
        session.send_mouse_button(MouseButtonKind::Left, true, 0.5, 0.25)?;
        session.send_mouse_button(MouseButtonKind::Left, false, 0.5, 0.25)?;
        session.send_mouse_wheel(0.0, -1.0, false, 0.5, 0.25)?;
        session.send_key_named(NamedKey::A, true, 0)?;
        session.send_key_named(NamedKey::A, false, 0)?;
        Ok(6)
    }
}

/// Extract canonical fingerprint sign material from an SDP `a=fingerprint:` line.
fn fingerprint_sign_material_from_sdp(sdp: &str) -> Option<String> {
    for line in sdp.lines() {
        let line = line.trim();
        let rest = match line.strip_prefix("a=fingerprint:") {
            Some(r) => r.trim(),
            None => continue,
        };
        let mut parts = rest.split_whitespace();
        let Some(algo) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            continue;
        };
        let algo = algo.to_ascii_lowercase();
        let value = value.to_ascii_uppercase();
        if algo == "sha-256" && !value.is_empty() {
            return Some(format!("{algo} {value}"));
        }
    }
    None
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
        // G3: skew metric always exportable.
        assert!(stats.has_required_skew_metric());
        let hud = stats.hud_line();
        assert!(hud.contains("skew_ms="), "{hud}");
    }

    #[test]
    fn mock_codec_loopback_decodes_h264_and_opus() {
        let (session, stats) = run_mock_codec_loopback(3, 4).unwrap();
        assert_eq!(stats.video_frames, 3);
        assert_eq!(stats.mock_h264_frames, 3);
        assert_eq!(stats.audio_packets, 4);
        assert_eq!(stats.mock_opus_packets, 4);
        assert!(stats.audio_played >= 1);
        assert!(stats.has_required_skew_metric());
        // With shared t0 and aligned PTS, skew should be near zero after jitter offsets cancel.
        assert!(
            stats.skew_ms.abs() < 50.0,
            "skew out of range: {}",
            stats.skew_ms
        );
        let frames = session.stats().video_frames;
        assert_eq!(frames, 3);
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
    fn capture_path_encode_send_on_mock_transport() {
        use remotelink_protocol::{decode_input, InputPayload, NamedKey};

        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec.clone()));

        let mut session = ViewerSession::new();
        session.set_always_capture(false);
        session.set_capture_rect(CaptureRect::new(0.0, 0.0, 100.0, 100.0));
        session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
            .unwrap();
        // begin_connect resets focus; re-apply policy after connect.
        session.set_focused(true);
        session.set_always_capture(false);

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

        let sent = session
            .push_raw_input(RawInput::MouseMove { px: 50.0, py: 25.0 })
            .unwrap();
        assert_eq!(sent, 1);
        let sent = session
            .push_raw_input(RawInput::KeyNamed {
                key: NamedKey::A,
                pressed: true,
                modifiers: 0,
            })
            .unwrap();
        assert_eq!(sent, 1);

        pair.peer_a.poll().unwrap();
        let snap = rec.snapshot();
        assert_eq!(snap.data.len(), 2);
        let move_ev = decode_input(std::str::from_utf8(&snap.data[0].data).unwrap()).unwrap();
        match move_ev.payload {
            InputPayload::MouseMove(m) => {
                assert!((m.x - 0.5).abs() < 1e-5, "x={}", m.x);
                assert!((m.y - 0.25).abs() < 1e-5, "y={}", m.y);
            }
            other => panic!("expected move, got {other:?}"),
        }
        let key_ev = decode_input(std::str::from_utf8(&snap.data[1].data).unwrap()).unwrap();
        match key_ev.payload {
            InputPayload::Key(k) => {
                assert_eq!(k.scancode, 0x1E);
                assert!(k.pressed);
            }
            other => panic!("expected key, got {other:?}"),
        }
    }

    #[test]
    fn focus_policy_blocks_unfocused_raw_input() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec.clone()));

        let mut session = ViewerSession::new();
        session.set_always_capture(false);
        session.set_focused(false);
        session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
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
        session.poll().unwrap();

        let sent = session
            .push_raw_input(RawInput::MouseMove { px: 0.5, py: 0.5 })
            .unwrap();
        assert_eq!(sent, 0);
        assert!(session.capture().blocked_unfocused() >= 1);

        pair.peer_a.poll().unwrap();
        assert!(rec.snapshot().data.is_empty());

        // Direct send_* still works (harness path that intentionally bypasses focus).
        session.send_mouse_move(0.1, 0.1).unwrap();
        pair.peer_a.poll().unwrap();
        assert_eq!(rec.snapshot().data.len(), 1);
    }

    #[test]
    fn session_default_always_capture_is_false() {
        let session = ViewerSession::new();
        assert!(
            !session.always_capture(),
            "DESIGN: focused-only default; demos must set_focused or always_capture"
        );
    }

    #[test]
    fn poll_input_capture_errors_when_not_connected() {
        let mut session = ViewerSession::new();
        let err = session.poll_input_capture().unwrap_err();
        assert!(matches!(err, ViewerError::InvalidState { .. }));
    }

    #[test]
    fn poll_input_capture_delivers_coalesced_move() {
        use std::thread;
        use std::time::Duration;

        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec.clone()));

        let mut session = ViewerSession::new();
        session.set_always_capture(false);
        session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
            .unwrap();
        session.set_focused(true);
        // Fast coalesce so the sleep stays short in CI.
        session.capture_mut().set_always_capture(true);
        // Rebuild coalescer rate via config: use always_capture path and low interval
        // by feeding moves then sleeping past default 90 Hz (~11 ms).
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

        assert_eq!(
            session
                .push_raw_input(RawInput::MouseMove { px: 0.1, py: 0.1 })
                .unwrap(),
            1
        );
        assert_eq!(
            session
                .push_raw_input(RawInput::MouseMove { px: 0.9, py: 0.9 })
                .unwrap(),
            0,
            "second move pending until poll"
        );
        assert!(session.input_next_poll_deadline().is_some());

        thread::sleep(Duration::from_millis(15));
        let n = session.poll_input_capture().unwrap();
        assert_eq!(n, 1, "poll should deliver coalesced move");

        pair.peer_a.poll().unwrap();
        assert_eq!(rec.snapshot().data.len(), 2);
    }

    #[test]
    fn inject_demo_input_sends_six_events() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_a.set_callbacks(Box::new(rec.clone()));

        let mut session = ViewerSession::new();
        session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
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
        session.poll().unwrap();

        let n = inject_demo_input(&mut session, true).unwrap();
        assert_eq!(n, 6);
        pair.peer_a.poll().unwrap();
        assert_eq!(rec.snapshot().data.len(), 6);
        assert_eq!(session.stats().input_events, 6);
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

    #[test]
    fn begin_connect_after_loopback_tears_down_transport() {
        let (mut session, stats) = run_synthetic_loopback(2, 1).unwrap();
        assert_eq!(stats.video_frames, 2);
        assert!(session.has_transport());
        assert_eq!(session.phase(), &ViewerPhase::Streaming);
        assert_eq!(session.transport_state(), Some(ConnectionState::Connected));

        let stub = session
            .begin_connect(&ConnectRequest::otp("other-host", "654321"))
            .unwrap();
        assert!(stub.accepted);
        assert_eq!(session.phase(), &ViewerPhase::Connecting);
        assert!(!session.has_transport());
        assert_eq!(session.transport_state(), None);
        assert!(session.recorded_video_nalus().is_empty());
        assert!(session.recorded_audio_packets().is_empty());
        assert_eq!(session.stats().video_frames, 0);
        // Input not allowed until a new peer is connected.
        let err = session.send_mouse_move(0.1, 0.1).unwrap_err();
        assert!(matches!(err, ViewerError::InvalidState { .. }));
    }

    #[test]
    fn attach_transport_closes_previous() {
        let mut session = ViewerSession::new();
        session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
            .unwrap();

        let mut pair1 = MockPeerPair::new();
        let first = std::mem::replace(
            &mut pair1.peer_b,
            remotelink_net::MockPeerTransport::new(Default::default()),
        );
        session.attach_transport(Box::new(first));
        assert!(session.has_transport());

        let mut pair2 = MockPeerPair::new();
        let second = std::mem::replace(
            &mut pair2.peer_b,
            remotelink_net::MockPeerTransport::new(Default::default()),
        );
        session.attach_transport(Box::new(second));
        assert!(session.has_transport());
        // Re-attach does not leave dual peers; machine still Connecting until offer.
        assert_eq!(session.phase(), &ViewerPhase::Connecting);
    }

    #[test]
    fn identity_bind_verifies_fingerprint_and_dc_challenge() {
        use remotelink_auth::{
            generate_device_keypair, sign_session_fingerprint, SessionBindKey,
            IDENTITY_CHANNEL_LABEL,
        };
        use remotelink_net::{DataMessage, MockPeerTransport, PeerTransport};

        let (sk, vk) = generate_device_keypair();
        let otp = "654321";
        let bind_key = SessionBindKey::from_mode_a_otp(otp).unwrap();

        let mut pair = MockPeerPair::new();
        let mut session = ViewerSession::new();
        session.set_host_verifying_key(vk);
        session.set_bind_key(bind_key);
        session.set_require_identity_for_input(true);
        let stub = session
            .begin_connect(&ConnectRequest::otp("host-pub", otp))
            .unwrap();
        let sid = stub.session_id.clone();
        session.mark_session_authorized();

        let viewer =
            std::mem::replace(&mut pair.peer_b, MockPeerTransport::new(Default::default()));
        session.attach_transport(Box::new(viewer));

        let offer = pair.peer_a.create_offer().unwrap();
        pair.peer_a.set_local_description(offer.clone()).unwrap();
        let host_fp = pair.peer_a.local_fingerprint().unwrap().as_sign_material();
        let sig = sign_session_fingerprint(&sk, &sid, &host_fp);

        let answer = session.accept_offer_with_sig(offer, Some(&sig)).unwrap();
        pair.peer_a.set_remote_description(answer).unwrap();
        if let Some(ice) = pair.peer_a.last_local_ice().cloned() {
            session.add_remote_ice(ice).unwrap();
        }
        for ice in session.take_pending_local_ice() {
            pair.peer_a.add_ice_candidate(ice).unwrap();
        }
        session.poll().unwrap();
        assert_eq!(session.transport_state(), Some(ConnectionState::Connected));
        assert!(!session.identity_bound());

        // Input blocked until identity bind when require flag is set.
        let err = session.send_mouse_move(0.1, 0.1).unwrap_err();
        assert!(matches!(err, ViewerError::InvalidState { .. }));

        // Host issues DC challenge; viewer answers on poll.
        let challenge = remotelink_auth::DcIdentityChallenge::issue();
        pair.peer_a
            .send_data(DataMessage {
                label: IDENTITY_CHANNEL_LABEL.into(),
                data: challenge.encode(),
                unordered: false,
            })
            .unwrap();
        session.poll().unwrap();
        assert!(session.identity_bound());
        assert!(session.stats().identity_messages >= 1);
        assert_eq!(session.stats().bind_status, BindStatus::Bound);

        // Viewer may send input after local identity bind.
        session.send_mouse_move(0.5, 0.5).unwrap();
    }

    #[test]
    fn wrong_fingerprint_sig_rejected_on_offer() {
        use remotelink_auth::{generate_device_keypair, sign_session_fingerprint};

        let (sk, vk) = generate_device_keypair();
        let mut pair = MockPeerPair::new();
        let mut session = ViewerSession::new();
        session.set_host_verifying_key(vk);
        let stub = session
            .begin_connect(&ConnectRequest::otp("h", "123456"))
            .unwrap();
        let _sid = stub.session_id;

        let viewer = std::mem::replace(
            &mut pair.peer_b,
            remotelink_net::MockPeerTransport::new(Default::default()),
        );
        session.attach_transport(Box::new(viewer));

        let offer = pair.peer_a.create_offer().unwrap();
        pair.peer_a.set_local_description(offer.clone()).unwrap();
        let host_fp = pair.peer_a.local_fingerprint().unwrap().as_sign_material();
        // Sign wrong session id so verification fails.
        let sig = sign_session_fingerprint(&sk, "other-session", &host_fp);
        let err = session
            .accept_offer_with_sig(offer, Some(&sig))
            .unwrap_err();
        assert!(matches!(err, ViewerError::Auth(_)));
    }

    #[test]
    fn stats_snapshot_exports_skew_and_jitter() {
        let (_session, stats) = run_mock_codec_loopback(2, 2).unwrap();
        assert!(stats.has_required_skew_metric());
        assert!(stats.video_jitter_target_ms > 0.0);
        assert!(stats.audio_jitter_target_ms > 0.0);
        // RTT is a required placeholder field (None until RTCP).
        assert!(stats.rtt_ms.is_none());
        let block = stats.hud_block();
        assert!(block.contains("A/V skew"), "{block}");
        assert!(block.contains("Bind:"), "{block}");
    }
}

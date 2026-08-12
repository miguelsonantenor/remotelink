//! Agent-side session media plane: synthetic A/V + in-process [`PeerTransport`].
//!
//! KD5: video NALUs and audio packets stay **inside** the agent and go only to
//! PeerTransport. Control IPC carries signaling ([`SignalForward`]) never media.
//!
//! # Identity binding (PR 13 / KD17)
//!
//! Host **MUST NOT** accept input until
//! [`IdentityBindState::input_allowed`](remotelink_auth::IdentityBindState::input_allowed)
//! (`identity_bound && session_authorized`). Mode A/B authorization and the
//! post-DTLS DataChannel challenge are driven through this manager.
//!
//! # Input injection (PR 18)
//!
//! Inbound DataChannel messages on [`INPUT_CHANNEL_LABEL`] are decoded as
//! [`InputEvent`](remotelink_protocol::InputEvent) and passed to a
//! [`AnyInjector`](remotelink_platform_windows::AnyInjector) **only** when the
//! identity gate is open and the policy/kill gate (see
//! [`SessionManager::set_input_policy_enabled`]) allows inject. Rate limit:
//! 200 events/s with drop metric on the injector.
//!
//! Real DTLS certificates are deferred; the mock PeerTransport uses synthetic
//! [`DtlsFingerprint::sha256`](remotelink_net::DtlsFingerprint::sha256) values
//! exported via [`PeerTransport::local_fingerprint`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use remotelink_auth::{
    authorize_mode_a, authorize_mode_b, complete_dc_bind, sign_session_fingerprint, AuthChallenge,
    AuthError, DcIdentityChallenge, DcIdentityMessage, HostSecret, IdentityBindState, OtpRecord,
    SessionBindKey, IDENTITY_CHANNEL_LABEL,
};
use remotelink_media::{AudioSource, MockOpusEncoder, OpusEncoder, RtpEpoch, VideoSource};
use remotelink_net::{
    create_peer_transport_with_config, AudioPacket, BoxPeerTransport, ConnectionState, DataMessage,
    LocalIceCandidate, MockPeerConfig, MockPeerTransport, NaluFormat, NetError, PeerRole,
    PeerTransport, PeerTransportCallbacks, SessionDescription, TransportConfig,
    TransportIceCandidate, TransportMode, VideoNalu,
};
use remotelink_platform_windows::ipc::message::{SignalForward, SignalHop};
use remotelink_platform_windows::{
    open_injector, AnyInjector, InjectError, InjectorConfig, InputMetrics, StubInjector,
};
use remotelink_protocol::{decode_input, IceCandidate, InputEvent};
use serde::{Deserialize, Serialize};

/// Max decoded events retained for diagnostics / tests (`take_accepted_input`).
///
/// Prevents unbounded growth on the hot inject path; older events are dropped
/// first (ring). Production should use injector metrics + stub recordings.
const ACCEPTED_INPUT_CAP: usize = 64;

/// DataChannel label for viewer → host input events.
pub const INPUT_CHANNEL_LABEL: &str = "input";

/// Signaling kinds carried on control IPC [`SignalForward::kind`].
pub mod signal_kind {
    /// Local/remote SDP offer (`payload` = [`SdpPayload`] JSON).
    pub const SESSION_OFFER: &str = "session_offer";
    /// Local/remote SDP answer (`payload` = [`SdpPayload`] JSON).
    pub const SESSION_ANSWER: &str = "session_answer";
    /// ICE candidate (`payload` = [`IceCandidate`] JSON).
    pub const ICE_CANDIDATE: &str = "ice_candidate";
}

/// JSON body for `session_offer` / `session_answer` SignalForward payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdpPayload {
    /// Full SDP body.
    pub sdp: String,
    /// Optional identity fingerprint signature (PR 13); empty in synthetic mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_sig: Option<String>,
}

/// Errors from the agent session media plane.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// PeerTransport / net layer failure.
    #[error("transport: {0}")]
    Transport(#[from] NetError),
    /// Media encode / source failure.
    #[error("media: {0}")]
    Media(String),
    /// Signaling payload could not be parsed or applied.
    #[error("signaling: {0}")]
    Signaling(String),
    /// Session / media not in the expected state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// Auth / identity bind failure.
    #[error("auth: {0}")]
    Auth(#[from] AuthError),
    /// Input rejected by identity gate.
    #[error("input rejected: {0}")]
    InputRejected(String),
}

/// Result alias for session media plane operations.
pub type Result<T> = std::result::Result<T, SessionError>;

/// Stats from one [`SessionManager::pump_media`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PumpStats {
    /// Video NALUs sent.
    pub video_sent: u32,
    /// Audio packets sent.
    pub audio_sent: u32,
    /// True when peer was not Connected (nothing sent).
    pub skipped_not_connected: bool,
}

/// Outcome of processing inbound DataChannel messages (identity + input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InboundStats {
    /// Identity DC messages handled.
    pub identity_messages: u32,
    /// Input events decoded and injected (or accepted into the inject path).
    pub input_accepted: u32,
    /// Input events rejected (not bound / not authorized / policy / kill).
    pub input_rejected: u32,
    /// Input messages that failed JSON decode.
    pub input_decode_errors: u32,
    /// Input events dropped by the 200 events/s rate limiter.
    pub input_rate_limited: u32,
}

/// Result of handling one input DataChannel payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputProcessOutcome {
    /// Decoded and injected into the OS / stub.
    Injected,
    /// Gate closed (`!identity_bound || !authorized || !policy`).
    RejectedGate,
    /// Payload was not valid [`InputEvent`] JSON.
    DecodeError,
    /// Gate open but dropped by the host rate limiter.
    RateLimited,
    /// Injector backend returned an error.
    InjectError,
}

/// Capture + encode state for one media start.
struct MediaPlane {
    video: crate::platform_capture::HostVideoSource,
    audio: crate::platform_capture::HostAudioSource,
    opus: MockOpusEncoder,
    /// H.264 encoder (software mock today; HW falls back until linked).
    h264: remotelink_platform_windows::AnyH264Encoder,
    epoch: RtpEpoch,
    video_frames_sent: u64,
    audio_packets_sent: u64,
    /// Capture backend label for stats / logs.
    video_backend: &'static str,
    audio_backend: &'static str,
    /// Encoder backend label for stats / logs.
    encode_backend: &'static str,
}

#[derive(Debug, Default)]
struct AgentPeerCallbacksInner {
    ice: Vec<LocalIceCandidate>,
    states: Vec<ConnectionState>,
    data: Vec<DataMessage>,
}

/// Shared callback sink used by both the transport and the session manager.
#[derive(Debug, Default, Clone)]
struct SharedAgentCallbacks {
    inner: Arc<Mutex<AgentPeerCallbacksInner>>,
}

impl SharedAgentCallbacks {
    fn new() -> Self {
        Self::default()
    }

    fn take_ice(&self) -> Vec<LocalIceCandidate> {
        self.inner
            .lock()
            .map(|mut g| std::mem::take(&mut g.ice))
            .unwrap_or_default()
    }

    fn take_data(&self) -> Vec<DataMessage> {
        self.inner
            .lock()
            .map(|mut g| std::mem::take(&mut g.data))
            .unwrap_or_default()
    }
}

impl PeerTransportCallbacks for SharedAgentCallbacks {
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

    fn on_track(&mut self, _data: remotelink_net::IncomingTrackData) {}

    fn on_data(&mut self, message: DataMessage) {
        if let Ok(mut g) = self.inner.lock() {
            g.data.push(message);
        }
    }
}

/// Owns the agent PeerTransport and (when started) synthetic A/V sources.
///
/// Media bytes never leave this process via control IPC.
///
/// Identity (PR 13): after Mode A/B auth and a successful DataChannel bind,
/// [`Self::input_allowed`] is true. Input inject (PR 18) also requires
/// [`Self::set_input_policy_enabled`] (policy + not killed).
pub struct SessionManager {
    session_id: Option<String>,
    peer: BoxPeerTransport,
    cb: SharedAgentCallbacks,
    media: Option<MediaPlane>,
    /// Pending control SignalForward messages for the service (A→S).
    outbound_signals: Vec<SignalForward>,
    /// Capture geometry (mock / synthetic); DXGI uses desktop size.
    synth_width: u32,
    synth_height: u32,
    synth_fps: u32,
    /// Video capture backend selection.
    video_kind: crate::platform_capture::VideoCaptureKind,
    /// Audio capture backend selection.
    audio_kind: crate::platform_capture::AudioCaptureKind,
    /// Identity bind flags for the attached session.
    identity: IdentityBindState,
    /// Enrolled device signing key (host). Required for `fingerprint_sig`.
    device_signing_key: Option<SigningKey>,
    /// Session bind key after Mode A/B authorization (for DC challenge).
    bind_key: Option<SessionBindKey>,
    /// Outstanding DC identity challenge awaiting viewer response.
    pending_dc_challenge: Option<DcIdentityChallenge>,
    /// Recent decoded+injected events (capped ring for tests / diagnostics).
    accepted_input: Vec<InputEvent>,
    /// Count of rejected input messages (gate closed).
    rejected_input_count: u64,
    /// Count of input messages that failed decode.
    input_decode_errors: u64,
    /// Count of events dropped by the rate limiter.
    input_rate_limited: u64,
    /// Policy + kill gate from the agent control plane (`enable_input && !killed`).
    policy_input_enabled: bool,
    /// Config used to (re)open the injector on each [`Self::attach`].
    ///
    /// Defaults to [`InjectorConfig::synthetic`] (StubOnly). Production should
    /// call [`Self::set_injector_config`] with PreferNative open mode so attach
    /// preserves the native backend instead of forcing the stub.
    injector_config: InjectorConfig,
    /// OS / stub injector (rate-limited); re-opened from `injector_config` on attach.
    injector: AnyInjector,
    /// Force the mock software H.264 path (needed so the viewer can reconstruct pixels).
    force_software: bool,
}

impl SessionManager {
    /// Create a session manager owning a standalone mock PeerTransport.
    pub fn new_mock() -> Self {
        Self::with_peer(Box::new(MockPeerTransport::new(MockPeerConfig {
            label: "host-agent".into(),
            fingerprint: None,
        })))
    }

    /// Create a session manager for the host **offerer** role using the transport factory.
    ///
    /// Respects `REMOTELINK_TRANSPORT` / [`TransportConfig`] (`mock` / `live` /
    /// `webrtc` / `auto`). Prefer this for production agent wiring so the media
    /// plane is not hard-coded to the mock backend.
    pub fn from_transport_config(config: &TransportConfig) -> Result<Self> {
        let peer = create_peer_transport_with_config(PeerRole::Offerer, config)?;
        Ok(Self::with_peer(peer))
    }

    /// [`Self::from_transport_config`] with [`TransportConfig::from_env`].
    pub fn from_env() -> Result<Self> {
        Self::from_transport_config(&TransportConfig::from_env())
    }

    /// [`Self::from_transport_config`] with an explicit [`TransportMode`].
    pub fn from_mode(mode: TransportMode) -> Result<Self> {
        Self::from_transport_config(&TransportConfig { mode })
    }

    /// Create a session manager that owns the given PeerTransport (e.g. mock pair side A).
    pub fn with_peer(mut peer: BoxPeerTransport) -> Self {
        let cb = SharedAgentCallbacks::new();
        peer.set_callbacks(Box::new(cb.clone()));
        Self {
            session_id: None,
            peer,
            cb,
            media: None,
            outbound_signals: Vec::new(),
            synth_width: 64,
            synth_height: 36,
            synth_fps: 30,
            video_kind: crate::platform_capture::default_video_kind(),
            audio_kind: crate::platform_capture::default_audio_kind(),
            identity: IdentityBindState::default(),
            device_signing_key: None,
            bind_key: None,
            pending_dc_challenge: None,
            accepted_input: Vec::new(),
            rejected_input_count: 0,
            input_decode_errors: 0,
            input_rate_limited: 0,
            policy_input_enabled: false,
            // CI-safe recording stub by default; use set_injector_config(PreferNative)
            // so attach re-opens the same mode (does not silently force StubOnly).
            injector_config: InjectorConfig::synthetic(),
            injector: open_injector(InjectorConfig::synthetic())
                .expect("stub injector always opens"),
            force_software: false,
        }
    }

    /// Force the software/mock H.264 encoder (viewer can reconstruct pixels).
    pub fn set_force_software(&mut self, force: bool) {
        self.force_software = force;
    }

    /// Select video capture backend (call before [`Self::start_media`]).
    pub fn set_video_kind(&mut self, kind: crate::platform_capture::VideoCaptureKind) {
        self.video_kind = kind;
    }

    /// Select audio capture backend (call before [`Self::start_media`]).
    pub fn set_audio_kind(&mut self, kind: crate::platform_capture::AudioCaptureKind) {
        self.audio_kind = kind;
    }

    /// Current video capture kind.
    pub fn video_kind(&self) -> crate::platform_capture::VideoCaptureKind {
        self.video_kind
    }

    /// Current audio capture kind.
    pub fn audio_kind(&self) -> crate::platform_capture::AudioCaptureKind {
        self.audio_kind
    }

    /// Config that will be used to re-open the injector on the next attach.
    pub fn injector_config(&self) -> InjectorConfig {
        self.injector_config
    }

    /// Open (or re-open) the injector from `config` and remember it for attach.
    ///
    /// Prefer this over [`Self::set_injector`] so session re-attach preserves
    /// `PreferNative` / rate-limit settings instead of resetting to StubOnly.
    pub fn set_injector_config(
        &mut self,
        config: InjectorConfig,
    ) -> std::result::Result<(), InjectError> {
        self.injector = open_injector(config)?;
        self.injector_config = config;
        Ok(())
    }

    /// Replace the live injector without changing the attach re-open config.
    ///
    /// For production wiring that must survive attach, use
    /// [`Self::set_injector_config`] instead.
    pub fn set_injector(&mut self, injector: AnyInjector) {
        self.injector = injector;
    }

    /// Policy/kill gate: agent sets this from `enable_input && !killed`.
    ///
    /// Even when identity-bound, inject is refused while this is `false`.
    pub fn set_input_policy_enabled(&mut self, enabled: bool) {
        self.policy_input_enabled = enabled;
    }

    /// Whether the agent policy currently allows inject (independent of identity).
    pub fn policy_input_enabled(&self) -> bool {
        self.policy_input_enabled
    }

    /// True when identity bind **and** policy/kill gate allow inject.
    pub fn injection_allowed(&self) -> bool {
        self.policy_input_enabled && self.identity.input_allowed()
    }

    /// Test/harness helper: mark session authorized + identity bound without DC.
    ///
    /// Production code must complete Mode A/B + DataChannel bind instead.
    pub fn force_identity_bound_for_tests(&mut self) {
        self.identity.mark_authorized();
        self.identity.mark_identity_bound();
    }

    /// Injector metrics (accepted + rate-limit drops).
    pub fn input_metrics(&self) -> InputMetrics {
        self.injector.metrics()
    }

    /// Injector backend name (`"stub"` / `"sendinput"`).
    pub fn input_backend_name(&self) -> &'static str {
        self.injector.backend_name()
    }

    /// Access the recording stub when the injector is stub-backed (tests).
    pub fn input_stub(&self) -> Option<&StubInjector> {
        self.injector.stub()
    }

    /// Mutable access to the recording stub when stub-backed (tests).
    pub fn input_stub_mut(&mut self) -> Option<&mut StubInjector> {
        self.injector.stub_mut()
    }

    /// Install the enrolled device signing key used for `fingerprint_sig`.
    pub fn set_device_signing_key(&mut self, key: SigningKey) {
        self.device_signing_key = Some(key);
    }

    /// Current identity bind state.
    pub fn identity(&self) -> &IdentityBindState {
        &self.identity
    }

    /// Host may accept remote input only when identity-bound and session-authorized.
    pub fn input_allowed(&self) -> bool {
        self.identity.input_allowed()
    }

    /// Recent accepted (decoded + injected) events (drain for tests).
    ///
    /// At most [`ACCEPTED_INPUT_CAP`] events are retained; older ones are dropped.
    pub fn take_accepted_input(&mut self) -> Vec<InputEvent> {
        std::mem::take(&mut self.accepted_input)
    }

    /// Diagnostic ring capacity for accepted input events.
    pub const fn accepted_input_cap() -> usize {
        ACCEPTED_INPUT_CAP
    }

    fn push_accepted_input(&mut self, event: InputEvent) {
        if self.accepted_input.len() >= ACCEPTED_INPUT_CAP {
            let overflow = self.accepted_input.len() + 1 - ACCEPTED_INPUT_CAP;
            self.accepted_input.drain(0..overflow);
        }
        self.accepted_input.push(event);
    }

    /// Number of input messages rejected by the identity/policy gate.
    pub fn rejected_input_count(&self) -> u64 {
        self.rejected_input_count
    }

    /// Number of input messages that failed protocol decode.
    pub fn input_decode_errors(&self) -> u64 {
        self.input_decode_errors
    }

    /// Number of events dropped by the host rate limiter.
    pub fn input_rate_limited(&self) -> u64 {
        self.input_rate_limited
    }

    /// Bind to a session id (does not start media). Resets identity flags.
    pub fn attach(&mut self, session_id: impl Into<String>) {
        let sid = session_id.into();
        self.session_id = Some(sid.clone());
        self.media = None;
        self.outbound_signals.clear();
        self.identity = IdentityBindState::new(sid);
        self.bind_key = None;
        self.pending_dc_challenge = None;
        self.accepted_input.clear();
        self.rejected_input_count = 0;
        self.input_decode_errors = 0;
        self.input_rate_limited = 0;
        self.policy_input_enabled = false;
        // Fresh metrics window; preserve open mode / rate cap from injector_config
        // (PreferNative stays PreferNative — not forced back to StubOnly).
        self.injector = open_injector(self.injector_config).unwrap_or_else(|_| {
            // PreferNative can fail only if NativeOnly; synthetic always opens.
            open_injector(InjectorConfig::synthetic()).expect("stub injector always opens")
        });
    }

    /// Clear session binding and stop media.
    pub fn detach(&mut self) {
        let _ = self.stop_media();
        self.session_id = None;
        self.outbound_signals.clear();
        self.identity = IdentityBindState::default();
        self.bind_key = None;
        self.pending_dc_challenge = None;
        self.accepted_input.clear();
        self.rejected_input_count = 0;
        self.input_decode_errors = 0;
        self.input_rate_limited = 0;
        self.policy_input_enabled = false;
    }

    /// Mode A (OTP): verify + consume host OTP; mark `session_authorized`.
    pub fn authorize_mode_a(
        &mut self,
        record: &mut OtpRecord,
        code: &str,
        pepper: &[u8],
        now_unix: u64,
    ) -> Result<()> {
        let key = authorize_mode_a(record, code, pepper, now_unix)?;
        self.bind_key = Some(key);
        self.identity.mark_authorized();
        Ok(())
    }

    /// Mode B (unattended): verify host-only challenge-response MAC.
    pub fn authorize_mode_b(
        &mut self,
        secret: &HostSecret,
        challenge: &AuthChallenge,
        fingerprint_host: &[u8],
        fingerprint_viewer: &[u8],
        mac: &[u8],
    ) -> Result<()> {
        let sid = self
            .session_id
            .as_deref()
            .ok_or_else(|| SessionError::InvalidState("no session attached".into()))?;
        let key = authorize_mode_b(
            secret,
            challenge,
            sid,
            fingerprint_host,
            fingerprint_viewer,
            mac,
        )?;
        self.bind_key = Some(key);
        self.identity.mark_authorized();
        Ok(())
    }

    /// Mode B without a pre-DTLS MAC: install the host-only secret as the bind key.
    pub fn authorize_unattended_secret(&mut self, secret: &HostSecret) -> Result<()> {
        if self.session_id.is_none() {
            return Err(SessionError::InvalidState("no session attached".into()));
        }
        self.bind_key = Some(SessionBindKey::from_mode_b_secret(secret));
        self.identity.mark_authorized();
        Ok(())
    }

    /// Issue a post-DTLS DataChannel identity challenge (host → viewer).
    ///
    /// Requires an attached session, Mode A/B authorization, and a Connected peer.
    pub fn start_identity_challenge(&mut self) -> Result<()> {
        if self.session_id.is_none() {
            return Err(SessionError::InvalidState("no session attached".into()));
        }
        if !self.identity.session_authorized {
            return Err(SessionError::Auth(AuthError::SessionNotAuthorized));
        }
        if self.bind_key.is_none() {
            return Err(SessionError::InvalidState(
                "no session bind key (authorize Mode A/B first)".into(),
            ));
        }
        if self.peer.connection_state() != ConnectionState::Connected {
            return Err(SessionError::InvalidState(
                "peer not connected for identity challenge".into(),
            ));
        }
        let challenge = DcIdentityChallenge::issue();
        let msg = DataMessage {
            label: IDENTITY_CHANNEL_LABEL.into(),
            data: challenge.encode(),
            unordered: false,
        };
        self.peer.send_data(msg)?;
        self.pending_dc_challenge = Some(challenge);
        Ok(())
    }

    /// Poll transport and process identity / input DataChannel messages.
    pub fn poll_inbound(&mut self) -> Result<InboundStats> {
        self.peer.poll()?;
        let messages = self.cb.take_data();
        let mut stats = InboundStats::default();
        for msg in messages {
            if msg.label == IDENTITY_CHANNEL_LABEL {
                self.handle_identity_data(&msg.data)?;
                stats.identity_messages = stats.identity_messages.saturating_add(1);
            } else if msg.label == INPUT_CHANNEL_LABEL {
                match self.process_input_message(&msg) {
                    InputProcessOutcome::Injected => {
                        stats.input_accepted = stats.input_accepted.saturating_add(1);
                    }
                    InputProcessOutcome::RateLimited => {
                        stats.input_rate_limited = stats.input_rate_limited.saturating_add(1);
                    }
                    InputProcessOutcome::RejectedGate | InputProcessOutcome::InjectError => {
                        stats.input_rejected = stats.input_rejected.saturating_add(1);
                    }
                    InputProcessOutcome::DecodeError => {
                        stats.input_decode_errors = stats.input_decode_errors.saturating_add(1);
                    }
                }
            }
            // Other labels ignored at this layer.
        }
        Ok(stats)
    }

    /// Accept a single input DataChannel message if gates are open.
    ///
    /// Steps: policy+identity gate → JSON decode → rate-limited inject.
    /// Returns `true` **only** when the event was actually injected into the
    /// backend. Rate-limit drops, decode errors, and gate rejects return
    /// `false` (use [`Self::process_input_message`] for the full outcome).
    pub fn try_accept_input(&mut self, msg: DataMessage) -> bool {
        matches!(
            self.process_input_message(&msg),
            InputProcessOutcome::Injected
        )
    }

    /// Process one input DataChannel message with full outcome classification.
    pub fn process_input_message(&mut self, msg: &DataMessage) -> InputProcessOutcome {
        if !self.injection_allowed() {
            self.rejected_input_count = self.rejected_input_count.saturating_add(1);
            return InputProcessOutcome::RejectedGate;
        }
        let text = match std::str::from_utf8(&msg.data) {
            Ok(s) => s,
            Err(_) => {
                self.input_decode_errors = self.input_decode_errors.saturating_add(1);
                return InputProcessOutcome::DecodeError;
            }
        };
        let event = match decode_input(text) {
            Ok(e) => e,
            Err(_) => {
                self.input_decode_errors = self.input_decode_errors.saturating_add(1);
                return InputProcessOutcome::DecodeError;
            }
        };
        match self.injector.try_inject(&event) {
            Ok(true) => {
                self.push_accepted_input(event);
                InputProcessOutcome::Injected
            }
            Ok(false) => {
                self.input_rate_limited = self.input_rate_limited.saturating_add(1);
                InputProcessOutcome::RateLimited
            }
            Err(e) => {
                // Treat OS inject failure as rejected for stats; keep session alive.
                self.rejected_input_count = self.rejected_input_count.saturating_add(1);
                let _ = e;
                InputProcessOutcome::InjectError
            }
        }
    }

    /// Explicit gate: return error unless identity bind allows input.
    pub fn ensure_input_allowed(&self) -> Result<()> {
        if self.identity.input_allowed() {
            Ok(())
        } else {
            Err(SessionError::Auth(self.identity.input_gate_error()))
        }
    }

    /// Explicit full inject gate (identity + policy/kill).
    pub fn ensure_injection_allowed(&self) -> Result<()> {
        if self.injection_allowed() {
            Ok(())
        } else if !self.identity.input_allowed() {
            Err(SessionError::Auth(self.identity.input_gate_error()))
        } else {
            Err(SessionError::InputRejected(
                "input disabled by policy or kill-switch".into(),
            ))
        }
    }

    fn handle_identity_data(&mut self, data: &[u8]) -> Result<()> {
        let parsed = DcIdentityMessage::parse(data)?;
        match parsed {
            DcIdentityMessage::Response(response) => {
                let challenge = self.pending_dc_challenge.take().ok_or_else(|| {
                    SessionError::Auth(AuthError::IdentityBind(
                        "dc response without pending challenge".into(),
                    ))
                })?;
                let bind_key = self.bind_key.as_ref().ok_or_else(|| {
                    SessionError::InvalidState("missing bind key for dc verify".into())
                })?;
                let fp_host = self.peer.local_fingerprint()?.as_sign_material();
                let fp_viewer = self
                    .peer
                    .remote_fingerprint()?
                    .ok_or_else(|| SessionError::InvalidState("remote fingerprint unknown".into()))?
                    .as_sign_material();
                complete_dc_bind(
                    &mut self.identity,
                    bind_key,
                    &challenge,
                    &response,
                    &fp_host,
                    &fp_viewer,
                )?;
                Ok(())
            }
            DcIdentityMessage::Challenge(_) => Err(SessionError::Auth(AuthError::IdentityBind(
                "host does not expect dc_challenge".into(),
            ))),
        }
    }

    /// Currently attached session id, if any.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether synthetic media sources are running.
    pub fn media_started(&self) -> bool {
        self.media.is_some()
    }

    /// Peer connection state.
    pub fn connection_state(&self) -> ConnectionState {
        self.peer.connection_state()
    }

    /// Mutable access to the owned transport (tests / advanced wiring).
    pub fn peer_mut(&mut self) -> &mut dyn PeerTransport {
        self.peer.as_mut()
    }

    /// Wait until the owned PeerTransport can send media/data (see
    /// [`PeerTransport::wait_ready`]).
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<()> {
        self.peer.wait_ready(timeout)?;
        Ok(())
    }

    /// Override synthetic capture geometry (default 64×36 @ 30 fps).
    pub fn set_synthetic_geometry(&mut self, width: u32, height: u32, fps: u32) {
        assert!(width > 0 && height > 0 && fps > 0);
        self.synth_width = width;
        self.synth_height = height;
        self.synth_fps = fps;
    }

    /// Synthetic geometry currently configured.
    pub fn synthetic_geometry(&self) -> (u32, u32, u32) {
        (self.synth_width, self.synth_height, self.synth_fps)
    }

    /// Start synthetic capture/encode.
    ///
    /// When the peer is still `New`, creates a local offer and queues
    /// `session_offer` + ICE [`SignalForward`] messages for the service.
    /// [`Self::pump_media`] only sends while [`ConnectionState::Connected`].
    pub fn start_media(&mut self) -> Result<()> {
        let sid = self
            .session_id
            .clone()
            .ok_or_else(|| SessionError::InvalidState("no session attached".into()))?;

        if self.media.is_some() {
            return Ok(());
        }

        let t0 = Duration::from_millis(0);
        let epoch = RtpEpoch::new(t0);
        let (video, audio) = crate::platform_capture::open_video_source(
            self.video_kind,
            self.synth_width,
            self.synth_height,
            self.synth_fps,
            t0,
        )
        .and_then(|v| {
            crate::platform_capture::open_audio_source(self.audio_kind, t0).map(|a| (v, a))
        })
        .map_err(|e| SessionError::Media(e.to_string()))?;
        let video_backend = video.backend_name();
        let audio_backend = audio.backend_name();
        let enc_cfg = remotelink_platform_windows::EncoderConfig {
            width: self.synth_width,
            height: self.synth_height,
            fps: self.synth_fps.max(1),
            target_bitrate_bps: remotelink_platform_windows::DEFAULT_TARGET_BITRATE_BPS,
            disable_hw_encode: self.force_software,
            force_software: self.force_software,
        };
        let h264 = remotelink_platform_windows::open_encoder(&enc_cfg)
            .map_err(|e| SessionError::Media(e.to_string()))?;
        use remotelink_platform_windows::H264Encoder;
        let encode_backend = h264.backend_kind().as_str();
        self.media = Some(MediaPlane {
            video,
            audio,
            opus: MockOpusEncoder::with_epoch(epoch),
            h264,
            epoch,
            video_frames_sent: 0,
            audio_packets_sent: 0,
            video_backend,
            audio_backend,
            encode_backend,
        });

        if self.peer.connection_state() == ConnectionState::New {
            self.create_local_offer_and_queue(&sid)?;
        } else {
            self.queue_pending_ice(&sid);
        }
        Ok(())
    }

    /// Stop synthetic media sources (peer stays open).
    pub fn stop_media(&mut self) -> Result<()> {
        self.media = None;
        Ok(())
    }

    /// Stop media and close the PeerTransport.
    pub fn shutdown(&mut self) -> Result<()> {
        self.stop_media()?;
        self.peer.close()?;
        Ok(())
    }

    /// Apply a control-plane SignalForward (SDP / ICE) to the PeerTransport.
    pub fn apply_signal(&mut self, kind: &str, payload: &str) -> Result<()> {
        match kind {
            signal_kind::SESSION_OFFER | signal_kind::SESSION_ANSWER => {
                let sdp = parse_sdp_payload(payload)?;
                let sdp_type = if kind == signal_kind::SESSION_OFFER {
                    remotelink_net::SdpType::Offer
                } else {
                    remotelink_net::SdpType::Answer
                };
                self.peer.set_remote_description(SessionDescription {
                    sdp_type,
                    sdp: sdp.sdp,
                })?;
                if let Some(sid) = self.session_id.clone() {
                    self.queue_pending_ice(&sid);
                }
                Ok(())
            }
            signal_kind::ICE_CANDIDATE => {
                let cand = parse_ice_payload(payload)?;
                self.peer.add_ice_candidate(cand)?;
                Ok(())
            }
            other => Err(SessionError::Signaling(format!(
                "unsupported signal kind `{other}` for peer transport"
            ))),
        }
    }

    /// Create local SDP offer, set it, queue offer + ICE for service hop.
    ///
    /// When a device signing key is installed, `fingerprint_sig` is the hex
    /// ed25519 signature over `session_id` + local DTLS fingerprint sign material.
    pub fn create_local_offer_and_queue(&mut self, session_id: &str) -> Result<()> {
        let offer = self.peer.create_offer()?;
        self.peer.set_local_description(offer.clone())?;
        // Omit fingerprint_sig when unsigned (None), never emit empty string —
        // viewers with an enrolled host key fail closed on missing sig.
        let fingerprint_sig = match &self.device_signing_key {
            Some(sk) => {
                let fp = self.peer.local_fingerprint()?.as_sign_material();
                Some(sign_session_fingerprint(sk, session_id, &fp))
            }
            None => None,
        };
        let payload = serde_json::to_string(&SdpPayload {
            sdp: offer.sdp,
            fingerprint_sig,
        })
        .map_err(|e| SessionError::Signaling(e.to_string()))?;
        self.outbound_signals.push(SignalForward {
            session_id: session_id.into(),
            kind: signal_kind::SESSION_OFFER.into(),
            payload,
            from: SignalHop::Agent,
        });
        self.queue_pending_ice(session_id);
        Ok(())
    }

    /// Generate and send up to `video_frames` synthetic video frames plus a
    /// proportional number of audio packets (3 × 10 ms audio per video frame).
    ///
    /// No media is written to control IPC.
    pub fn pump_media(&mut self, video_frames: u32) -> Result<PumpStats> {
        if self.media.is_none() {
            return Err(SessionError::InvalidState("media not started".into()));
        }
        if self.peer.connection_state() != ConnectionState::Connected {
            return Ok(PumpStats {
                video_sent: 0,
                audio_sent: 0,
                skipped_not_connected: true,
            });
        }

        let mut video_sent = 0u32;
        let mut audio_sent = 0u32;

        for _ in 0..video_frames {
            for _ in 0..3 {
                let media = self.media.as_mut().expect("media checked");
                let Some(frame) = media
                    .audio
                    .next_frame()
                    .map_err(|e| SessionError::Media(e.to_string()))?
                else {
                    // Native WASAPI returns None when a 10 ms packet is not
                    // ready yet (silence / still filling). Keep pumping video.
                    continue;
                };
                let opus = media
                    .opus
                    .encode(&frame)
                    .map_err(|e| SessionError::Media(e.to_string()))?;
                let pkt = AudioPacket {
                    pts_host_mono: opus.pts_host_mono,
                    rtp_ts: Some(opus.rtp_ts),
                    sample_rate: frame.sample_rate,
                    channels: frame.channels,
                    data: opus.data,
                };
                media.audio_packets_sent += 1;
                self.peer.send_audio(pkt)?;
                audio_sent += 1;
            }

            let media = self.media.as_mut().expect("media checked");
            // DXGI may return Ok(None) when idle — retry a few times for mock/desktop.
            let mut frame = None;
            for _ in 0..8 {
                match media
                    .video
                    .next_frame()
                    .map_err(|e| SessionError::Media(e.to_string()))?
                {
                    Some(f) => {
                        frame = Some(f);
                        break;
                    }
                    None => continue,
                }
            }
            let Some(frame) = frame else {
                // DXGI returns idle when the desktop has not updated; skip this slot.
                continue;
            };
            let force_key = media.video_frames_sent.is_multiple_of(30);
            let rtp_ts = media.epoch.video_ts(frame.pts_host_mono);
            let capture_frame = media_frame_to_capture(&frame)?;
            use remotelink_platform_windows::H264Encoder;
            let au = media
                .h264
                .encode(&capture_frame, force_key)
                .map_err(|e| SessionError::Media(e.to_string()))?;
            let nalu = VideoNalu {
                pts_host_mono: au.pts_host_mono,
                rtp_ts: Some(rtp_ts),
                keyframe: au.keyframe,
                format: match au.format {
                    remotelink_platform_windows::EncodeNaluFormat::AnnexB => NaluFormat::AnnexB,
                    remotelink_platform_windows::EncodeNaluFormat::Avcc => NaluFormat::Avcc,
                },
                data: au.data,
            };
            media.video_frames_sent += 1;
            self.peer.send_video_nalu(nalu)?;
            video_sent += 1;
        }

        let _ = self.peer.poll();
        Ok(PumpStats {
            video_sent,
            audio_sent,
            skipped_not_connected: false,
        })
    }

    /// Take queued A→S SignalForward messages (offer/answer/ICE).
    pub fn take_outbound_signals(&mut self) -> Vec<SignalForward> {
        std::mem::take(&mut self.outbound_signals)
    }

    /// (video_frames_sent, audio_packets_sent) since last media start.
    pub fn media_counters(&self) -> (u64, u64) {
        match &self.media {
            Some(m) => (m.video_frames_sent, m.audio_packets_sent),
            None => (0, 0),
        }
    }

    /// Active capture backend names `(video, audio)` when media is started.
    pub fn capture_backends(&self) -> Option<(&'static str, &'static str)> {
        self.media
            .as_ref()
            .map(|m| (m.video_backend, m.audio_backend))
    }

    /// Active H.264 encode backend label when media is started.
    pub fn encode_backend(&self) -> Option<&'static str> {
        self.media.as_ref().map(|m| m.encode_backend)
    }

    /// Request a video keyframe on the next pump (PLI / FIR).
    pub fn request_video_keyframe(&mut self) {
        if let Some(m) = self.media.as_mut() {
            use remotelink_platform_windows::H264Encoder;
            m.h264.request_keyframe();
        }
    }

    /// Adapt encode bitrate from GCC / transport-cc feedback.
    pub fn set_video_bitrate_bps(&mut self, bps: u32) {
        if let Some(m) = self.media.as_mut() {
            use remotelink_platform_windows::H264Encoder;
            m.h264.set_target_bitrate_bps(bps);
        }
    }

    fn queue_pending_ice(&mut self, session_id: &str) {
        for ice in self.cb.take_ice() {
            let payload = match serde_json::to_string(&ice.candidate) {
                Ok(p) => p,
                Err(_) => continue,
            };
            self.outbound_signals.push(SignalForward {
                session_id: session_id.into(),
                kind: signal_kind::ICE_CANDIDATE.into(),
                payload,
                from: SignalHop::Agent,
            });
        }
    }
}

/// Convert a media crate frame into a platform-windows capture frame for encode.
fn media_frame_to_capture(
    frame: &remotelink_media::VideoFrame,
) -> std::result::Result<remotelink_platform_windows::CaptureVideoFrame, SessionError> {
    use remotelink_platform_windows::{CapturePixelFormat, CaptureVideoFrame};
    let format = match frame.format {
        remotelink_media::PixelFormat::Bgra8 => CapturePixelFormat::Bgra8,
        remotelink_media::PixelFormat::Rgba8 => CapturePixelFormat::Rgba8,
        remotelink_media::PixelFormat::Rgb24 => CapturePixelFormat::Rgb24,
        other => {
            return Err(SessionError::Media(format!(
                "unsupported pixel format for H.264 encode: {other:?}"
            )));
        }
    };
    Ok(CaptureVideoFrame::packed(
        frame.pts_host_mono,
        frame.width,
        frame.height,
        format,
        frame.data.clone(),
    ))
}

/// Parse SDP SignalForward payload (JSON [`SdpPayload`] or bare SDP text).
pub fn parse_sdp_payload(payload: &str) -> Result<SdpPayload> {
    let trimmed = payload.trim();
    if trimmed.starts_with('{') {
        serde_json::from_str::<SdpPayload>(trimmed)
            .map_err(|e| SessionError::Signaling(format!("sdp json: {e}")))
    } else if !trimmed.is_empty() {
        Ok(SdpPayload {
            sdp: payload.to_string(),
            fingerprint_sig: None,
        })
    } else {
        Err(SessionError::Signaling("empty sdp payload".into()))
    }
}

/// Parse ICE SignalForward payload (IceCandidate JSON, nested, or bare string).
pub fn parse_ice_payload(payload: &str) -> Result<TransportIceCandidate> {
    let trimmed = payload.trim();
    if let Ok(c) = serde_json::from_str::<IceCandidate>(trimmed) {
        return Ok(c);
    }
    #[derive(Deserialize)]
    struct Wrap {
        candidate: IceCandidate,
    }
    if let Ok(w) = serde_json::from_str::<Wrap>(trimmed) {
        return Ok(w.candidate);
    }
    if trimmed.starts_with("candidate:") {
        return Ok(IceCandidate {
            candidate: trimmed.to_string(),
            sdp_mid: None,
            sdp_m_line_index: None,
            username_fragment: None,
        });
    }
    Err(SessionError::Signaling(format!(
        "unrecognized ice payload: {}",
        &trimmed[..trimmed.len().min(64)]
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_auth::{
        generate_device_keypair, mint_otp_record, mode_b_viewer_response, respond_dc_challenge,
        verify_session_fingerprint, HostSecret,
    };
    use remotelink_net::{IncomingTrackData, MockPeerPair, PeerTransport, SharedRecording};
    use remotelink_platform_windows::ipc::message::FORBIDDEN_MEDIA_METHODS;
    use remotelink_platform_windows::InjectorOpenMode;

    fn handshake_mgr_with_viewer(
        mgr: &mut SessionManager,
        peer_b: &mut MockPeerTransport,
    ) -> SdpPayload {
        mgr.start_media().unwrap();
        let out = mgr.take_outbound_signals();
        let offer = out
            .iter()
            .find(|s| s.kind == signal_kind::SESSION_OFFER)
            .expect("session_offer");
        let sdp = parse_sdp_payload(&offer.payload).unwrap();
        peer_b
            .set_remote_description(SessionDescription::offer(sdp.sdp.clone()))
            .unwrap();
        let answer = peer_b.create_answer().unwrap();
        peer_b.set_local_description(answer.clone()).unwrap();
        mgr.apply_signal(
            signal_kind::SESSION_ANSWER,
            &serde_json::to_string(&SdpPayload {
                sdp: answer.sdp,
                fingerprint_sig: None,
            })
            .unwrap(),
        )
        .unwrap();
        for sig in mgr.take_outbound_signals() {
            if sig.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&sig.payload).unwrap();
                peer_b.add_ice_candidate(c).unwrap();
            }
        }
        if let Some(ice) = peer_b.last_local_ice().cloned() {
            mgr.apply_signal(
                signal_kind::ICE_CANDIDATE,
                &serde_json::to_string(&ice).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(mgr.connection_state(), ConnectionState::Connected);
        sdp
    }

    #[test]
    fn from_mode_mock_builds_offerer() {
        let mut mgr = SessionManager::from_mode(TransportMode::Mock).unwrap();
        assert_eq!(mgr.connection_state(), ConnectionState::New);
        let fp = mgr.peer_mut().local_fingerprint().unwrap();
        assert_eq!(fp.algorithm, "sha-256");
    }

    #[test]
    fn start_media_offer_and_pump_to_mock_viewer() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec.clone()));

        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.attach("sess-av");
        let _ = handshake_mgr_with_viewer(&mut mgr, &mut peer_b);

        let stats = mgr.pump_media(3).unwrap();
        assert_eq!(stats.video_sent, 3);
        assert_eq!(stats.audio_sent, 9);
        assert!(!stats.skipped_not_connected);

        peer_b.poll().unwrap();
        let snap = rec.snapshot();
        let videos = snap
            .tracks
            .iter()
            .filter(|t| matches!(t, IncomingTrackData::Video(_)))
            .count();
        let audios = snap
            .tracks
            .iter()
            .filter(|t| matches!(t, IncomingTrackData::Audio(_)))
            .count();
        assert_eq!(videos, 3, "viewer should receive synthetic video");
        assert_eq!(audios, 9, "viewer should receive synthetic audio");

        for m in FORBIDDEN_MEDIA_METHODS {
            assert!(!m.is_empty());
        }
    }

    #[test]
    fn preconnected_pair_pumps_without_offer() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec.clone()));
        pair.handshake().unwrap();

        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.attach("s1");
        mgr.start_media().unwrap();
        assert_eq!(mgr.connection_state(), ConnectionState::Connected);
        let stats = mgr.pump_media(2).unwrap();
        assert_eq!(stats.video_sent, 2);
        peer_b.poll().unwrap();
        assert!(rec.snapshot().tracks.len() >= 2);
    }

    #[test]
    fn sdp_and_ice_payload_parsers() {
        let p = parse_sdp_payload(r#"{"sdp":"v=0\r\n"}"#).unwrap();
        assert_eq!(p.sdp, "v=0\r\n");
        let p2 = parse_sdp_payload("v=0\r\no=- 0 0\r\n").unwrap();
        assert!(p2.sdp.starts_with("v=0"));

        let ice = parse_ice_payload(
            r#"{"candidate":"candidate:1 1 UDP 1 127.0.0.1 9 typ host","sdp_mid":"0","sdp_m_line_index":0}"#,
        )
        .unwrap();
        assert!(ice.candidate.starts_with("candidate:"));
    }

    #[test]
    fn new_mock_attach_geometry_and_shutdown() {
        let mut mgr = SessionManager::new_mock();
        mgr.set_synthetic_geometry(32, 18, 15);
        assert_eq!(mgr.synthetic_geometry(), (32, 18, 15));
        mgr.attach("geo");
        assert_eq!(mgr.session_id(), Some("geo"));
        assert_eq!(mgr.peer_mut().connection_state(), ConnectionState::New);
        mgr.shutdown().unwrap();
        assert_eq!(mgr.connection_state(), ConnectionState::Closed);
    }

    #[test]
    fn bind_success_accepts_input() {
        let (sk, vk) = generate_device_keypair();
        let pepper = b"host-otp-pepper-xx!";
        let (otp, mut rec) = mint_otp_record(6, pepper, u64::MAX).unwrap();

        let pair = MockPeerPair::new();
        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.set_device_signing_key(sk);
        mgr.attach("sess-bind-ok");
        mgr.authorize_mode_a(&mut rec, otp.as_str(), pepper, 0)
            .unwrap();
        assert!(mgr.identity().session_authorized);
        assert!(!mgr.input_allowed());

        let sdp = handshake_mgr_with_viewer(&mut mgr, &mut peer_b);
        let sig = sdp.fingerprint_sig.expect("fingerprint_sig present");
        assert!(!sig.is_empty());
        let host_fp = mgr
            .peer_mut()
            .local_fingerprint()
            .unwrap()
            .as_sign_material();
        verify_session_fingerprint(&vk, "sess-bind-ok", &host_fp, &sig).unwrap();

        // Viewer bind key from OTP digits alone (no host pepper).
        let bind_key = remotelink_auth::SessionBindKey::from_mode_a_otp(otp.as_str()).unwrap();
        let rec = SharedRecording::new();
        peer_b.set_callbacks(Box::new(rec.clone()));
        mgr.start_identity_challenge().unwrap();
        peer_b.poll().unwrap();
        let snap = rec.snapshot();
        let chal_msg = snap
            .data
            .iter()
            .find(|d| d.label == IDENTITY_CHANNEL_LABEL)
            .expect("dc challenge");
        let chal = match DcIdentityMessage::parse(&chal_msg.data).unwrap() {
            DcIdentityMessage::Challenge(c) => c,
            _ => panic!("expected challenge"),
        };
        let fp_host = peer_b
            .remote_fingerprint()
            .unwrap()
            .unwrap()
            .as_sign_material();
        let fp_viewer = peer_b.local_fingerprint().unwrap().as_sign_material();
        let resp = respond_dc_challenge(&bind_key, "sess-bind-ok", &chal, &fp_host, &fp_viewer);
        peer_b
            .send_data(DataMessage {
                label: IDENTITY_CHANNEL_LABEL.into(),
                data: resp.encode(),
                unordered: false,
            })
            .unwrap();
        let stats = mgr.poll_inbound().unwrap();
        assert_eq!(stats.identity_messages, 1);
        assert!(mgr.input_allowed());
        // Policy must also enable inject (agent SetPolicy / enable_input).
        mgr.set_input_policy_enabled(true);
        assert!(mgr.injection_allowed());

        // Input accepted after bind + policy.
        let event_json = remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: remotelink_protocol::InputPayload::MouseMove(remotelink_protocol::MouseMove {
                x: 0.5,
                y: 0.5,
                display_id: 0,
            }),
        })
        .unwrap();
        peer_b
            .send_data(DataMessage {
                label: INPUT_CHANNEL_LABEL.into(),
                data: event_json.into_bytes(),
                unordered: true,
            })
            .unwrap();
        let stats = mgr.poll_inbound().unwrap();
        assert_eq!(stats.input_accepted, 1);
        assert_eq!(stats.input_rejected, 0);
        assert_eq!(mgr.take_accepted_input().len(), 1);
        assert_eq!(mgr.input_stub().map(|s| s.len()), Some(1));
    }

    #[test]
    fn no_input_before_bind() {
        let mut pair = MockPeerPair::new();
        pair.handshake().unwrap();
        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.attach("sess-early");
        mgr.set_input_policy_enabled(true);
        assert!(!mgr.input_allowed());
        assert!(!mgr.injection_allowed());
        mgr.ensure_input_allowed().unwrap_err();

        let event_json = remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: remotelink_protocol::InputPayload::MouseMove(remotelink_protocol::MouseMove {
                x: 0.1,
                y: 0.2,
                display_id: 0,
            }),
        })
        .unwrap();
        peer_b
            .send_data(DataMessage {
                label: INPUT_CHANNEL_LABEL.into(),
                data: event_json.into_bytes(),
                unordered: false,
            })
            .unwrap();
        let stats = mgr.poll_inbound().unwrap();
        assert_eq!(stats.input_rejected, 1);
        assert_eq!(stats.input_accepted, 0);
        assert_eq!(mgr.rejected_input_count(), 1);
        assert!(mgr.take_accepted_input().is_empty());
        assert!(mgr.input_stub().map(|s| s.is_empty()).unwrap_or(false));
    }

    #[test]
    fn bind_fail_rejects_input() {
        let pepper = b"pepper-for-bind-fail!";
        let (otp, mut rec) = mint_otp_record(6, pepper, u64::MAX).unwrap();
        let pair = MockPeerPair::new();
        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.attach("sess-fail");
        mgr.authorize_mode_a(&mut rec, otp.as_str(), pepper, 0)
            .unwrap();
        let _ = handshake_mgr_with_viewer(&mut mgr, &mut peer_b);

        let bad_key = remotelink_auth::SessionBindKey::try_new(b"wrong-bind-key!!!!").unwrap();

        // Viewer responds with wrong key material.
        let rec = SharedRecording::new();
        peer_b.set_callbacks(Box::new(rec.clone()));
        mgr.start_identity_challenge().unwrap();
        peer_b.poll().unwrap();
        let chal_msg = rec
            .snapshot()
            .data
            .into_iter()
            .find(|d| d.label == IDENTITY_CHANNEL_LABEL)
            .unwrap();
        let chal = match DcIdentityMessage::parse(&chal_msg.data).unwrap() {
            DcIdentityMessage::Challenge(c) => c,
            _ => panic!("challenge"),
        };
        let fp_host = peer_b
            .remote_fingerprint()
            .unwrap()
            .unwrap()
            .as_sign_material();
        let fp_viewer = peer_b.local_fingerprint().unwrap().as_sign_material();
        let resp = respond_dc_challenge(&bad_key, "sess-fail", &chal, &fp_host, &fp_viewer);
        peer_b
            .send_data(DataMessage {
                label: IDENTITY_CHANNEL_LABEL.into(),
                data: resp.encode(),
                unordered: false,
            })
            .unwrap();
        let err = mgr.poll_inbound().unwrap_err();
        assert!(matches!(err, SessionError::Auth(_)));
        assert!(!mgr.input_allowed());
        mgr.set_input_policy_enabled(true);

        let event_json = remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: remotelink_protocol::InputPayload::Key(remotelink_protocol::KeyEvent {
                scancode: 0x1C,
                extended: false,
                pressed: true,
                modifiers: 0,
            }),
        })
        .unwrap();
        peer_b
            .send_data(DataMessage {
                label: INPUT_CHANNEL_LABEL.into(),
                data: event_json.into_bytes(),
                unordered: false,
            })
            .unwrap();
        let stats = mgr.poll_inbound().unwrap();
        assert_eq!(stats.input_rejected, 1);
        assert!(mgr.input_stub().map(|s| s.is_empty()).unwrap_or(true));
    }

    #[test]
    fn gate_rejects_when_policy_disabled_after_bind() {
        // Minimal path: force identity flags without full DC handshake.
        let mut mgr = SessionManager::new_mock();
        mgr.attach("policy-gate");
        mgr.force_identity_bound_for_tests();
        assert!(mgr.input_allowed());
        assert!(!mgr.injection_allowed());

        let msg = DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
                client_ts_us: 1,
                seq: 1,
                payload: remotelink_protocol::InputPayload::MouseMove(
                    remotelink_protocol::MouseMove {
                        x: 0.0,
                        y: 0.0,
                        display_id: 0,
                    },
                ),
            })
            .unwrap()
            .into_bytes(),
            unordered: true,
        };
        assert!(!mgr.try_accept_input(msg));
        assert_eq!(mgr.rejected_input_count(), 1);
        assert!(mgr.input_stub().unwrap().is_empty());
    }

    #[test]
    fn after_bind_stub_records_events() {
        let mut mgr = SessionManager::new_mock();
        mgr.attach("stub-record");
        mgr.force_identity_bound_for_tests();
        mgr.set_input_policy_enabled(true);

        let key = remotelink_protocol::InputEvent {
            client_ts_us: 10,
            seq: 7,
            payload: remotelink_protocol::InputPayload::Key(remotelink_protocol::KeyEvent {
                scancode: 0x48,
                extended: true,
                pressed: true,
                modifiers: 0,
            }),
        };
        let msg = DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: remotelink_protocol::encode_input(&key)
                .unwrap()
                .into_bytes(),
            unordered: false,
        };
        assert!(mgr.try_accept_input(msg));
        let recorded = mgr.input_stub().unwrap().recorded();
        assert_eq!(recorded.len(), 1);
        match &recorded[0].payload {
            remotelink_protocol::InputPayload::Key(k) => {
                assert_eq!(k.scancode, 0x48);
                assert!(k.extended);
                assert!(k.pressed);
            }
            other => panic!("expected key, got {other:?}"),
        }
        assert_eq!(mgr.take_accepted_input().len(), 1);
    }

    #[test]
    fn rate_limit_drops_excess_events() {
        let mut mgr = SessionManager::new_mock();
        mgr.attach("rate-limit");
        mgr.force_identity_bound_for_tests();
        mgr.set_input_policy_enabled(true);

        let mut cfg = InjectorConfig::synthetic();
        cfg.max_events_per_sec = 3;
        mgr.set_injector_config(cfg).unwrap();

        let mut injected = 0u32;
        let mut limited = 0u32;
        for i in 0..10 {
            let msg = DataMessage {
                label: INPUT_CHANNEL_LABEL.into(),
                data: remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
                    client_ts_us: u64::from(i),
                    seq: i,
                    payload: remotelink_protocol::InputPayload::MouseMove(
                        remotelink_protocol::MouseMove {
                            x: 0.5,
                            y: 0.5,
                            display_id: 0,
                        },
                    ),
                })
                .unwrap()
                .into_bytes(),
                unordered: true,
            };
            // try_accept_input is honest: true only when actually injected.
            if mgr.try_accept_input(msg) {
                injected += 1;
            } else {
                limited += 1;
            }
        }
        assert_eq!(injected, 3);
        assert_eq!(limited, 7);
        assert_eq!(mgr.input_rate_limited(), 7);
        assert_eq!(mgr.input_metrics().dropped_rate_limit, 7);
        assert_eq!(mgr.input_stub().unwrap().len(), 3);
    }

    #[test]
    fn attach_preserves_injector_open_mode() {
        let mut mgr = SessionManager::new_mock();
        assert_eq!(mgr.injector_config().open_mode, InjectorOpenMode::StubOnly);

        let cfg = InjectorConfig {
            open_mode: InjectorOpenMode::PreferNative,
            max_events_per_sec: 100,
            ..InjectorConfig::default()
        };
        mgr.set_injector_config(cfg).unwrap();
        assert_eq!(
            mgr.injector_config().open_mode,
            InjectorOpenMode::PreferNative
        );
        assert_eq!(mgr.injector_config().max_events_per_sec, 100);
        let backend_before = mgr.input_backend_name();

        // attach re-opens with the same config (not forced StubOnly).
        mgr.attach("preserve-mode");
        assert_eq!(
            mgr.injector_config().open_mode,
            InjectorOpenMode::PreferNative
        );
        assert_eq!(mgr.injector_config().max_events_per_sec, 100);
        // Backend kind is preserved for PreferNative (sendinput on Windows, stub elsewhere).
        assert_eq!(mgr.input_backend_name(), backend_before);
        assert_eq!(mgr.input_metrics().accepted, 0);
    }

    #[test]
    fn accepted_input_ring_is_bounded() {
        let mut mgr = SessionManager::new_mock();
        mgr.attach("ring");
        mgr.force_identity_bound_for_tests();
        mgr.set_input_policy_enabled(true);

        let cap = SessionManager::accepted_input_cap();
        for i in 0..(cap as u32 + 20) {
            let msg = DataMessage {
                label: INPUT_CHANNEL_LABEL.into(),
                data: remotelink_protocol::encode_input(&remotelink_protocol::InputEvent {
                    client_ts_us: u64::from(i),
                    seq: i,
                    payload: remotelink_protocol::InputPayload::MouseMove(
                        remotelink_protocol::MouseMove {
                            x: 0.0,
                            y: 0.0,
                            display_id: 0,
                        },
                    ),
                })
                .unwrap()
                .into_bytes(),
                unordered: true,
            };
            assert!(mgr.try_accept_input(msg));
        }
        let taken = mgr.take_accepted_input();
        assert_eq!(taken.len(), cap);
        // Oldest dropped: first retained seq is 20.
        assert_eq!(taken[0].seq, 20);
        assert_eq!(taken[cap - 1].seq, cap as u32 + 19);
    }

    #[test]
    fn wrong_fingerprint_sig_fails_verify() {
        let (sk, vk) = generate_device_keypair();
        let pair = MockPeerPair::new();
        let MockPeerPair { peer_a, mut peer_b } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.set_device_signing_key(sk);
        mgr.attach("sess-fp");
        let sdp = handshake_mgr_with_viewer(&mut mgr, &mut peer_b);
        let sig = sdp.fingerprint_sig.unwrap();
        let host_fp = mgr
            .peer_mut()
            .local_fingerprint()
            .unwrap()
            .as_sign_material();
        // Tampered fingerprint material.
        let bad_fp = host_fp.replace('A', "B");
        assert!(verify_session_fingerprint(&vk, "sess-fp", &bad_fp, &sig).is_err());
        // Wrong session id.
        assert!(verify_session_fingerprint(&vk, "other", &host_fp, &sig).is_err());
        // Correct verifies.
        verify_session_fingerprint(&vk, "sess-fp", &host_fp, &sig).unwrap();
        let _ = peer_b;
    }

    #[test]
    fn mode_b_mac_verify_on_host() {
        let secret = HostSecret::try_new(b"unattended-host-secret!".to_vec()).unwrap();
        let challenge = AuthChallenge::issue();
        let mut pair = MockPeerPair::new();
        pair.handshake().unwrap();
        let MockPeerPair { peer_a, .. } = pair;
        let mut mgr = SessionManager::with_peer(Box::new(peer_a));
        mgr.attach("sess-mode-b");
        let mac =
            mode_b_viewer_response(&secret, "sess-mode-b", challenge.nonce.as_bytes(), b"", b"");
        mgr.authorize_mode_b(&secret, &challenge, b"", b"", &mac)
            .unwrap();
        assert!(mgr.identity().session_authorized);
        assert!(!mgr.identity().identity_bound);

        // Wrong MAC rejected.
        let mut mgr2 = SessionManager::new_mock();
        mgr2.attach("sess-mode-b");
        let bad = [0u8; 32];
        assert!(mgr2
            .authorize_mode_b(&secret, &challenge, b"", b"", &bad)
            .is_err());
    }
}

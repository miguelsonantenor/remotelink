//! Host **session agent** role (KD5).
//!
//! Owns: synthetic (later real) capture/encode, **PeerTransport**, control-IPC
//! handling. Receives control-only IPC from the service (`AttachSession`,
//! `SignalForward`, `SetPolicy`, `StartMedia` / `StopMedia`, `QueryStats`,
//! `KillSwitch`, chrome/shutdown). No media byte methods on the wire.
//!
//! `AgentSessionState::handle` returns [`ControlMessage::Ack`] only when state
//! was applied; session mismatch / not attached / kill-latched produce
//! [`ControlMessage::Error`] with stable codes.
//!
//! [`AgentSession`] layers a [`SessionManager`] (PeerTransport + synthetic A/V)
//! on top of that control state machine.

use remotelink_platform_windows::ipc::message::{
    error_codes, Ack, ControlError, ControlMessage, LocalConfirmResult, SignalForward, StatsPush,
};
use remotelink_platform_windows::{decode_control, encode_control};

use crate::session::{signal_kind, SessionManager};

/// Minimal agent-side session state for the attach / policy skeleton.
#[derive(Debug, Default, Clone)]
pub struct AgentSessionState {
    /// Currently attached session id, if any.
    pub session_id: Option<String>,
    /// Whether media plane has been requested to start.
    pub media_started: bool,
    /// Input injection allowed only after policy + identity bind (PR 13).
    ///
    /// Service may request enable via `SetPolicy`, but the agent must still
    /// refuse inject until [`SessionManager::input_allowed`].
    pub enable_input: bool,
    /// Whether Mode B unattended is requested for the current session.
    ///
    /// Cleared on session end; cannot be set `true` while
    /// [`Self::unattended_disabled`] is latched by a local kill-switch.
    pub unattended: bool,
    /// Session chrome visibility.
    pub chrome_visible: bool,
    /// Kill-switch latched (current session surface ended).
    pub killed: bool,
    /// Local kill-switch latched unattended Mode B off until
    /// [`Self::reenable_unattended_local`] (not clearable by remote `SetPolicy`).
    ///
    /// Survives detach/shutdown/re-attach; only cleared by local re-enable.
    pub unattended_disabled: bool,
}

impl AgentSessionState {
    fn ack(method: &str, session_id: Option<String>) -> ControlMessage {
        ControlMessage::Ack(Ack {
            for_method: Some(method.into()),
            session_id,
        })
    }

    fn err(code: &str, message: &str, session_id: Option<String>) -> ControlMessage {
        ControlMessage::Error(ControlError {
            code: code.into(),
            message: message.into(),
            session_id,
        })
    }

    /// Reset session-scoped fields while preserving the host-local
    /// [`Self::unattended_disabled`] latch (G9 / kill-switch).
    fn clear_session_keep_unattended_latch(&mut self) {
        let unattended_disabled = self.unattended_disabled;
        *self = Self::default();
        self.unattended_disabled = unattended_disabled;
    }

    /// Local-only: clear the unattended-disabled latch so Mode B may be
    /// re-enabled via `SetPolicy`. Remote control paths must not call this.
    pub fn reenable_unattended_local(&mut self) {
        self.unattended_disabled = false;
    }

    /// Ensure `session_id` matches the attached session.
    /// Returns `Some(Error)` when the check fails.
    fn require_session(&self, session_id: &str) -> Option<ControlMessage> {
        match &self.session_id {
            None => Some(Self::err(
                error_codes::NOT_ATTACHED,
                "no session attached",
                Some(session_id.into()),
            )),
            Some(cur) if cur == session_id => None,
            Some(_) => Some(Self::err(
                error_codes::SESSION_MISMATCH,
                "session id does not match attached session",
                Some(session_id.into()),
            )),
        }
    }

    /// Returns `Some(Error)` when kill-switch is latched.
    fn require_not_killed(&self, session_id: Option<String>) -> Option<ControlMessage> {
        if self.killed {
            Some(Self::err(
                error_codes::KILLED,
                "kill-switch is latched",
                session_id,
            ))
        } else {
            None
        }
    }

    /// Apply a control message to local skeleton state.
    ///
    /// Returns `Ack` only when the command was applied; otherwise a stable
    /// `Error` (`not_attached`, `session_mismatch`, `busy`, `killed`,
    /// `chrome_mandatory`, `unattended_disabled`, `unexpected`).
    pub fn handle(&mut self, msg: &ControlMessage) -> ControlMessage {
        match msg {
            ControlMessage::AttachSession(a) => {
                // Single-controller (G9): reject a second attach while a live
                // session is bound. After kill-switch latch, re-attach is allowed
                // and clears the kill latch for the new session.
                // `unattended_disabled` is host-local and is NOT cleared here.
                if self.session_id.is_some() && !self.killed {
                    return Self::err(
                        error_codes::BUSY,
                        "session already active on host",
                        Some(a.session_id.clone()),
                    );
                }
                self.session_id = Some(a.session_id.clone());
                self.media_started = false;
                self.enable_input = false;
                self.unattended = false;
                // Mandatory connection chrome while attached (G9); cannot be
                // remote-hidden while the session remains live.
                self.chrome_visible = true;
                self.killed = false;
                Self::ack("attach_session", Some(a.session_id.clone()))
            }
            ControlMessage::DetachSession(d) => {
                if let Some(e) = self.require_session(&d.session_id) {
                    return e;
                }
                self.clear_session_keep_unattended_latch();
                Self::ack("detach_session", Some(d.session_id.clone()))
            }
            ControlMessage::SetPolicy(p) => {
                if let Some(e) = self.require_session(&p.session_id) {
                    return e;
                }
                if let Some(e) = self.require_not_killed(Some(p.session_id.clone())) {
                    return e;
                }
                // Kill-switch may latch unattended off; remote policy cannot
                // re-enable Mode B without local `reenable_unattended_local`.
                if p.unattended && self.unattended_disabled {
                    return Self::err(
                        error_codes::UNATTENDED_DISABLED,
                        "unattended disabled by local kill-switch until re-enabled on host",
                        Some(p.session_id.clone()),
                    );
                }
                self.enable_input = p.enable_input;
                self.unattended = p.unattended;
                Self::ack("set_policy", Some(p.session_id.clone()))
            }
            ControlMessage::StartMedia(s) => {
                if let Some(e) = self.require_session(&s.session_id) {
                    return e;
                }
                if let Some(e) = self.require_not_killed(Some(s.session_id.clone())) {
                    return e;
                }
                self.media_started = true;
                Self::ack("start_media", Some(s.session_id.clone()))
            }
            ControlMessage::StopMedia(s) => {
                if let Some(e) = self.require_session(&s.session_id) {
                    return e;
                }
                // Stop is allowed after kill so resources can still drain.
                self.media_started = false;
                Self::ack("stop_media", Some(s.session_id.clone()))
            }
            ControlMessage::QueryStats(q) => {
                if let Some(ref sid) = q.session_id {
                    if let Some(e) = self.require_session(sid) {
                        return e;
                    }
                } else if self.session_id.is_none() {
                    return Self::err(error_codes::NOT_ATTACHED, "no session attached", None);
                }
                let sid = q
                    .session_id
                    .clone()
                    .or_else(|| self.session_id.clone())
                    .unwrap_or_default();
                ControlMessage::StatsPush(StatsPush {
                    session_id: sid,
                    rtt_ms: None,
                    video_bitrate_bps: None,
                    audio_bitrate_bps: None,
                    ice_path: None,
                    av_skew_ms: None,
                    fps: None,
                    loss: None,
                })
            }
            ControlMessage::ShowSessionChrome(c) => {
                if let Some(e) = self.require_session(&c.session_id) {
                    return e;
                }
                // After kill: refuse all chrome commands (no re-enable of a
                // dead session surface; no hide-as-success either).
                if self.killed {
                    return Self::err(
                        error_codes::KILLED,
                        "kill-switch is latched; chrome commands refused until re-attach",
                        Some(c.session_id.clone()),
                    );
                }
                // G9: mandatory chrome cannot be remote-disabled while live.
                // Refuse hide with a stable error so callers do not treat Ack
                // as success while chrome remains forced on.
                if !c.visible {
                    self.chrome_visible = true;
                    return Self::err(
                        error_codes::CHROME_MANDATORY,
                        "session chrome is mandatory while session is live",
                        Some(c.session_id.clone()),
                    );
                }
                self.chrome_visible = true;
                Self::ack("show_session_chrome", Some(c.session_id.clone()))
            }
            ControlMessage::ShutdownSession(s) => {
                if let Some(e) = self.require_session(&s.session_id) {
                    return e;
                }
                self.clear_session_keep_unattended_latch();
                Self::ack("shutdown_session", Some(s.session_id.clone()))
            }
            ControlMessage::KillSwitch(k) => {
                let applies = match (&k.session_id, &self.session_id) {
                    (None, _) => true,
                    (Some(target), Some(cur)) => target == cur,
                    (Some(_), None) => false,
                };
                if !applies {
                    return match (&k.session_id, &self.session_id) {
                        (Some(target), None) => Self::err(
                            error_codes::NOT_ATTACHED,
                            "no session attached for kill-switch",
                            Some(target.clone()),
                        ),
                        (Some(target), Some(_)) => Self::err(
                            error_codes::SESSION_MISMATCH,
                            "kill-switch session id does not match",
                            Some(target.clone()),
                        ),
                        (None, _) => unreachable!("global kill always applies"),
                    };
                }
                // Local kill: end session surface — latch kill, clear media,
                // disable input, hide chrome. Session id retained so mismatch
                // checks still work until detach/re-attach; `killed` blocks
                // further control until a fresh attach.
                self.killed = true;
                self.media_started = false;
                self.enable_input = false;
                self.unattended = false;
                self.chrome_visible = false;
                if k.disable_unattended {
                    self.unattended_disabled = true;
                }
                Self::ack("kill_switch", k.session_id.clone())
            }
            ControlMessage::SignalForward(s) => {
                if let Some(e) = self.require_session(&s.session_id) {
                    return e;
                }
                if let Some(e) = self.require_not_killed(Some(s.session_id.clone())) {
                    return e;
                }
                // Opaque at this layer; AgentSession hands SDP/ICE to PeerTransport.
                Self::ack("signal_forward", Some(s.session_id.clone()))
            }
            // Agent-originated or response types are not handled as S→A commands.
            ControlMessage::LocalConfirmResult(_)
            | ControlMessage::StatsPush(_)
            | ControlMessage::Ack(_)
            | ControlMessage::Error(_) => Self::err(
                error_codes::UNEXPECTED,
                "message not expected as agent command",
                self.session_id.clone(),
            ),
        }
    }
}

/// Full agent session: control state + PeerTransport session manager.
pub struct AgentSession {
    /// Control-plane flags (attach / policy / kill).
    pub state: AgentSessionState,
    /// Media plane + PeerTransport (synthetic A/V in this PR).
    pub manager: SessionManager,
}

impl AgentSession {
    /// New agent session with a standalone mock PeerTransport.
    pub fn new_mock() -> Self {
        Self {
            state: AgentSessionState::default(),
            manager: SessionManager::new_mock(),
        }
    }

    /// New agent session owning the given PeerTransport (tests / colocate).
    pub fn with_manager(manager: SessionManager) -> Self {
        Self {
            state: AgentSessionState::default(),
            manager,
        }
    }

    /// True when policy requested input **and** identity bind is complete
    /// **and** kill-switch is not latched.
    ///
    /// Host MUST NOT inject input unless this returns true (KD17 / PR 18).
    pub fn input_injection_allowed(&self) -> bool {
        self.state.enable_input && !self.state.killed && self.manager.input_allowed()
    }

    /// Sync the session manager policy/kill gate from control-plane flags.
    ///
    /// Call after handling control messages or before polling inbound input.
    pub fn sync_input_policy(&mut self) {
        self.manager
            .set_input_policy_enabled(self.state.enable_input && !self.state.killed);
    }

    /// Poll PeerTransport inbound DataChannels (identity + input inject).
    ///
    /// Updates the inject policy gate from `enable_input && !killed` before
    /// processing. Input is decoded and injected only when
    /// [`Self::input_injection_allowed`] would be true after bind.
    pub fn poll_inbound(&mut self) -> crate::session::Result<crate::session::InboundStats> {
        self.sync_input_policy();
        self.manager.poll_inbound()
    }

    /// Apply a control message: update state and drive the session manager.
    ///
    /// SDP/ICE `SignalForward` kinds are applied to PeerTransport via
    /// [`SessionManager::apply_signal`]. Other kinds only update control state.
    pub fn handle(&mut self, msg: &ControlMessage) -> ControlMessage {
        let reply = self.state.handle(msg);
        if !matches!(reply, ControlMessage::Ack(_) | ControlMessage::StatsPush(_)) {
            return reply;
        }

        match msg {
            ControlMessage::AttachSession(a) => {
                self.manager.attach(&a.session_id);
            }
            ControlMessage::DetachSession(_) | ControlMessage::ShutdownSession(_) => {
                self.manager.detach();
            }
            ControlMessage::SetPolicy(_) => {
                // enable_input applied on state; sync inject gate.
                self.sync_input_policy();
            }
            ControlMessage::StartMedia(_) => {
                if let Err(e) = self.manager.start_media() {
                    self.state.media_started = false;
                    return AgentSessionState::err(
                        error_codes::UNEXPECTED,
                        &format!("start_media failed: {e}"),
                        self.state.session_id.clone(),
                    );
                }
            }
            ControlMessage::StopMedia(_) => {
                let _ = self.manager.stop_media();
            }
            ControlMessage::KillSwitch(_) => {
                // End media plane + clear session binding on the manager so
                // input/media cannot continue after local kill-switch.
                let _ = self.manager.stop_media();
                self.manager.detach();
                self.sync_input_policy();
            }
            ControlMessage::SignalForward(s) => {
                if is_peer_signal_kind(&s.kind) {
                    if let Err(e) = self.manager.apply_signal(&s.kind, &s.payload) {
                        return AgentSessionState::err(
                            error_codes::UNEXPECTED,
                            &format!("signal_forward apply failed: {e}"),
                            Some(s.session_id.clone()),
                        );
                    }
                }
            }
            ControlMessage::QueryStats(_) => {
                // Enrich stats with media counters / connection path when available.
                if let ControlMessage::StatsPush(mut stats) = reply {
                    let (v, a) = self.manager.media_counters();
                    if v > 0 {
                        stats.video_bitrate_bps = Some(v.saturating_mul(8_000) as u32);
                    }
                    if a > 0 {
                        stats.audio_bitrate_bps = Some(a.saturating_mul(1_600) as u32);
                    }
                    stats.ice_path = Some(self.manager.connection_state().as_str().into());
                    return ControlMessage::StatsPush(stats);
                }
            }
            _ => {}
        }
        reply
    }

    /// Drain A→S SignalForward messages produced by the session manager.
    pub fn take_outbound_signals(&mut self) -> Vec<ControlMessage> {
        self.manager
            .take_outbound_signals()
            .into_iter()
            .map(ControlMessage::SignalForward)
            .collect()
    }

    /// Pump synthetic A/V into PeerTransport (no-op if media not started / not connected).
    pub fn pump_media(
        &mut self,
        video_frames: u32,
    ) -> crate::session::Result<crate::session::PumpStats> {
        self.manager.pump_media(video_frames)
    }
}

fn is_peer_signal_kind(kind: &str) -> bool {
    matches!(
        kind,
        signal_kind::SESSION_OFFER | signal_kind::SESSION_ANSWER | signal_kind::ICE_CANDIDATE
    )
}

/// Run the agent role (synthetic session demo without real display).
///
/// Uses mock PeerTransport (CI-safe default).
pub fn run() {
    run_with_transport(remotelink_net::TransportMode::Mock);
}

/// Run the agent role with an explicit transport mode (`mock` / `live` / `auto`).
///
/// - **mock** (default): in-process [`MockPeerPair`] stands in for a viewer.
/// - **live**: localhost TCP offerer+answerer pair — real sockets, still
///   single-process for the agent demo (`remotelink-net` feature `live`).
pub fn run_with_transport(mode: remotelink_net::TransportMode) {
    let resolved = remotelink_net::TransportConfig { mode }.resolved_mode();
    println!(
        "agent: session manager + {} PeerTransport (synthetic A/V)",
        resolved.as_str()
    );

    let result = match resolved {
        remotelink_net::TransportMode::Live => run_agent_only_live_synthetic("agent-live-session"),
        _ => run_agent_only_synthetic("agent-synthetic-session"),
    };

    match result {
        Ok(summary) => {
            println!("agent: {summary}");
            println!("agent: idle exit (named pipe client loop later)");
        }
        Err(e) => {
            eprintln!("agent: synthetic session failed: {e}");
        }
    }
}

/// Agent-only live TCP loopback: offerer (host) + answerer (viewer stand-in).
///
/// Demonstrates real sockets and length-prefixed media/data without full
/// WebRTC. Full control-IPC + SessionManager path remains on mock for CI.
fn run_agent_only_live_synthetic(session_id: &str) -> Result<String, String> {
    use std::time::Duration;

    use remotelink_net::{
        live_handshake, AudioPacket, DataMessage, IncomingTrackData, LivePeerConfig,
        LivePeerTransport, NaluFormat, PeerRole, PeerTransport, SharedRecording, VideoNalu,
    };

    let mut offerer = LivePeerTransport::new(PeerRole::Offerer, LivePeerConfig::default())
        .map_err(|e| e.to_string())?;
    let mut answerer = LivePeerTransport::new(PeerRole::Answerer, LivePeerConfig::default())
        .map_err(|e| e.to_string())?;
    let rec = SharedRecording::new();
    answerer.set_callbacks(Box::new(rec.clone()));

    live_handshake(&mut offerer, &mut answerer).map_err(|e| e.to_string())?;

    let fp = offerer.local_fingerprint().map_err(|e| e.to_string())?;
    offerer
        .send_video_nalu(VideoNalu {
            pts_host_mono: Duration::from_millis(0),
            rtp_ts: Some(0),
            keyframe: true,
            format: NaluFormat::AnnexB,
            data: vec![0, 0, 0, 1, 0x67],
        })
        .map_err(|e| e.to_string())?;
    offerer
        .send_audio(AudioPacket {
            pts_host_mono: Duration::from_millis(0),
            rtp_ts: Some(0),
            sample_rate: 48_000,
            channels: 2,
            data: vec![0, 1, 2, 3],
        })
        .map_err(|e| e.to_string())?;
    offerer
        .send_data(DataMessage {
            label: "control".into(),
            data: format!("session={session_id}").into_bytes(),
            unordered: false,
        })
        .map_err(|e| e.to_string())?;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        answerer.poll().map_err(|e| e.to_string())?;
        let snap = rec.snapshot();
        if snap.tracks.len() >= 2 && !snap.data.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "live demo timeout tracks={} data={}",
                snap.tracks.len(),
                snap.data.len()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let snap = rec.snapshot();
    let video_n = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, IncomingTrackData::Video(_)))
        .count();
    let audio_n = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, IncomingTrackData::Audio(_)))
        .count();

    offerer.close().map_err(|e| e.to_string())?;
    answerer.close().map_err(|e| e.to_string())?;

    Ok(format!(
        "live TCP session={session_id} fp={} video_rx={video_n} audio_rx={audio_n} data_rx={}",
        fp.as_sign_material(),
        snap.data.len()
    ))
}

/// Agent-only synthetic session: mock viewer peer in-process, no real display.
pub fn run_agent_only_synthetic(session_id: &str) -> Result<String, String> {
    use remotelink_net::{MockPeerPair, PeerTransport, SessionDescription, SharedRecording};
    use remotelink_platform_windows::ipc::message::{
        AttachSession, SetPolicy, StartMedia, FORBIDDEN_MEDIA_METHODS,
    };

    let mut pair = MockPeerPair::new();
    let rec = SharedRecording::new();
    pair.peer_b.set_callbacks(Box::new(rec.clone()));

    let MockPeerPair { peer_a, mut peer_b } = pair;

    let mut agent = AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));

    // Service-like control sequence over encode/decode (proves control-only IPC).
    let attach = ControlMessage::AttachSession(AttachSession {
        session_id: session_id.into(),
        viewer_label: Some("synthetic".into()),
        feature_flags: Default::default(),
        turn_uris: vec![],
        boot_secret: None,
    });
    let attach_frame = encode_control(&attach).map_err(|e| e.to_string())?;
    let (attach_msg, _) = decode_control(&attach_frame).map_err(|e| e.to_string())?;
    let reply = agent.handle(&attach_msg);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("attach failed: {reply:?}"));
    }

    let policy = ControlMessage::SetPolicy(SetPolicy {
        session_id: session_id.into(),
        enable_input: false,
        unattended: false,
        max_bitrate_bps: 0,
        disable_hw_encode: true,
    });
    let reply = agent.handle(&policy);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("set_policy failed: {reply:?}"));
    }

    let start = ControlMessage::StartMedia(StartMedia {
        session_id: session_id.into(),
        display_id: None,
    });
    let start_frame = encode_control(&start).map_err(|e| e.to_string())?;
    // Ensure no forbidden media method names appear on the wire.
    let start_json = String::from_utf8_lossy(&start_frame[4..]); // skip length prefix
    for forbidden in FORBIDDEN_MEDIA_METHODS {
        if start_json.contains(forbidden) {
            return Err(format!("media method `{forbidden}` leaked onto IPC"));
        }
    }
    let (start_msg, _) = decode_control(&start_frame).map_err(|e| e.to_string())?;
    let reply = agent.handle(&start_msg);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("start_media failed: {reply:?}"));
    }

    // Complete offer/answer with the in-process mock viewer.
    let outbound = agent.take_outbound_signals();
    let offer_fwd = outbound
        .iter()
        .find_map(|m| match m {
            ControlMessage::SignalForward(s) if s.kind == signal_kind::SESSION_OFFER => Some(s),
            _ => None,
        })
        .ok_or_else(|| "no session_offer from agent".to_string())?;
    let sdp = crate::session::parse_sdp_payload(&offer_fwd.payload).map_err(|e| e.to_string())?;
    peer_b
        .set_remote_description(SessionDescription::offer(sdp.sdp))
        .map_err(|e| e.to_string())?;
    let answer = peer_b.create_answer().map_err(|e| e.to_string())?;
    peer_b
        .set_local_description(answer.clone())
        .map_err(|e| e.to_string())?;

    let answer_payload = serde_json::to_string(&crate::session::SdpPayload {
        sdp: answer.sdp,
        fingerprint_sig: None,
    })
    .map_err(|e| e.to_string())?;
    let answer_fwd = ControlMessage::SignalForward(SignalForward {
        session_id: session_id.into(),
        kind: signal_kind::SESSION_ANSWER.into(),
        payload: answer_payload,
        from: remotelink_platform_windows::ipc::message::SignalHop::Service,
    });
    // Service → agent via control IPC framing.
    let answer_frame = encode_control(&answer_fwd).map_err(|e| e.to_string())?;
    let (answer_msg, _) = decode_control(&answer_frame).map_err(|e| e.to_string())?;
    let reply = agent.handle(&answer_msg);
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("session_answer failed: {reply:?}"));
    }

    // ICE both ways (also via SignalForward encoding).
    for sig in agent.take_outbound_signals() {
        if let ControlMessage::SignalForward(s) = sig {
            if s.kind == signal_kind::ICE_CANDIDATE {
                let c = crate::session::parse_ice_payload(&s.payload).map_err(|e| e.to_string())?;
                peer_b.add_ice_candidate(c).map_err(|e| e.to_string())?;
            }
        }
    }
    if let Some(ice) = peer_b.last_local_ice().cloned() {
        let ice_fwd = ControlMessage::SignalForward(SignalForward {
            session_id: session_id.into(),
            kind: signal_kind::ICE_CANDIDATE.into(),
            payload: serde_json::to_string(&ice).map_err(|e| e.to_string())?,
            from: remotelink_platform_windows::ipc::message::SignalHop::Service,
        });
        let frame = encode_control(&ice_fwd).map_err(|e| e.to_string())?;
        let (msg, _) = decode_control(&frame).map_err(|e| e.to_string())?;
        let reply = agent.handle(&msg);
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!("ice_candidate failed: {reply:?}"));
        }
    }

    let stats = agent.pump_media(5).map_err(|e| e.to_string())?;
    peer_b.poll().map_err(|e| e.to_string())?;
    let snap = rec.snapshot();
    let videos = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, remotelink_net::IncomingTrackData::Video(_)))
        .count();
    let audios = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, remotelink_net::IncomingTrackData::Audio(_)))
        .count();

    if videos == 0 || audios == 0 {
        return Err(format!(
            "mock peer received video={videos} audio={audios} (expected both > 0)"
        ));
    }

    Ok(format!(
        "synthetic ok session={session_id} video_sent={} audio_sent={} viewer_video={videos} viewer_audio={audios} ipc_control_only=true",
        stats.video_sent, stats.audio_sent
    ))
}

/// Helper: local accept result the agent would send upward.
pub fn local_confirm(session_id: &str, accepted: bool) -> ControlMessage {
    ControlMessage::LocalConfirmResult(LocalConfirmResult {
        session_id: session_id.into(),
        accepted,
        reason: if accepted {
            None
        } else {
            Some("user_denied".into())
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_net::{IncomingTrackData, MockPeerPair, PeerTransport, SharedRecording};
    use remotelink_platform_windows::ipc::message::{
        AttachSession, DetachSession, KillSwitch, KillSwitchSource, QueryStats, SetPolicy,
        ShowSessionChrome, ShutdownSession, SignalForward, SignalHop, StartMedia, StatsPush,
        StopMedia, FORBIDDEN_MEDIA_METHODS,
    };
    use remotelink_platform_windows::{decode_control, encode_control, ControlMessage};

    fn attach(state: &mut AgentSessionState, id: &str) {
        let r = state.handle(&ControlMessage::AttachSession(AttachSession {
            session_id: id.into(),
            viewer_label: None,
            feature_flags: Default::default(),
            turn_uris: vec![],
            boot_secret: None,
        }));
        assert!(
            matches!(r, ControlMessage::Ack(_)),
            "attach should ack, got {r:?}"
        );
    }

    fn assert_error(msg: ControlMessage, code: &str) {
        match msg {
            ControlMessage::Error(e) => assert_eq!(e.code, code, "error body {e:?}"),
            other => panic!("expected Error({code}), got {other:?}"),
        }
    }

    #[test]
    fn attach_start_and_kill_switch() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        assert_eq!(state.session_id.as_deref(), Some("s1"));
        assert!(state.chrome_visible, "mandatory chrome on attach (G9)");

        let r = state.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "s1".into(),
            enable_input: true,
            unattended: false,
            max_bitrate_bps: 0,
            disable_hw_encode: false,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.enable_input);

        let r = state.handle(&ControlMessage::StartMedia(StartMedia {
            session_id: "s1".into(),
            display_id: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.media_started);

        let r = state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: None,
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.killed);
        assert!(!state.media_started);
        assert!(!state.enable_input);
        assert!(!state.chrome_visible);
        assert!(state.unattended_disabled);
    }

    #[test]
    fn second_attach_rejected_while_session_active() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        assert_error(
            state.handle(&ControlMessage::AttachSession(AttachSession {
                session_id: "s2".into(),
                viewer_label: None,
                feature_flags: Default::default(),
                turn_uris: vec![],
                boot_secret: None,
            })),
            error_codes::BUSY,
        );
        assert_eq!(state.session_id.as_deref(), Some("s1"));

        // After kill, re-attach is allowed (clears latch).
        state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: Some("s1".into()),
            disable_unattended: true,
            source: KillSwitchSource::Tray,
        }));
        attach(&mut state, "s2");
        assert_eq!(state.session_id.as_deref(), Some("s2"));
        assert!(!state.killed);
        assert!(state.chrome_visible);
    }

    #[test]
    fn chrome_cannot_be_remote_hidden_while_live() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        assert!(state.chrome_visible);

        // Hide refused with stable error (not Ack) so callers do not treat as success.
        assert_error(
            state.handle(&ControlMessage::ShowSessionChrome(ShowSessionChrome {
                session_id: "s1".into(),
                visible: false,
                label: None,
            })),
            error_codes::CHROME_MANDATORY,
        );
        assert!(
            state.chrome_visible,
            "remote hide must not disable mandatory chrome while live"
        );

        // Show while live still acks.
        let r = state.handle(&ControlMessage::ShowSessionChrome(ShowSessionChrome {
            session_id: "s1".into(),
            visible: true,
            label: Some("v".into()),
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.chrome_visible);
    }

    #[test]
    fn chrome_commands_refused_after_kill() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: None,
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        assert!(state.killed);
        assert!(!state.chrome_visible);

        // Neither show nor hide may mutate chrome after kill.
        assert_error(
            state.handle(&ControlMessage::ShowSessionChrome(ShowSessionChrome {
                session_id: "s1".into(),
                visible: true,
                label: None,
            })),
            error_codes::KILLED,
        );
        assert!(!state.chrome_visible);
        assert_error(
            state.handle(&ControlMessage::ShowSessionChrome(ShowSessionChrome {
                session_id: "s1".into(),
                visible: false,
                label: None,
            })),
            error_codes::KILLED,
        );
        assert!(!state.chrome_visible);
    }

    #[test]
    fn kill_switch_latches_unattended_disabled() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        let r = state.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "s1".into(),
            enable_input: false,
            unattended: true,
            max_bitrate_bps: 0,
            disable_hw_encode: false,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.unattended);

        state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: Some("s1".into()),
            disable_unattended: true,
            source: KillSwitchSource::Tray,
        }));
        assert!(state.unattended_disabled);
        assert!(!state.unattended);

        // Re-attach does not clear the host-local latch.
        attach(&mut state, "s2");
        assert!(state.unattended_disabled);
        assert_error(
            state.handle(&ControlMessage::SetPolicy(SetPolicy {
                session_id: "s2".into(),
                enable_input: false,
                unattended: true,
                max_bitrate_bps: 0,
                disable_hw_encode: false,
            })),
            error_codes::UNATTENDED_DISABLED,
        );
        assert!(!state.unattended);

        // Other policy (unattended:false) still applies.
        let r = state.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "s2".into(),
            enable_input: true,
            unattended: false,
            max_bitrate_bps: 0,
            disable_hw_encode: false,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.enable_input);
        assert!(!state.unattended);

        // Local re-enable only.
        state.reenable_unattended_local();
        assert!(!state.unattended_disabled);
        let r = state.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "s2".into(),
            enable_input: false,
            unattended: true,
            max_bitrate_bps: 0,
            disable_hw_encode: false,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.unattended);

        // Detach preserves latch if re-latched by another kill later — also
        // verify detach itself does not invent a latch.
        state.handle(&ControlMessage::DetachSession(DetachSession {
            session_id: "s2".into(),
            reason: None,
        }));
        assert!(!state.unattended_disabled);
        assert!(state.session_id.is_none());
    }

    #[test]
    fn detach_preserves_unattended_disabled_latch() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: None,
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        assert!(state.unattended_disabled);
        // Detach after kill: session_id still matches.
        let r = state.handle(&ControlMessage::DetachSession(DetachSession {
            session_id: "s1".into(),
            reason: Some("post-kill".into()),
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(state.session_id.is_none());
        assert!(
            state.unattended_disabled,
            "unattended_disabled must survive detach"
        );
    }

    #[test]
    fn agent_kill_switch_clears_media_session() {
        let mut agent = AgentSession::new_mock();
        let r = agent.handle(&ControlMessage::AttachSession(AttachSession {
            session_id: "kill-media".into(),
            viewer_label: None,
            feature_flags: Default::default(),
            turn_uris: vec![],
            boot_secret: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        let r = agent.handle(&ControlMessage::StartMedia(StartMedia {
            session_id: "kill-media".into(),
            display_id: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(agent.state.media_started);
        assert!(agent.manager.session_id().is_some());

        let r = agent.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: Some("kill-media".into()),
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(agent.state.killed);
        assert!(!agent.state.media_started);
        assert!(!agent.state.enable_input);
        assert!(agent.manager.session_id().is_none());
    }

    #[test]
    fn session_mismatch_and_not_attached_errors() {
        let mut state = AgentSessionState::default();
        assert_error(
            state.handle(&ControlMessage::StartMedia(StartMedia {
                session_id: "s1".into(),
                display_id: None,
            })),
            error_codes::NOT_ATTACHED,
        );

        attach(&mut state, "s1");
        assert_error(
            state.handle(&ControlMessage::SetPolicy(SetPolicy {
                session_id: "other".into(),
                enable_input: true,
                unattended: false,
                max_bitrate_bps: 0,
                disable_hw_encode: false,
            })),
            error_codes::SESSION_MISMATCH,
        );
        assert_error(
            state.handle(&ControlMessage::DetachSession(DetachSession {
                session_id: "other".into(),
                reason: None,
            })),
            error_codes::SESSION_MISMATCH,
        );
    }

    #[test]
    fn killed_refuses_start_and_policy_and_signal() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");
        state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: Some("s1".into()),
            disable_unattended: true,
            source: KillSwitchSource::Tray,
        }));
        assert!(state.killed);

        assert_error(
            state.handle(&ControlMessage::StartMedia(StartMedia {
                session_id: "s1".into(),
                display_id: None,
            })),
            error_codes::KILLED,
        );
        assert_error(
            state.handle(&ControlMessage::SetPolicy(SetPolicy {
                session_id: "s1".into(),
                enable_input: true,
                unattended: false,
                max_bitrate_bps: 0,
                disable_hw_encode: false,
            })),
            error_codes::KILLED,
        );
        assert_error(
            state.handle(&ControlMessage::SignalForward(SignalForward {
                session_id: "s1".into(),
                kind: "ice".into(),
                payload: "{}".into(),
                from: SignalHop::Service,
            })),
            error_codes::KILLED,
        );

        // Stop still works after kill.
        let r = state.handle(&ControlMessage::StopMedia(StopMedia {
            session_id: "s1".into(),
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
    }

    #[test]
    fn handle_every_control_variant_happy_or_expected() {
        let mut state = AgentSessionState::default();
        attach(&mut state, "s1");

        let expect_ack = |reply: &ControlMessage, name: &str| {
            assert!(
                matches!(reply, ControlMessage::Ack(_)),
                "{name}: expected Ack, got {reply:?}"
            );
        };

        let r = state.handle(&ControlMessage::SignalForward(SignalForward {
            session_id: "s1".into(),
            kind: "session_offer".into(),
            payload: r#"{"sdp":"v=0"}"#.into(),
            from: SignalHop::Service,
        }));
        expect_ack(&r, "signal_forward");

        let r = state.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "s1".into(),
            enable_input: false,
            unattended: false,
            max_bitrate_bps: 1_000_000,
            disable_hw_encode: true,
        }));
        expect_ack(&r, "set_policy");

        let r = state.handle(&ControlMessage::StartMedia(StartMedia {
            session_id: "s1".into(),
            display_id: Some("d0".into()),
        }));
        expect_ack(&r, "start_media");

        let stats = state.handle(&ControlMessage::QueryStats(QueryStats {
            session_id: Some("s1".into()),
        }));
        assert!(
            matches!(stats, ControlMessage::StatsPush(_)),
            "query_stats: {stats:?}"
        );

        let r = state.handle(&ControlMessage::ShowSessionChrome(ShowSessionChrome {
            session_id: "s1".into(),
            visible: true,
            label: Some("v".into()),
        }));
        expect_ack(&r, "show_session_chrome");

        let r = state.handle(&ControlMessage::StopMedia(StopMedia {
            session_id: "s1".into(),
        }));
        expect_ack(&r, "stop_media");

        let r = state.handle(&ControlMessage::ShutdownSession(ShutdownSession {
            session_id: "s1".into(),
            reason: Some("done".into()),
        }));
        expect_ack(&r, "shutdown_session");

        // Detach after re-attach.
        attach(&mut state, "s1");
        let r = state.handle(&ControlMessage::DetachSession(DetachSession {
            session_id: "s1".into(),
            reason: None,
        }));
        expect_ack(&r, "detach_session");
        assert!(state.session_id.is_none());

        // Global kill with no session still acks.
        let r = state.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: None,
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        expect_ack(&r, "kill_switch");
        assert!(state.killed);

        // Agent-originated types → unexpected.
        assert_error(
            state.handle(&ControlMessage::StatsPush(StatsPush {
                session_id: "s1".into(),
                rtt_ms: None,
                video_bitrate_bps: None,
                audio_bitrate_bps: None,
                ice_path: None,
                av_skew_ms: None,
                fps: None,
                loss: None,
            })),
            error_codes::UNEXPECTED,
        );
        assert_error(
            state.handle(&ControlMessage::Ack(Ack {
                for_method: None,
                session_id: None,
            })),
            error_codes::UNEXPECTED,
        );
        assert_error(
            state.handle(&ControlMessage::Error(ControlError {
                code: "x".into(),
                message: "y".into(),
                session_id: None,
            })),
            error_codes::UNEXPECTED,
        );
        assert_error(
            state.handle(&local_confirm("s1", true)),
            error_codes::UNEXPECTED,
        );
    }

    #[test]
    fn agent_messages_are_control_only() {
        let msg = local_confirm("s1", false);
        let frame = encode_control(&msg).unwrap();
        let (back, _) = decode_control(&frame).unwrap();
        assert_eq!(back.method_name(), "local_confirm_result");
        for forbidden in FORBIDDEN_MEDIA_METHODS {
            assert_ne!(back.method_name(), *forbidden);
        }
    }

    #[test]
    fn agent_session_starts_media_mock_peer_receives_av_no_media_ipc() {
        let mut pair = MockPeerPair::new();
        let rec = SharedRecording::new();
        pair.peer_b.set_callbacks(Box::new(rec.clone()));
        pair.handshake().unwrap();

        let MockPeerPair { peer_a, mut peer_b } = pair;

        let mut agent = AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));

        let r = agent.handle(&ControlMessage::AttachSession(AttachSession {
            session_id: "s-media".into(),
            viewer_label: None,
            feature_flags: Default::default(),
            turn_uris: vec![],
            boot_secret: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));

        // Control IPC frames for StartMedia must not carry media methods.
        let start = ControlMessage::StartMedia(StartMedia {
            session_id: "s-media".into(),
            display_id: None,
        });
        let frame = encode_control(&start).unwrap();
        let json = String::from_utf8_lossy(&frame);
        for forbidden in FORBIDDEN_MEDIA_METHODS {
            assert!(
                !json.contains(forbidden),
                "IPC must not contain `{forbidden}`"
            );
        }
        let r = agent.handle(&start);
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(agent.state.media_started);
        assert!(agent.manager.media_started());

        let stats = agent.pump_media(4).unwrap();
        assert_eq!(stats.video_sent, 4);
        assert!(stats.audio_sent >= 4);

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
        assert_eq!(videos, 4);
        assert!(audios >= 4);

        // SignalForward of a non-peer kind still acks (opaque).
        let r = agent.handle(&ControlMessage::SignalForward(SignalForward {
            session_id: "s-media".into(),
            kind: "auth_challenge".into(),
            payload: "{}".into(),
            from: SignalHop::Service,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
    }

    #[test]
    fn agent_session_applies_signal_forward_sdp_ice() {
        // Full path via control messages (no pre-handshake).
        let summary = run_agent_only_synthetic("s-signal").unwrap();
        assert!(summary.contains("viewer_video="));
        assert!(summary.contains("ipc_control_only=true"));
    }

    #[test]
    fn agent_session_new_mock_attach_via_control() {
        let mut agent = AgentSession::new_mock();
        let r = agent.handle(&ControlMessage::AttachSession(AttachSession {
            session_id: "m1".into(),
            viewer_label: None,
            feature_flags: Default::default(),
            turn_uris: vec![],
            boot_secret: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert_eq!(agent.manager.session_id(), Some("m1"));
        assert_eq!(agent.state.session_id.as_deref(), Some("m1"));
    }

    #[test]
    fn agent_input_gate_rejects_before_bind_and_records_after() {
        use crate::session::INPUT_CHANNEL_LABEL;
        use remotelink_net::DataMessage;
        use remotelink_protocol::{encode_input, InputEvent, InputPayload, KeyEvent, MouseMove};

        let mut agent = AgentSession::new_mock();
        let r = agent.handle(&ControlMessage::AttachSession(AttachSession {
            session_id: "in-gate".into(),
            viewer_label: None,
            feature_flags: Default::default(),
            turn_uris: vec![],
            boot_secret: None,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));

        // Policy enables input, but identity not bound yet.
        let r = agent.handle(&ControlMessage::SetPolicy(SetPolicy {
            session_id: "in-gate".into(),
            enable_input: true,
            unattended: false,
            max_bitrate_bps: 0,
            disable_hw_encode: true,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(agent.state.enable_input);
        assert!(!agent.input_injection_allowed());
        agent.sync_input_policy();
        assert!(agent.manager.policy_input_enabled());
        assert!(!agent.manager.injection_allowed());

        let mv = encode_input(&InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: InputPayload::MouseMove(MouseMove {
                x: 0.25,
                y: 0.75,
                display_id: 0,
            }),
        })
        .unwrap();
        let msg = DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: mv.into_bytes(),
            unordered: true,
        };
        assert!(!agent.manager.try_accept_input(msg));
        assert_eq!(agent.manager.rejected_input_count(), 1);
        assert!(agent.manager.input_stub().unwrap().is_empty());

        // After identity bind, stub records injects.
        agent.manager.force_identity_bound_for_tests();
        assert!(agent.input_injection_allowed());
        agent.sync_input_policy();

        let key = encode_input(&InputEvent {
            client_ts_us: 2,
            seq: 2,
            payload: InputPayload::Key(KeyEvent {
                scancode: 0x1C,
                extended: false,
                pressed: true,
                modifiers: 0,
            }),
        })
        .unwrap();
        let msg = DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: key.into_bytes(),
            unordered: false,
        };
        assert!(agent.manager.try_accept_input(msg));
        assert_eq!(agent.manager.input_stub().unwrap().len(), 1);

        // Kill-switch must block further inject.
        let r = agent.handle(&ControlMessage::KillSwitch(KillSwitch {
            session_id: Some("in-gate".into()),
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        }));
        assert!(matches!(r, ControlMessage::Ack(_)));
        assert!(agent.state.killed);
        assert!(!agent.input_injection_allowed());
    }
}

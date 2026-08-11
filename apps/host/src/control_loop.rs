//! KD5 control IPC loop: service ↔ session agent over framed [`ControlMessage`]s.
//!
//! Transport is [`ControlEndpoint`] (TCP localhost for CI/dev; named pipe later).
//! After each command reply the agent drains A→S [`SignalForward`] messages, then
//! sends a sentinel `Ack(for_method=drain_complete)`.

use std::time::Duration;

use remotelink_net::{TransportConfig, TransportMode};
use remotelink_platform_windows::ipc::message::{Ack, ControlMessage};
use remotelink_platform_windows::{
    connect_control, listen_control, ControlEndpoint, ControlStream, TransportError,
};

use crate::agent::AgentSession;

/// Sentinel `for_method` after outbound SignalForward drain (A→S).
pub const DRAIN_COMPLETE: &str = "drain_complete";

/// Errors from the control IPC client/server.
#[derive(Debug, thiserror::Error)]
pub enum ControlLoopError {
    /// Transport failure.
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    /// Unexpected message shape.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Agent session construction failed.
    #[error("agent: {0}")]
    Agent(String),
}

/// Service-side client: request/response + drained agent signals.
pub struct ServiceAgentClient {
    stream: ControlStream,
}

impl ServiceAgentClient {
    /// Connect to an agent control endpoint.
    pub fn connect(endpoint: &ControlEndpoint) -> Result<Self, ControlLoopError> {
        let stream = connect_control(endpoint)?;
        // Avoid hanging forever if the agent dies mid-call (TCP or named pipe).
        let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        Ok(Self { stream })
    }

    /// Send one control command; return agent reply + any A→S signals drained after it.
    pub fn request(
        &mut self,
        msg: &ControlMessage,
    ) -> Result<(ControlMessage, Vec<ControlMessage>), ControlLoopError> {
        self.stream.send(msg)?;
        let reply = self.stream.recv()?;
        let mut outbound = Vec::new();
        loop {
            let next = self.stream.recv()?;
            match next {
                ControlMessage::Ack(ref a) if a.for_method.as_deref() == Some(DRAIN_COMPLETE) => {
                    break;
                }
                ControlMessage::SignalForward(_)
                | ControlMessage::StatsPush(_)
                | ControlMessage::LocalConfirmResult(_) => {
                    outbound.push(next);
                }
                other => {
                    return Err(ControlLoopError::Protocol(format!(
                        "unexpected message while draining outbound: {}",
                        other.method_name()
                    )));
                }
            }
        }
        Ok((reply, outbound))
    }

    /// Convenience: attach + policy + start_media sequence.
    pub fn start_session(
        &mut self,
        session_id: &str,
        enable_input: bool,
    ) -> Result<Vec<ControlMessage>, ControlLoopError> {
        let mut all_outbound = Vec::new();
        for msg in crate::service::build_session_start_sequence(session_id, enable_input) {
            let (reply, outbound) = self.request(&msg)?;
            if !matches!(reply, ControlMessage::Ack(_)) {
                return Err(ControlLoopError::Protocol(format!(
                    "agent rejected {}: {reply:?}",
                    msg.method_name()
                )));
            }
            all_outbound.extend(outbound);
        }
        Ok(all_outbound)
    }

    /// Graceful half-close of the control stream (TCP or named pipe).
    pub fn close(self) {
        self.stream.shutdown();
    }
}

/// Parse a control endpoint string into a [`ControlEndpoint`].
///
/// Examples:
/// - TCP: `tcp:0`, `tcp:7900`, `7900`
/// - Named pipe (Windows): `pipe`, `pipe:remotelink-host-control`,
///   `\\.\pipe\remotelink-host-control`
pub fn parse_control_endpoint(s: &str) -> Result<ControlEndpoint, ControlLoopError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ControlLoopError::Protocol(
            "empty control endpoint (expected tcp:PORT or pipe[:name])".into(),
        ));
    }

    // Named pipe forms (Windows only).
    #[cfg(windows)]
    {
        if s.eq_ignore_ascii_case("pipe") || s.eq_ignore_ascii_case("named-pipe") {
            return Ok(ControlEndpoint::default_named_pipe());
        }
        if let Some(name) = s
            .strip_prefix("pipe:")
            .or_else(|| s.strip_prefix("PIPE:"))
        {
            let name = name.trim();
            if name.is_empty() {
                return Ok(ControlEndpoint::default_named_pipe());
            }
            return Ok(ControlEndpoint::named_pipe(name));
        }
        let lower = s.to_ascii_lowercase();
        if lower.starts_with(r"\\.\pipe\") || lower.starts_with("//./pipe/") {
            return Ok(ControlEndpoint::named_pipe(s));
        }
    }
    #[cfg(not(windows))]
    {
        if s.eq_ignore_ascii_case("pipe")
            || s.eq_ignore_ascii_case("named-pipe")
            || s.to_ascii_lowercase().starts_with("pipe:")
            || s.to_ascii_lowercase().starts_with(r"\\.\pipe\")
        {
            return Err(ControlLoopError::Protocol(
                "named pipe control endpoints are only supported on Windows".into(),
            ));
        }
    }

    let port_str = s
        .strip_prefix("tcp:")
        .or_else(|| s.strip_prefix("TCP:"))
        .unwrap_or(s);
    let port: u16 = port_str.parse().map_err(|_| {
        ControlLoopError::Protocol(format!(
            "invalid control endpoint `{s}` (expected tcp:PORT or pipe[:name])"
        ))
    })?;
    Ok(ControlEndpoint::tcp_localhost(port))
}

/// Format a bound endpoint for CLI printout.
pub fn format_endpoint(endpoint: &ControlEndpoint) -> String {
    match endpoint {
        ControlEndpoint::TcpLocalhost { port } => format!("tcp:{port}"),
        #[cfg(windows)]
        ControlEndpoint::NamedPipe { path } => {
            // Prefer compact pipe:leaf form when under \\.\pipe\.
            if let Some(leaf) = path
                .strip_prefix(r"\\.\pipe\")
                .or_else(|| path.strip_prefix(r"\\.\PIPE\"))
            {
                format!("pipe:{leaf}")
            } else {
                path.clone()
            }
        }
    }
}

fn new_agent_session(transport: TransportMode) -> Result<AgentSession, ControlLoopError> {
    match transport {
        TransportMode::Mock | TransportMode::Auto => Ok(AgentSession::new_mock()),
        other => AgentSession::from_mode(other)
            .map_err(|e| ControlLoopError::Agent(e.to_string())),
    }
}

/// Run the agent control server: accept service connections and serve until process exit.
///
/// After each service disconnect, accepts the next client (service reconnect).
/// Prints `CONTROL_LISTEN=tcp:PORT` for the service `--agent-control` flag.
pub fn run_agent_control_server(
    listen: ControlEndpoint,
    transport: TransportMode,
) -> Result<(), ControlLoopError> {
    let listener = listen_control(listen)?;
    let bound = listener.endpoint().clone();
    println!(
        "agent: control listening on {} (service: --agent-control={})",
        format_endpoint(&bound),
        format_endpoint(&bound)
    );
    println!("CONTROL_LISTEN={}", format_endpoint(&bound));
    println!(
        "agent: session manager transport={}",
        TransportConfig { mode: transport }.resolved_mode().as_str()
    );

    loop {
        let mut stream = listener.accept()?;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
        println!("agent: service connected on control IPC");
        let mut agent = new_agent_session(transport)?;
        match serve_agent_connection(&mut stream, &mut agent, transport) {
            Ok(()) => println!("agent: service session ended; waiting for next connect"),
            Err(e) => eprintln!("agent: serve error: {e}; waiting for next connect"),
        }
    }
}

/// Serve control messages on an accepted stream (testable without bind).
///
/// `transport` is used to rebuild a fresh [`AgentSession`] after detach so the
/// next attach gets a new PeerTransport.
pub fn serve_agent_connection(
    stream: &mut ControlStream,
    agent: &mut AgentSession,
    transport: TransportMode,
) -> Result<(), ControlLoopError> {
    loop {
        let msg = match stream.recv() {
            Ok(m) => m,
            Err(TransportError::Io(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof
                    || e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::ConnectionAborted =>
            {
                println!("agent: service disconnected");
                break;
            }
            // read_frame maps UnexpectedEof on the 4-byte header to HeaderTruncated.
            Err(TransportError::Codec(ref e))
                if e.to_string().contains("truncated")
                    || e.to_string().contains("eof")
                    || e.to_string().contains("incomplete") =>
            {
                println!("agent: service disconnected ({e})");
                break;
            }
            Err(e) => return Err(e.into()),
        };

        // Media must never appear as method names on this pipe.
        let name = msg.method_name();
        for forbidden in remotelink_platform_windows::ipc::message::FORBIDDEN_MEDIA_METHODS {
            if name == *forbidden {
                stream.send(&ControlMessage::Error(
                    remotelink_platform_windows::ipc::message::ControlError {
                        code: remotelink_platform_windows::ipc::message::error_codes::UNEXPECTED
                            .into(),
                        message: format!("media method `{forbidden}` forbidden on control IPC"),
                        session_id: None,
                    },
                ))?;
                stream.send(&ControlMessage::Ack(Ack {
                    for_method: Some(DRAIN_COMPLETE.into()),
                    session_id: None,
                }))?;
                continue;
            }
        }

        let was_detach = matches!(
            msg,
            ControlMessage::DetachSession(_) | ControlMessage::ShutdownSession(_)
        );

        let reply = agent.handle(&msg);
        // Reply first so the service is never blocked behind media send.
        stream.send(&reply)?;

        // Live TCP / webrtc need poll() to finish the handshake (remote Hello)
        // and to surface further local ICE. Always poll when a session is live.
        if agent.state.session_id.is_some() && !agent.state.killed {
            let _ = agent.manager.peer_mut().poll();
        }
        for outbound in agent.take_outbound_signals() {
            stream.send(&outbound)?;
        }
        stream.send(&ControlMessage::Ack(Ack {
            for_method: Some(DRAIN_COMPLETE.into()),
            session_id: None,
        }))?;

        // After the request fully completes, pump synthetic media when Connected.
        // No PumpMedia control method on the wire (media stays off control IPC).
        // Trigger after answer/ICE or QueryStats (service uses stats to poke poll).
        let may_pump = match &msg {
            ControlMessage::SignalForward(s) => {
                use crate::session::signal_kind;
                matches!(
                    s.kind.as_str(),
                    signal_kind::SESSION_ANSWER | signal_kind::ICE_CANDIDATE
                )
            }
            ControlMessage::QueryStats(_) => true,
            _ => false,
        };
        if may_pump
            && agent.state.media_started
            && agent.manager.connection_state() == remotelink_net::ConnectionState::Connected
        {
            // Prefer a short wait_ready so live Hello can land, then pump.
            let _ = agent.manager.wait_ready(Duration::from_millis(200));
            let _ = agent.pump_media(3);
        }

        // Fresh PeerTransport for the next session (live/webrtc sockets not reusable).
        if was_detach && matches!(reply, ControlMessage::Ack(_)) {
            *agent = new_agent_session(transport)?;
            println!(
                "agent: rebuilt session manager for next attach (transport={})",
                TransportConfig { mode: transport }.resolved_mode().as_str()
            );
        }
    }
    Ok(())
}

/// In-process demo: agent thread + service client over TCP control IPC (mock media).
pub fn run_ipc_colocate_demo(session_id: &str) -> Result<String, String> {
    use crate::session::{
        parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager,
    };
    use remotelink_net::{MockPeerPair, PeerTransport, SessionDescription, SharedRecording};
    use remotelink_platform_windows::ipc::message::SignalHop;

    let listener = listen_control(ControlEndpoint::tcp_localhost(0)).map_err(|e| e.to_string())?;
    let port = listener.tcp_port().unwrap();
    let endpoint = ControlEndpoint::tcp_localhost(port);

    // Agent owns one side of a mock pair; viewer is local for handshake.
    let mut pair = MockPeerPair::new();
    let rec = SharedRecording::new();
    pair.peer_b.set_callbacks(Box::new(rec.clone()));
    let MockPeerPair { peer_a, mut peer_b } = pair;

    let agent_thread = std::thread::spawn(move || {
        let mut stream = listener.accept().map_err(|e| e.to_string())?;
        let mut agent =
            AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));
        serve_agent_connection(&mut stream, &mut agent, TransportMode::Mock)
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    });

    let mut client = ServiceAgentClient::connect(&endpoint).map_err(|e| e.to_string())?;
    let outbound = client
        .start_session(session_id, false)
        .map_err(|e| format!("start_session: {e}"))?;

    let mut offer_sdp = None;
    let mut host_ice = Vec::new();
    for msg in &outbound {
        if let ControlMessage::SignalForward(s) = msg {
            match s.kind.as_str() {
                k if k == signal_kind::SESSION_OFFER => {
                    offer_sdp =
                        Some(parse_sdp_payload(&s.payload).map_err(|e| format!("parse offer: {e}"))?);
                }
                k if k == signal_kind::ICE_CANDIDATE => {
                    host_ice.push(
                        parse_ice_payload(&s.payload).map_err(|e| format!("parse ice: {e}"))?,
                    );
                }
                _ => {}
            }
        }
    }
    let offer = offer_sdp.ok_or_else(|| "no session_offer over IPC".to_string())?;
    peer_b
        .set_remote_description(SessionDescription::offer(offer.sdp))
        .map_err(|e| format!("viewer set offer: {e}"))?;
    let answer = peer_b
        .create_answer()
        .map_err(|e| format!("viewer create_answer: {e}"))?;
    peer_b
        .set_local_description(answer.clone())
        .map_err(|e| format!("viewer set local answer: {e}"))?;

    let answer_payload = serde_json::to_string(&SdpPayload {
        sdp: answer.sdp,
        fingerprint_sig: None,
    })
    .map_err(|e| format!("encode answer: {e}"))?;
    let answer_msg =
        crate::service::signal_to_agent(session_id, signal_kind::SESSION_ANSWER, &answer_payload);
    let (reply, _) = client
        .request(&answer_msg)
        .map_err(|e| format!("forward answer: {e}"))?;
    if !matches!(reply, ControlMessage::Ack(_)) {
        return Err(format!("answer rejected: {reply:?}"));
    }

    for ice in host_ice {
        peer_b
            .add_ice_candidate(ice)
            .map_err(|e| format!("viewer add host ice: {e}"))?;
    }
    if let Some(ice) = peer_b.last_local_ice().cloned() {
        let ice_payload =
            serde_json::to_string(&ice).map_err(|e| format!("encode viewer ice: {e}"))?;
        let ice_msg =
            crate::service::signal_to_agent(session_id, signal_kind::ICE_CANDIDATE, &ice_payload);
        let (reply, _) = client
            .request(&ice_msg)
            .map_err(|e| format!("forward ice: {e}"))?;
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!("ice rejected: {reply:?}"));
        }
    }

    // Answer SignalForward triggers agent-side pump_media (serve_agent_connection).
    client.close();
    let _ = agent_thread
        .join()
        .map_err(|_| "agent thread panic".to_string())??;

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
    if videos == 0 {
        return Err(format!(
            "ipc-colocate: no video over mock peer (audio={audios})"
        ));
    }
    let _ = SignalHop::Service;
    Ok(format!(
        "ipc-colocate ok session={session_id} control=tcp:{port} \
         offer_ok=true viewer_video_rx={videos} viewer_audio_rx={audios}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use remotelink_net::MockPeerPair;
    use remotelink_platform_windows::ipc::message::{
        AttachSession, ControlMessage, FeatureFlags, StartMedia,
    };

    #[test]
    fn parse_tcp_endpoint() {
        assert_eq!(
            parse_control_endpoint("tcp:0").unwrap(),
            ControlEndpoint::tcp_localhost(0)
        );
        assert_eq!(
            parse_control_endpoint("7901").unwrap(),
            ControlEndpoint::tcp_localhost(7901)
        );
    }

    #[cfg(windows)]
    #[test]
    fn parse_pipe_endpoint() {
        assert_eq!(
            parse_control_endpoint("pipe").unwrap(),
            ControlEndpoint::default_named_pipe()
        );
        assert_eq!(
            parse_control_endpoint("pipe:my-control").unwrap(),
            ControlEndpoint::named_pipe("my-control")
        );
        assert_eq!(
            parse_control_endpoint(r"\\.\pipe\remotelink-host-control").unwrap(),
            ControlEndpoint::default_named_pipe()
        );
        assert_eq!(
            format_endpoint(&ControlEndpoint::default_named_pipe()),
            "pipe:remotelink-host-control"
        );
    }

    #[test]
    fn ipc_attach_start_emits_offer_over_pipe() {
        let listener = listen_control(ControlEndpoint::tcp_localhost(0)).unwrap();
        let port = listener.tcp_port().unwrap();
        let endpoint = ControlEndpoint::tcp_localhost(port);

        let pair = MockPeerPair::new();
        let MockPeerPair { peer_a, peer_b: _ } = pair;

        let agent_thread = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut agent =
                AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));
            // Serve until client disconnects.
            let _ = serve_agent_connection(&mut stream, &mut agent, TransportMode::Mock);
        });

        let mut client = ServiceAgentClient::connect(&endpoint).unwrap();
        let (reply, _) = client
            .request(&ControlMessage::AttachSession(AttachSession {
                session_id: "ipc-1".into(),
                viewer_label: Some("test".into()),
                feature_flags: FeatureFlags::default(),
                turn_uris: vec![],
                boot_secret: None,
            }))
            .unwrap();
        assert!(matches!(reply, ControlMessage::Ack(_)));

        let (reply, outbound) = client
            .request(&ControlMessage::StartMedia(StartMedia {
                session_id: "ipc-1".into(),
                display_id: None,
            }))
            .unwrap();
        assert!(matches!(reply, ControlMessage::Ack(_)));
        let has_offer = outbound.iter().any(|m| {
            matches!(
                m,
                ControlMessage::SignalForward(s) if s.kind == "session_offer"
            )
        });
        assert!(has_offer, "expected session_offer in drain, got {outbound:?}");

        client.close();
        let _ = agent_thread.join();
    }

    #[test]
    fn ipc_start_session_sequence_works() {
        let listener = listen_control(ControlEndpoint::tcp_localhost(0)).unwrap();
        let port = listener.tcp_port().unwrap();
        let endpoint = ControlEndpoint::tcp_localhost(port);
        let pair = MockPeerPair::new();
        let MockPeerPair { peer_a, peer_b: _ } = pair;
        let agent_thread = std::thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut agent =
                AgentSession::with_manager(SessionManager::with_peer(Box::new(peer_a)));
            let _ = serve_agent_connection(&mut stream, &mut agent, TransportMode::Mock);
        });
        let mut client = ServiceAgentClient::connect(&endpoint).unwrap();
        let outbound = client.start_session("ipc-seq", false).expect("start_session");
        assert!(
            outbound.iter().any(|m| {
                matches!(m, ControlMessage::SignalForward(s) if s.kind == "session_offer")
            }),
            "outbound={outbound:?}"
        );
        client.close();
        let _ = agent_thread.join();
    }

    #[test]
    fn ipc_colocate_demo_delivers_media() {
        let summary = run_ipc_colocate_demo("ipc-demo-test").expect("ipc colocate");
        assert!(
            summary.contains("viewer_video_rx="),
            "summary={summary}"
        );
        assert!(
            !summary.contains("viewer_video_rx=0"),
            "expected video frames: {summary}"
        );
    }
}

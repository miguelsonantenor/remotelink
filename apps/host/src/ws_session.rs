//! Host WSS session agent / long-lived service.
//!
//! # One-shot lab (default)
//!
//! ```text
//! remotelink-host --role=ws --server=http://127.0.0.1:8080 --transport=live
//! ```
//!
//! # Persistent service (multi-session + reconnect)
//!
//! ```text
//! remotelink-host --role=service --server=http://127.0.0.1:8080 --transport=live --sessions=0
//! ```
//!
//! Register once, keep a signaling WebSocket up, accept sessions back-to-back,
//! and reconnect with backoff if the socket drops. `sessions=0` means unlimited.

use std::path::PathBuf;
use std::time::Duration;

use remotelink_auth::generate_device_keypair;
use remotelink_net::{
    create_peer_transport_with_config, PeerRole, TransportConfig, TransportMode,
};
use remotelink_platform_windows::ControlEndpoint;
use remotelink_platform_windows::ipc::message::{ControlMessage, DetachSession};
use remotelink_protocol::SignalMessage;
use remotelink_signaling::{
    http_to_ws_url, post_otp_hash, refresh_device_token, register_device, HostCredentialFile,
    SignalingClient, DEFAULT_CREDS_PATH,
};

use crate::control_loop::ServiceAgentClient;
use crate::platform_capture::{AudioCaptureKind, VideoCaptureKind};
use crate::policy::{HostAuthService, HostLocalConfig, DEFAULT_OTP_TTL_SECS};
use crate::service::signal_to_agent;
use crate::session::{parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager};
use crate::tray::{default_status_path, HostTray};
use remotelink_platform_windows::InjectorConfig;

/// Product-shell lab signaling URL (8080 is often taken on Windows).
pub const DEFAULT_LAB_SERVER: &str = "http://127.0.0.1:18080";

/// Configuration for [`run_ws_host`] / [`run_ws_host_service`].
#[derive(Debug, Clone)]
pub struct WsHostConfig {
    /// HTTP(S) base of the signaling server (`http://127.0.0.1:8080`).
    pub server: String,
    /// Optional display name for enrollment.
    pub display_name: String,
    /// Transport mode (mock is coerced to live).
    pub transport: TransportMode,
    /// Synthetic video frames to pump after each connect.
    ///
    /// **`0` = live session**: keep capturing until the viewer hangs up, the
    /// tray ends the session, or the peer drops. Lab/e2e use a small positive
    /// count (burst then `host_media_complete`).
    pub video_frames: u32,
    /// How long to wait for each `session_incoming`.
    pub wait_incoming: Duration,
    /// When set, skip HTTP register and use these credentials (tests / restarts).
    pub existing: Option<ExistingHostCreds>,
    /// Max sessions to serve before exit. **`0` = unlimited** (service mode).
    pub max_sessions: u32,
    /// Reconnect the signaling WebSocket after disconnect (service mode).
    pub reconnect: bool,
    /// Base backoff between reconnect attempts.
    pub reconnect_backoff: Duration,
    /// Path to persist credentials (default [`.remotelink-host.json`](DEFAULT_CREDS_PATH)).
    pub creds_path: PathBuf,
    /// When true, load credentials from `creds_path` if present (skip re-register).
    pub load_creds: bool,
    /// When true, write credentials after register/refresh.
    pub save_creds: bool,
    /// Mint a Mode A OTP, post hash to server, print plaintext for the viewer.
    pub mint_otp: bool,
    /// When set, drive media via KD5 agent control IPC instead of in-process
    /// [`SessionManager`] (service owns WSS only).
    pub agent_control: Option<ControlEndpoint>,
    /// Enable host tray surface (console panel + status JSON file).
    pub tray: bool,
    /// Enable Windows notification-area icon (ignored on non-Windows).
    pub os_tray: bool,
    /// Path for `.remotelink-host-status.json` (default next to `creds_path`).
    pub status_path: Option<PathBuf>,
    /// KD5 control IPC boot secret (must match agent `--boot-secret`).
    pub boot_secret: Option<String>,
}

/// Pre-enrolled host credentials (from a prior [`register_device`] call).
#[derive(Debug, Clone)]
pub struct ExistingHostCreds {
    /// Host public id.
    pub public_id: String,
    /// Access token for WSS hello.
    pub access_token: String,
    /// Refresh token (optional; enables rotation when access expires).
    pub refresh_token: Option<String>,
}

impl Default for WsHostConfig {
    fn default() -> Self {
        Self {
            server: DEFAULT_LAB_SERVER.into(),
            display_name: "remotelink-host".into(),
            transport: TransportMode::Live,
            video_frames: 5,
            wait_incoming: Duration::from_secs(120),
            existing: None,
            max_sessions: 1,
            reconnect: false,
            reconnect_backoff: Duration::from_secs(2),
            creds_path: PathBuf::from(DEFAULT_CREDS_PATH),
            load_creds: true,
            save_creds: true,
            mint_otp: true,
            agent_control: None,
            tray: true,
            os_tray: cfg!(windows),
            status_path: None,
            boot_secret: None,
        }
    }
}

fn coerce_transport(mode: TransportMode) -> TransportMode {
    if mode == TransportMode::Mock || mode == TransportMode::Auto {
        eprintln!(
            "ws-host: transport `{}` is not multi-process safe; using live TCP",
            mode.as_str()
        );
        TransportMode::Live
    } else {
        mode
    }
}

/// Enrolled identity returned to the service loop.
struct EnrolledHost {
    public_id: String,
    access_token: String,
    /// Kept for future mid-session refresh / re-save after rotate.
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

async fn enroll_or_reuse(cfg: &WsHostConfig) -> Result<EnrolledHost, String> {
    if let Some(ex) = &cfg.existing {
        println!(
            "ws-host: using existing public_id={} (viewer: --host {})",
            ex.public_id, ex.public_id
        );
        return Ok(EnrolledHost {
            public_id: ex.public_id.clone(),
            access_token: ex.access_token.clone(),
            refresh_token: ex.refresh_token.clone(),
        });
    }

    // Load from disk when enabled.
    if cfg.load_creds && cfg.creds_path.exists() {
        match HostCredentialFile::load(&cfg.creds_path) {
            Ok(mut file) => {
                // Prefer file server if caller left default; else keep CLI server.
                let server = if !is_builtin_lab_server(&cfg.server) {
                    cfg.server.clone()
                } else if !file.server.is_empty() {
                    file.server.clone()
                } else {
                    cfg.server.clone()
                };
                // Best-effort refresh so restarts survive access expiry.
                let mut refresh_ok = file.refresh_token.is_empty();
                if !file.refresh_token.is_empty() {
                    match refresh_device_token(&server, &file.public_id, &file.refresh_token).await
                    {
                        Ok(tokens) => {
                            file.access_token = tokens.access_token;
                            file.refresh_token = tokens.refresh_token;
                            file.expires_at = Some(tokens.expires_at);
                            file.server = server.clone();
                            if cfg.save_creds {
                                let _ = file.save(&cfg.creds_path);
                            }
                            println!(
                                "ws-host: refreshed tokens for public_id={} (from {})",
                                file.public_id,
                                cfg.creds_path.display()
                            );
                            refresh_ok = true;
                        }
                        Err(e) => {
                            eprintln!(
                                "ws-host: token refresh failed ({e}); registering a new device \
                                 (in-memory servers forget devices on restart)"
                            );
                        }
                    }
                }
                if refresh_ok {
                    println!(
                        "ws-host: loaded public_id={} from {} (viewer: --host {})",
                        file.public_id,
                        cfg.creds_path.display(),
                        file.public_id
                    );
                    return Ok(EnrolledHost {
                        public_id: file.public_id,
                        access_token: file.access_token,
                        refresh_token: Some(file.refresh_token),
                    });
                }
            }
            Err(e) => {
                eprintln!(
                    "ws-host: could not load {}: {e}; registering new device",
                    cfg.creds_path.display()
                );
            }
        }
    }

    let (_sk, vk) = generate_device_keypair();
    let pk = vk.to_bytes();
    let reg = register_device(&cfg.server, &pk, Some(&cfg.display_name))
        .await
        .map_err(|e| format!("register: {e}"))?;
    println!(
        "ws-host: registered public_id={} (viewer: --host {} --ws-connect)",
        reg.public_id, reg.public_id
    );
    if cfg.save_creds {
        let file = HostCredentialFile::from_registration(&cfg.server, &reg);
        file.save(&cfg.creds_path)
            .map_err(|e| format!("save creds: {e}"))?;
        println!("ws-host: saved credentials to {}", cfg.creds_path.display());
    }
    Ok(EnrolledHost {
        public_id: reg.public_id,
        access_token: reg.access_token,
        refresh_token: Some(reg.refresh_token),
    })
}

/// Result of minting a Mode A OTP for the tray / CLI.
struct MintedOtp {
    code: String,
    expires_at: String,
}

fn is_builtin_lab_server(server: &str) -> bool {
    matches!(
        server,
        "http://127.0.0.1:8080"
            | "http://localhost:8080"
            | "http://127.0.0.1:18080"
            | "http://localhost:18080"
    )
}

/// Host-local OTP window used for Mode A identity bind on live sessions.
struct LiveOtpHub {
    policy: HostAuthService,
}

impl LiveOtpHub {
    fn new() -> Self {
        Self {
            policy: HostAuthService::new(
                HostLocalConfig::default(),
                crate::policy::DEFAULT_HOST_OTP_PEPPER.to_vec(),
            ),
        }
    }

    fn mint_local(&mut self) -> Result<(String, remotelink_auth::OtpHash), String> {
        let code = self
            .policy
            .mint_otp()
            .map_err(|e| format!("mint_otp: {e}"))?;
        let hash = self
            .policy
            .active_otp()
            .ok_or_else(|| "OTP window missing after mint".to_string())?
            .hash()
            .clone();
        Ok((code.to_ui_string(), hash))
    }

    fn authorize_live(&mut self, mgr: &mut SessionManager) -> Result<(), String> {
        let code = self
            .policy
            .last_otp_code()
            .ok_or_else(|| "no OTP minted".to_string())?
            .to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.policy
            .authorize_session_mode_a(mgr, &code, now)
            .map_err(|e| format!("authorize mode a: {e}"))?;
        mgr.set_input_policy_enabled(true);
        mgr.start_identity_challenge()
            .map_err(|e| format!("identity challenge: {e}"))?;
        Ok(())
    }
}

/// Mint Mode A OTP, post hash to server, print code for the viewer CLI.
async fn maybe_mint_otp(
    cfg: &WsHostConfig,
    enrolled: &EnrolledHost,
    hub: &mut LiveOtpHub,
) -> Result<Option<MintedOtp>, String> {
    if !cfg.mint_otp {
        return Ok(None);
    }
    let (code, hash) = hub.mint_local()?;
    let digest_hex = hex::encode(hash.digest);
    let salt_hex = hex::encode(hash.salt);
    let resp = post_otp_hash(
        &cfg.server,
        &enrolled.public_id,
        &enrolled.access_token,
        &digest_hex,
        &salt_hex,
        hash.keyed,
        Some(DEFAULT_OTP_TTL_SECS),
    )
    .await
    .map_err(|e| format!("post otp hash: {e}"))?;
    let expires_at = resp.expires_at.to_string();
    println!(
        "ws-host: Mode A OTP for viewer (expires {expires_at}): {code}"
    );
    println!(
        "ws-host: viewer example: remotelink-viewer --ws-connect --server={} --host {} --otp {code} --transport=live",
        cfg.server, enrolled.public_id
    );
    Ok(Some(MintedOtp { code, expires_at }))
}

/// Wait for intent, accept on WSS, return session id.
async fn accept_incoming_session(
    sig: &mut SignalingClient,
    wait_incoming: Duration,
) -> Result<String, String> {
    println!(
        "ws-host: waiting for session_incoming (timeout {}s)…",
        wait_incoming.as_secs()
    );
    let incoming = sig
        .recv_until(wait_incoming, |m| {
            matches!(m, SignalMessage::SessionIncoming { .. })
        })
        .await
        .map_err(|e| format!("wait incoming: {e}"))?;

    let (session_id, incoming_seq) = match &incoming {
        SignalMessage::SessionIncoming {
            session_id,
            signal_seq,
            ..
        } => (session_id.clone(), *signal_seq),
        _ => unreachable!(),
    };
    println!("ws-host: session_incoming session_id={session_id}");

    sig.next_seq = incoming_seq.saturating_add(1);
    let accept_seq = sig.take_seq();
    sig.send(&SignalMessage::SessionAccept {
        session_id: session_id.clone(),
        signal_seq: accept_seq,
    })
    .await
    .map_err(|e| format!("accept: {e}"))?;
    println!("ws-host: session_accept seq={accept_seq}");
    Ok(session_id)
}

/// Serve one session using an **in-process** SessionManager (legacy / single binary).
async fn handle_one_session_local(
    sig: &mut SignalingClient,
    public_id: &str,
    mode: TransportMode,
    transport_cfg: &TransportConfig,
    video_frames: u32,
    wait_incoming: Duration,
    tray: Option<&HostTray>,
    otp_hub: &mut LiveOtpHub,
) -> Result<String, String> {
    let session_id = accept_incoming_session(sig, wait_incoming).await?;
    if let Some(t) = tray {
        t.begin_session(&session_id, None);
    }

    let offerer = create_peer_transport_with_config(PeerRole::Offerer, transport_cfg)
        .map_err(|e| format!("create offerer: {e}"))?;
    let mut mgr = SessionManager::with_peer(offerer);
    configure_session_media(&mut mgr, video_frames == 0);
    mgr.attach(&session_id);
    if let Err(e) = mgr.start_media() {
        if video_frames == 0 {
            eprintln!("ws-host: live capture open failed ({e}); falling back to mock desktop");
            mgr.set_video_kind(VideoCaptureKind::WindowsMock);
            mgr.set_synthetic_geometry(1280, 720, 20);
            mgr.start_media()
                .map_err(|e| format!("start_media (mock fallback): {e}"))?;
        } else {
            return Err(format!("start_media: {e}"));
        }
    }
    if video_frames == 0 {
        let (v, a) = mgr.capture_backends().unwrap_or(("?", "?"));
        let enc = mgr.encode_backend().unwrap_or("?");
        println!("ws-host: live capture video={v} audio={a} encode={enc}");
    }

    let outbound = mgr.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .ok_or_else(|| "no session_offer from SessionManager".to_string())?;
    let offer = parse_sdp_payload(&offer_sig.payload).map_err(|e| e.to_string())?;
    let mut pending_host_ice: Vec<_> = outbound
        .iter()
        .filter(|s| s.kind == signal_kind::ICE_CANDIDATE)
        .cloned()
        .collect();

    let relay =
        relay_offer_answer_ice_local(sig, &session_id, &offer, &mut pending_host_ice, &mut mgr, tray)
            .await;
    if let Err(e) = relay {
        if let Some(t) = tray {
            if e.contains("tray") {
                t.apply_kill();
            } else {
                t.end_session();
            }
        }
        return Err(e);
    }

    if let Err(e) = mgr.wait_ready(Duration::from_secs(10)) {
        if let Some(t) = tray {
            t.end_session();
        }
        return Err(format!("wait_ready: {e}"));
    }
    if let Some(t) = tray {
        t.mark_session_active();
    }
    if video_frames == 0 {
        match otp_hub.authorize_live(&mut mgr) {
            Ok(()) => {
                // Give the viewer a moment to answer the DC challenge.
                for _ in 0..40 {
                    let _ = mgr.poll_inbound();
                    if mgr.identity().identity_bound {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                if mgr.identity().identity_bound {
                    println!("ws-host: identity bound (Mode A OTP)");
                } else {
                    eprintln!(
                        "ws-host: identity challenge sent; waiting for viewer bind during media"
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "ws-host: Mode A bind failed ({e}); opening input after signaling OTP accept"
                );
                mgr.force_identity_bound_for_tests();
                mgr.set_input_policy_enabled(true);
            }
        }
    }
    let (pump, end_reason) =
        drive_local_media(sig, &mut mgr, video_frames, tray).await?;
    if pump.skipped_not_connected || (video_frames > 0 && pump.video_sent == 0) {
        let _ = mgr.shutdown();
        if let Some(t) = tray {
            t.end_session();
        }
        return Err(format!(
            "pump failed (skipped={} video_sent={})",
            pump.skipped_not_connected, pump.video_sent
        ));
    }

    let fp = mgr
        .peer_mut()
        .local_fingerprint()
        .map_err(|e| e.to_string())?;
    let (v_tx, a_tx) = mgr.media_counters();
    let summary = format!(
        "ws-host ok public_id={public_id} session={session_id} media=local transport={} video_tx={} audio_tx={} live={} reason={end_reason} fp={}",
        mode.as_str(),
        v_tx.max(pump.video_sent as u64),
        a_tx.max(pump.audio_sent as u64),
        video_frames == 0,
        fp.as_sign_material()
    );

    end_wss_session(sig, &session_id, &end_reason).await;
    let _ = mgr.shutdown();
    if let Some(t) = tray {
        t.end_session();
    }
    Ok(summary)
}

/// Serve one session with media on a **remote agent** over control IPC (KD5).
async fn handle_one_session_agent(
    sig: &mut SignalingClient,
    agent: &mut ServiceAgentClient,
    public_id: &str,
    mode: TransportMode,
    video_frames: u32,
    wait_incoming: Duration,
    tray: Option<&HostTray>,
) -> Result<String, String> {
    let session_id = accept_incoming_session(sig, wait_incoming).await?;
    if let Some(t) = tray {
        t.begin_session(&session_id, None);
    }

    let outbound = agent
        .start_session(&session_id, false)
        .map_err(|e| format!("agent start_session: {e}"))?;

    let mut offer_sdp = None;
    let mut pending_host_ice = Vec::new();
    for msg in &outbound {
        if let ControlMessage::SignalForward(s) = msg {
            match s.kind.as_str() {
                k if k == signal_kind::SESSION_OFFER => {
                    offer_sdp = Some(
                        parse_sdp_payload(&s.payload).map_err(|e| format!("parse offer: {e}"))?,
                    );
                }
                k if k == signal_kind::ICE_CANDIDATE => {
                    pending_host_ice.push(s.clone());
                }
                _ => {}
            }
        }
    }
    let offer = offer_sdp.ok_or_else(|| "agent did not emit session_offer".to_string())?;

    let offer_seq = sig.take_seq();
    sig.send(&SignalMessage::SessionOffer {
        session_id: session_id.clone(),
        signal_seq: offer_seq,
        sdp: offer.sdp.clone(),
        fingerprint_sig: offer.fingerprint_sig.clone().unwrap_or_default(),
    })
    .await
    .map_err(|e| format!("send offer: {e}"))?;
    println!(
        "ws-host: session_offer (via agent) seq={offer_seq} sdp_len={}",
        offer.sdp.len()
    );

    // Wait for viewer answer; do not trickle host ICE yet.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut got_answer = false;
    let mut early_viewer_ice = Vec::new();
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match sig
            .recv_timeout(remaining.min(Duration::from_millis(250)))
            .await
        {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => continue,
            Err(e) => return Err(format!("recv: {e}")),
        };
        match msg {
            SignalMessage::SessionAnswer { sdp, signal_seq, .. } => {
                println!("ws-host: session_answer seq={signal_seq}");
                let payload = serde_json::to_string(&SdpPayload {
                    sdp,
                    fingerprint_sig: None,
                })
                .map_err(|e| e.to_string())?;
                let (reply, more) = agent
                    .request(&signal_to_agent(
                        &session_id,
                        signal_kind::SESSION_ANSWER,
                        &payload,
                    ))
                    .map_err(|e| format!("agent answer: {e}"))?;
                if !matches!(reply, ControlMessage::Ack(_)) {
                    return Err(format!("agent rejected answer: {reply:?}"));
                }
                for m in more {
                    if let ControlMessage::SignalForward(s) = m {
                        if s.kind == signal_kind::ICE_CANDIDATE {
                            pending_host_ice.push(s);
                        }
                    }
                }
                got_answer = true;
                break;
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                early_viewer_ice.push(candidate);
            }
            other => println!("ws-host: ignore {other:?}"),
        }
    }
    if !got_answer {
        let _ = agent.request(&ControlMessage::DetachSession(DetachSession {
            session_id: session_id.clone(),
            reason: Some("answer_timeout".into()),
        }));
        if let Some(t) = tray {
            t.end_session();
        }
        return Err("timeout waiting for session_answer".into());
    }

    // Host ICE → WSS (after answer).
    for s in &pending_host_ice {
        let c = parse_ice_payload(&s.payload).map_err(|e| e.to_string())?;
        let ice_seq = sig.take_seq();
        sig.send(&SignalMessage::IceCandidate {
            session_id: session_id.clone(),
            signal_seq: ice_seq,
            candidate: c,
        })
        .await
        .map_err(|e| format!("send host ice: {e}"))?;
    }

    // Early + trickle viewer ICE → agent (and forward any new agent ICE).
    for candidate in early_viewer_ice {
        let payload = serde_json::to_string(&candidate).map_err(|e| e.to_string())?;
        let (reply, more) = agent
            .request(&signal_to_agent(
                &session_id,
                signal_kind::ICE_CANDIDATE,
                &payload,
            ))
            .map_err(|e| format!("agent ice: {e}"))?;
        if !matches!(reply, ControlMessage::Ack(_)) {
            return Err(format!("agent rejected ice: {reply:?}"));
        }
        forward_agent_ice_to_wss(sig, &session_id, &more).await?;
    }

    // Trickle remaining ICE and poke the agent so it can poll → Connected → pump.
    // QueryStats is the control-plane heartbeat that drives agent-side poll/pump
    // without putting media bytes on the IPC pipe.
    let ice_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut agent_video = 0u64;
    let mut agent_connected = false;
    while tokio::time::Instant::now() < ice_deadline {
        if tray.map(|t| t.take_end_session()).unwrap_or(false) {
            let _ = agent.request(&ControlMessage::DetachSession(DetachSession {
                session_id: session_id.clone(),
                reason: Some("tray_end_session".into()),
            }));
            if let Some(t) = tray {
                t.apply_kill();
            }
            return Err("session ended from tray".into());
        }
        if let Ok(msg) = sig.recv_timeout(Duration::from_millis(40)).await {
            if let SignalMessage::IceCandidate { candidate, .. } = msg {
                let payload = serde_json::to_string(&candidate).map_err(|e| e.to_string())?;
                let (reply, more) = agent
                    .request(&signal_to_agent(
                        &session_id,
                        signal_kind::ICE_CANDIDATE,
                        &payload,
                    ))
                    .map_err(|e| format!("agent ice: {e}"))?;
                if !matches!(reply, ControlMessage::Ack(_)) {
                    return Err(format!("agent rejected ice: {reply:?}"));
                }
                forward_agent_ice_to_wss(sig, &session_id, &more).await?;
            }
        }

        let (stats_reply, more) = agent
            .request(&ControlMessage::QueryStats(
                remotelink_platform_windows::ipc::message::QueryStats {
                    session_id: Some(session_id.clone()),
                },
            ))
            .map_err(|e| format!("agent query_stats: {e}"))?;
        forward_agent_ice_to_wss(sig, &session_id, &more).await?;
        if let ControlMessage::StatsPush(s) = stats_reply {
            if let Some(ref path) = s.ice_path {
                if path == "connected" {
                    agent_connected = true;
                }
            }
            // video_bitrate_bps is derived from frames_sent on the agent.
            if s.video_bitrate_bps.unwrap_or(0) > 0 {
                agent_video = agent_video.max(1);
            }
        }

        if agent_connected && agent_video > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    if !agent_connected {
        let _ = agent.request(&ControlMessage::DetachSession(DetachSession {
            session_id: session_id.clone(),
            reason: Some("agent_not_connected".into()),
        }));
        if let Some(t) = tray {
            t.end_session();
        }
        return Err("agent media plane never reached Connected".into());
    }
    if let Some(t) = tray {
        t.mark_session_active();
    }
    if agent_video == 0 && video_frames > 0 {
        // One last poke: QueryStats again after a short settle.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (stats_reply, _) = agent
            .request(&ControlMessage::QueryStats(
                remotelink_platform_windows::ipc::message::QueryStats {
                    session_id: Some(session_id.clone()),
                },
            ))
            .map_err(|e| format!("agent query_stats: {e}"))?;
        if let ControlMessage::StatsPush(s) = stats_reply {
            if s.video_bitrate_bps.unwrap_or(0) > 0 {
                agent_video = 1;
            }
        }
    }
    if agent_video == 0 {
        let _ = agent.request(&ControlMessage::DetachSession(DetachSession {
            session_id: session_id.clone(),
            reason: Some("agent_no_video".into()),
        }));
        if let Some(t) = tray {
            t.end_session();
        }
        return Err("agent connected but pumped no video".into());
    }

    let mut end_reason = "host_media_complete".to_string();
    if video_frames == 0 {
        end_reason = drive_agent_live(sig, agent, &session_id, tray).await?;
    }

    // Detach agent so the next session can attach a fresh media plane.
    let _ = agent.request(&ControlMessage::DetachSession(DetachSession {
        session_id: session_id.clone(),
        reason: Some(end_reason.clone()),
    }));

    end_wss_session(sig, &session_id, &end_reason).await;
    if let Some(t) = tray {
        t.end_session();
    }
    Ok(format!(
        "ws-host ok public_id={public_id} session={session_id} media=agent transport={} video_tx>={}",
        mode.as_str(),
        video_frames.min(3)
    ))
}

/// Relay any agent-emitted ICE SignalForward messages onto the host WSS.
async fn forward_agent_ice_to_wss(
    sig: &mut SignalingClient,
    session_id: &str,
    messages: &[ControlMessage],
) -> Result<(), String> {
    for m in messages {
        if let ControlMessage::SignalForward(s) = m {
            if s.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&s.payload).map_err(|e| e.to_string())?;
                let ice_seq = sig.take_seq();
                sig.send(&SignalMessage::IceCandidate {
                    session_id: session_id.into(),
                    signal_seq: ice_seq,
                    candidate: c,
                })
                .await
                .map_err(|e| format!("send agent ice: {e}"))?;
            }
        }
    }
    Ok(())
}

async fn relay_offer_answer_ice_local(
    sig: &mut SignalingClient,
    session_id: &str,
    offer: &SdpPayload,
    pending_host_ice: &mut Vec<remotelink_platform_windows::ipc::message::SignalForward>,
    mgr: &mut SessionManager,
    tray: Option<&HostTray>,
) -> Result<(), String> {
    let offer_seq = sig.take_seq();
    sig.send(&SignalMessage::SessionOffer {
        session_id: session_id.into(),
        signal_seq: offer_seq,
        sdp: offer.sdp.clone(),
        fingerprint_sig: offer.fingerprint_sig.clone().unwrap_or_default(),
    })
    .await
    .map_err(|e| format!("send offer: {e}"))?;
    println!(
        "ws-host: session_offer seq={offer_seq} sdp_len={}",
        offer.sdp.len()
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut got_answer = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let msg = match sig
            .recv_timeout(remaining.min(Duration::from_millis(250)))
            .await
        {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => continue,
            Err(e) => return Err(format!("recv: {e}")),
        };

        match msg {
            SignalMessage::SessionAnswer { sdp, signal_seq, .. } => {
                println!("ws-host: session_answer seq={signal_seq}");
                mgr.apply_signal(
                    signal_kind::SESSION_ANSWER,
                    &serde_json::to_string(&SdpPayload {
                        sdp,
                        fingerprint_sig: None,
                    })
                    .map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply answer: {e}"))?;
                got_answer = true;
                break;
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                mgr.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply ice: {e}"))?;
            }
            other => println!("ws-host: ignore {other:?}"),
        }
    }
    if !got_answer {
        return Err("timeout waiting for session_answer".into());
    }

    let _ = mgr.peer_mut().poll();
    pending_host_ice.extend(
        mgr.take_outbound_signals()
            .into_iter()
            .filter(|s| s.kind == signal_kind::ICE_CANDIDATE),
    );
    for ice_sig in pending_host_ice.iter() {
        let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
        let ice_seq = sig.take_seq();
        sig.send(&SignalMessage::IceCandidate {
            session_id: session_id.into(),
            signal_seq: ice_seq,
            candidate: c,
        })
        .await
        .map_err(|e| format!("send host ice: {e}"))?;
    }

    let ice_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < ice_deadline {
        if tray.map(|t| t.take_end_session()).unwrap_or(false) {
            return Err("session ended from tray".into());
        }
        let _ = mgr.peer_mut().poll();
        for ice_sig in mgr.take_outbound_signals() {
            if ice_sig.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
                let ice_seq = sig.take_seq();
                sig.send(&SignalMessage::IceCandidate {
                    session_id: session_id.into(),
                    signal_seq: ice_seq,
                    candidate: c,
                })
                .await
                .map_err(|e| format!("send host ice: {e}"))?;
            }
        }
        if let Ok(msg) = sig.recv_timeout(Duration::from_millis(50)).await {
            if let SignalMessage::IceCandidate { candidate, .. } = msg {
                mgr.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("apply ice: {e}"))?;
            }
        }
        if mgr.connection_state() == remotelink_net::ConnectionState::Connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

async fn end_wss_session(sig: &mut SignalingClient, session_id: &str, reason: &str) {
    let end_seq = sig.take_seq();
    let _ = sig
        .send(&SignalMessage::SessionEnd {
            session_id: session_id.into(),
            signal_seq: end_seq,
            reason: reason.into(),
        })
        .await;
}

fn configure_session_media(mgr: &mut SessionManager, live: bool) {
    if !live {
        return;
    }
    mgr.set_force_software(true);
    mgr.set_synthetic_geometry(1280, 720, 20);
    let _ = mgr.set_injector_config(InjectorConfig::default());
    if cfg!(windows) {
        mgr.set_video_kind(VideoCaptureKind::WindowsDxgi);
        mgr.set_audio_kind(AudioCaptureKind::WindowsWasapiPreferNative);
    }
}

async fn drive_local_media(
    sig: &mut SignalingClient,
    mgr: &mut SessionManager,
    video_frames: u32,
    tray: Option<&HostTray>,
) -> Result<(crate::session::PumpStats, String), String> {
    if video_frames > 0 {
        let pump = mgr
            .pump_media(video_frames)
            .map_err(|e| format!("pump: {e}"))?;
        return Ok((pump, "host_media_complete".into()));
    }

    let mut totals = crate::session::PumpStats::default();
    loop {
        if tray.map(|t| t.take_end_session()).unwrap_or(false) {
            return Ok((totals, "tray_end_session".into()));
        }
        if mgr.connection_state() != remotelink_net::ConnectionState::Connected {
            return Ok((totals, "peer_disconnected".into()));
        }

        match sig.recv_timeout(Duration::from_millis(5)).await {
            Ok(SignalMessage::SessionEnd { reason, .. }) => {
                return Ok((
                    totals,
                    if reason.is_empty() {
                        "viewer_session_end".into()
                    } else {
                        reason
                    },
                ));
            }
            Ok(SignalMessage::IceCandidate { candidate, .. }) => {
                let _ = mgr.apply_signal(
                    signal_kind::ICE_CANDIDATE,
                    &serde_json::to_string(&candidate).unwrap_or_default(),
                );
            }
            Ok(_) => {}
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {}
            Err(e) => return Ok((totals, format!("signaling:{e}"))),
        }

        let _ = mgr.poll_inbound();
        match mgr.pump_media(1) {
            Ok(s) => {
                totals.video_sent = totals.video_sent.saturating_add(s.video_sent);
                totals.audio_sent = totals.audio_sent.saturating_add(s.audio_sent);
                if s.skipped_not_connected {
                    return Ok((totals, "peer_disconnected".into()));
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("idle") {
                    tokio::time::sleep(Duration::from_millis(16)).await;
                    continue;
                }
                return Ok((totals, format!("pump:{msg}")));
            }
        }
        tokio::time::sleep(Duration::from_millis(32)).await;
    }
}

async fn drive_agent_live(
    sig: &mut SignalingClient,
    agent: &mut ServiceAgentClient,
    session_id: &str,
    tray: Option<&HostTray>,
) -> Result<String, String> {
    loop {
        if tray.map(|t| t.take_end_session()).unwrap_or(false) {
            return Ok("tray_end_session".into());
        }
        match sig.recv_timeout(Duration::from_millis(40)).await {
            Ok(SignalMessage::SessionEnd { reason, .. }) => {
                return Ok(if reason.is_empty() {
                    "viewer_session_end".into()
                } else {
                    reason
                });
            }
            Ok(SignalMessage::IceCandidate { candidate, .. }) => {
                let payload = serde_json::to_string(&candidate).map_err(|e| e.to_string())?;
                let (reply, more) = agent
                    .request(&signal_to_agent(
                        session_id,
                        signal_kind::ICE_CANDIDATE,
                        &payload,
                    ))
                    .map_err(|e| format!("agent ice: {e}"))?;
                if !matches!(reply, ControlMessage::Ack(_)) {
                    return Ok("agent_rejected_ice".into());
                }
                let _ = forward_agent_ice_to_wss(sig, session_id, &more).await;
            }
            Ok(_) => {}
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {}
            Err(e) => return Ok(format!("signaling:{e}")),
        }
        let _ = agent.request(&ControlMessage::QueryStats(
            remotelink_platform_windows::ipc::message::QueryStats {
                session_id: Some(session_id.into()),
            },
        ));
        tokio::time::sleep(Duration::from_millis(32)).await;
    }
}

/// One-shot: register, connect, serve **one** session, close.
pub async fn run_ws_host(cfg: WsHostConfig) -> Result<String, String> {
    let mut one = cfg;
    one.max_sessions = 1;
    one.reconnect = false;
    run_ws_host_service(one).await
}

/// Long-lived host: enroll once, serve up to `max_sessions` (0 = unlimited),
/// reconnect signaling on failure when `reconnect` is set.
///
/// When [`WsHostConfig::agent_control`] is set, media runs on the agent over
/// KD5 control IPC; this process only owns WSS + enrollment.
pub async fn run_ws_host_service(cfg: WsHostConfig) -> Result<String, String> {
    let mode = coerce_transport(cfg.transport);
    let transport_cfg = TransportConfig { mode };
    let enrolled = enroll_or_reuse(&cfg).await?;
    let public_id = enrolled.public_id.clone();
    let access_token = enrolled.access_token.clone();
    let ws_url = http_to_ws_url(&cfg.server).map_err(|e| format!("ws url: {e}"))?;

    let status_path = cfg
        .status_path
        .clone()
        .unwrap_or_else(|| default_status_path(&cfg.creds_path));
    let tray = if cfg.tray {
        let t = HostTray::new(
            cfg.display_name.clone(),
            status_path.clone(),
            true,
            cfg.os_tray,
        );
        t.set_identity(&public_id, Some(&cfg.display_name));
        println!(
            "ws-host: tray status file {}",
            status_path.display()
        );
        Some(t)
    } else {
        None
    };

    let mut otp_hub = LiveOtpHub::new();
    if let Some(otp) = maybe_mint_otp(&cfg, &enrolled, &mut otp_hub).await? {
        if let Some(ref t) = tray {
            t.set_otp(&otp.code, otp.expires_at);
        }
    }

    let mut agent_client = if let Some(ref ep) = cfg.agent_control {
        println!(
            "ws-host: KD5 mode — connecting agent control at {}{}",
            crate::control_loop::format_endpoint(ep),
            if cfg.boot_secret.is_some() {
                " (boot secret set)"
            } else {
                ""
            }
        );
        Some(
            ServiceAgentClient::connect_with_secret(ep, cfg.boot_secret.clone())
                .map_err(|e| format!("agent control connect: {e}"))?,
        )
    } else {
        None
    };

    let unlimited = cfg.max_sessions == 0;
    let mut completed: u32 = 0;
    let mut last_summary = String::new();
    let mut backoff = cfg.reconnect_backoff;

    loop {
        if !unlimited && completed >= cfg.max_sessions {
            break;
        }

        let connect_result = async {
            let mut sig = SignalingClient::connect(&ws_url)
                .await
                .map_err(|e| format!("ws connect: {e}"))?;
            let hello = sig
                .hello_host(&access_token)
                .await
                .map_err(|e| format!("hello: {e}"))?;
            if let SignalMessage::HelloOk { feature_flags, .. } = &hello {
                println!(
                    "ws-host: hello_ok sdp_relay={} public_id={public_id} media={}",
                    feature_flags
                        .get("sdp_relay")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    if agent_client.is_some() {
                        "agent-ipc"
                    } else {
                        "local"
                    }
                );
            }

            // Serve sessions until max or WS dies.
            loop {
                if !unlimited && completed >= cfg.max_sessions {
                    let _ = sig.close().await;
                    return Ok::<_, String>(());
                }
                let session_result = if let Some(ref mut agent) = agent_client {
                    handle_one_session_agent(
                        &mut sig,
                        agent,
                        &public_id,
                        mode,
                        cfg.video_frames,
                        cfg.wait_incoming,
                        tray.as_ref(),
                    )
                    .await
                } else {
                    handle_one_session_local(
                        &mut sig,
                        &public_id,
                        mode,
                        &transport_cfg,
                        cfg.video_frames,
                        cfg.wait_incoming,
                        tray.as_ref(),
                        &mut otp_hub,
                    )
                    .await
                };
                let remint_after = cfg.video_frames == 0
                    && cfg.mint_otp
                    && match &session_result {
                        Ok(_) => true,
                        Err(e) => {
                            !e.contains("wait incoming") && !e.contains("timeout waiting")
                        }
                    };
                match session_result {
                    Ok(summary) => {
                        println!("{summary}");
                        last_summary = summary;
                        completed = completed.saturating_add(1);
                    }
                    Err(e) => {
                        if e.contains("ended from tray") {
                            eprintln!("ws-host: {e}");
                            // "Exit host" sets both end_session and exit.
                            if tray.as_ref().map(|t| t.take_exit()).unwrap_or(false) {
                                let _ = sig.close().await;
                                return Ok::<_, String>(());
                            }
                            // End-session only: accept the next viewer when multi-session.
                            if cfg.reconnect || unlimited || cfg.max_sessions > 1 {
                                continue;
                            }
                            let _ = sig.close().await;
                            return Ok(());
                        }
                        if cfg.reconnect || unlimited || cfg.max_sessions > 1 {
                            eprintln!("ws-host: session ended: {e}");
                            if e.contains("connection closed")
                                || e.contains("connect")
                                || e.contains("ws ")
                                || e.contains("agent control")
                            {
                                let _ = sig.close().await;
                                return Err(e);
                            }
                            if e.contains("wait incoming") || e.contains("timeout") {
                                if !cfg.reconnect && !unlimited && cfg.max_sessions <= 1 {
                                    let _ = sig.close().await;
                                    return Err(e);
                                }
                                continue;
                            }
                            let _ = sig.close().await;
                            return Err(e);
                        }
                        let _ = sig.close().await;
                        return Err(e);
                    }
                }
                if remint_after {
                    match maybe_mint_otp(&cfg, &enrolled, &mut otp_hub).await {
                        Ok(Some(otp)) => {
                            if let Some(ref t) = tray {
                                t.set_otp(&otp.code, otp.expires_at);
                            }
                            println!("ws-host: new OTP minted for next viewer");
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("ws-host: remint OTP failed: {e}"),
                    }
                }
                // Tray "Exit host" between sessions (idle).
                if tray.as_ref().map(|t| t.take_exit()).unwrap_or(false) {
                    println!("ws-host: exit requested from tray");
                    let _ = sig.close().await;
                    return Ok::<_, String>(());
                }
            }
        }
        .await;

        match connect_result {
            Ok(()) => {
                if !unlimited && completed >= cfg.max_sessions {
                    break;
                }
                if !cfg.reconnect {
                    break;
                }
            }
            Err(e) => {
                if !cfg.reconnect {
                    return Err(e);
                }
                eprintln!(
                    "ws-host: signaling error: {e}; reconnecting in {}s…",
                    backoff.as_secs()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        }

        // Successful drain of max sessions.
        break;
    }

    if last_summary.is_empty() {
        Ok(format!(
            "ws-host service exit public_id={public_id} sessions={completed}"
        ))
    } else {
        Ok(format!(
            "{last_summary}; service sessions_completed={completed}"
        ))
    }
}

/// Blocking entry for the host binary (one-shot or service).
pub fn run_ws_host_blocking(cfg: WsHostConfig) -> Result<String, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    if cfg.reconnect || cfg.max_sessions != 1 {
        rt.block_on(run_ws_host_service(cfg))
    } else {
        rt.block_on(run_ws_host(cfg))
    }
}

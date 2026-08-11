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

use remotelink_auth::{generate_device_keypair, mint_otp};
use remotelink_net::{
    create_peer_transport_with_config, PeerRole, TransportConfig, TransportMode,
};
use remotelink_protocol::SignalMessage;
use remotelink_signaling::{
    http_to_ws_url, post_otp_hash, refresh_device_token, register_device, HostCredentialFile,
    SignalingClient, DEFAULT_CREDS_PATH,
};

use crate::policy::{DEFAULT_HOST_OTP_PEPPER, DEFAULT_OTP_TTL_SECS};
use crate::session::{parse_ice_payload, parse_sdp_payload, signal_kind, SdpPayload, SessionManager};

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
            server: "http://127.0.0.1:8080".into(),
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
                let server = if cfg.server != "http://127.0.0.1:8080" {
                    cfg.server.clone()
                } else if !file.server.is_empty() {
                    file.server.clone()
                } else {
                    cfg.server.clone()
                };
                // Best-effort refresh so restarts survive access expiry.
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
                        }
                        Err(e) => {
                            eprintln!(
                                "ws-host: token refresh failed ({e}); using stored access token"
                            );
                        }
                    }
                }
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

/// Mint Mode A OTP, post hash to server, print code for the viewer CLI.
async fn maybe_mint_otp(cfg: &WsHostConfig, enrolled: &EnrolledHost) -> Result<(), String> {
    if !cfg.mint_otp {
        return Ok(());
    }
    let (code, hash) = mint_otp(6, DEFAULT_HOST_OTP_PEPPER)
        .map_err(|e| format!("mint_otp: {e}"))?;
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
    println!(
        "ws-host: Mode A OTP for viewer (expires {}): {}",
        resp.expires_at,
        code.as_str()
    );
    println!(
        "ws-host: viewer example: remotelink-viewer --ws-connect --server={} --host {} --otp {} --transport=live",
        cfg.server, enrolled.public_id, code.as_str()
    );
    Ok(())
}

/// Serve one media session on an already-authenticated signaling client.
async fn handle_one_session(
    sig: &mut SignalingClient,
    public_id: &str,
    mode: TransportMode,
    transport_cfg: &TransportConfig,
    video_frames: u32,
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

    // Per-session seq: accept must be >= server next (incoming_seq + 1).
    // Reset client cursor so multi-session service does not skip forever.
    sig.next_seq = incoming_seq.saturating_add(1);
    let accept_seq = sig.take_seq();
    sig.send(&SignalMessage::SessionAccept {
        session_id: session_id.clone(),
        signal_seq: accept_seq,
    })
    .await
    .map_err(|e| format!("accept: {e}"))?;
    println!("ws-host: session_accept seq={accept_seq}");

    let offerer = create_peer_transport_with_config(PeerRole::Offerer, transport_cfg)
        .map_err(|e| format!("create offerer: {e}"))?;
    let mut mgr = SessionManager::with_peer(offerer);
    mgr.attach(&session_id);
    mgr.start_media().map_err(|e| format!("start_media: {e}"))?;

    let outbound = mgr.take_outbound_signals();
    let offer_sig = outbound
        .iter()
        .find(|s| s.kind == signal_kind::SESSION_OFFER)
        .ok_or_else(|| "no session_offer from SessionManager".to_string())?;
    let offer = parse_sdp_payload(&offer_sig.payload).map_err(|e| e.to_string())?;

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
        "ws-host: session_offer seq={offer_seq} sdp_len={}",
        offer.sdp.len()
    );

    // Do not trickle host ICE until after session_answer (strict signal_seq).
    let mut pending_host_ice: Vec<_> = outbound
        .iter()
        .filter(|s| s.kind == signal_kind::ICE_CANDIDATE)
        .cloned()
        .collect();

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
            other => {
                println!("ws-host: ignore {other:?}");
            }
        }
    }

    if !got_answer {
        let _ = mgr.shutdown();
        return Err("timeout waiting for session_answer".into());
    }

    let _ = mgr.peer_mut().poll();
    pending_host_ice.extend(
        mgr.take_outbound_signals()
            .into_iter()
            .filter(|s| s.kind == signal_kind::ICE_CANDIDATE),
    );
    for ice_sig in pending_host_ice {
        let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
        let ice_seq = sig.take_seq();
        sig.send(&SignalMessage::IceCandidate {
            session_id: session_id.clone(),
            signal_seq: ice_seq,
            candidate: c,
        })
        .await
        .map_err(|e| format!("send host ice: {e}"))?;
    }

    let ice_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while tokio::time::Instant::now() < ice_deadline {
        let _ = mgr.peer_mut().poll();
        for ice_sig in mgr.take_outbound_signals() {
            if ice_sig.kind == signal_kind::ICE_CANDIDATE {
                let c = parse_ice_payload(&ice_sig.payload).map_err(|e| e.to_string())?;
                let ice_seq = sig.take_seq();
                sig.send(&SignalMessage::IceCandidate {
                    session_id: session_id.clone(),
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

    mgr.wait_ready(Duration::from_secs(10))
        .map_err(|e| format!("wait_ready: {e}"))?;
    let pump = mgr
        .pump_media(video_frames)
        .map_err(|e| format!("pump: {e}"))?;
    if pump.skipped_not_connected || pump.video_sent == 0 {
        let _ = mgr.shutdown();
        return Err(format!(
            "pump failed (skipped={} video_sent={})",
            pump.skipped_not_connected, pump.video_sent
        ));
    }

    let fp = mgr
        .peer_mut()
        .local_fingerprint()
        .map_err(|e| e.to_string())?;
    let summary = format!(
        "ws-host ok public_id={public_id} session={session_id} transport={} video_tx={} audio_tx={} fp={}",
        mode.as_str(),
        pump.video_sent,
        pump.audio_sent,
        fp.as_sign_material()
    );

    // End session on the media plane; keep WSS for the next viewer.
    let end_seq = sig.take_seq();
    let _ = sig
        .send(&SignalMessage::SessionEnd {
            session_id: session_id.clone(),
            signal_seq: end_seq,
            reason: "host_media_complete".into(),
        })
        .await;
    let _ = mgr.shutdown();
    Ok(summary)
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
pub async fn run_ws_host_service(cfg: WsHostConfig) -> Result<String, String> {
    let mode = coerce_transport(cfg.transport);
    let transport_cfg = TransportConfig { mode };
    let enrolled = enroll_or_reuse(&cfg).await?;
    maybe_mint_otp(&cfg, &enrolled).await?;
    let public_id = enrolled.public_id.clone();
    let access_token = enrolled.access_token.clone();
    let ws_url = http_to_ws_url(&cfg.server).map_err(|e| format!("ws url: {e}"))?;

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
                    "ws-host: hello_ok sdp_relay={} public_id={public_id}",
                    feature_flags
                        .get("sdp_relay")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                );
            }

            // Serve sessions until max or WS dies.
            loop {
                if !unlimited && completed >= cfg.max_sessions {
                    let _ = sig.close().await;
                    return Ok::<_, String>(());
                }
                match handle_one_session(
                    &mut sig,
                    &public_id,
                    mode,
                    &transport_cfg,
                    cfg.video_frames,
                    cfg.wait_incoming,
                )
                .await
                {
                    Ok(summary) => {
                        println!("{summary}");
                        last_summary = summary;
                        completed = completed.saturating_add(1);
                    }
                    Err(e) => {
                        if cfg.reconnect || unlimited || cfg.max_sessions > 1 {
                            eprintln!("ws-host: session ended: {e}");
                            if e.contains("connection closed")
                                || e.contains("connect")
                                || e.contains("ws ")
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

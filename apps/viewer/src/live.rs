//! Long-lived WSS viewer session for the product shell (Phase 3).
//!
//! Unlike [`crate::run_ws_viewer`], this keeps signaling + media open and
//! publishes the latest decoded frame for a UI to present.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use remotelink_net::{
    create_peer_transport_with_config, ConnectionState, PeerRole, SessionDescription,
    TransportConfig,
};
use remotelink_protocol::{NamedKey, SignalMessage};
use remotelink_signaling::{http_to_ws_url, SignalingClient};
use remotelink_viewer_core::{
    ConnectRequest, DecodedVideoFrame, RawInput, ViewerPhase, ViewerSession,
};

use crate::ws_connect::WsViewerConfig;

/// Latest frame + stats the desktop window can paint.
#[derive(Debug, Clone, Default)]
pub struct LiveViewerSnapshot {
    /// Human status line.
    pub status: String,
    /// Connection phase label.
    pub phase: String,
    /// Decoded video frames received.
    pub video_rx: u64,
    /// Audio packets received.
    pub audio_rx: u64,
    /// Latest frame width.
    pub width: u32,
    /// Latest frame height.
    pub height: u32,
    /// Latest frame as tightly packed RGBA8.
    pub rgba: Option<Vec<u8>>,
    /// Stats HUD line from viewer-core.
    pub hud: String,
    /// True when Mode A identity bind completed (input is allowed).
    pub identity_bound: bool,
    /// Fatal error (session ended).
    pub error: Option<String>,
    /// True when the background thread has finished.
    pub ended: bool,
}

/// Handle to a background live viewer.
pub struct LiveViewerHandle {
    snapshot: Arc<Mutex<LiveViewerSnapshot>>,
    stop: Arc<AtomicBool>,
    input_tx: Sender<RawInput>,
    join: Option<JoinHandle<()>>,
}

impl LiveViewerHandle {
    /// Start a live WSS connect in a background thread.
    pub fn start(cfg: WsViewerConfig) -> Self {
        let snapshot = Arc::new(Mutex::new(LiveViewerSnapshot {
            status: "Connecting…".into(),
            phase: ViewerPhase::Connecting.as_str().into(),
            ..LiveViewerSnapshot::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (input_tx, input_rx) = mpsc::channel();
        let snap = Arc::clone(&snapshot);
        let stop_flag = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("remotelink-live-viewer".into())
            .spawn(move || {
                let result = run_live_blocking(cfg, snap.clone(), stop_flag, input_rx);
                if let Ok(mut g) = snap.lock() {
                    g.ended = true;
                    match result {
                        Ok(reason) => {
                            if g.error.is_none() {
                                g.status = format!("Session ended ({reason})");
                            }
                        }
                        Err(e) => {
                            g.error = Some(e.clone());
                            g.status = format!("Connect failed: {e}");
                        }
                    }
                }
            })
            .expect("spawn live viewer");
        Self {
            snapshot,
            stop,
            input_tx,
            join: Some(join),
        }
    }

    /// Copy of the latest snapshot.
    pub fn snapshot(&self) -> LiveViewerSnapshot {
        self.snapshot.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Queue a raw input sample for the host (mouse / keys).
    pub fn send_input(&self, raw: RawInput) {
        let _ = self.input_tx.send(raw);
    }

    /// Ask the session to hang up.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether the worker has exited.
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map(|j| j.is_finished()).unwrap_or(true)
    }
}

impl Drop for LiveViewerHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            if join.is_finished() {
                let _ = join.join();
            }
        }
    }
}

fn run_live_blocking(
    cfg: WsViewerConfig,
    snapshot: Arc<Mutex<LiveViewerSnapshot>>,
    stop: Arc<AtomicBool>,
    input_rx: Receiver<RawInput>,
) -> Result<String, String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    rt.block_on(run_live(cfg, snapshot, stop, input_rx))
}

async fn run_live(
    cfg: WsViewerConfig,
    snapshot: Arc<Mutex<LiveViewerSnapshot>>,
    stop: Arc<AtomicBool>,
    input_rx: Receiver<RawInput>,
) -> Result<String, String> {
    if cfg.host_public_id.is_empty() {
        return Err("remote ID is required".into());
    }
    let mode = cfg.transport.for_multi_process();
    let transport_cfg = TransportConfig { mode };

    let ws_url = http_to_ws_url(&cfg.server).map_err(|e| format!("ws url: {e}"))?;
    let mut sig = SignalingClient::connect(&ws_url)
        .await
        .map_err(|e| format!("ws connect: {e}"))?;
    sig.hello_viewer_anonymous()
        .await
        .map_err(|e| format!("hello: {e}"))?;
    publish(&snapshot, |s| s.status = "Waiting for host…".into());

    let session_id = format!("viewer-{}", {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    });
    let intent_seq = sig.take_seq();
    let req = ConnectRequest::otp(&cfg.host_public_id, &cfg.otp);
    let intent = req
        .session_intent_message(&session_id, intent_seq)
        .map_err(|e| format!("intent: {e}"))?;
    sig.send(&intent)
        .await
        .map_err(|e| format!("send intent: {e}"))?;

    let accept = sig
        .recv_until(Duration::from_secs(60), |m| {
            matches!(
                m,
                SignalMessage::SessionAccept { .. } | SignalMessage::SessionReject { .. }
            )
        })
        .await
        .map_err(|e| {
            let msg = e.to_string();
            if msg.contains("OTP prefilter") || msg.contains("OTP required") {
                format!(
                    "OTP rejected. Copy the current OTP from the other PC (codes expire and are replaced when that app restarts). {msg}"
                )
            } else {
                format!("wait accept: {msg}")
            }
        })?;
    match accept {
        SignalMessage::SessionAccept { .. } => {
            publish(&snapshot, |s| s.status = "Accepted — negotiating…".into());
        }
        SignalMessage::SessionReject { reason, .. } => {
            return Err(format!("session rejected: {reason:?}"));
        }
        _ => unreachable!(),
    }

    let answerer = create_peer_transport_with_config(PeerRole::Answerer, &transport_cfg)
        .map_err(|e| format!("create answerer: {e}"))?;
    let mut viewer = ViewerSession::new();
    viewer.set_record_limit(Some(4));
    viewer.set_focused(true);
    viewer.set_always_capture(true);
    if let Ok(key) = remotelink_auth::SessionBindKey::from_mode_a_otp(&cfg.otp) {
        viewer.set_bind_key(key);
        viewer.set_require_identity_for_input(true);
    }
    viewer
        .begin_connect(&req)
        .map_err(|e| format!("begin_connect: {e}"))?;
    if remotelink_auth::SessionBindKey::from_mode_a_otp(&cfg.otp).is_ok() {
        viewer.mark_session_authorized();
    }
    viewer.attach_transport(answerer);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut got_offer = false;
    while tokio::time::Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            let _ = sig.close().await;
            return Ok("cancelled".into());
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match sig
            .recv_timeout(remaining.min(Duration::from_millis(250)))
            .await
        {
            Ok(m) => m,
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {
                let _ = viewer.poll();
                continue;
            }
            Err(e) => return Err(format!("recv: {e}")),
        };
        match msg {
            SignalMessage::SessionOffer {
                sdp,
                fingerprint_sig,
                ..
            } => {
                let answer = viewer
                    .accept_offer_with_sig(
                        SessionDescription::offer(sdp),
                        if fingerprint_sig.is_empty() {
                            None
                        } else {
                            Some(fingerprint_sig.as_str())
                        },
                    )
                    .map_err(|e| format!("accept_offer: {e}"))?;
                let ans_seq = sig.take_seq();
                sig.send(&SignalMessage::SessionAnswer {
                    session_id: session_id.clone(),
                    signal_seq: ans_seq,
                    sdp: answer.sdp,
                })
                .await
                .map_err(|e| format!("send answer: {e}"))?;
                got_offer = true;
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    sig.send(&SignalMessage::IceCandidate {
                        session_id: session_id.clone(),
                        signal_seq: ice_seq,
                        candidate: ice,
                    })
                    .await
                    .map_err(|e| format!("send ice: {e}"))?;
                }
            }
            SignalMessage::IceCandidate { candidate, .. } => {
                let _ = viewer.add_remote_ice(candidate);
                let _ = viewer.poll();
                for ice in viewer.take_pending_local_ice() {
                    let ice_seq = sig.take_seq();
                    let _ = sig
                        .send(&SignalMessage::IceCandidate {
                            session_id: session_id.clone(),
                            signal_seq: ice_seq,
                            candidate: ice,
                        })
                        .await;
                }
            }
            SignalMessage::SessionEnd { reason, .. } => {
                return Ok(reason);
            }
            _ => {}
        }
        let _ = viewer.poll();
        if got_offer && viewer.transport_state() == Some(ConnectionState::Connected) {
            break;
        }
    }
    if !got_offer {
        return Err("timeout waiting for session_offer".into());
    }

    publish(&snapshot, |s| s.status = "Streaming".into());

    let mut end_reason = "viewer_disconnect".to_string();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        if matches!(
            viewer.transport_state(),
            Some(ConnectionState::Failed) | Some(ConnectionState::Closed)
        ) {
            end_reason = "peer_disconnected".into();
            break;
        }

        match sig.recv_timeout(Duration::from_millis(8)).await {
            Ok(SignalMessage::IceCandidate { candidate, .. }) => {
                let _ = viewer.add_remote_ice(candidate);
            }
            Ok(SignalMessage::SessionEnd { reason, .. }) => {
                end_reason = if reason.is_empty() {
                    "host_session_end".into()
                } else {
                    reason
                };
                break;
            }
            Ok(_) => {}
            Err(remotelink_signaling::SignalingError::Timeout(_)) => {}
            Err(e) if e.is_stale_signal_seq() => {}
            Err(e) => {
                end_reason = format!("signaling:{e}");
                break;
            }
        }

        let _ = viewer.poll();
        while let Ok(raw) = input_rx.try_recv() {
            let _ = viewer.push_raw_input(raw);
        }
        let _ = viewer.poll_input_capture();

        let frames = viewer.drain_video_frames();
        if let Some(last) = frames.last() {
            let rgba = frame_to_rgba(last);
            let stats = viewer.stats().clone();
            publish(&snapshot, |s| {
                s.status = "In session".into();
                s.phase = viewer.phase().as_str().into();
                s.video_rx = stats.video_frames;
                s.audio_rx = stats.audio_packets;
                s.width = last.frame.width;
                s.height = last.frame.height;
                s.rgba = Some(rgba);
                s.hud = stats.hud_line();
                s.identity_bound = stats.identity_bound;
            });
        } else {
            let stats = viewer.stats().clone();
            publish(&snapshot, |s| {
                s.phase = viewer.phase().as_str().into();
                s.video_rx = stats.video_frames;
                s.audio_rx = stats.audio_packets;
                s.hud = stats.hud_line();
                s.identity_bound = stats.identity_bound;
            });
        }
        tokio::time::sleep(Duration::from_millis(8)).await;
    }

    let end_seq = sig.take_seq();
    let _ = sig
        .send(&SignalMessage::SessionEnd {
            session_id,
            signal_seq: end_seq,
            reason: end_reason.clone(),
        })
        .await;
    let _ = viewer.close();
    let _ = sig.close().await;
    Ok(end_reason)
}

fn publish(snapshot: &Arc<Mutex<LiveViewerSnapshot>>, f: impl FnOnce(&mut LiveViewerSnapshot)) {
    if let Ok(mut g) = snapshot.lock() {
        f(&mut g);
    }
}

fn frame_to_rgba(decoded: &DecodedVideoFrame) -> Vec<u8> {
    use remotelink_media::PixelFormat;
    let w = decoded.frame.width as usize;
    let h = decoded.frame.height as usize;
    let src = &decoded.frame.data;
    let mut out = vec![0u8; w.saturating_mul(h).saturating_mul(4)];
    match decoded.frame.format {
        PixelFormat::Rgba8 => {
            let n = out.len().min(src.len());
            out[..n].copy_from_slice(&src[..n]);
        }
        PixelFormat::Bgra8 => {
            for (dst, px) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                dst[0] = px[2];
                dst[1] = px[1];
                dst[2] = px[0];
                dst[3] = px[3];
            }
        }
        PixelFormat::Rgb24 => {
            for (dst, px) in out.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                dst[0] = px[0];
                dst[1] = px[1];
                dst[2] = px[2];
                dst[3] = 255;
            }
        }
        PixelFormat::Gray8 => {
            for (dst, g) in out.chunks_exact_mut(4).zip(src.iter().copied()) {
                dst[0] = g;
                dst[1] = g;
                dst[2] = g;
                dst[3] = 255;
            }
        }
    }
    out
}

/// Map a few egui keys to protocol [`NamedKey`] (letters + common controls).
pub fn named_key_from_name(name: &str) -> Option<NamedKey> {
    Some(match name {
        "A" => NamedKey::A,
        "B" => NamedKey::B,
        "C" => NamedKey::C,
        "D" => NamedKey::D,
        "E" => NamedKey::E,
        "F" => NamedKey::F,
        "G" => NamedKey::G,
        "H" => NamedKey::H,
        "I" => NamedKey::I,
        "J" => NamedKey::J,
        "K" => NamedKey::K,
        "L" => NamedKey::L,
        "M" => NamedKey::M,
        "N" => NamedKey::N,
        "O" => NamedKey::O,
        "P" => NamedKey::P,
        "Q" => NamedKey::Q,
        "R" => NamedKey::R,
        "S" => NamedKey::S,
        "T" => NamedKey::T,
        "U" => NamedKey::U,
        "V" => NamedKey::V,
        "W" => NamedKey::W,
        "X" => NamedKey::X,
        "Y" => NamedKey::Y,
        "Z" => NamedKey::Z,
        "Enter" => NamedKey::Enter,
        "Escape" => NamedKey::Escape,
        "Backspace" => NamedKey::Backspace,
        "Tab" => NamedKey::Tab,
        "Space" => NamedKey::Space,
        "ArrowLeft" => NamedKey::ArrowLeft,
        "ArrowRight" => NamedKey::ArrowRight,
        "ArrowUp" => NamedKey::ArrowUp,
        "ArrowDown" => NamedKey::ArrowDown,
        "Delete" => NamedKey::Delete,
        _ => return None,
    })
}

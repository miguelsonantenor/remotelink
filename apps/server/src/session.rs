//! In-memory session registry and WebSocket presence (Redis later).
//!
//! Tracks:
//! - Connected hosts/viewers (connection id → outbound channel)
//! - Host presence by `public_id` (single live WS per host; re-hello replaces prior)
//! - Pending/active sessions with a **single-session busy lock** per host
//! - Session TTLs so busy locks cannot pin a host indefinitely
//! - Monotonic per-session `signal_seq` (strict: inbound must be ≥ `next_signal_seq`)
//! - Short-lived viewer session tokens (until `POST /v1/sessions`)
//!
//! Postgres `devices.active_session_id` CAS is deferred to multi-node / Redis work;
//! this map is the single-node source of truth for PR 5a.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use remotelink_protocol::{Role, SessionMode, SignalMessage};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};

use crate::credentials::{hash_token, random_token_body};
use crate::models::Device;

/// Outbound channel for a single WebSocket connection.
pub type ConnTx = mpsc::UnboundedSender<SignalMessage>;

/// Opaque connection identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnId(u64);

impl ConnId {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Viewer intent delivered; waiting for host accept/reject.
    Pending,
    /// Host accepted; SDP/ICE media signaling may be relayed.
    Active,
    /// Terminal — rejected or ended; lock released.
    Closed,
}

/// Default how long a pending (unaccepted) session may hold the busy lock.
pub const PENDING_SESSION_TTL: Duration = Duration::minutes(2);

/// Default how long an active session may live without re-auth (idle ceiling).
pub const ACTIVE_SESSION_TTL: Duration = Duration::hours(8);

/// In-memory session record.
#[derive(Debug, Clone)]
pub struct Session {
    pub session_id: String,
    pub host_public_id: String,
    pub host_device_id: i64,
    pub host_conn: ConnId,
    pub viewer_conn: ConnId,
    pub mode: SessionMode,
    pub state: SessionState,
    /// Next acceptable `signal_seq` for inbound session-scoped messages.
    /// Server-originated messages also consume this counter.
    pub next_signal_seq: u64,
    pub created_at: DateTime<Utc>,
    /// Absolute expiry; pending uses [`PENDING_SESSION_TTL`], active uses
    /// [`ACTIVE_SESSION_TTL`] (refreshed on accept).
    pub expires_at: DateTime<Utc>,
}

/// Authenticated (or anonymous) peer bound to a live WS connection.
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    pub role: Role,
    /// Host device public_id, or enrolled viewer device id when token is a device token.
    pub device_public_id: Option<String>,
    pub device_id: Option<i64>,
    /// True when viewer connected with empty `device_token`.
    pub anonymous: bool,
}

/// Short-lived viewer token record (plaintext never stored).
#[derive(Debug, Clone)]
struct ViewerTokenRecord {
    expires_at: DateTime<Utc>,
}

/// Default TTL for viewer session tokens minted for tests / pre-session.
pub const VIEWER_TOKEN_TTL: Duration = Duration::minutes(15);

/// Thread-safe in-memory session + presence registry.
#[derive(Debug)]
pub struct SessionRegistry {
    inner: Mutex<RegistryInner>,
    pending_ttl: Duration,
    active_ttl: Duration,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            pending_ttl: PENDING_SESSION_TTL,
            active_ttl: ACTIVE_SESSION_TTL,
        }
    }
}

#[derive(Debug, Default)]
struct RegistryInner {
    /// Live connections and their send handles.
    conns: HashMap<ConnId, ConnTx>,
    /// Host public_id → connection (re-hello replaces prior; see [`SessionRegistry::bind_peer`]).
    host_by_public_id: HashMap<String, ConnId>,
    /// Connection → peer identity after successful hello.
    peers: HashMap<ConnId, PeerIdentity>,
    /// session_id → session.
    sessions: HashMap<String, Session>,
    /// Host public_id → session_id while busy (pending or active).
    /// Single-node lock; multi-node will use Postgres `devices.active_session_id` / Redis.
    host_busy: HashMap<String, String>,
    /// Viewer access-token hash → expiry.
    viewer_tokens: HashMap<String, ViewerTokenRecord>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with custom TTLs (tests / tighter deployments).
    pub fn with_ttls(pending_ttl: Duration, active_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(RegistryInner::default()),
            pending_ttl,
            active_ttl,
        }
    }

    /// Register a new WS connection; returns its id and the receiver half
    /// for the connection task (caller keeps the sender for registry).
    pub async fn register_conn(&self) -> (ConnId, mpsc::UnboundedReceiver<SignalMessage>) {
        let id = ConnId::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let mut g = self.inner.lock().await;
        g.conns.insert(id, tx);
        (id, rx)
    }

    /// Drop a connection and release any sessions it still owns.
    pub async fn unregister_conn(&self, conn: ConnId) {
        let mut g = self.inner.lock().await;
        Self::unregister_conn_locked(&mut g, conn, "peer_disconnected");
    }

    fn unregister_conn_locked(g: &mut RegistryInner, conn: ConnId, end_reason: &str) {
        g.conns.remove(&conn);
        g.peers.remove(&conn);

        // Remove host presence only if this conn is still the published one.
        let host_ids: Vec<String> = g
            .host_by_public_id
            .iter()
            .filter_map(|(pid, cid)| {
                if *cid == conn {
                    Some(pid.clone())
                } else {
                    None
                }
            })
            .collect();
        for pid in host_ids {
            g.host_by_public_id.remove(&pid);
        }

        // Close sessions that still reference this connection.
        let affected: Vec<String> = g
            .sessions
            .iter()
            .filter_map(|(sid, s)| {
                if s.host_conn == conn || s.viewer_conn == conn {
                    Some(sid.clone())
                } else {
                    None
                }
            })
            .collect();

        for sid in affected {
            Self::close_session_locked(g, &sid, end_reason);
        }
    }

    /// Bind identity after a successful `hello`. Hosts are published for presence.
    ///
    /// If the same host `public_id` already has a live socket, that socket is
    /// replaced: in-flight sessions rebind `host_conn` to the new connection and
    /// the old connection is dropped with `connection_replaced`.
    pub async fn bind_peer(&self, conn: ConnId, identity: PeerIdentity) {
        let mut g = self.inner.lock().await;
        if identity.role == Role::Host {
            if let Some(ref public_id) = identity.device_public_id {
                if let Some(old) = g.host_by_public_id.insert(public_id.clone(), conn) {
                    if old != conn {
                        // Transfer pending/active sessions to the new host socket.
                        for session in g.sessions.values_mut() {
                            if session.host_conn == old && session.host_public_id == *public_id {
                                session.host_conn = conn;
                            }
                        }
                        // Evict old connection (do not end rebinding sessions —
                        // they no longer reference `old`).
                        if let Some(tx) = g.conns.remove(&old) {
                            let _ = tx.send(SignalMessage::Error {
                                code: "connection_replaced".into(),
                                message: "host reconnected on another socket".into(),
                            });
                        }
                        g.peers.remove(&old);
                    }
                }
            }
        }
        g.peers.insert(conn, identity);
    }

    pub async fn peer(&self, conn: ConnId) -> Option<PeerIdentity> {
        let g = self.inner.lock().await;
        g.peers.get(&conn).cloned()
    }

    /// Send a message to a live connection; returns false if gone.
    pub async fn send_to(&self, conn: ConnId, msg: SignalMessage) -> bool {
        let g = self.inner.lock().await;
        if let Some(tx) = g.conns.get(&conn) {
            tx.send(msg).is_ok()
        } else {
            false
        }
    }

    /// Mint a short-lived viewer session token (plaintext returned once).
    pub async fn mint_viewer_token(&self, now: DateTime<Utc>) -> String {
        let token = format!("rl_vt_{}", random_token_body());
        let record = ViewerTokenRecord {
            expires_at: now + VIEWER_TOKEN_TTL,
        };
        let mut g = self.inner.lock().await;
        g.viewer_tokens.insert(hash_token(&token), record);
        token
    }

    /// Validate a viewer session token; returns true if live and unexpired.
    pub async fn validate_viewer_token(&self, token: &str, now: DateTime<Utc>) -> bool {
        if token.is_empty() {
            return false;
        }
        let h = hash_token(token);
        let g = self.inner.lock().await;
        matches!(g.viewer_tokens.get(&h), Some(r) if r.expires_at >= now)
    }

    /// Reap sessions past `expires_at`; releases busy locks and notifies peers.
    pub async fn reap_expired(&self, now: DateTime<Utc>) -> usize {
        let mut g = self.inner.lock().await;
        Self::reap_expired_locked(&mut g, now)
    }

    fn reap_expired_locked(g: &mut RegistryInner, now: DateTime<Utc>) -> usize {
        let expired: Vec<String> = g
            .sessions
            .iter()
            .filter_map(|(sid, s)| {
                if s.state != SessionState::Closed && s.expires_at <= now {
                    Some(sid.clone())
                } else {
                    None
                }
            })
            .collect();
        let n = expired.len();
        for sid in expired {
            Self::close_session_locked(g, &sid, "session_ttl");
        }
        n
    }

    /// Close a session, release busy, notify both peers with `session_end`.
    fn close_session_locked(g: &mut RegistryInner, session_id: &str, reason: &str) {
        let Some(session) = g.sessions.remove(session_id) else {
            return;
        };
        g.host_busy.remove(&session.host_public_id);
        if session.state == SessionState::Closed {
            return;
        }
        let seq = session.next_signal_seq;
        let msg = SignalMessage::SessionEnd {
            session_id: session_id.to_string(),
            signal_seq: seq,
            reason: reason.into(),
        };
        if let Some(tx) = g.conns.get(&session.host_conn) {
            let _ = tx.send(msg.clone());
        }
        if let Some(tx) = g.conns.get(&session.viewer_conn) {
            let _ = tx.send(msg);
        }
    }

    /// Create a pending session and notify the host, or return a terminal error message
    /// for the viewer (Error / SessionReject busy).
    pub async fn create_pending_session(
        &self,
        req: CreatePendingSession,
    ) -> Result<(), SignalMessage> {
        let CreatePendingSession {
            viewer_conn,
            session_id,
            host_public_id,
            host_device,
            mode,
            signal_seq,
            viewer_info,
        } = req;

        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        if g.sessions.contains_key(&session_id) {
            return Err(error_msg(
                "conflict",
                format!("session_id {session_id} already exists"),
            ));
        }

        if g.host_busy.contains_key(&host_public_id) {
            return Err(SignalMessage::SessionReject {
                session_id,
                signal_seq: signal_seq.saturating_add(1),
                reason: remotelink_protocol::RejectReason::Busy,
            });
        }

        let Some(&host_conn) = g.host_by_public_id.get(&host_public_id) else {
            return Err(error_msg(
                "host_offline",
                format!("host {host_public_id} is not connected"),
            ));
        };

        // Incoming uses the next monotonic seq after the intent.
        let incoming_seq = match signal_seq.checked_add(1) {
            Some(s) => s,
            None => {
                return Err(error_msg(
                    "bad_request",
                    "signal_seq overflow; start a new session",
                ));
            }
        };
        let next_signal_seq = match incoming_seq.checked_add(1) {
            Some(s) => s,
            None => {
                return Err(error_msg(
                    "bad_request",
                    "signal_seq overflow; start a new session",
                ));
            }
        };

        let session = Session {
            session_id: session_id.clone(),
            host_public_id: host_public_id.clone(),
            host_device_id: host_device.id,
            host_conn,
            viewer_conn,
            mode,
            state: SessionState::Pending,
            next_signal_seq,
            created_at: now,
            expires_at: now + self.pending_ttl,
        };

        g.host_busy.insert(host_public_id, session_id.clone());
        g.sessions.insert(session_id.clone(), session);

        let incoming = SignalMessage::SessionIncoming {
            session_id: session_id.clone(),
            signal_seq: incoming_seq,
            viewer_info,
            mode,
            otp_prefilter: remotelink_protocol::OtpPrefilterStatus::Skipped,
        };

        if let Some(tx) = g.conns.get(&host_conn) {
            if tx.send(incoming).is_ok() {
                return Ok(());
            }
        }

        // Rollback if we could not deliver.
        if let Some(s) = g.sessions.remove(&session_id) {
            g.host_busy.remove(&s.host_public_id);
        }
        Err(error_msg(
            "host_offline",
            "host disconnected before session_incoming",
        ))
    }

    /// Host accept: mark active and forward to viewer.
    ///
    /// **signal_seq policy (strict):** inbound `signal_seq` must be ≥
    /// `session.next_signal_seq`; otherwise `stale_signal_seq`. On success
    /// `next_signal_seq = signal_seq + 1` and the client seq is forwarded.
    pub async fn accept_session(
        &self,
        host_conn: ConnId,
        session_id: &str,
        signal_seq: u64,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        let Some(session) = g.sessions.get_mut(session_id) else {
            return Err(error_msg("not_found", "unknown session_id"));
        };
        if session.host_conn != host_conn {
            return Err(error_msg("unauthorized", "not the host for this session"));
        }
        if session.state != SessionState::Pending {
            return Err(error_msg(
                "invalid_state",
                format!("session is {:?}, expected pending", session.state),
            ));
        }
        if signal_seq < session.next_signal_seq {
            return Err(error_msg(
                "stale_signal_seq",
                format!("signal_seq {signal_seq} < next {}", session.next_signal_seq),
            ));
        }
        session.state = SessionState::Active;
        session.expires_at = now + self.active_ttl;
        session.next_signal_seq = signal_seq.saturating_add(1);
        let viewer = session.viewer_conn;
        let msg = SignalMessage::SessionAccept {
            session_id: session_id.to_string(),
            signal_seq,
        };
        if let Some(tx) = g.conns.get(&viewer) {
            let _ = tx.send(msg);
        }
        Ok(())
    }

    /// Host reject: close session, release busy lock, forward to viewer.
    /// Strict `signal_seq` (see [`Self::accept_session`]).
    pub async fn reject_session(
        &self,
        host_conn: ConnId,
        session_id: &str,
        signal_seq: u64,
        reason: remotelink_protocol::RejectReason,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        let Some(session) = g.sessions.get_mut(session_id) else {
            return Err(error_msg("not_found", "unknown session_id"));
        };
        if session.host_conn != host_conn {
            return Err(error_msg("unauthorized", "not the host for this session"));
        }
        if session.state != SessionState::Pending {
            return Err(error_msg(
                "invalid_state",
                format!("session is {:?}, expected pending", session.state),
            ));
        }
        if signal_seq < session.next_signal_seq {
            return Err(error_msg(
                "stale_signal_seq",
                format!("signal_seq {signal_seq} < next {}", session.next_signal_seq),
            ));
        }
        let host_public_id = session.host_public_id.clone();
        let viewer = session.viewer_conn;
        g.host_busy.remove(&host_public_id);
        g.sessions.remove(session_id);

        let msg = SignalMessage::SessionReject {
            session_id: session_id.to_string(),
            signal_seq,
            reason,
        };
        if let Some(tx) = g.conns.get(&viewer) {
            let _ = tx.send(msg);
        }
        Ok(())
    }

    /// Either peer ends a pending/active session: release busy lock and notify the other side.
    /// Strict `signal_seq` (see [`Self::accept_session`]).
    pub async fn end_session(
        &self,
        conn: ConnId,
        session_id: &str,
        signal_seq: u64,
        reason: String,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        let Some(session) = g.sessions.get(session_id).cloned() else {
            return Err(error_msg("not_found", "unknown session_id"));
        };
        if session.host_conn != conn && session.viewer_conn != conn {
            return Err(error_msg("unauthorized", "not a party to this session"));
        }
        if session.state == SessionState::Closed {
            return Ok(());
        }
        if signal_seq < session.next_signal_seq {
            return Err(error_msg(
                "stale_signal_seq",
                format!("signal_seq {signal_seq} < next {}", session.next_signal_seq),
            ));
        }
        g.host_busy.remove(&session.host_public_id);
        g.sessions.remove(session_id);

        let peer = if session.host_conn == conn {
            session.viewer_conn
        } else {
            session.host_conn
        };
        let msg = SignalMessage::SessionEnd {
            session_id: session_id.to_string(),
            signal_seq,
            reason,
        };
        if let Some(tx) = g.conns.get(&peer) {
            let _ = tx.send(msg);
        }
        Ok(())
    }

    /// Operator force-disconnect: close session and notify **both** peers.
    ///
    /// Used by `POST /v1/admin/sessions/{id}/force-disconnect`. Returns `Ok(true)`
    /// when a live (non-closed) session was closed, `Ok(false)` if unknown.
    pub async fn force_disconnect(&self, session_id: &str, reason: &str) -> Result<bool, String> {
        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        let Some(session) = g.sessions.get(session_id) else {
            return Ok(false);
        };
        if session.state == SessionState::Closed {
            // Treat terminal row as gone for admin API.
            g.sessions.remove(session_id);
            return Ok(false);
        }
        Self::close_session_locked(&mut g, session_id, reason);
        Ok(true)
    }

    /// Test/helper: current busy session for a host, if any (after reap).
    pub async fn busy_session_for_host(&self, host_public_id: &str) -> Option<String> {
        let mut g = self.inner.lock().await;
        Self::reap_expired_locked(&mut g, Utc::now());
        g.host_busy.get(host_public_id).cloned()
    }

    /// Test/helper: session state (after reap).
    pub async fn session_state(&self, session_id: &str) -> Option<SessionState> {
        let mut g = self.inner.lock().await;
        Self::reap_expired_locked(&mut g, Utc::now());
        g.sessions.get(session_id).map(|s| s.state)
    }

    /// Test/helper: force a session's `expires_at` into the past, then reap.
    pub async fn force_expire_session(&self, session_id: &str) -> bool {
        let mut g = self.inner.lock().await;
        let Some(session) = g.sessions.get_mut(session_id) else {
            return false;
        };
        session.expires_at = Utc::now() - Duration::seconds(1);
        Self::reap_expired_locked(&mut g, Utc::now());
        true
    }

    /// Test/helper: next expected signal_seq for a session.
    pub async fn next_signal_seq(&self, session_id: &str) -> Option<u64> {
        let g = self.inner.lock().await;
        g.sessions.get(session_id).map(|s| s.next_signal_seq)
    }

    /// Relay an **active-session** media/control signal (SDP offer/answer, ICE,
    /// auth challenge/response, media restart, renegotiate, stats) to the peer.
    ///
    /// # Rules
    ///
    /// - Session must be [`SessionState::Active`].
    /// - Sender must be host or viewer for the session.
    /// - Role checks: `session_offer` host-only; `session_answer` viewer-only;
    ///   ICE / restart / renegotiate / auth / stats: either party.
    /// - Strict `signal_seq` (same as [`Self::accept_session`]).
    /// - Message is forwarded **verbatim** (opaque SRTP / SDP; server does not
    ///   parse media). Payload size limits are enforced by protocol decode.
    pub async fn relay_media_signal(
        &self,
        from: ConnId,
        msg: SignalMessage,
    ) -> Result<(), SignalMessage> {
        let (session_id, signal_seq) = match &msg {
            SignalMessage::SessionOffer {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::SessionAnswer {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::IceCandidate {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::MediaRestart {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::Renegotiate {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::AuthChallenge {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::AuthResponse {
                session_id,
                signal_seq,
                ..
            }
            | SignalMessage::Stats {
                session_id,
                signal_seq,
                ..
            } => (session_id.clone(), *signal_seq),
            other => {
                return Err(error_msg(
                    "protocol_error",
                    format!(
                        "relay_media_signal: unsupported type (session_id={:?})",
                        other.session_id()
                    ),
                ));
            }
        };

        let mut g = self.inner.lock().await;
        let now = Utc::now();
        Self::reap_expired_locked(&mut g, now);

        let Some(session) = g.sessions.get_mut(&session_id) else {
            return Err(error_msg("not_found", "unknown session_id"));
        };
        if session.host_conn != from && session.viewer_conn != from {
            return Err(error_msg("unauthorized", "not a party to this session"));
        }
        if session.state != SessionState::Active {
            return Err(error_msg(
                "invalid_state",
                format!(
                    "session is {:?}, expected active for media relay",
                    session.state
                ),
            ));
        }
        if signal_seq < session.next_signal_seq {
            // Host and viewer each allocate seqs independently; ICE often
            // collides. Protocol: ignore stale sequences. Drop ICE; reject
            // offer/answer so handshake cannot go backwards.
            if matches!(msg, SignalMessage::IceCandidate { .. }) {
                return Ok(());
            }
            return Err(error_msg(
                "stale_signal_seq",
                format!("signal_seq {signal_seq} < next {}", session.next_signal_seq),
            ));
        }

        // Role gates for offer/answer (host = offerer, viewer = answerer).
        let is_host = session.host_conn == from;
        match &msg {
            SignalMessage::SessionOffer { .. } if !is_host => {
                return Err(error_msg(
                    "unauthorized",
                    "only the host may send session_offer",
                ));
            }
            SignalMessage::SessionAnswer { .. } if is_host => {
                return Err(error_msg(
                    "unauthorized",
                    "only the viewer may send session_answer",
                ));
            }
            _ => {}
        }

        let peer = if is_host {
            session.viewer_conn
        } else {
            session.host_conn
        };
        session.next_signal_seq = signal_seq.saturating_add(1);

        if let Some(tx) = g.conns.get(&peer) {
            if tx.send(msg).is_err() {
                return Err(error_msg(
                    "peer_offline",
                    "peer disconnected; could not relay media signal",
                ));
            }
        } else {
            return Err(error_msg(
                "peer_offline",
                "peer not connected; could not relay media signal",
            ));
        }
        Ok(())
    }
}

fn error_msg(code: &str, message: impl Into<String>) -> SignalMessage {
    SignalMessage::Error {
        code: code.into(),
        message: message.into(),
    }
}

/// Parameters for [`SessionRegistry::create_pending_session`].
pub struct CreatePendingSession {
    pub viewer_conn: ConnId,
    pub session_id: String,
    pub host_public_id: String,
    pub host_device: Device,
    pub mode: SessionMode,
    pub signal_seq: u64,
    pub viewer_info: Value,
}

/// Default viewer_info payload when the client does not supply richer data.
pub fn default_viewer_info(peer: &PeerIdentity) -> Value {
    json!({
        "anonymous": peer.anonymous,
        "device_public_id": peer.device_public_id,
    })
}

/// Shared handle stored on [`crate::state::AppState`].
pub type SharedSessionRegistry = Arc<SessionRegistry>;

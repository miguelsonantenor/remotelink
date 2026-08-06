//! In-memory session registry and WebSocket presence (Redis later).
//!
//! Tracks:
//! - Connected hosts/viewers (connection id → outbound channel)
//! - Host presence by `public_id` (single live WS per host)
//! - Pending/active sessions with a **single-session busy lock** per host
//! - Short-lived viewer session tokens (until `POST /v1/sessions`)

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

/// Session lifecycle state for PR 5a (SDP/ICE relay is PR 5b).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Viewer intent delivered; waiting for host accept/reject.
    Pending,
    /// Host accepted; media signaling may follow (PR 5b).
    Active,
    /// Terminal — rejected or ended; lock released.
    Closed,
}

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
    /// Monotonic counter; next value to assign on server-originated messages.
    pub next_signal_seq: u64,
    pub created_at: DateTime<Utc>,
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
#[derive(Debug, Default)]
pub struct SessionRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    /// Live connections and their send handles.
    conns: HashMap<ConnId, ConnTx>,
    /// Host public_id → connection (last hello wins).
    host_by_public_id: HashMap<String, ConnId>,
    /// Connection → peer identity after successful hello.
    peers: HashMap<ConnId, PeerIdentity>,
    /// session_id → session.
    sessions: HashMap<String, Session>,
    /// Host public_id → session_id while busy (pending or active).
    host_busy: HashMap<String, String>,
    /// Viewer access-token hash → expiry.
    viewer_tokens: HashMap<String, ViewerTokenRecord>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
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

    /// Drop a connection and release any sessions it owned.
    pub async fn unregister_conn(&self, conn: ConnId) {
        let mut g = self.inner.lock().await;
        g.conns.remove(&conn);
        g.peers.remove(&conn);

        // Remove host presence if this conn was the live host socket.
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

        // Close sessions involving this connection; notify peer when possible.
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
            let Some(mut session) = g.sessions.remove(&sid) else {
                continue;
            };
            g.host_busy.remove(&session.host_public_id);
            if session.state == SessionState::Closed {
                continue;
            }
            session.state = SessionState::Closed;
            let peer = if session.host_conn == conn {
                session.viewer_conn
            } else {
                session.host_conn
            };
            let seq = session.next_signal_seq;
            session.next_signal_seq = seq.saturating_add(1);
            // Keep closed record briefly? drop for memory simplicity.
            let _ = session;
            if let Some(tx) = g.conns.get(&peer) {
                let _ = tx.send(SignalMessage::SessionEnd {
                    session_id: sid,
                    signal_seq: seq,
                    reason: "peer_disconnected".into(),
                });
            }
        }
    }

    /// Bind identity after a successful `hello`. Hosts are published for presence.
    pub async fn bind_peer(&self, conn: ConnId, identity: PeerIdentity) {
        let mut g = self.inner.lock().await;
        if identity.role == Role::Host {
            if let Some(ref public_id) = identity.device_public_id {
                // Last connection wins for presence.
                g.host_by_public_id.insert(public_id.clone(), conn);
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
        let incoming_seq = signal_seq.saturating_add(1);
        let session = Session {
            session_id: session_id.clone(),
            host_public_id: host_public_id.clone(),
            host_device_id: host_device.id,
            host_conn,
            viewer_conn,
            mode,
            state: SessionState::Pending,
            next_signal_seq: incoming_seq.saturating_add(1),
            created_at: Utc::now(),
        };

        g.host_busy.insert(host_public_id, session_id.clone());
        g.sessions.insert(session_id.clone(), session);

        let incoming = SignalMessage::SessionIncoming {
            session_id: session_id.clone(),
            signal_seq: incoming_seq,
            viewer_info,
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
    pub async fn accept_session(
        &self,
        host_conn: ConnId,
        session_id: &str,
        signal_seq: u64,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
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
        session.state = SessionState::Active;
        if signal_seq >= session.next_signal_seq {
            session.next_signal_seq = signal_seq.saturating_add(1);
        }
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
    pub async fn reject_session(
        &self,
        host_conn: ConnId,
        session_id: &str,
        signal_seq: u64,
        reason: remotelink_protocol::RejectReason,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
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
        session.state = SessionState::Closed;
        let host_public_id = session.host_public_id.clone();
        let viewer = session.viewer_conn;
        g.host_busy.remove(&host_public_id);
        // Drop closed session entry.
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
    pub async fn end_session(
        &self,
        conn: ConnId,
        session_id: &str,
        signal_seq: u64,
        reason: String,
    ) -> Result<(), SignalMessage> {
        let mut g = self.inner.lock().await;
        let Some(session) = g.sessions.get(session_id).cloned() else {
            return Err(error_msg("not_found", "unknown session_id"));
        };
        if session.host_conn != conn && session.viewer_conn != conn {
            return Err(error_msg("unauthorized", "not a party to this session"));
        }
        if session.state == SessionState::Closed {
            return Ok(());
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

    /// Test/helper: current busy session for a host, if any.
    pub async fn busy_session_for_host(&self, host_public_id: &str) -> Option<String> {
        let g = self.inner.lock().await;
        g.host_busy.get(host_public_id).cloned()
    }

    /// Test/helper: session state.
    pub async fn session_state(&self, session_id: &str) -> Option<SessionState> {
        let g = self.inner.lock().await;
        g.sessions.get(session_id).map(|s| s.state)
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

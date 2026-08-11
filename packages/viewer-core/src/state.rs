//! Viewer connection state machine (toolkit-agnostic).

use remotelink_net::ConnectionState;

/// High-level viewer session phase for UI and headless drivers.
///
/// Transitions (happy path):
/// `Idle` → `Connecting` → `Answering` → `Connected` → `Streaming` → `Disconnected`
/// Failures go to `Failed`; user hangup / close → `Disconnected` / `Closed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerPhase {
    /// No session; ready for connect credentials.
    Idle,
    /// Credentials accepted; signaling / transport setup in progress.
    Connecting,
    /// Remote offer applied; local answer being prepared or applied.
    Answering,
    /// Peer connection established (ICE+DTLS or mock connected).
    Connected,
    /// Receiving media (at least one video or audio unit recorded).
    Streaming,
    /// Transient or permanent failure with a human-readable reason.
    Failed(String),
    /// Peer disconnected (may recover via ICE restart in later PRs).
    Disconnected,
    /// Session closed by the application.
    Closed,
}

impl ViewerPhase {
    /// Stable wire/debug label.
    pub fn as_str(&self) -> &str {
        match self {
            ViewerPhase::Idle => "idle",
            ViewerPhase::Connecting => "connecting",
            ViewerPhase::Answering => "answering",
            ViewerPhase::Connected => "connected",
            ViewerPhase::Streaming => "streaming",
            ViewerPhase::Failed(_) => "failed",
            ViewerPhase::Disconnected => "disconnected",
            ViewerPhase::Closed => "closed",
        }
    }

    /// True when the UI should treat the session as actively linked.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ViewerPhase::Connecting
                | ViewerPhase::Answering
                | ViewerPhase::Connected
                | ViewerPhase::Streaming
        )
    }

    /// True when media playout / render paths may run.
    pub fn can_play_media(&self) -> bool {
        matches!(self, ViewerPhase::Connected | ViewerPhase::Streaming)
    }

    /// True when input events may be emitted toward the host.
    pub fn can_send_input(&self) -> bool {
        matches!(self, ViewerPhase::Connected | ViewerPhase::Streaming)
    }
}

/// Connection state machine driven by transport + app events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionMachine {
    phase: ViewerPhase,
    /// Last transport-level connection state observed.
    transport: ConnectionState,
    /// Host public ID for the active / last attempt (for UI).
    host_public_id: Option<String>,
    /// Failure reason when phase is [`ViewerPhase::Failed`].
    last_error: Option<String>,
}

impl Default for ConnectionMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionMachine {
    /// Create an idle machine.
    pub fn new() -> Self {
        Self {
            phase: ViewerPhase::Idle,
            transport: ConnectionState::New,
            host_public_id: None,
            last_error: None,
        }
    }

    /// Current viewer phase.
    pub fn phase(&self) -> &ViewerPhase {
        &self.phase
    }

    /// Last observed [`ConnectionState`] from the peer transport.
    pub fn transport_state(&self) -> ConnectionState {
        self.transport
    }

    /// Host public ID associated with this session attempt.
    pub fn host_public_id(&self) -> Option<&str> {
        self.host_public_id.as_deref()
    }

    /// Last failure message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Begin a connect attempt (credentials validated by the caller).
    pub fn begin_connect(&mut self, host_public_id: impl Into<String>) {
        self.host_public_id = Some(host_public_id.into());
        self.last_error = None;
        self.phase = ViewerPhase::Connecting;
        self.transport = ConnectionState::New;
    }

    /// Mark that a remote offer is being answered.
    ///
    /// Only valid from [`ViewerPhase::Connecting`] or already
    /// [`ViewerPhase::Answering`] (idempotent). Does not accept `Idle`.
    pub fn begin_answer(&mut self) {
        if matches!(self.phase, ViewerPhase::Connecting | ViewerPhase::Answering) {
            self.phase = ViewerPhase::Answering;
        }
    }

    /// Apply a transport connection-state update.
    pub fn on_transport_state(&mut self, state: ConnectionState) {
        self.transport = state;
        match state {
            ConnectionState::New => {}
            ConnectionState::Connecting => {
                if matches!(self.phase, ViewerPhase::Idle | ViewerPhase::Connecting) {
                    // Keep Connecting / Answering; do not downgrade Streaming.
                } else if matches!(self.phase, ViewerPhase::Answering) {
                    // stay answering while ICE runs
                }
            }
            ConnectionState::Connected => {
                if matches!(
                    self.phase,
                    ViewerPhase::Connecting
                        | ViewerPhase::Answering
                        | ViewerPhase::Connected
                        | ViewerPhase::Streaming
                ) && self.phase != ViewerPhase::Streaming
                {
                    self.phase = ViewerPhase::Connected;
                }
            }
            ConnectionState::Disconnected => {
                if self.phase.is_active() || matches!(self.phase, ViewerPhase::Connected) {
                    self.phase = ViewerPhase::Disconnected;
                }
            }
            ConnectionState::Failed => {
                self.fail("transport failed");
            }
            ConnectionState::Closed => {
                self.phase = ViewerPhase::Closed;
            }
        }
    }

    /// First (or subsequent) media unit received while connected.
    ///
    /// Only advances from [`ViewerPhase::Connected`] (or stays on
    /// [`ViewerPhase::Streaming`]). Media observed during `Answering` is
    /// ignored for phase purposes so input is not advertised before Connected.
    pub fn on_media_received(&mut self) {
        if matches!(self.phase, ViewerPhase::Connected | ViewerPhase::Streaming) {
            self.phase = ViewerPhase::Streaming;
        }
    }

    /// Record a hard failure.
    pub fn fail(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.last_error = Some(reason.clone());
        self.phase = ViewerPhase::Failed(reason);
    }

    /// Reset to idle (user disconnect / new connect).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Mark application-initiated close.
    pub fn close(&mut self) {
        self.phase = ViewerPhase::Closed;
        self.transport = ConnectionState::Closed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_idle_to_streaming() {
        let mut m = ConnectionMachine::new();
        assert_eq!(m.phase(), &ViewerPhase::Idle);
        m.begin_connect("host-abc");
        assert_eq!(m.phase(), &ViewerPhase::Connecting);
        m.begin_answer();
        assert_eq!(m.phase(), &ViewerPhase::Answering);
        m.on_transport_state(ConnectionState::Connected);
        assert_eq!(m.phase(), &ViewerPhase::Connected);
        m.on_media_received();
        assert_eq!(m.phase(), &ViewerPhase::Streaming);
        assert!(m.phase().can_send_input());
    }

    #[test]
    fn fail_sets_reason() {
        let mut m = ConnectionMachine::new();
        m.begin_connect("h");
        m.fail("auth rejected");
        assert!(matches!(m.phase(), ViewerPhase::Failed(_)));
        assert_eq!(m.last_error(), Some("auth rejected"));
    }

    #[test]
    fn disconnect_from_connected() {
        let mut m = ConnectionMachine::new();
        m.begin_connect("h");
        m.begin_answer();
        m.on_transport_state(ConnectionState::Connected);
        m.on_transport_state(ConnectionState::Disconnected);
        assert_eq!(m.phase(), &ViewerPhase::Disconnected);
    }

    #[test]
    fn begin_answer_from_idle_is_noop() {
        let mut m = ConnectionMachine::new();
        m.begin_answer();
        assert_eq!(m.phase(), &ViewerPhase::Idle);
        assert!(!m.phase().can_send_input());
    }

    #[test]
    fn media_in_answering_does_not_stream_or_enable_input() {
        let mut m = ConnectionMachine::new();
        m.begin_connect("h");
        m.begin_answer();
        assert_eq!(m.phase(), &ViewerPhase::Answering);
        m.on_media_received();
        assert_eq!(m.phase(), &ViewerPhase::Answering);
        assert!(!m.phase().can_send_input());
        // Only after Connected does media advance to Streaming.
        m.on_transport_state(ConnectionState::Connected);
        assert_eq!(m.phase(), &ViewerPhase::Connected);
        m.on_media_received();
        assert_eq!(m.phase(), &ViewerPhase::Streaming);
        assert!(m.phase().can_send_input());
    }
}

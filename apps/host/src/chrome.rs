//! Mandatory session indicator + tray chrome state machine (DESIGN G9).
//!
//! The host **service** owns [`SessionIndicator`]. It cannot be remote-disabled:
//! only local attach / media-start / detach / kill-switch mutate the flags.
//! Tray chrome is a presentation layer over that indicator
//! ([`SessionChrome`]: `Inactive` | `Active`).

use std::fmt;

/// Service-owned connection visibility flags (G9).
///
/// - `connected`: a control session is bound (attach succeeded).
/// - `active`: the session is live for control/media (indicator shows "in session").
///
/// Neither flag can be cleared by remote IPC; only [`SessionIndicator::end_session`]
/// and [`SessionIndicator::apply_kill`] (local host actions) clear them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionIndicator {
    /// Signaling/session is bound.
    pub connected: bool,
    /// Session is actively controlling (media or control live).
    pub active: bool,
    /// Bound session id while connected.
    pub session_id: Option<String>,
    /// Optional viewer label for tray / chrome.
    pub viewer_label: Option<String>,
}

impl SessionIndicator {
    /// Fresh inactive indicator.
    pub fn new() -> Self {
        Self::default()
    }

    /// True only while a live session is bound and marked active.
    pub fn is_active(&self) -> bool {
        self.active && self.connected && self.session_id.is_some()
    }

    /// True while any session is bound (even if media not yet started).
    pub fn is_connected(&self) -> bool {
        self.connected && self.session_id.is_some()
    }

    /// Local attach: mark connected (not yet active until media/control starts).
    ///
    /// Returns `Err` with the existing session id when a live session is already
    /// bound (single-controller / host-side busy).
    pub fn begin_session(
        &mut self,
        session_id: impl Into<String>,
        viewer_label: Option<String>,
    ) -> Result<(), String> {
        if self.is_connected() {
            return Err(self.session_id.clone().unwrap_or_else(|| "unknown".into()));
        }
        let sid = session_id.into();
        self.connected = true;
        self.active = false;
        self.session_id = Some(sid);
        self.viewer_label = viewer_label;
        Ok(())
    }

    /// Mark the bound session as actively controlling (media/control started).
    ///
    /// No-op when not connected. Cannot be used to *clear* active remotely.
    pub fn mark_active(&mut self) {
        if self.connected && self.session_id.is_some() {
            self.active = true;
        }
    }

    /// Local end of session (detach / shutdown / remote session_end handled locally).
    pub fn end_session(&mut self) {
        *self = Self::default();
    }

    /// Local kill-switch: clear all flags (session ends).
    pub fn apply_kill(&mut self) {
        self.end_session();
    }
}

/// Tray / on-desktop chrome presentation state (stub — printed/loggable).
///
/// Derived from [`SessionIndicator`]: active only when the service indicator
/// reports a live session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionChrome {
    /// No session chrome (tray idle).
    #[default]
    Inactive,
    /// Mandatory connection chrome while host is controlled.
    Active {
        /// Bound session id.
        session_id: String,
        /// Optional viewer label.
        label: Option<String>,
    },
}

impl SessionChrome {
    /// Project service indicator into tray chrome.
    ///
    /// Chrome is **Active** only when the indicator reports a live active session.
    /// This path has no remote-disable knob.
    pub fn from_indicator(ind: &SessionIndicator) -> Self {
        if ind.is_active() {
            if let Some(ref sid) = ind.session_id {
                return Self::Active {
                    session_id: sid.clone(),
                    label: ind.viewer_label.clone(),
                };
            }
        }
        Self::Inactive
    }

    /// Whether chrome should be drawn / tray shows "in session".
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// Stable status string for logs / CLI demos.
    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Inactive => "Inactive",
            Self::Active { .. } => "Active",
        }
    }
}

impl fmt::Display for SessionChrome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inactive => write!(f, "SessionChrome::Inactive"),
            Self::Active { session_id, label } => match label {
                Some(l) => write!(f, "SessionChrome::Active(session={session_id}, label={l})"),
                None => write!(f, "SessionChrome::Active(session={session_id})"),
            },
        }
    }
}

/// Service-side coordinator: mandatory indicator + chrome + single-session guard.
///
/// All mutations are **local** (service process). Remote viewers cannot clear
/// the indicator.
#[derive(Debug, Clone, Default)]
pub struct HostSessionUx {
    indicator: SessionIndicator,
}

impl HostSessionUx {
    /// Create an idle coordinator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the service-owned indicator.
    pub fn indicator(&self) -> &SessionIndicator {
        &self.indicator
    }

    /// Current tray chrome projection.
    pub fn chrome(&self) -> SessionChrome {
        SessionChrome::from_indicator(&self.indicator)
    }

    /// Attempt to bind a new session. Fails with existing session id if busy.
    pub fn begin_session(
        &mut self,
        session_id: impl Into<String>,
        viewer_label: Option<String>,
    ) -> Result<(), String> {
        self.indicator.begin_session(session_id, viewer_label)
    }

    /// Mark session active (e.g. after StartMedia ack).
    pub fn mark_active(&mut self) {
        self.indicator.mark_active();
    }

    /// End session (detach / shutdown).
    pub fn end_session(&mut self) {
        self.indicator.end_session();
    }

    /// Apply local kill-switch to indicator (session ends; chrome → Inactive).
    pub fn apply_kill(&mut self) {
        self.indicator.apply_kill();
    }

    /// Loggable one-line status for tray stub / CLI.
    pub fn status_line(&self) -> String {
        let chrome = self.chrome();
        let ind = &self.indicator;
        format!(
            "session_indicator connected={} active={} chrome={} session_id={}",
            ind.connected,
            ind.active,
            chrome.status_label(),
            ind.session_id.as_deref().unwrap_or("-")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_active_only_when_session_live() {
        let mut ind = SessionIndicator::new();
        assert!(!ind.is_active());
        assert!(!ind.is_connected());
        assert_eq!(SessionChrome::from_indicator(&ind), SessionChrome::Inactive);

        ind.begin_session("s1", Some("viewer-a".into())).unwrap();
        assert!(ind.is_connected());
        assert!(!ind.is_active(), "connected but not yet active");
        assert_eq!(SessionChrome::from_indicator(&ind), SessionChrome::Inactive);

        ind.mark_active();
        assert!(ind.is_active());
        assert_eq!(
            SessionChrome::from_indicator(&ind),
            SessionChrome::Active {
                session_id: "s1".into(),
                label: Some("viewer-a".into()),
            }
        );

        ind.apply_kill();
        assert!(!ind.is_active());
        assert!(!ind.is_connected());
        assert_eq!(SessionChrome::from_indicator(&ind), SessionChrome::Inactive);
    }

    #[test]
    fn second_begin_rejected_while_connected() {
        let mut ind = SessionIndicator::new();
        ind.begin_session("s1", None).unwrap();
        let err = ind.begin_session("s2", None).unwrap_err();
        assert_eq!(err, "s1");
        assert_eq!(ind.session_id.as_deref(), Some("s1"));
    }

    #[test]
    fn mark_active_no_op_when_idle() {
        let mut ind = SessionIndicator::new();
        ind.mark_active();
        assert!(!ind.is_active());
        assert!(!ind.connected);
    }

    #[test]
    fn host_ux_status_and_kill() {
        let mut ux = HostSessionUx::new();
        assert!(!ux.chrome().is_active());
        ux.begin_session("sess", None).unwrap();
        ux.mark_active();
        assert!(ux.indicator().is_active());
        assert!(ux.chrome().is_active());
        let line = ux.status_line();
        assert!(line.contains("active=true"), "{line}");
        assert!(line.contains("chrome=Active"), "{line}");

        ux.apply_kill();
        assert!(!ux.indicator().is_active());
        assert!(!ux.chrome().is_active());
        let line = ux.status_line();
        assert!(line.contains("active=false"), "{line}");
        assert!(line.contains("chrome=Inactive"), "{line}");
    }

    #[test]
    fn chrome_display_format() {
        let inactive = SessionChrome::Inactive;
        assert_eq!(inactive.to_string(), "SessionChrome::Inactive");
        let active = SessionChrome::Active {
            session_id: "s".into(),
            label: Some("v".into()),
        };
        assert!(active.to_string().contains("Active"));
        assert!(active.to_string().contains("session=s"));
    }
}

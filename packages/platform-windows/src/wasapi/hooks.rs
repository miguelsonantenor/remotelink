//! Device-change and exclusive-mode warning hooks for loopback capture.

use std::fmt;
use std::sync::{Arc, Mutex};

/// Why a loopback device change was reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceChangeReason {
    /// Default render device changed (user switched output).
    DefaultDeviceChanged,
    /// Endpoint added.
    DeviceAdded,
    /// Endpoint removed / unplugged.
    DeviceRemoved,
    /// Format or state changed on the active endpoint.
    DeviceStateChanged,
    /// Test / synthetic injection.
    Synthetic,
}

impl fmt::Display for DeviceChangeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultDeviceChanged => write!(f, "default_device_changed"),
            Self::DeviceAdded => write!(f, "device_added"),
            Self::DeviceRemoved => write!(f, "device_removed"),
            Self::DeviceStateChanged => write!(f, "device_state_changed"),
            Self::Synthetic => write!(f, "synthetic"),
        }
    }
}

/// Exclusive-mode silence warning payload (tray + viewer banner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExclusiveModeWarning {
    /// How long near-zero energy was observed (milliseconds).
    pub sustained_silence_ms: u64,
    /// Optional human-readable hint.
    pub message: String,
}

/// Events the capture path may surface to the session agent / tray.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackEvent {
    /// Render endpoint graph changed; agent should `media_restart`.
    DeviceChanged {
        /// Why the change was raised.
        reason: DeviceChangeReason,
    },
    /// Loopback appears silent under likely exclusive-mode use.
    ExclusiveMode {
        /// Warning details.
        warning: ExclusiveModeWarning,
    },
    /// Opening the capture client failed (do not crash the session).
    ClientOpenFailed {
        /// Error text for logs / UI.
        message: String,
    },
}

/// Callbacks for loopback lifecycle events.
///
/// Production: tray warning + `media_restart` signaling. Tests: record events.
pub trait LoopbackHooks: Send {
    /// Deliver a loopback event.
    fn on_event(&mut self, event: LoopbackEvent);
}

/// No-op hooks (default).
#[derive(Debug, Default, Clone, Copy)]
pub struct NullHooks;

impl LoopbackHooks for NullHooks {
    fn on_event(&mut self, _event: LoopbackEvent) {}
}

/// Thread-safe event recorder for unit tests.
#[derive(Clone, Default)]
pub struct RecordingHooks {
    events: Arc<Mutex<Vec<LoopbackEvent>>>,
}

impl RecordingHooks {
    /// Create empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of recorded events.
    pub fn events(&self) -> Vec<LoopbackEvent> {
        self.events
            .lock()
            .expect("recording hooks mutex poisoned")
            .clone()
    }

    /// Shared handle usable from multiple owners.
    pub fn shared_sink(&self) -> Arc<Mutex<Vec<LoopbackEvent>>> {
        Arc::clone(&self.events)
    }
}

impl LoopbackHooks for RecordingHooks {
    fn on_event(&mut self, event: LoopbackEvent) {
        self.events
            .lock()
            .expect("recording hooks mutex poisoned")
            .push(event);
    }
}

/// `LoopbackHooks` for a shared event log (agent / multi-owner).
pub struct SharedHooks {
    events: Arc<Mutex<Vec<LoopbackEvent>>>,
}

impl SharedHooks {
    /// Wrap an existing event log.
    pub fn new(events: Arc<Mutex<Vec<LoopbackEvent>>>) -> Self {
        Self { events }
    }
}

impl LoopbackHooks for SharedHooks {
    fn on_event(&mut self, event: LoopbackEvent) {
        self.events
            .lock()
            .expect("shared hooks mutex poisoned")
            .push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_hooks_collect_events() {
        let mut h = RecordingHooks::new();
        h.on_event(LoopbackEvent::DeviceChanged {
            reason: DeviceChangeReason::DefaultDeviceChanged,
        });
        h.on_event(LoopbackEvent::ExclusiveMode {
            warning: ExclusiveModeWarning {
                sustained_silence_ms: 500,
                message: "test".into(),
            },
        });
        assert_eq!(h.events().len(), 2);
    }
}

//! System-audio **monitor** capture for the Linux session agent.
//!
//! Captures what the host would hear on a selected sink (speakers/headphones),
//! analogous to WASAPI loopback on Windows. Microphone capture is out of scope.
//!
//! # Backends
//!
//! - [`MockMonitorSource`]: deterministic PCM tone for CI / units (all OS).
//! - [`NativePipeWireMonitor`]: PipeWire preferred, Pulse monitor fallback —
//!   skeleton until native libs are linked.
//!
//! Prefer [`open_monitor`] which selects by [`MonitorOpenMode`].

mod mock;
mod monitor;
mod pipewire;

pub use mock::MockMonitorSource;
pub use monitor::{
    open_monitor, open_monitor_with_name, AnyMonitor, MonitorConfig, MonitorError, MonitorOpenMode,
    MonitorSource, DEFAULT_CHANNELS, DEFAULT_PACKET_MS, DEFAULT_SAMPLE_RATE,
};
pub use pipewire::NativePipeWireMonitor;

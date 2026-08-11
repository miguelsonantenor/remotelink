//! Linux host platform support for RemoteLink (secondary platform).
//!
//! # Scope
//!
//! - **Screen capture** via PipeWire (portal / screencast session) — skeleton + mock.
//! - **System audio** via PipeWire / Pulse **monitor** source — skeleton + mock.
//!
//! Windows remains the primary closed-beta host. This crate is always built so
//! unit tests and Windows CI can exercise the mock path without libpipewire.
//!
//! # cfg policy
//!
//! | Backend | Non-Linux | Linux |
//! |---------|-----------|-------|
//! | Mock / stub | synthetic frames | synthetic frames |
//! | Platform native | `Unsupported` / `NativeUnavailable` | skeleton (same until linked) |
//!
//! See package `README.md` for secondary-platform status.

#![deny(missing_docs)]

pub mod audio;
pub mod capture;

pub use audio::{
    open_monitor, open_monitor_with_name, AnyMonitor, MockMonitorSource, MonitorConfig,
    MonitorError, MonitorOpenMode, MonitorSource, NativePipeWireMonitor, DEFAULT_CHANNELS,
    DEFAULT_PACKET_MS, DEFAULT_SAMPLE_RATE,
};
pub use capture::{
    host_mono_now, open_capture, pump_frame, CaptureBackend, CaptureConfig, CaptureError,
    CollectingFrameSink, DisplayCapture, FrameSink, MockVideoSource, NativePipeWireCapture,
    PumpError,
};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable platform label for logs / stats.
pub const PLATFORM_LABEL: &str = "linux";

/// Whether this build targets Linux OS (compile-time).
pub const fn is_linux_target() -> bool {
    cfg!(target_os = "linux")
}

/// Whether a real PipeWire-linked capture path is available in this build.
///
/// Always `false` until `libpipewire` bindings are wired; mocks remain the
/// supported path for CI on every OS.
pub fn native_capture_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        NativePipeWireCapture::is_available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Whether a real PipeWire/Pulse monitor path is available in this build.
pub fn native_monitor_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        NativePipeWireMonitor::is_available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_nonempty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn platform_label() {
        assert_eq!(PLATFORM_LABEL, "linux");
    }

    #[test]
    fn native_flags_false_until_linked() {
        assert!(!native_capture_available());
        assert!(!native_monitor_available());
    }

    #[test]
    fn is_linux_target_matches_cfg() {
        assert_eq!(is_linux_target(), cfg!(target_os = "linux"));
    }
}

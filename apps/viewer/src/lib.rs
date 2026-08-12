//! RemoteLink viewer library — WSS connect path shared by CLI and desktop shell.

#![deny(missing_docs)]

pub mod live;
pub mod ws_connect;

pub use live::{named_key_from_name, LiveViewerHandle, LiveViewerSnapshot};
pub use remotelink_viewer_core::{RawInput, ViewerPhase};
pub use remotelink_protocol::MouseButtonKind;
pub use ws_connect::{run_ws_viewer, run_ws_viewer_blocking, WsViewerConfig};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

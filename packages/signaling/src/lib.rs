//! RemoteLink client helpers for HTTP enrollment and WSS signaling.
//!
//! Used by host/viewer CLIs and e2e tests. Does **not** own PeerTransport media.

#![deny(missing_docs)]

mod client;
mod register;

pub use client::{SignalingClient, SignalingError, SignalingResult};
pub use register::{register_device, http_to_ws_url, DeviceRegistration, RegisterError};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

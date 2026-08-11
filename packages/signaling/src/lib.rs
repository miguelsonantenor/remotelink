//! RemoteLink client helpers for HTTP enrollment and WSS signaling.
//!
//! Used by host/viewer CLIs and e2e tests. Does **not** own PeerTransport media.

#![deny(missing_docs)]

mod client;
mod creds;
mod http_api;
mod register;

pub use client::{SignalingClient, SignalingError, SignalingResult};
pub use creds::{HostCredentialFile, DEFAULT_CREDS_PATH};
pub use http_api::{
    apply_refresh, post_otp_hash, refresh_device_token, OtpMintHttpResponse, TokenHttpResponse,
};
pub use register::{http_to_ws_url, register_device, DeviceRegistration, RegisterError};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

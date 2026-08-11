//! Transport factory: select mock / live / webrtc backends from env / CLI.
//!
//! # Defaults (CI-safe)
//!
//! - Env `REMOTELINK_TRANSPORT` defaults to **`mock`** when unset.
//! - Values: `mock` | `live` | `webrtc` | `auto`.
//! - `auto` prefers **webrtc** when feature `webrtc-rs` is enabled, else **live**
//!   when feature `live` is enabled, else **mock**.
//!
//! Real WebRTC (DTLS-SRTP / ICE) is selected with `REMOTELINK_TRANSPORT=webrtc`
//! when built with `--features webrtc-rs`. See `docs/spike-webrtc.md`.

use std::env;
use std::str::FromStr;

use crate::error::{NetError, Result};
#[cfg(feature = "live")]
use crate::live_loopback::{LivePeerConfig, LivePeerTransport};
#[cfg(feature = "mock")]
use crate::mock::{MockPeerConfig, MockPeerTransport};
use crate::transport::BoxPeerTransport;
#[cfg(feature = "webrtc-rs")]
use crate::webrtc_rs::{WebrtcPeerConfig, WebrtcPeerTransport};

/// Which side of the peer connection this process owns.
///
/// Host / session agent is the **offerer**; viewer is the **answerer**
/// (DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Creates the SDP offer (host session agent).
    Offerer,
    /// Creates the SDP answer (viewer).
    Answerer,
}

impl PeerRole {
    /// Wire / CLI label.
    pub fn as_str(self) -> &'static str {
        match self {
            PeerRole::Offerer => "offerer",
            PeerRole::Answerer => "answerer",
        }
    }
}

impl FromStr for PeerRole {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "offerer" | "offer" | "host" | "agent" => Ok(PeerRole::Offerer),
            "answerer" | "answer" | "viewer" => Ok(PeerRole::Answerer),
            other => Err(NetError::Internal(format!(
                "unknown peer role `{other}` (expected offerer|answerer)"
            ))),
        }
    }
}

/// Named transport backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportMode {
    /// In-process mock (default; CI path).
    Mock,
    /// Length-prefixed TCP media plane (feature `live`).
    Live,
    /// Pure-Rust webrtc-rs PeerConnection (feature `webrtc-rs`).
    Webrtc,
    /// Prefer webrtc (if compiled), else live, else mock.
    Auto,
}

impl TransportMode {
    /// Wire / CLI / env label.
    pub fn as_str(self) -> &'static str {
        match self {
            TransportMode::Mock => "mock",
            TransportMode::Live => "live",
            TransportMode::Webrtc => "webrtc",
            TransportMode::Auto => "auto",
        }
    }
}

impl FromStr for TransportMode {
    type Err = NetError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mock" | "synthetic" => Ok(TransportMode::Mock),
            "live" | "tcp" | "loopback" => Ok(TransportMode::Live),
            "webrtc" | "webrtc-rs" | "webrtc_rs" => Ok(TransportMode::Webrtc),
            "auto" => Ok(TransportMode::Auto),
            other => Err(NetError::BackendUnavailable(format!(
                "unknown transport mode `{other}` (expected mock|live|webrtc|auto)"
            ))),
        }
    }
}

/// Configuration for [`create_peer_transport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// Backend mode (`mock` / `live` / `webrtc` / `auto`).
    pub mode: TransportMode,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            mode: TransportMode::Mock,
        }
    }
}

impl TransportConfig {
    /// Read `REMOTELINK_TRANSPORT` (`mock` | `live` | `webrtc` | `auto`). Unset → mock.
    pub fn from_env() -> Self {
        match env::var("REMOTELINK_TRANSPORT") {
            Ok(v) if !v.trim().is_empty() => match TransportMode::from_str(&v) {
                Ok(mode) => Self { mode },
                Err(_) => {
                    // Invalid env: stay CI-safe.
                    Self::default()
                }
            },
            _ => Self::default(),
        }
    }

    /// Build from a CLI/env string (`mock` / `live` / `webrtc` / `auto`).
    pub fn parse(s: &str) -> Result<Self> {
        Ok(Self {
            mode: TransportMode::from_str(s)?,
        })
    }

    /// Resolve `auto` to a concrete mode given compiled features.
    ///
    /// Preference: **webrtc-rs** (if feature on) → **live** (if feature on) → **mock**.
    pub fn resolved_mode(&self) -> TransportMode {
        match self.mode {
            TransportMode::Auto => {
                #[cfg(feature = "webrtc-rs")]
                {
                    TransportMode::Webrtc
                }
                #[cfg(all(not(feature = "webrtc-rs"), feature = "live"))]
                {
                    TransportMode::Live
                }
                #[cfg(all(not(feature = "webrtc-rs"), not(feature = "live")))]
                {
                    TransportMode::Mock
                }
            }
            other => other,
        }
    }
}

/// Create a peer transport for `role` using [`TransportConfig::from_env`].
pub fn create_peer_transport(role: PeerRole) -> Result<BoxPeerTransport> {
    create_peer_transport_with_config(role, &TransportConfig::from_env())
}

/// Create a peer transport for `role` with an explicit config.
pub fn create_peer_transport_with_config(
    role: PeerRole,
    config: &TransportConfig,
) -> Result<BoxPeerTransport> {
    match config.resolved_mode() {
        TransportMode::Mock => create_mock(role),
        TransportMode::Live => create_live(role),
        TransportMode::Webrtc => create_webrtc(role),
        TransportMode::Auto => unreachable!("resolved_mode never returns Auto"),
    }
}

#[cfg(feature = "mock")]
fn create_mock(role: PeerRole) -> Result<BoxPeerTransport> {
    let label = match role {
        PeerRole::Offerer => "mock-offerer",
        PeerRole::Answerer => "mock-answerer",
    };
    Ok(Box::new(MockPeerTransport::new(MockPeerConfig {
        label: label.into(),
        fingerprint: None,
    })))
}

#[cfg(not(feature = "mock"))]
fn create_mock(_role: PeerRole) -> Result<BoxPeerTransport> {
    Err(NetError::BackendUnavailable(
        "mock backend not compiled (enable feature `mock`)".into(),
    ))
}

#[cfg(feature = "live")]
fn create_live(role: PeerRole) -> Result<BoxPeerTransport> {
    Ok(Box::new(LivePeerTransport::new(
        role,
        LivePeerConfig::from_env(),
    )?))
}

#[cfg(not(feature = "live"))]
fn create_live(_role: PeerRole) -> Result<BoxPeerTransport> {
    Err(NetError::BackendUnavailable(
        "live TCP backend not compiled (enable feature `live`)".into(),
    ))
}

#[cfg(feature = "webrtc-rs")]
fn create_webrtc(role: PeerRole) -> Result<BoxPeerTransport> {
    Ok(Box::new(WebrtcPeerTransport::new(
        role,
        WebrtcPeerConfig::from_env(),
    )?))
}

#[cfg(not(feature = "webrtc-rs"))]
fn create_webrtc(_role: PeerRole) -> Result<BoxPeerTransport> {
    Err(NetError::BackendUnavailable(
        "webrtc-rs backend not compiled (enable feature `webrtc-rs`)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConnectionState;

    #[test]
    fn default_config_is_mock() {
        assert_eq!(TransportConfig::default().mode, TransportMode::Mock);
        assert_eq!(
            TransportConfig::default().resolved_mode(),
            TransportMode::Mock
        );
    }

    #[test]
    fn parse_modes() {
        assert_eq!(
            TransportConfig::parse("mock").unwrap().mode,
            TransportMode::Mock
        );
        assert_eq!(
            TransportConfig::parse("LIVE").unwrap().mode,
            TransportMode::Live
        );
        assert_eq!(
            TransportConfig::parse("auto").unwrap().mode,
            TransportMode::Auto
        );
        assert_eq!(
            TransportConfig::parse("webrtc").unwrap().mode,
            TransportMode::Webrtc
        );
        assert_eq!(
            TransportConfig::parse("webrtc-rs").unwrap().mode,
            TransportMode::Webrtc
        );
        assert!(TransportConfig::parse("nope").is_err());
    }

    #[test]
    fn parse_roles() {
        assert_eq!(PeerRole::from_str("offerer").unwrap(), PeerRole::Offerer);
        assert_eq!(PeerRole::from_str("viewer").unwrap(), PeerRole::Answerer);
        assert!(PeerRole::from_str("nope").is_err());
    }

    #[test]
    fn factory_mock_offerer() {
        let mut t = create_peer_transport_with_config(
            PeerRole::Offerer,
            &TransportConfig {
                mode: TransportMode::Mock,
            },
        )
        .unwrap();
        assert_eq!(t.connection_state(), ConnectionState::New);
        let offer = t.create_offer().unwrap();
        assert!(!offer.sdp.is_empty());
        t.close().unwrap();
    }

    #[cfg(feature = "live")]
    #[test]
    fn factory_live_answerer() {
        let mut t = create_peer_transport_with_config(
            PeerRole::Answerer,
            &TransportConfig {
                mode: TransportMode::Live,
            },
        )
        .unwrap();
        assert_eq!(t.connection_state(), ConnectionState::New);
        let fp = t.local_fingerprint().unwrap();
        assert_eq!(fp.algorithm, "sha-256");
        t.close().unwrap();
    }

    #[cfg(all(feature = "live", not(feature = "webrtc-rs")))]
    #[test]
    fn auto_resolves_to_live_when_webrtc_off() {
        let cfg = TransportConfig {
            mode: TransportMode::Auto,
        };
        assert_eq!(cfg.resolved_mode(), TransportMode::Live);
    }

    #[cfg(feature = "webrtc-rs")]
    #[test]
    fn auto_resolves_to_webrtc_when_feature_on() {
        let cfg = TransportConfig {
            mode: TransportMode::Auto,
        };
        assert_eq!(cfg.resolved_mode(), TransportMode::Webrtc);
    }

    #[cfg(feature = "webrtc-rs")]
    #[test]
    fn factory_webrtc_offerer() {
        let mut t = create_peer_transport_with_config(
            PeerRole::Offerer,
            &TransportConfig {
                mode: TransportMode::Webrtc,
            },
        )
        .unwrap();
        assert_eq!(t.connection_state(), ConnectionState::New);
        let fp = t.local_fingerprint().unwrap();
        assert_eq!(fp.algorithm, "sha-256");
        t.close().unwrap();
    }

    #[cfg(not(feature = "webrtc-rs"))]
    #[test]
    fn webrtc_mode_unavailable_without_feature() {
        let result = create_peer_transport_with_config(
            PeerRole::Offerer,
            &TransportConfig {
                mode: TransportMode::Webrtc,
            },
        );
        assert!(
            matches!(result, Err(NetError::BackendUnavailable(_))),
            "expected BackendUnavailable without webrtc-rs feature"
        );
    }
}

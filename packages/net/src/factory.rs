//! Transport factory: select mock vs live backends from env / CLI.
//!
//! # Defaults (CI-safe)
//!
//! - Env `REMOTELINK_TRANSPORT` defaults to **`mock`** when unset.
//! - Values: `mock` | `live` | `auto`.
//! - `auto` prefers the live TCP backend when the `live` feature is enabled,
//!   otherwise falls back to mock.
//!
//! Real WebRTC (DTLS-SRTP / ICE) is **not** selected here yet — see
//! `docs/spike-webrtc.md`. The live backend is a pragmatic multi-process TCP
//! path for local demos, not a production media stack.

use std::env;
use std::str::FromStr;

use crate::error::{NetError, Result};
#[cfg(feature = "live")]
use crate::live_loopback::{LivePeerConfig, LivePeerTransport};
#[cfg(feature = "mock")]
use crate::mock::{MockPeerConfig, MockPeerTransport};
use crate::transport::BoxPeerTransport;

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
    /// Prefer live when compiled in; otherwise mock.
    Auto,
}

impl TransportMode {
    /// Wire / CLI / env label.
    pub fn as_str(self) -> &'static str {
        match self {
            TransportMode::Mock => "mock",
            TransportMode::Live => "live",
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
            "auto" => Ok(TransportMode::Auto),
            other => Err(NetError::BackendUnavailable(format!(
                "unknown transport mode `{other}` (expected mock|live|auto)"
            ))),
        }
    }
}

/// Configuration for [`create_peer_transport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportConfig {
    /// Backend mode (`mock` / `live` / `auto`).
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
    /// Read `REMOTELINK_TRANSPORT` (`mock` | `live` | `auto`). Unset → mock.
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

    /// Build from a CLI/env string (`mock` / `live` / `auto`).
    pub fn parse(s: &str) -> Result<Self> {
        Ok(Self {
            mode: TransportMode::from_str(s)?,
        })
    }

    /// Resolve `auto` to a concrete mode given compiled features.
    pub fn resolved_mode(&self) -> TransportMode {
        match self.mode {
            TransportMode::Auto => {
                #[cfg(feature = "live")]
                {
                    TransportMode::Live
                }
                #[cfg(not(feature = "live"))]
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
        assert!(TransportConfig::parse("webrtc").is_err());
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

    #[cfg(feature = "live")]
    #[test]
    fn auto_resolves_to_live_when_feature_on() {
        let cfg = TransportConfig {
            mode: TransportMode::Auto,
        };
        assert_eq!(cfg.resolved_mode(), TransportMode::Live);
    }
}

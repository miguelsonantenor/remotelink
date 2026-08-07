//! Toolkit-agnostic RemoteLink viewer core.
//!
//! # Responsibilities
//!
//! - Connection state machine ([`state::ConnectionMachine`])
//! - Connect credentials + session-intent stubs ([`connect`])
//! - H.264 / synthetic video decode hooks ([`decode`])
//! - Opus playout queue + sink trait ([`audio`])
//! - Exportable beta stats with required A/V skew ([`stats`])
//! - Input event emitter ([`input`])
//! - Session driver over [`remotelink_net::PeerTransport`] answerer ([`session`])
//!
//! GUI toolkits (egui) and CLI shells live in `apps/viewer` and depend on this
//! crate so unit tests never pull in a windowing system.

#![deny(missing_docs)]

pub mod audio;
pub mod connect;
pub mod decode;
pub mod error;
pub mod input;
pub mod session;
pub mod state;
pub mod stats;

pub use audio::{
    AudioPlayoutQueue, AudioPlayoutSink, MockAudioPlayoutSink, NullAudioPlayoutSink, PlayoutPacket,
};
pub use connect::{connect_stub, ConnectRequest, ConnectSecret, ConnectStubResult};
pub use decode::{
    DecodedVideoFrame, MockH264VideoDecoder, MockOrSyntheticDecoder, RecordingDecodeHook,
    SyntheticVideoDecoder, VideoDecodeHook,
};
pub use error::{Result, ViewerError};
pub use input::{InputEmitter, INPUT_CHANNEL_LABEL};
pub use session::{run_mock_codec_loopback, run_synthetic_loopback, ViewerSession};
pub use state::{ConnectionMachine, ViewerPhase};
pub use stats::{BindStatus, SessionStats};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn version_nonempty() {
        assert!(!crate::VERSION.is_empty());
    }
}

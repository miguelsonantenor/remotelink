//! Windows host platform support for RemoteLink.
//!
//! Implements the **agent-media** process model (DESIGN KD5):
//! - Host **service** owns enrollment, signaling WS, policy, kill-switch.
//! - Session **agent** owns capture/encode/PeerTransport (later PRs).
//! - IPC between them is **control-plane only** — no media bytes.
//!
//! Types and the length-prefixed JSON codec are cross-platform so unit tests
//! and Linux CI can exercise framing without Windows named pipes. Windows-
//! specific transports (named pipes, hotkeys) are gated with `cfg(windows)`.

#![deny(missing_docs)]

pub mod ipc;
pub mod kill_switch;

pub use ipc::{
    codec::{
        decode_control, decode_frame, encode_control, encode_frame, read_control, read_frame,
        write_control, write_frame, CodecError, MAX_FRAME_PAYLOAD,
    },
    message::*,
    transport::{
        connect_control, listen_control, ControlEndpoint, ControlListener, ControlStream,
        TransportError,
    },
};
pub use kill_switch::{KillSwitchError, KillSwitchHandle, KillSwitchRegistrar};

//! Windows host platform support for RemoteLink.
//!
//! Implements the **agent-media** process model (DESIGN KD5):
//! - Host **service** owns enrollment, signaling WS, policy, kill-switch.
//! - Session **agent** owns capture/encode/PeerTransport and input inject.
//! - IPC between them is **control-plane only** — no media bytes.
//!
//! Types and the length-prefixed JSON codec are cross-platform so unit tests
//! and Linux CI can exercise framing without Windows named pipes. Windows-
//! specific transports (named pipes, hotkeys) and `SendInput` injection are
//! gated with `cfg(windows)`.
//!
//! # Secure desktop / UAC (v1 known gap)
//!
//! See [`input`]: capture and injection do not work on Winlogon/UAC secure
//! desktop without a separate signed path — out of scope for v1.

#![deny(missing_docs)]

pub mod input;
pub mod ipc;
pub mod kill_switch;

#[cfg(windows)]
pub use input::WindowsInjector;
pub use input::{
    open_injector, AnyInjector, InjectError, InjectorConfig, InjectorOpenMode, InputInjector,
    InputMetrics, RateLimitedInjector, StubInjector, MAX_INPUT_EVENTS_PER_SEC,
};
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

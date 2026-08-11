//! WASAPI loopback capture for host system audio (PR 16c).
//!
//! # Design (DESIGN § Windows audio capture)
//!
//! - Shared-mode `IAudioClient` with `AUDCLNT_STREAMFLAGS_LOOPBACK` on the
//!   selected **render** endpoint (not the capture/mic endpoint).
//! - Device-change notifications (`IMMNotificationClient`) → stop, reopen,
//!   emit `media_restart`, brief mute.
//! - Exclusive-mode games: sustained near-zero loopback energy → tray/viewer
//!   warning; session must not crash.
//!
//! # What this PR ships
//!
//! 1. [`LoopbackSource`] trait consumed by media / session agent (`AudioSource`).
//! 2. [`StubLoopbackCapture`] — CI-safe synthetic PCM path (no COM / no device).
//! 3. [`NativeLoopbackCapture`] skeleton documenting the real COM sequence;
//!    `try_open` returns [`LoopbackError::NativeUnavailable`] until COM is wired.
//! 4. Device-change + exclusive-mode warning hooks via [`LoopbackHooks`].
//! 5. PCM energy helper [`pcm_is_near_silence`] for exclusive-mode detection.
//!
//! Prefer [`open_loopback`] which selects native (when available) or stub.

mod capture;
mod energy;
mod hooks;
mod native;
mod stub;

pub use capture::{
    open_loopback, open_loopback_with_hooks, AnyLoopback, LoopbackConfig, LoopbackError,
    LoopbackOpenMode, LoopbackSource, DEFAULT_CHANNELS, DEFAULT_PACKET_MS, DEFAULT_SAMPLE_RATE,
};
pub use energy::{pcm_is_near_silence, pcm_is_near_silence_default, NEAR_SILENCE_PEAK};
pub use hooks::{
    DeviceChangeReason, ExclusiveModeWarning, LoopbackEvent, LoopbackHooks, NullHooks,
    RecordingHooks, SharedHooks,
};
pub use native::{
    position_to_host_mono, NativeLoopbackCapture, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
pub use stub::StubLoopbackCapture;

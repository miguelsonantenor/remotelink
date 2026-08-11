//! Host input injection (viewer → OS) for the session agent.
//!
//! # Identity gate (KD17 / PR 13)
//!
//! Injection is **only** enabled after identity bind and session authorization
//! (`identity_bound && session_authorized`). The host agent also requires
//! policy `enable_input` and a non-killed session. This module performs the
//! OS inject; the agent / [`SessionManager`] owns the security gate.
//!
//! # Key encoding
//!
//! Keys use **Windows scan set 1** scancodes plus the extended (E0) flag from
//! [`remotelink_protocol::KeyEvent`]. Injection uses `KEYEVENTF_SCANCODE`
//! (and `KEYEVENTF_EXTENDEDKEY` when `extended` is set). The host OS keyboard
//! layout maps scancodes to characters; the viewer does not send Unicode for
//! key-down/up shortcuts.
//!
//! # Mouse coordinates
//!
//! Protocol `x,y ∈ [0,1]` are defined over the selected capture rectangle.
//! The Windows backend maps them with `MOUSEEVENTF_ABSOLUTE |
//! MOUSEEVENTF_VIRTUALDESK` (virtual desktop, not primary-only). Capture-rect
//! origin/size remapping for multi-monitor sub-rects is deferred (v1
//! `display_id == 0`).
//!
//! # Rate limit
//!
//! Host applies a hard cap of [`MAX_INPUT_EVENTS_PER_SEC`] (200) events per
//! second (fixed 1s window). Excess events are **dropped** and counted on
//! [`InputMetrics::dropped_rate_limit`] (DESIGN metric `input_drop_rate`).
//! Callers of rate-limited inject see `Ok(false)` / non-injected outcomes for
//! drops — the bool means “actually applied”, not “gate open”.
//!
//! # Secure desktop / UAC (v1 known gap)
//!
//! Capture and injection **do not work** on the Winlogon / UAC secure desktop
//! without a separate signed path (credential provider / special driver) —
//! **out of scope for v1**. Remote users cannot interact with UAC prompts or
//! Ctrl+Alt+Del secure desktop; the host user must complete those locally.
//! The tray kill-switch remains available on the normal desktop only.
//!
//! # Testing without a real desktop
//!
//! Use [`StubInjector`] (default on non-Windows / CI). It records every
//! accepted event for assertions and never calls OS APIs.

mod rate_limit;
mod stub;

#[cfg(windows)]
mod win;

pub use rate_limit::{InputMetrics, RateLimitedInjector, MAX_INPUT_EVENTS_PER_SEC};
pub use stub::StubInjector;

#[cfg(windows)]
pub use win::WindowsInjector;

use thiserror::Error;

use remotelink_protocol::InputEvent;

/// Errors from opening or performing input injection.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum InjectError {
    /// Platform inject path is unavailable on this build / OS.
    #[error("input injection unsupported on this platform: {0}")]
    Unsupported(&'static str),
    /// OS API call failed.
    #[error("input injection failed: {0}")]
    Os(String),
    /// Event payload was invalid for injection (e.g. out-of-range scancode).
    #[error("invalid input event: {0}")]
    InvalidEvent(&'static str),
}

/// Backend that applies a single decoded [`InputEvent`] to the host OS (or a stub).
pub trait InputInjector: Send {
    /// Inject one event (mouse move/button/wheel or key down/up).
    fn inject(&mut self, event: &InputEvent) -> Result<(), InjectError>;

    /// Backend name for logs (`"stub"` / `"sendinput"`).
    fn backend_name(&self) -> &'static str;
}

/// How [`open_injector`] selects a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InjectorOpenMode {
    /// Prefer native `SendInput` on Windows; fall back to stub elsewhere.
    #[default]
    PreferNative,
    /// Always use the recording stub (CI / unit tests).
    StubOnly,
    /// Require native Windows inject (error on non-Windows or open failure).
    NativeOnly,
}

/// Configuration for opening an input injector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InjectorConfig {
    /// Backend selection policy.
    pub open_mode: InjectorOpenMode,
    /// Max events accepted per second ([`MAX_INPUT_EVENTS_PER_SEC`] by default).
    pub max_events_per_sec: u32,
    /// Virtual desktop width used to map normalized x ∈ [0,1] (stub / fallback).
    pub screen_width: u32,
    /// Virtual desktop height used to map normalized y ∈ [0,1] (stub / fallback).
    pub screen_height: u32,
}

impl Default for InjectorConfig {
    fn default() -> Self {
        Self {
            open_mode: InjectorOpenMode::PreferNative,
            max_events_per_sec: MAX_INPUT_EVENTS_PER_SEC,
            screen_width: 1920,
            screen_height: 1080,
        }
    }
}

impl InjectorConfig {
    /// CI / unit-test defaults: recording stub, 200 events/s cap.
    pub fn synthetic() -> Self {
        Self {
            open_mode: InjectorOpenMode::StubOnly,
            ..Self::default()
        }
    }
}

/// Concrete injector used by the agent (always rate-limited).
pub enum AnyInjector {
    /// Recording stub (tests / non-Windows).
    Stub(RateLimitedInjector<StubInjector>),
    /// Native `SendInput` path (Windows).
    #[cfg(windows)]
    Windows(RateLimitedInjector<WindowsInjector>),
}

impl std::fmt::Debug for AnyInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stub(_) => f.write_str("AnyInjector::Stub(..)"),
            #[cfg(windows)]
            Self::Windows(_) => f.write_str("AnyInjector::Windows(..)"),
        }
    }
}

impl AnyInjector {
    /// Inject one event; returns `Ok(true)` when applied, `Ok(false)` when
    /// dropped by the rate limiter.
    pub fn try_inject(&mut self, event: &InputEvent) -> Result<bool, InjectError> {
        match self {
            Self::Stub(s) => s.try_inject(event),
            #[cfg(windows)]
            Self::Windows(w) => w.try_inject(event),
        }
    }

    /// Cumulative inject metrics (accepted + rate-limit drops).
    pub fn metrics(&self) -> InputMetrics {
        match self {
            Self::Stub(s) => s.metrics(),
            #[cfg(windows)]
            Self::Windows(w) => w.metrics(),
        }
    }

    /// Backend name (`"stub"` / `"sendinput"`).
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Stub(s) => s.inner().backend_name(),
            #[cfg(windows)]
            Self::Windows(w) => w.inner().backend_name(),
        }
    }

    /// Access the recording stub when this injector is stub-backed.
    ///
    /// Returns `None` for the native Windows backend.
    pub fn stub(&self) -> Option<&StubInjector> {
        match self {
            Self::Stub(s) => Some(s.inner()),
            #[cfg(windows)]
            Self::Windows(_) => None,
        }
    }

    /// Mutable access to the recording stub when stub-backed.
    pub fn stub_mut(&mut self) -> Option<&mut StubInjector> {
        match self {
            Self::Stub(s) => Some(s.inner_mut()),
            #[cfg(windows)]
            Self::Windows(_) => None,
        }
    }
}

/// Open a rate-limited injector for the given config.
///
/// - [`InjectorOpenMode::StubOnly`]: always [`StubInjector`].
/// - [`InjectorOpenMode::PreferNative`]: Windows `SendInput` when available; else stub.
/// - [`InjectorOpenMode::NativeOnly`]: Windows only; error on non-Windows.
pub fn open_injector(config: InjectorConfig) -> Result<AnyInjector, InjectError> {
    let max = config.max_events_per_sec.max(1);
    match config.open_mode {
        InjectorOpenMode::StubOnly => Ok(AnyInjector::Stub(RateLimitedInjector::new(
            StubInjector::new(config.screen_width, config.screen_height),
            max,
        ))),
        InjectorOpenMode::PreferNative => open_prefer_native(config, max),
        InjectorOpenMode::NativeOnly => open_native_only(config, max),
    }
}

fn open_prefer_native(config: InjectorConfig, max: u32) -> Result<AnyInjector, InjectError> {
    #[cfg(windows)]
    {
        match WindowsInjector::open(config.screen_width, config.screen_height) {
            Ok(win) => Ok(AnyInjector::Windows(RateLimitedInjector::new(win, max))),
            Err(_) => Ok(AnyInjector::Stub(RateLimitedInjector::new(
                StubInjector::new(config.screen_width, config.screen_height),
                max,
            ))),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = config;
        Ok(AnyInjector::Stub(RateLimitedInjector::new(
            StubInjector::new(config.screen_width, config.screen_height),
            max,
        )))
    }
}

fn open_native_only(config: InjectorConfig, max: u32) -> Result<AnyInjector, InjectError> {
    #[cfg(windows)]
    {
        let win = WindowsInjector::open(config.screen_width, config.screen_height)?;
        Ok(AnyInjector::Windows(RateLimitedInjector::new(win, max)))
    }
    #[cfg(not(windows))]
    {
        let _ = (config, max);
        Err(InjectError::Unsupported(
            "native SendInput requires Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_protocol::{
        InputPayload, KeyEvent, MouseButton, MouseButtonKind, MouseMove, MouseWheel,
    };

    fn sample_move(seq: u32) -> InputEvent {
        InputEvent {
            client_ts_us: u64::from(seq) * 1000,
            seq,
            payload: InputPayload::MouseMove(MouseMove {
                x: 0.5,
                y: 0.5,
                display_id: 0,
            }),
        }
    }

    #[test]
    fn stub_only_records_all_event_kinds() {
        let mut inj = open_injector(InjectorConfig::synthetic()).unwrap();
        assert_eq!(inj.backend_name(), "stub");

        inj.try_inject(&sample_move(1)).unwrap();
        inj.try_inject(&InputEvent {
            client_ts_us: 2,
            seq: 2,
            payload: InputPayload::MouseButton(MouseButton {
                button: MouseButtonKind::Left,
                pressed: true,
                x: 0.1,
                y: 0.2,
                display_id: 0,
            }),
        })
        .unwrap();
        inj.try_inject(&InputEvent {
            client_ts_us: 3,
            seq: 3,
            payload: InputPayload::MouseWheel(MouseWheel {
                delta_x: 0.0,
                delta_y: -1.0,
                precise: false,
                x: 0.5,
                y: 0.5,
                display_id: 0,
            }),
        })
        .unwrap();
        inj.try_inject(&InputEvent {
            client_ts_us: 4,
            seq: 4,
            payload: InputPayload::Key(KeyEvent {
                scancode: 0x1C,
                extended: false,
                pressed: true,
                modifiers: 0,
            }),
        })
        .unwrap();

        let stub = inj.stub().expect("stub backend");
        assert_eq!(stub.recorded().len(), 4);
        assert_eq!(inj.metrics().accepted, 4);
        assert_eq!(inj.metrics().dropped_rate_limit, 0);
    }

    #[test]
    fn rate_limit_drops_excess_with_metric() {
        let mut cfg = InjectorConfig::synthetic();
        cfg.max_events_per_sec = 5;
        let mut inj = open_injector(cfg).unwrap();

        let mut accepted = 0u64;
        let mut dropped = 0u64;
        for i in 0..20 {
            match inj.try_inject(&sample_move(i)).unwrap() {
                true => accepted += 1,
                false => dropped += 1,
            }
        }
        assert_eq!(accepted, 5);
        assert_eq!(dropped, 15);
        assert_eq!(inj.metrics().accepted, 5);
        assert_eq!(inj.metrics().dropped_rate_limit, 15);
        assert_eq!(inj.stub().unwrap().recorded().len(), 5);
    }

    #[test]
    fn native_only_on_non_windows_is_unsupported() {
        let cfg = InjectorConfig {
            open_mode: InjectorOpenMode::NativeOnly,
            ..InjectorConfig::default()
        };
        let result = open_injector(cfg);
        #[cfg(not(windows))]
        {
            assert!(matches!(result, Err(InjectError::Unsupported(_))));
        }
        #[cfg(windows)]
        {
            // On Windows, open should succeed (or only fail with Os — never panic).
            match result {
                Ok(inj) => assert_eq!(inj.backend_name(), "sendinput"),
                Err(InjectError::Os(_)) => {}
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
    }

    #[test]
    fn key_scancode_and_extended_flag_recorded() {
        let mut inj = open_injector(InjectorConfig::synthetic()).unwrap();
        let ev = InputEvent {
            client_ts_us: 1,
            seq: 1,
            payload: InputPayload::Key(KeyEvent {
                scancode: 0x48, // up arrow (extended)
                extended: true,
                pressed: false,
                modifiers: 0,
            }),
        };
        assert!(inj.try_inject(&ev).unwrap());
        match &inj.stub().unwrap().recorded()[0].payload {
            InputPayload::Key(k) => {
                assert_eq!(k.scancode, 0x48);
                assert!(k.extended);
                assert!(!k.pressed);
            }
            other => panic!("expected key, got {other:?}"),
        }
    }
}

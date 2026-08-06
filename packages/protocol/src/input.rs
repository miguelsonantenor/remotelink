//! Viewer → host input events (DataChannel payload schema, v1 freeze).

use serde::{Deserialize, Serialize};

/// Bitflags for keyboard modifiers (`KeyEvent.modifiers`).
pub mod modifiers {
    pub const CTRL: u32 = 1 << 0;
    pub const ALT: u32 = 1 << 1;
    pub const SHIFT: u32 = 1 << 2;
    pub const META: u32 = 1 << 3;
}

/// Mouse button identifiers (v1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// Normalized mouse move over the selected capture rectangle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouseMove {
    /// Horizontal position in `[0.0, 1.0]`.
    pub x: f32,
    /// Vertical position in `[0.0, 1.0]`.
    pub y: f32,
    /// Display index; always `0` in v1.
    pub display_id: u32,
}

/// Mouse button press or release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouseButton {
    pub button: MouseButtonKind,
    pub pressed: bool,
    /// Cursor position at press/release, normalized `[0.0, 1.0]`.
    pub x: f32,
    pub y: f32,
    pub display_id: u32,
}

/// Mouse wheel / high-resolution scroll.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MouseWheel {
    /// Horizontal scroll delta.
    pub delta_x: f32,
    /// Vertical scroll delta.
    pub delta_y: f32,
    /// When true, deltas are high-resolution (pixel-ish); otherwise line notches.
    pub precise: bool,
    pub x: f32,
    pub y: f32,
    pub display_id: u32,
}

/// Keyboard event using Windows scan-set-1 scancodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyEvent {
    /// Scan set 1 scancode.
    pub scancode: u32,
    /// Extended-key flag (E0 prefix on Windows).
    pub extended: bool,
    /// `true` = key down, `false` = key up.
    pub pressed: bool,
    /// Bitflags: see [`modifiers`].
    pub modifiers: u32,
}

/// Discriminated input payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputPayload {
    MouseMove(MouseMove),
    MouseButton(MouseButton),
    MouseWheel(MouseWheel),
    Key(KeyEvent),
}

/// Single input event from viewer to host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    /// Client send timestamp in microseconds (viewer clock).
    pub client_ts_us: u64,
    /// Monotonic sequence number from the viewer.
    pub seq: u32,
    pub payload: InputPayload,
}

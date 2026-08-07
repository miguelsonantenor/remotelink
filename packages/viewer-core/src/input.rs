//! Viewer → host input: capture policy, coalesce, encode, DataChannel send.
//!
//! # Pipeline
//!
//! 1. **Capture** — toolkit/CLI feeds [`RawInput`] samples (pixel coords + keys).
//! 2. **Focus policy** — by default only when the window is focused; optional
//!    [`InputCaptureConfig::always_capture`] (CLI: `--always-capture`).
//! 3. **Normalize** — mouse coords mapped to `[0.0, 1.0]` over the capture rect.
//! 4. **Coalesce** — mouse moves limited to [`DEFAULT_COALESCE_HZ`] (60–120 Hz
//!    band); buttons / keys / wheel are never dropped and flush any pending move.
//! 5. **Encode** — [`InputEmitter`] builds sequenced [`InputEvent`] JSON on
//!    DataChannel label [`INPUT_CHANNEL_LABEL`] (`"input"`).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use remotelink_net::DataMessage;
use remotelink_protocol::{
    encode_input, lookup_scancode, InputEvent, InputPayload, KeyEvent, MouseButton,
    MouseButtonKind, MouseMove, MouseWheel, NamedKey, ScanCode,
};

use crate::error::{Result, ViewerError};

/// Label used on the PeerTransport DataChannel for input.
pub const INPUT_CHANNEL_LABEL: &str = "input";

/// Default mouse-move coalesce rate (Hz). Within the DESIGN 60–120 band.
pub const DEFAULT_COALESCE_HZ: f32 = 90.0;

/// Minimum allowed coalesce rate (Hz).
pub const MIN_COALESCE_HZ: f32 = 60.0;

/// Maximum allowed coalesce rate (Hz).
pub const MAX_COALESCE_HZ: f32 = 120.0;

/// Capture rectangle in surface/pixel space used to normalize mouse coords.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CaptureRect {
    /// Origin X (left) in surface coordinates.
    pub x: f32,
    /// Origin Y (top) in surface coordinates.
    pub y: f32,
    /// Width; must be `> 0`.
    pub width: f32,
    /// Height; must be `> 0`.
    pub height: f32,
}

impl CaptureRect {
    /// Full unit square `[0,1]×[0,1]` (identity normalize).
    pub const UNIT: Self = Self {
        x: 0.0,
        y: 0.0,
        width: 1.0,
        height: 1.0,
    };

    /// Create a rect; zero/negative size falls back to 1.0.
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width: if width > 0.0 { width } else { 1.0 },
            height: if height > 0.0 { height } else { 1.0 },
        }
    }

    /// Normalize absolute surface coords into `[0.0, 1.0]`.
    pub fn normalize(&self, px: f32, py: f32) -> (f32, f32) {
        let nx = ((px - self.x) / self.width).clamp(0.0, 1.0);
        let ny = ((py - self.y) / self.height).clamp(0.0, 1.0);
        (nx, ny)
    }
}

impl Default for CaptureRect {
    fn default() -> Self {
        Self::UNIT
    }
}

/// Platform-agnostic raw input sample before policy / coalesce / encode.
#[derive(Debug, Clone, PartialEq)]
pub enum RawInput {
    /// Pointer moved to surface coordinates `(px, py)`.
    MouseMove {
        /// Absolute X in capture surface space.
        px: f32,
        /// Absolute Y in capture surface space.
        py: f32,
    },
    /// Mouse button press/release at surface coordinates.
    MouseButton {
        /// Button identity.
        button: MouseButtonKind,
        /// `true` = down, `false` = up.
        pressed: bool,
        /// Absolute X in capture surface space.
        px: f32,
        /// Absolute Y in capture surface space.
        py: f32,
    },
    /// Scroll wheel / trackpad scroll at surface coordinates.
    MouseWheel {
        /// Horizontal delta.
        delta_x: f32,
        /// Vertical delta.
        delta_y: f32,
        /// High-resolution when true; otherwise line notches.
        precise: bool,
        /// Absolute X in capture surface space.
        px: f32,
        /// Absolute Y in capture surface space.
        py: f32,
    },
    /// Key event via shared [`NamedKey`] → scan-set-1 table.
    KeyNamed {
        /// Logical key.
        key: NamedKey,
        /// `true` = down, `false` = up.
        pressed: bool,
        /// Modifier bitflags ([`remotelink_protocol::modifiers`]).
        modifiers: u32,
    },
    /// Key event with an already-resolved scancode (tests / special keys).
    KeyScan {
        /// Scan set 1 code.
        scancode: u32,
        /// Extended (E0) flag.
        extended: bool,
        /// `true` = down, `false` = up.
        pressed: bool,
        /// Modifier bitflags.
        modifiers: u32,
    },
}

/// Normalized payload ready for [`InputEmitter`] (coords already in 0..1).
#[derive(Debug, Clone, PartialEq)]
pub enum CapturedInput {
    /// Coalesced / normalized mouse move.
    MouseMove {
        /// Normalized X.
        x: f32,
        /// Normalized Y.
        y: f32,
    },
    /// Mouse button.
    MouseButton {
        /// Button identity.
        button: MouseButtonKind,
        /// Pressed state.
        pressed: bool,
        /// Normalized X.
        x: f32,
        /// Normalized Y.
        y: f32,
    },
    /// Mouse wheel.
    MouseWheel {
        /// Horizontal delta.
        delta_x: f32,
        /// Vertical delta.
        delta_y: f32,
        /// Precise flag.
        precise: bool,
        /// Normalized X.
        x: f32,
        /// Normalized Y.
        y: f32,
    },
    /// Key with resolved scancode.
    Key {
        /// Scan set 1 code.
        scancode: u32,
        /// Extended flag.
        extended: bool,
        /// Pressed state.
        pressed: bool,
        /// Modifiers.
        modifiers: u32,
    },
}

/// Configuration for [`InputCapture`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct InputCaptureConfig {
    /// When true, send input even if the window is unfocused (CLI: `--always-capture`).
    pub always_capture: bool,
    /// Mouse-move coalesce rate in Hz (clamped to [`MIN_COALESCE_HZ`]..=[`MAX_COALESCE_HZ`]).
    pub coalesce_hz: f32,
    /// Capture rectangle for coordinate normalization.
    pub rect: CaptureRect,
}

impl Default for InputCaptureConfig {
    fn default() -> Self {
        Self {
            always_capture: false,
            coalesce_hz: DEFAULT_COALESCE_HZ,
            rect: CaptureRect::UNIT,
        }
    }
}

impl InputCaptureConfig {
    /// Clamp `coalesce_hz` into the DESIGN band.
    pub fn clamped_hz(self) -> f32 {
        self.coalesce_hz.clamp(MIN_COALESCE_HZ, MAX_COALESCE_HZ)
    }

    /// Minimum interval between coalesced mouse moves.
    pub fn coalesce_interval(self) -> Duration {
        let hz = self.clamped_hz();
        Duration::from_secs_f64(1.0 / f64::from(hz))
    }
}

/// Mouse-move coalescer: at most one move per interval; keeps latest position.
#[derive(Debug, Clone)]
pub struct MouseMoveCoalescer {
    interval: Duration,
    pending: Option<(f32, f32)>,
    last_emit: Option<Instant>,
    /// Moves accepted into the coalescer (including overwrites).
    accepted: u64,
    /// Moves emitted after rate limiting.
    emitted: u64,
    /// Moves dropped because a newer pending position replaced them.
    coalesced_away: u64,
}

impl MouseMoveCoalescer {
    /// Create with the given minimum interval between emits.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval: if interval.is_zero() {
                Duration::from_millis(1)
            } else {
                interval
            },
            pending: None,
            last_emit: None,
            accepted: 0,
            emitted: 0,
            coalesced_away: 0,
        }
    }

    /// Create from a coalesce rate in Hz (clamped to 60–120).
    pub fn from_hz(hz: f32) -> Self {
        let hz = hz.clamp(MIN_COALESCE_HZ, MAX_COALESCE_HZ);
        Self::new(Duration::from_secs_f64(1.0 / f64::from(hz)))
    }

    /// Number of moves that were replaced by a newer pending position.
    pub fn coalesced_away(&self) -> u64 {
        self.coalesced_away
    }

    /// Number of moves emitted.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Number of moves accepted (before coalesce).
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Whether a move is waiting to be flushed.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Note a new normalized mouse position.
    ///
    /// Returns `Some((x,y))` immediately if the coalesce interval has elapsed
    /// (or this is the first move); otherwise stores as pending.
    pub fn push(&mut self, x: f32, y: f32, now: Instant) -> Option<(f32, f32)> {
        self.accepted = self.accepted.saturating_add(1);
        if let Some(prev) = self.pending.replace((x, y)) {
            let _ = prev;
            self.coalesced_away = self.coalesced_away.saturating_add(1);
        }
        if self.should_emit(now) {
            self.take_emit(now)
        } else {
            None
        }
    }

    /// If a pending move exists and the interval has elapsed, emit it.
    pub fn poll(&mut self, now: Instant) -> Option<(f32, f32)> {
        if self.pending.is_some() && self.should_emit(now) {
            self.take_emit(now)
        } else {
            None
        }
    }

    /// Force-emit any pending move (e.g. before a button/key/wheel).
    pub fn flush(&mut self, now: Instant) -> Option<(f32, f32)> {
        if self.pending.is_some() {
            self.take_emit(now)
        } else {
            None
        }
    }

    fn should_emit(&self, now: Instant) -> bool {
        match self.last_emit {
            None => true,
            Some(t) => now.duration_since(t) >= self.interval,
        }
    }

    fn take_emit(&mut self, now: Instant) -> Option<(f32, f32)> {
        let pos = self.pending.take()?;
        self.last_emit = Some(now);
        self.emitted = self.emitted.saturating_add(1);
        Some(pos)
    }
}

/// Focus-aware input capture: normalize + coalesce → [`CapturedInput`] stream.
#[derive(Debug, Clone)]
pub struct InputCapture {
    config: InputCaptureConfig,
    focused: bool,
    coalescer: MouseMoveCoalescer,
    /// Samples dropped because focus policy blocked them.
    blocked_unfocused: u64,
    /// Keys that could not be mapped to a scancode.
    unmapped_keys: u64,
}

impl Default for InputCapture {
    fn default() -> Self {
        Self::new(InputCaptureConfig::default())
    }
}

impl InputCapture {
    /// Create with the given config (window starts unfocused).
    pub fn new(config: InputCaptureConfig) -> Self {
        let coalescer = MouseMoveCoalescer::from_hz(config.clamped_hz());
        Self {
            config,
            focused: false,
            coalescer,
            blocked_unfocused: 0,
            unmapped_keys: 0,
        }
    }

    /// Current config.
    pub fn config(&self) -> &InputCaptureConfig {
        &self.config
    }

    /// Update the capture rectangle (e.g. video pane resize).
    pub fn set_rect(&mut self, rect: CaptureRect) {
        self.config.rect = rect;
    }

    /// Set window focus state.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Whether the window is considered focused.
    pub fn focused(&self) -> bool {
        self.focused
    }

    /// Enable/disable always-capture (ignore focus).
    pub fn set_always_capture(&mut self, always: bool) {
        self.config.always_capture = always;
    }

    /// Whether always-capture is enabled.
    pub fn always_capture(&self) -> bool {
        self.config.always_capture
    }

    /// True when input may leave the capture stage (focus policy).
    pub fn may_capture(&self) -> bool {
        self.config.always_capture || self.focused
    }

    /// Samples dropped due to unfocused window (when not always-capture).
    pub fn blocked_unfocused(&self) -> u64 {
        self.blocked_unfocused
    }

    /// Unmapped named keys.
    pub fn unmapped_keys(&self) -> u64 {
        self.unmapped_keys
    }

    /// Access the move coalescer (metrics / tests).
    pub fn coalescer(&self) -> &MouseMoveCoalescer {
        &self.coalescer
    }

    /// Push a raw sample; returns zero or more captured events ready to send.
    ///
    /// Reliable events (button/key/wheel) flush any pending move first so the
    /// ordered stream still reflects the last pointer position.
    pub fn push(&mut self, raw: RawInput, now: Instant) -> Vec<CapturedInput> {
        if !self.may_capture() {
            self.blocked_unfocused = self.blocked_unfocused.saturating_add(1);
            return Vec::new();
        }
        match raw {
            RawInput::MouseMove { px, py } => {
                let (x, y) = self.config.rect.normalize(px, py);
                match self.coalescer.push(x, y, now) {
                    Some((x, y)) => vec![CapturedInput::MouseMove { x, y }],
                    None => Vec::new(),
                }
            }
            RawInput::MouseButton {
                button,
                pressed,
                px,
                py,
            } => {
                let mut out = Vec::new();
                if let Some((x, y)) = self.coalescer.flush(now) {
                    out.push(CapturedInput::MouseMove { x, y });
                }
                let (x, y) = self.config.rect.normalize(px, py);
                out.push(CapturedInput::MouseButton {
                    button,
                    pressed,
                    x,
                    y,
                });
                out
            }
            RawInput::MouseWheel {
                delta_x,
                delta_y,
                precise,
                px,
                py,
            } => {
                let mut out = Vec::new();
                if let Some((x, y)) = self.coalescer.flush(now) {
                    out.push(CapturedInput::MouseMove { x, y });
                }
                let (x, y) = self.config.rect.normalize(px, py);
                out.push(CapturedInput::MouseWheel {
                    delta_x,
                    delta_y,
                    precise,
                    x,
                    y,
                });
                out
            }
            RawInput::KeyNamed {
                key,
                pressed,
                modifiers,
            } => {
                let Some(ScanCode { code, extended }) = lookup_scancode(key) else {
                    self.unmapped_keys = self.unmapped_keys.saturating_add(1);
                    return Vec::new();
                };
                let mut out = Vec::new();
                if let Some((x, y)) = self.coalescer.flush(now) {
                    out.push(CapturedInput::MouseMove { x, y });
                }
                out.push(CapturedInput::Key {
                    scancode: code,
                    extended,
                    pressed,
                    modifiers,
                });
                out
            }
            RawInput::KeyScan {
                scancode,
                extended,
                pressed,
                modifiers,
            } => {
                let mut out = Vec::new();
                if let Some((x, y)) = self.coalescer.flush(now) {
                    out.push(CapturedInput::MouseMove { x, y });
                }
                out.push(CapturedInput::Key {
                    scancode,
                    extended,
                    pressed,
                    modifiers,
                });
                out
            }
        }
    }

    /// Poll for a deferred coalesced mouse move.
    pub fn poll(&mut self, now: Instant) -> Option<CapturedInput> {
        if !self.may_capture() {
            return None;
        }
        self.coalescer
            .poll(now)
            .map(|(x, y)| CapturedInput::MouseMove { x, y })
    }

    /// Flush any pending coalesced move (session tick / send path).
    pub fn flush_pending_move(&mut self, now: Instant) -> Option<CapturedInput> {
        if !self.may_capture() {
            return None;
        }
        self.coalescer
            .flush(now)
            .map(|(x, y)| CapturedInput::MouseMove { x, y })
    }
}

/// Builds sequenced [`InputEvent`]s and encoded [`DataMessage`]s for the host.
#[derive(Debug, Clone)]
pub struct InputEmitter {
    next_seq: u32,
    /// When true, mouse-move messages are marked unordered (partial reliability hint).
    moves_unordered: bool,
    /// Events successfully emitted (encoded).
    emitted: u64,
}

impl Default for InputEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl InputEmitter {
    /// Create an emitter starting at sequence 1.
    pub fn new() -> Self {
        Self {
            next_seq: 1,
            moves_unordered: true,
            emitted: 0,
        }
    }

    /// Number of events encoded so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Next sequence number that will be assigned.
    pub fn next_seq(&self) -> u32 {
        self.next_seq
    }

    /// Build a full [`InputEvent`] with the next sequence and current client timestamp.
    pub fn make_event(&mut self, payload: InputPayload) -> InputEvent {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        InputEvent {
            client_ts_us: client_now_us(),
            seq,
            payload,
        }
    }

    /// Encode an event as a DataChannel message on the input label.
    pub fn encode_message(&mut self, event: &InputEvent) -> Result<DataMessage> {
        let json = encode_input(event)?;
        self.emitted = self.emitted.saturating_add(1);
        let unordered = matches!(event.payload, InputPayload::MouseMove(_));
        Ok(DataMessage {
            label: INPUT_CHANNEL_LABEL.into(),
            data: json.into_bytes(),
            unordered: unordered && self.moves_unordered,
        })
    }

    /// Encode a captured (already normalized) input into an event + message.
    pub fn encode_captured(
        &mut self,
        captured: &CapturedInput,
    ) -> Result<(InputEvent, DataMessage)> {
        let payload = match captured {
            CapturedInput::MouseMove { x, y } => InputPayload::MouseMove(MouseMove {
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
                display_id: 0,
            }),
            CapturedInput::MouseButton {
                button,
                pressed,
                x,
                y,
            } => InputPayload::MouseButton(MouseButton {
                button: *button,
                pressed: *pressed,
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
                display_id: 0,
            }),
            CapturedInput::MouseWheel {
                delta_x,
                delta_y,
                precise,
                x,
                y,
            } => InputPayload::MouseWheel(MouseWheel {
                delta_x: *delta_x,
                delta_y: *delta_y,
                precise: *precise,
                x: x.clamp(0.0, 1.0),
                y: y.clamp(0.0, 1.0),
                display_id: 0,
            }),
            CapturedInput::Key {
                scancode,
                extended,
                pressed,
                modifiers,
            } => InputPayload::Key(KeyEvent {
                scancode: *scancode,
                extended: *extended,
                pressed: *pressed,
                modifiers: *modifiers,
            }),
        };
        let event = self.make_event(payload);
        let msg = self.encode_message(&event)?;
        Ok((event, msg))
    }

    /// Convenience: mouse move in normalized coordinates.
    pub fn mouse_move(&mut self, x: f32, y: f32) -> Result<(InputEvent, DataMessage)> {
        self.encode_captured(&CapturedInput::MouseMove {
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        })
    }

    /// Convenience: mouse button.
    pub fn mouse_button(
        &mut self,
        button: MouseButtonKind,
        pressed: bool,
        x: f32,
        y: f32,
    ) -> Result<(InputEvent, DataMessage)> {
        self.encode_captured(&CapturedInput::MouseButton {
            button,
            pressed,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        })
    }

    /// Convenience: mouse wheel.
    pub fn mouse_wheel(
        &mut self,
        delta_x: f32,
        delta_y: f32,
        precise: bool,
        x: f32,
        y: f32,
    ) -> Result<(InputEvent, DataMessage)> {
        self.encode_captured(&CapturedInput::MouseWheel {
            delta_x,
            delta_y,
            precise,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
        })
    }

    /// Convenience: key event.
    pub fn key(
        &mut self,
        scancode: u32,
        extended: bool,
        pressed: bool,
        modifiers: u32,
    ) -> Result<(InputEvent, DataMessage)> {
        self.encode_captured(&CapturedInput::Key {
            scancode,
            extended,
            pressed,
            modifiers,
        })
    }

    /// Convenience: named key via shared scancode table.
    pub fn key_named(
        &mut self,
        key: NamedKey,
        pressed: bool,
        modifiers: u32,
    ) -> Result<(InputEvent, DataMessage)> {
        let sc = lookup_scancode(key)
            .ok_or_else(|| ViewerError::Internal(format!("no scancode for {key:?}")))?;
        self.key(sc.code, sc.extended, pressed, modifiers)
    }
}

fn client_now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_protocol::decode_input;
    use std::time::Duration;

    #[test]
    fn sequences_increase() {
        let mut em = InputEmitter::new();
        let (e1, _) = em.mouse_move(0.5, 0.5).unwrap();
        let (e2, _) = em.mouse_move(0.6, 0.5).unwrap();
        assert_eq!(e1.seq, 1);
        assert_eq!(e2.seq, 2);
        assert_eq!(em.emitted(), 2);
    }

    #[test]
    fn message_roundtrips_protocol() {
        let mut em = InputEmitter::new();
        let (event, msg) = em
            .mouse_button(MouseButtonKind::Left, true, 0.1, 0.2)
            .unwrap();
        assert_eq!(msg.label, INPUT_CHANNEL_LABEL);
        let decoded = decode_input(std::str::from_utf8(&msg.data).unwrap()).unwrap();
        assert_eq!(decoded.seq, event.seq);
        assert!(matches!(
            decoded.payload,
            InputPayload::MouseButton(MouseButton {
                button: MouseButtonKind::Left,
                pressed: true,
                ..
            })
        ));
    }

    #[test]
    fn mouse_move_marked_unordered() {
        let mut em = InputEmitter::new();
        let (_, msg) = em.mouse_move(0.0, 0.0).unwrap();
        assert!(msg.unordered);
        let (_, msg2) = em.key(0x1C, false, true, 0).unwrap();
        assert!(!msg2.unordered);
    }

    #[test]
    fn normalize_coords_over_capture_rect() {
        let rect = CaptureRect::new(100.0, 50.0, 200.0, 100.0);
        let (x, y) = rect.normalize(100.0, 50.0);
        assert!((x - 0.0).abs() < 1e-5 && (y - 0.0).abs() < 1e-5);
        let (x, y) = rect.normalize(200.0, 100.0);
        assert!((x - 0.5).abs() < 1e-5 && (y - 0.5).abs() < 1e-5);
        let (x, y) = rect.normalize(300.0, 150.0);
        assert!((x - 1.0).abs() < 1e-5 && (y - 1.0).abs() < 1e-5);
        // Outside clamps.
        let (x, y) = rect.normalize(0.0, 0.0);
        assert_eq!((x, y), (0.0, 0.0));
        let (x, y) = rect.normalize(999.0, 999.0);
        assert_eq!((x, y), (1.0, 1.0));
    }

    #[test]
    fn coalesce_limits_move_rate() {
        // 100 Hz → 10 ms interval.
        let mut c = MouseMoveCoalescer::from_hz(100.0);
        let t0 = Instant::now();
        assert!(c.push(0.1, 0.1, t0).is_some(), "first move emits");
        // Immediate second move is pending, not emitted.
        assert!(c.push(0.2, 0.2, t0).is_none());
        assert!(c.push(0.3, 0.3, t0).is_none());
        assert!(c.has_pending());
        assert!(c.coalesced_away() >= 1);

        // Before interval: still pending.
        let t_early = t0 + Duration::from_millis(5);
        assert!(c.poll(t_early).is_none());

        // After interval: emit latest (0.3).
        let t_late = t0 + Duration::from_millis(11);
        let pos = c.poll(t_late).expect("emit after interval");
        assert!((pos.0 - 0.3).abs() < 1e-5);
        assert!((pos.1 - 0.3).abs() < 1e-5);
        assert!(!c.has_pending());
    }

    #[test]
    fn coalesce_flush_on_button_preserves_order() {
        let mut cap = InputCapture::new(InputCaptureConfig {
            always_capture: true,
            coalesce_hz: 60.0,
            rect: CaptureRect::UNIT,
        });
        let t0 = Instant::now();
        // First move emits immediately.
        let e = cap.push(RawInput::MouseMove { px: 0.1, py: 0.2 }, t0);
        assert_eq!(e.len(), 1);
        // Burst of moves within interval → pending only.
        assert!(cap
            .push(RawInput::MouseMove { px: 0.5, py: 0.5 }, t0)
            .is_empty());
        // Button flushes pending move then the button.
        let out = cap.push(
            RawInput::MouseButton {
                button: MouseButtonKind::Left,
                pressed: true,
                px: 0.5,
                py: 0.5,
            },
            t0,
        );
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], CapturedInput::MouseMove { .. }));
        assert!(matches!(
            out[1],
            CapturedInput::MouseButton {
                button: MouseButtonKind::Left,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn focus_policy_blocks_when_unfocused() {
        let mut cap = InputCapture::new(InputCaptureConfig::default());
        assert!(!cap.focused());
        assert!(!cap.always_capture());
        let t0 = Instant::now();
        let out = cap.push(RawInput::MouseMove { px: 0.5, py: 0.5 }, t0);
        assert!(out.is_empty());
        assert_eq!(cap.blocked_unfocused(), 1);

        // Focus enables capture.
        cap.set_focused(true);
        let out = cap.push(RawInput::MouseMove { px: 0.5, py: 0.5 }, t0);
        assert_eq!(out.len(), 1);

        // Unfocus blocks again.
        cap.set_focused(false);
        let out = cap.push(
            RawInput::KeyNamed {
                key: NamedKey::A,
                pressed: true,
                modifiers: 0,
            },
            t0,
        );
        assert!(out.is_empty());
        assert!(cap.blocked_unfocused() >= 2);
    }

    #[test]
    fn always_capture_ignores_focus() {
        let mut cap = InputCapture::new(InputCaptureConfig {
            always_capture: true,
            ..Default::default()
        });
        assert!(!cap.focused());
        let t0 = Instant::now();
        let out = cap.push(
            RawInput::KeyNamed {
                key: NamedKey::Enter,
                pressed: true,
                modifiers: 0,
            },
            t0,
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            CapturedInput::Key {
                scancode,
                extended,
                pressed,
                ..
            } => {
                assert_eq!(*scancode, 0x1C);
                assert!(!*extended);
                assert!(*pressed);
            }
            other => panic!("expected key, got {other:?}"),
        }
    }

    #[test]
    fn named_key_maps_via_shared_table() {
        let mut em = InputEmitter::new();
        let (ev, msg) = em.key_named(NamedKey::A, true, 0).unwrap();
        assert_eq!(msg.label, INPUT_CHANNEL_LABEL);
        match ev.payload {
            InputPayload::Key(k) => {
                assert_eq!(k.scancode, 0x1E);
                assert!(!k.extended);
            }
            other => panic!("{other:?}"),
        }
        let (ev, _) = em.key_named(NamedKey::ArrowUp, false, 0).unwrap();
        match ev.payload {
            InputPayload::Key(k) => {
                assert_eq!(k.scancode, 0x48);
                assert!(k.extended);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn encode_captured_roundtrip() {
        let mut em = InputEmitter::new();
        let cap = CapturedInput::MouseWheel {
            delta_x: 0.0,
            delta_y: -1.0,
            precise: false,
            x: 0.4,
            y: 0.6,
        };
        let (ev, msg) = em.encode_captured(&cap).unwrap();
        let decoded = decode_input(std::str::from_utf8(&msg.data).unwrap()).unwrap();
        assert_eq!(decoded, ev);
        assert!(!msg.unordered);
    }
}

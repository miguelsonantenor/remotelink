//! Shared Windows scan-set-1 scancode table (viewer → host key encoding).
//!
//! The host injects keys with `KEYEVENTF_SCANCODE` (+ extended/E0 flag). The
//! viewer maps platform-agnostic key ids to these scancodes via this table so
//! layout is decided on the host (DESIGN: not Unicode for key-down/up).

/// Windows scan-set-1 scancode + extended (E0) flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanCode {
    /// Scan set 1 code (low byte; high byte unused in v1).
    pub code: u32,
    /// Extended-key flag (`KEYEVENTF_EXTENDEDKEY` / E0 prefix).
    pub extended: bool,
}

impl ScanCode {
    /// Create a non-extended scancode.
    pub const fn new(code: u32) -> Self {
        Self {
            code,
            extended: false,
        }
    }

    /// Create an extended (E0) scancode.
    pub const fn extended(code: u32) -> Self {
        Self {
            code,
            extended: true,
        }
    }
}

/// Stable, platform-agnostic key identifiers used by the viewer capture path.
///
/// Toolkits (egui/winit, OS APIs) map their native key codes into this enum,
/// then [`lookup_scancode`] produces the wire scancode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NamedKey {
    // Letters (US QWERTY physical positions / scan set 1).
    /// A key.
    A,
    /// B key.
    B,
    /// C key.
    C,
    /// D key.
    D,
    /// E key.
    E,
    /// F key.
    F,
    /// G key.
    G,
    /// H key.
    H,
    /// I key.
    I,
    /// J key.
    J,
    /// K key.
    K,
    /// L key.
    L,
    /// M key.
    M,
    /// N key.
    N,
    /// O key.
    O,
    /// P key.
    P,
    /// Q key.
    Q,
    /// R key.
    R,
    /// S key.
    S,
    /// T key.
    T,
    /// U key.
    U,
    /// V key.
    V,
    /// W key.
    W,
    /// X key.
    X,
    /// Y key.
    Y,
    /// Z key.
    Z,
    // Digits (top row).
    /// Digit 0.
    Digit0,
    /// Digit 1.
    Digit1,
    /// Digit 2.
    Digit2,
    /// Digit 3.
    Digit3,
    /// Digit 4.
    Digit4,
    /// Digit 5.
    Digit5,
    /// Digit 6.
    Digit6,
    /// Digit 7.
    Digit7,
    /// Digit 8.
    Digit8,
    /// Digit 9.
    Digit9,
    // Function keys.
    /// F1.
    F1,
    /// F2.
    F2,
    /// F3.
    F3,
    /// F4.
    F4,
    /// F5.
    F5,
    /// F6.
    F6,
    /// F7.
    F7,
    /// F8.
    F8,
    /// F9.
    F9,
    /// F10.
    F10,
    /// F11.
    F11,
    /// F12.
    F12,
    // Modifiers / locks.
    /// Left Control.
    ControlLeft,
    /// Right Control (extended).
    ControlRight,
    /// Left Shift.
    ShiftLeft,
    /// Right Shift.
    ShiftRight,
    /// Left Alt.
    AltLeft,
    /// Right Alt / AltGr (extended).
    AltRight,
    /// Left Meta / Win (extended).
    MetaLeft,
    /// Right Meta / Win (extended).
    MetaRight,
    /// Caps Lock.
    CapsLock,
    /// Num Lock.
    NumLock,
    /// Scroll Lock.
    ScrollLock,
    // Navigation / editing.
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Space.
    Space,
    /// Enter / Return.
    Enter,
    /// Backspace.
    Backspace,
    /// Insert (extended).
    Insert,
    /// Delete (extended).
    Delete,
    /// Home (extended).
    Home,
    /// End (extended).
    End,
    /// Page Up (extended).
    PageUp,
    /// Page Down (extended).
    PageDown,
    /// Arrow Up (extended).
    ArrowUp,
    /// Arrow Down (extended).
    ArrowDown,
    /// Arrow Left (extended).
    ArrowLeft,
    /// Arrow Right (extended).
    ArrowRight,
    // Punctuation (US layout scan positions).
    /// `-` / `_`.
    Minus,
    /// `=` / `+`.
    Equal,
    /// `[` / `{`.
    BracketLeft,
    /// `]` / `}`.
    BracketRight,
    /// `\` / `|`.
    Backslash,
    /// `;` / `:`.
    Semicolon,
    /// `'` / `"`.
    Quote,
    /// `` ` `` / `~`.
    Backquote,
    /// `,` / `<`.
    Comma,
    /// `.` / `>`.
    Period,
    /// `/` / `?`.
    Slash,
    // Numpad.
    /// Numpad 0.
    Numpad0,
    /// Numpad 1.
    Numpad1,
    /// Numpad 2.
    Numpad2,
    /// Numpad 3.
    Numpad3,
    /// Numpad 4.
    Numpad4,
    /// Numpad 5.
    Numpad5,
    /// Numpad 6.
    Numpad6,
    /// Numpad 7.
    Numpad7,
    /// Numpad 8.
    Numpad8,
    /// Numpad 9.
    Numpad9,
    /// Numpad `*`.
    NumpadMultiply,
    /// Numpad `+`.
    NumpadAdd,
    /// Numpad `-`.
    NumpadSubtract,
    /// Numpad `.`.
    NumpadDecimal,
    /// Numpad `/` (extended).
    NumpadDivide,
    /// Numpad Enter (extended).
    NumpadEnter,
    /// Print Screen (extended).
    PrintScreen,
    /// Pause.
    Pause,
    /// Menu / App key (extended).
    ContextMenu,
}

/// Look up the Windows scan-set-1 scancode for a [`NamedKey`].
///
/// Returns `None` only if the key is not in the v1 table (reserved for future
/// keys); all current [`NamedKey`] variants map successfully.
pub fn lookup_scancode(key: NamedKey) -> Option<ScanCode> {
    Some(match key {
        // Letters — set 1 (make codes).
        NamedKey::A => ScanCode::new(0x1E),
        NamedKey::B => ScanCode::new(0x30),
        NamedKey::C => ScanCode::new(0x2E),
        NamedKey::D => ScanCode::new(0x20),
        NamedKey::E => ScanCode::new(0x12),
        NamedKey::F => ScanCode::new(0x21),
        NamedKey::G => ScanCode::new(0x22),
        NamedKey::H => ScanCode::new(0x23),
        NamedKey::I => ScanCode::new(0x17),
        NamedKey::J => ScanCode::new(0x24),
        NamedKey::K => ScanCode::new(0x25),
        NamedKey::L => ScanCode::new(0x26),
        NamedKey::M => ScanCode::new(0x32),
        NamedKey::N => ScanCode::new(0x31),
        NamedKey::O => ScanCode::new(0x18),
        NamedKey::P => ScanCode::new(0x19),
        NamedKey::Q => ScanCode::new(0x10),
        NamedKey::R => ScanCode::new(0x13),
        NamedKey::S => ScanCode::new(0x1F),
        NamedKey::T => ScanCode::new(0x14),
        NamedKey::U => ScanCode::new(0x16),
        NamedKey::V => ScanCode::new(0x2F),
        NamedKey::W => ScanCode::new(0x11),
        NamedKey::X => ScanCode::new(0x2D),
        NamedKey::Y => ScanCode::new(0x15),
        NamedKey::Z => ScanCode::new(0x2C),
        // Digits.
        NamedKey::Digit1 => ScanCode::new(0x02),
        NamedKey::Digit2 => ScanCode::new(0x03),
        NamedKey::Digit3 => ScanCode::new(0x04),
        NamedKey::Digit4 => ScanCode::new(0x05),
        NamedKey::Digit5 => ScanCode::new(0x06),
        NamedKey::Digit6 => ScanCode::new(0x07),
        NamedKey::Digit7 => ScanCode::new(0x08),
        NamedKey::Digit8 => ScanCode::new(0x09),
        NamedKey::Digit9 => ScanCode::new(0x0A),
        NamedKey::Digit0 => ScanCode::new(0x0B),
        // Function.
        NamedKey::F1 => ScanCode::new(0x3B),
        NamedKey::F2 => ScanCode::new(0x3C),
        NamedKey::F3 => ScanCode::new(0x3D),
        NamedKey::F4 => ScanCode::new(0x3E),
        NamedKey::F5 => ScanCode::new(0x3F),
        NamedKey::F6 => ScanCode::new(0x40),
        NamedKey::F7 => ScanCode::new(0x41),
        NamedKey::F8 => ScanCode::new(0x42),
        NamedKey::F9 => ScanCode::new(0x43),
        NamedKey::F10 => ScanCode::new(0x44),
        NamedKey::F11 => ScanCode::new(0x57),
        NamedKey::F12 => ScanCode::new(0x58),
        // Modifiers.
        NamedKey::ControlLeft => ScanCode::new(0x1D),
        NamedKey::ControlRight => ScanCode::extended(0x1D),
        NamedKey::ShiftLeft => ScanCode::new(0x2A),
        NamedKey::ShiftRight => ScanCode::new(0x36),
        NamedKey::AltLeft => ScanCode::new(0x38),
        NamedKey::AltRight => ScanCode::extended(0x38),
        NamedKey::MetaLeft => ScanCode::extended(0x5B),
        NamedKey::MetaRight => ScanCode::extended(0x5C),
        NamedKey::CapsLock => ScanCode::new(0x3A),
        NamedKey::NumLock => ScanCode::new(0x45),
        NamedKey::ScrollLock => ScanCode::new(0x46),
        // Navigation / editing.
        NamedKey::Escape => ScanCode::new(0x01),
        NamedKey::Tab => ScanCode::new(0x0F),
        NamedKey::Space => ScanCode::new(0x39),
        NamedKey::Enter => ScanCode::new(0x1C),
        NamedKey::Backspace => ScanCode::new(0x0E),
        NamedKey::Insert => ScanCode::extended(0x52),
        NamedKey::Delete => ScanCode::extended(0x53),
        NamedKey::Home => ScanCode::extended(0x47),
        NamedKey::End => ScanCode::extended(0x4F),
        NamedKey::PageUp => ScanCode::extended(0x49),
        NamedKey::PageDown => ScanCode::extended(0x51),
        NamedKey::ArrowUp => ScanCode::extended(0x48),
        NamedKey::ArrowDown => ScanCode::extended(0x50),
        NamedKey::ArrowLeft => ScanCode::extended(0x4B),
        NamedKey::ArrowRight => ScanCode::extended(0x4D),
        // Punctuation.
        NamedKey::Minus => ScanCode::new(0x0C),
        NamedKey::Equal => ScanCode::new(0x0D),
        NamedKey::BracketLeft => ScanCode::new(0x1A),
        NamedKey::BracketRight => ScanCode::new(0x1B),
        NamedKey::Backslash => ScanCode::new(0x2B),
        NamedKey::Semicolon => ScanCode::new(0x27),
        NamedKey::Quote => ScanCode::new(0x28),
        NamedKey::Backquote => ScanCode::new(0x29),
        NamedKey::Comma => ScanCode::new(0x33),
        NamedKey::Period => ScanCode::new(0x34),
        NamedKey::Slash => ScanCode::new(0x35),
        // Numpad.
        NamedKey::Numpad0 => ScanCode::new(0x52),
        NamedKey::Numpad1 => ScanCode::new(0x4F),
        NamedKey::Numpad2 => ScanCode::new(0x50),
        NamedKey::Numpad3 => ScanCode::new(0x51),
        NamedKey::Numpad4 => ScanCode::new(0x4B),
        NamedKey::Numpad5 => ScanCode::new(0x4C),
        NamedKey::Numpad6 => ScanCode::new(0x4D),
        NamedKey::Numpad7 => ScanCode::new(0x47),
        NamedKey::Numpad8 => ScanCode::new(0x48),
        NamedKey::Numpad9 => ScanCode::new(0x49),
        NamedKey::NumpadMultiply => ScanCode::new(0x37),
        NamedKey::NumpadAdd => ScanCode::new(0x4E),
        NamedKey::NumpadSubtract => ScanCode::new(0x4A),
        NamedKey::NumpadDecimal => ScanCode::new(0x53),
        NamedKey::NumpadDivide => ScanCode::extended(0x35),
        NamedKey::NumpadEnter => ScanCode::extended(0x1C),
        NamedKey::PrintScreen => ScanCode::extended(0x37),
        NamedKey::Pause => ScanCode::new(0x45), // set-1 pause is multi-byte; 0x45 used with special handling on host
        NamedKey::ContextMenu => ScanCode::extended(0x5D),
    })
}

/// Convenience: scancode for a key, panicking only in debug if the table is incomplete.
pub fn scancode_of(key: NamedKey) -> ScanCode {
    lookup_scancode(key).expect("NamedKey missing from scan-set-1 table")
}

/// Parse a single ASCII letter / digit to [`NamedKey`] (case-insensitive letters).
///
/// Useful for CLI / tests (`"a"` → [`NamedKey::A`]).
pub fn named_key_from_char(c: char) -> Option<NamedKey> {
    match c {
        'a' | 'A' => Some(NamedKey::A),
        'b' | 'B' => Some(NamedKey::B),
        'c' | 'C' => Some(NamedKey::C),
        'd' | 'D' => Some(NamedKey::D),
        'e' | 'E' => Some(NamedKey::E),
        'f' | 'F' => Some(NamedKey::F),
        'g' | 'G' => Some(NamedKey::G),
        'h' | 'H' => Some(NamedKey::H),
        'i' | 'I' => Some(NamedKey::I),
        'j' | 'J' => Some(NamedKey::J),
        'k' | 'K' => Some(NamedKey::K),
        'l' | 'L' => Some(NamedKey::L),
        'm' | 'M' => Some(NamedKey::M),
        'n' | 'N' => Some(NamedKey::N),
        'o' | 'O' => Some(NamedKey::O),
        'p' | 'P' => Some(NamedKey::P),
        'q' | 'Q' => Some(NamedKey::Q),
        'r' | 'R' => Some(NamedKey::R),
        's' | 'S' => Some(NamedKey::S),
        't' | 'T' => Some(NamedKey::T),
        'u' | 'U' => Some(NamedKey::U),
        'v' | 'V' => Some(NamedKey::V),
        'w' | 'W' => Some(NamedKey::W),
        'x' | 'X' => Some(NamedKey::X),
        'y' | 'Y' => Some(NamedKey::Y),
        'z' | 'Z' => Some(NamedKey::Z),
        '0' => Some(NamedKey::Digit0),
        '1' => Some(NamedKey::Digit1),
        '2' => Some(NamedKey::Digit2),
        '3' => Some(NamedKey::Digit3),
        '4' => Some(NamedKey::Digit4),
        '5' => Some(NamedKey::Digit5),
        '6' => Some(NamedKey::Digit6),
        '7' => Some(NamedKey::Digit7),
        '8' => Some(NamedKey::Digit8),
        '9' => Some(NamedKey::Digit9),
        ' ' => Some(NamedKey::Space),
        '\n' | '\r' => Some(NamedKey::Enter),
        '\t' => Some(NamedKey::Tab),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_and_a_are_set1() {
        let enter = scancode_of(NamedKey::Enter);
        assert_eq!(enter, ScanCode::new(0x1C));
        assert!(!enter.extended);

        let a = scancode_of(NamedKey::A);
        assert_eq!(a, ScanCode::new(0x1E));
    }

    #[test]
    fn arrows_are_extended() {
        for key in [
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Insert,
            NamedKey::Delete,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
        ] {
            let sc = scancode_of(key);
            assert!(sc.extended, "{key:?} should be extended");
        }
        assert_eq!(scancode_of(NamedKey::ArrowUp).code, 0x48);
    }

    #[test]
    fn right_modifiers_extended() {
        assert!(scancode_of(NamedKey::ControlRight).extended);
        assert!(scancode_of(NamedKey::AltRight).extended);
        assert!(scancode_of(NamedKey::MetaLeft).extended);
        assert!(!scancode_of(NamedKey::ControlLeft).extended);
        assert!(!scancode_of(NamedKey::ShiftRight).extended); // set-1 right shift is non-E0
    }

    #[test]
    fn all_named_keys_have_scancodes() {
        // Exhaustive match via array of every variant we define.
        let keys = [
            NamedKey::A,
            NamedKey::B,
            NamedKey::C,
            NamedKey::D,
            NamedKey::E,
            NamedKey::F,
            NamedKey::G,
            NamedKey::H,
            NamedKey::I,
            NamedKey::J,
            NamedKey::K,
            NamedKey::L,
            NamedKey::M,
            NamedKey::N,
            NamedKey::O,
            NamedKey::P,
            NamedKey::Q,
            NamedKey::R,
            NamedKey::S,
            NamedKey::T,
            NamedKey::U,
            NamedKey::V,
            NamedKey::W,
            NamedKey::X,
            NamedKey::Y,
            NamedKey::Z,
            NamedKey::Digit0,
            NamedKey::Digit1,
            NamedKey::Digit2,
            NamedKey::Digit3,
            NamedKey::Digit4,
            NamedKey::Digit5,
            NamedKey::Digit6,
            NamedKey::Digit7,
            NamedKey::Digit8,
            NamedKey::Digit9,
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
            NamedKey::ControlLeft,
            NamedKey::ControlRight,
            NamedKey::ShiftLeft,
            NamedKey::ShiftRight,
            NamedKey::AltLeft,
            NamedKey::AltRight,
            NamedKey::MetaLeft,
            NamedKey::MetaRight,
            NamedKey::CapsLock,
            NamedKey::NumLock,
            NamedKey::ScrollLock,
            NamedKey::Escape,
            NamedKey::Tab,
            NamedKey::Space,
            NamedKey::Enter,
            NamedKey::Backspace,
            NamedKey::Insert,
            NamedKey::Delete,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Minus,
            NamedKey::Equal,
            NamedKey::BracketLeft,
            NamedKey::BracketRight,
            NamedKey::Backslash,
            NamedKey::Semicolon,
            NamedKey::Quote,
            NamedKey::Backquote,
            NamedKey::Comma,
            NamedKey::Period,
            NamedKey::Slash,
            NamedKey::Numpad0,
            NamedKey::Numpad1,
            NamedKey::Numpad2,
            NamedKey::Numpad3,
            NamedKey::Numpad4,
            NamedKey::Numpad5,
            NamedKey::Numpad6,
            NamedKey::Numpad7,
            NamedKey::Numpad8,
            NamedKey::Numpad9,
            NamedKey::NumpadMultiply,
            NamedKey::NumpadAdd,
            NamedKey::NumpadSubtract,
            NamedKey::NumpadDecimal,
            NamedKey::NumpadDivide,
            NamedKey::NumpadEnter,
            NamedKey::PrintScreen,
            NamedKey::Pause,
            NamedKey::ContextMenu,
        ];
        for k in keys {
            let sc = lookup_scancode(k).expect("mapped");
            assert!(sc.code <= 0xFF, "scancode for {k:?} too large: {}", sc.code);
        }
    }

    #[test]
    fn named_key_from_char_roundtrip_letters() {
        assert_eq!(named_key_from_char('a'), Some(NamedKey::A));
        assert_eq!(named_key_from_char('A'), Some(NamedKey::A));
        assert_eq!(named_key_from_char('5'), Some(NamedKey::Digit5));
        assert_eq!(named_key_from_char('@'), None);
        let sc = scancode_of(named_key_from_char('a').unwrap());
        assert_eq!(sc.code, 0x1E);
    }
}

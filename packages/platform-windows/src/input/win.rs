//! Windows `SendInput` injection (scan set 1 + absolute mouse).
//!
//! # Coordinate space
//!
//! Protocol coordinates are normalized `x,y ∈ [0,1]` over the **selected
//! capture rectangle** (DESIGN). This backend maps them to absolute
//! `SendInput` coords spanning the **virtual desktop**
//! (`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`, range 0..65535).
//!
//! v1 single-display / full-desktop capture: virtual-desktop mapping matches
//! the capture rect. When capture is a sub-rectangle or non-primary monitor,
//! the agent must map protocol coords through capture origin+size (DPI-aware)
//! before inject — that rect mapping is not applied here yet (`display_id` is
//! always 0 in v1). `screen_width` / `screen_height` record the virtual
//! desktop size for diagnostics and that future map.
//!
//! # Secure desktop / UAC
//!
//! See the parent module docs: inject does **not** reach Winlogon/UAC secure
//! desktop in v1. Calls may still succeed from the agent process while the user
//! is on a secure desktop, but the OS will not deliver them to that desktop.

use remotelink_protocol::{
    InputEvent, InputPayload, KeyEvent, MouseButton, MouseButtonKind, MouseMove, MouseWheel,
};

use super::{InjectError, InputInjector};

/// Native Windows injector using `SendInput`.
#[derive(Debug)]
pub struct WindowsInjector {
    /// Virtual screen width (pixels); retained for diagnostics / future DPI map.
    pub screen_width: u32,
    /// Virtual screen height (pixels).
    pub screen_height: u32,
}

impl WindowsInjector {
    /// Open a `SendInput` injector.
    ///
    /// Queries the virtual screen size when `screen_width`/`screen_height` are 0;
    /// otherwise uses the provided dimensions for absolute mouse mapping.
    pub fn open(screen_width: u32, screen_height: u32) -> Result<Self, InjectError> {
        let (w, h) = if screen_width == 0 || screen_height == 0 {
            query_virtual_screen()?
        } else {
            (screen_width.max(1), screen_height.max(1))
        };
        Ok(Self {
            screen_width: w,
            screen_height: h,
        })
    }

    fn map_abs(x: f32, y: f32) -> (i32, i32) {
        // Absolute coords: 0..65535 over the virtual desktop when VIRTUALDESK is set.
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 1.0);
        let ax = (x * 65535.0).round() as i32;
        let ay = (y * 65535.0).round() as i32;
        (ax, ay)
    }

    /// Absolute mouse flags: move over the full virtual desktop (not primary-only).
    fn abs_mouse_flags(extra: u32) -> u32 {
        extra | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
    }
}

impl InputInjector for WindowsInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<(), InjectError> {
        match &event.payload {
            InputPayload::MouseMove(m) => inject_mouse_move(m),
            InputPayload::MouseButton(b) => inject_mouse_button(b),
            InputPayload::MouseWheel(w) => inject_mouse_wheel(w),
            InputPayload::Key(k) => inject_key(k),
        }
    }

    fn backend_name(&self) -> &'static str {
        "sendinput"
    }
}

fn query_virtual_screen() -> Result<(u32, u32), InjectError> {
    // SM_CXVIRTUALSCREEN / SM_CYVIRTUALSCREEN
    let w = unsafe { GetSystemMetrics(78) };
    let h = unsafe { GetSystemMetrics(79) };
    if w <= 0 || h <= 0 {
        // Fall back to primary screen.
        let w = unsafe { GetSystemMetrics(0) };
        let h = unsafe { GetSystemMetrics(1) };
        if w <= 0 || h <= 0 {
            return Err(InjectError::Os(
                "GetSystemMetrics returned non-positive screen size".into(),
            ));
        }
        return Ok((w as u32, h as u32));
    }
    Ok((w as u32, h as u32))
}

fn inject_mouse_move(m: &MouseMove) -> Result<(), InjectError> {
    let (ax, ay) = WindowsInjector::map_abs(m.x, m.y);
    send_mouse(
        ax,
        ay,
        0,
        WindowsInjector::abs_mouse_flags(MOUSEEVENTF_MOVE),
    )
}

fn inject_mouse_button(b: &MouseButton) -> Result<(), InjectError> {
    let (ax, ay) = WindowsInjector::map_abs(b.x, b.y);
    // Position cursor first so button events land at the reported coords.
    send_mouse(
        ax,
        ay,
        0,
        WindowsInjector::abs_mouse_flags(MOUSEEVENTF_MOVE),
    )?;
    let flags = match (b.button, b.pressed) {
        (MouseButtonKind::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButtonKind::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButtonKind::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButtonKind::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButtonKind::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButtonKind::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        (MouseButtonKind::X1, true) | (MouseButtonKind::X2, true) => MOUSEEVENTF_XDOWN,
        (MouseButtonKind::X1, false) | (MouseButtonKind::X2, false) => MOUSEEVENTF_XUP,
    };
    let data = match b.button {
        MouseButtonKind::X1 => XBUTTON1,
        MouseButtonKind::X2 => XBUTTON2,
        _ => 0,
    };
    send_mouse(ax, ay, data, WindowsInjector::abs_mouse_flags(flags))
}

fn inject_mouse_wheel(w: &MouseWheel) -> Result<(), InjectError> {
    let (ax, ay) = WindowsInjector::map_abs(w.x, w.y);
    send_mouse(
        ax,
        ay,
        0,
        WindowsInjector::abs_mouse_flags(MOUSEEVENTF_MOVE),
    )?;

    // WHEEL_DELTA = 120. Non-precise: line notches; precise: scale similarly.
    let scale = if w.precise { 1.0 } else { 120.0 };
    if w.delta_y != 0.0 {
        let data = (w.delta_y * scale).round() as i32;
        send_mouse(
            ax,
            ay,
            data as u32,
            WindowsInjector::abs_mouse_flags(MOUSEEVENTF_WHEEL),
        )?;
    }
    if w.delta_x != 0.0 {
        let data = (w.delta_x * scale).round() as i32;
        send_mouse(
            ax,
            ay,
            data as u32,
            WindowsInjector::abs_mouse_flags(MOUSEEVENTF_HWHEEL),
        )?;
    }
    Ok(())
}

fn inject_key(k: &KeyEvent) -> Result<(), InjectError> {
    if k.scancode > 0xFFFF {
        return Err(InjectError::InvalidEvent("scancode exceeds 16 bits"));
    }
    let mut flags = KEYEVENTF_SCANCODE;
    if k.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !k.pressed {
        flags |= KEYEVENTF_KEYUP;
    }
    send_key(k.scancode as u16, flags)
}

fn send_mouse(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> Result<(), InjectError> {
    let mut input = WinInput {
        input_type: INPUT_MOUSE,
        u: WinInputUnion {
            mi: MouseInput {
                dx,
                dy,
                mouse_data,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    };
    let sent = unsafe { SendInput(1, &mut input, std::mem::size_of::<WinInput>() as i32) };
    if sent != 1 {
        return Err(InjectError::Os(format!(
            "SendInput(mouse) returned {sent}, flags=0x{flags:x}"
        )));
    }
    Ok(())
}

fn send_key(scancode: u16, flags: u32) -> Result<(), InjectError> {
    let mut input = WinInput {
        input_type: INPUT_KEYBOARD,
        u: WinInputUnion {
            ki: KeybdInput {
                virtual_key: 0,
                scan_code: scancode,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    };
    let sent = unsafe { SendInput(1, &mut input, std::mem::size_of::<WinInput>() as i32) };
    if sent != 1 {
        return Err(InjectError::Os(format!(
            "SendInput(key) returned {sent}, scancode=0x{scancode:x} flags=0x{flags:x}"
        )));
    }
    Ok(())
}

// --- Minimal Win32 bindings (avoid heavy windows crate on MinGW CI when unused) ---
//
// Layout matches Win32 `INPUT` / `MOUSEINPUT` / `KEYBDINPUT` for x86_64.

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;

const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
/// Map absolute coords over the entire virtual desktop (not primary monitor only).
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;

/// Win32 `MOUSEINPUT` (snake_case field names; same layout as the C struct).
#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

/// Win32 `KEYBDINPUT`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KeybdInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
union WinInputUnion {
    mi: MouseInput,
    ki: KeybdInput,
}

/// Win32 `INPUT`.
#[repr(C)]
#[derive(Clone, Copy)]
struct WinInput {
    input_type: u32,
    u: WinInputUnion,
}

#[link(name = "user32")]
extern "system" {
    fn SendInput(c_inputs: u32, p_inputs: *mut WinInput, cb_size: i32) -> u32;
    fn GetSystemMetrics(n_index: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_abs_corners() {
        assert_eq!(WindowsInjector::map_abs(0.0, 0.0), (0, 0));
        assert_eq!(WindowsInjector::map_abs(1.0, 1.0), (65535, 65535));
        assert_eq!(WindowsInjector::map_abs(0.5, 0.5), (32768, 32768));
    }

    #[test]
    fn abs_mouse_flags_include_virtualdesk() {
        let f = WindowsInjector::abs_mouse_flags(MOUSEEVENTF_MOVE);
        assert_ne!(f & MOUSEEVENTF_ABSOLUTE, 0);
        assert_ne!(f & MOUSEEVENTF_VIRTUALDESK, 0);
        assert_ne!(f & MOUSEEVENTF_MOVE, 0);
    }

    #[test]
    fn open_reports_sendinput_backend() {
        let inj = WindowsInjector::open(1920, 1080).unwrap();
        assert_eq!(inj.backend_name(), "sendinput");
        assert_eq!(inj.screen_width, 1920);
        assert_eq!(inj.screen_height, 1080);
    }

    #[test]
    fn inject_key_rejects_huge_scancode() {
        let mut inj = WindowsInjector::open(100, 100).unwrap();
        let err = inj
            .inject(&InputEvent {
                client_ts_us: 1,
                seq: 1,
                payload: InputPayload::Key(KeyEvent {
                    scancode: 0x1_0000,
                    extended: false,
                    pressed: true,
                    modifiers: 0,
                }),
            })
            .unwrap_err();
        assert!(matches!(err, InjectError::InvalidEvent(_)));
    }
}

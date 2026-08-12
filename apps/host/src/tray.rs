//! Host tray / status surface (DESIGN G9).
//!
//! Owns a single [`TrayState`] shared by:
//! - **Console panel** (always available; CI-safe)
//! - **Status file** (JSON snapshot for external tools / GUI shells)
//! - **Windows notification area** (`NotifyIcon`, Windows only) with tooltip
//!   and OTP balloon when a Mode A code is minted
//!
//! Session visibility is projected from [`crate::chrome::HostSessionUx`] and
//! cannot be remote-disabled.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chrome::{HostSessionUx, SessionChrome};

/// Local actions raised by the tray menu (or tests). Polled by the WSS service.
#[derive(Debug, Clone, Default)]
pub struct TrayCommands {
    end_session: Arc<AtomicBool>,
    exit: Arc<AtomicBool>,
    /// How many times "Copy OTP" succeeded (tests / metrics).
    copy_otp_ok: Arc<AtomicU64>,
}

impl TrayCommands {
    /// Request local session end (kill-switch from tray).
    pub fn request_end_session(&self) {
        self.end_session.store(true, Ordering::SeqCst);
    }

    /// Request process exit after the current control step.
    pub fn request_exit(&self) {
        self.exit.store(true, Ordering::SeqCst);
    }

    /// Consume a pending end-session request (true once).
    pub fn take_end_session(&self) -> bool {
        self.end_session.swap(false, Ordering::SeqCst)
    }

    /// Consume a pending exit request (true once).
    pub fn take_exit(&self) -> bool {
        self.exit.swap(false, Ordering::SeqCst)
    }

    /// Number of successful clipboard OTP copies.
    pub fn copy_otp_count(&self) -> u64 {
        self.copy_otp_ok.load(Ordering::SeqCst)
    }

    fn note_copy_otp_ok(&self) {
        self.copy_otp_ok.fetch_add(1, Ordering::SeqCst);
    }
}

/// Snapshot of what the tray / status file should show.
#[derive(Debug, Clone, Default)]
pub struct TrayState {
    /// Enrollment display name.
    pub display_name: String,
    /// Device public id (10-digit) when enrolled.
    pub public_id: Option<String>,
    /// Latest Mode A OTP plaintext (host-local only).
    pub otp_code: Option<String>,
    /// Server-reported OTP expiry (ISO string or raw).
    pub otp_expires_at: Option<String>,
    /// Mandatory session indicator + chrome.
    pub ux: HostSessionUx,
    /// Last human-readable event (e.g. "otp_minted", "session_end").
    pub last_event: Option<String>,
}

impl TrayState {
    /// Short tooltip for the notification area (≤127 chars).
    pub fn tooltip(&self) -> String {
        let id = self.public_id.as_deref().unwrap_or("—");
        let chrome = self.ux.chrome();
        let line = match &chrome {
            SessionChrome::Active { session_id, label } => {
                let who = label.as_deref().unwrap_or("viewer");
                format!("RemoteLink IN SESSION · {who} · {session_id}")
            }
            SessionChrome::Inactive => {
                if let Some(ref otp) = self.otp_code {
                    format!("RemoteLink ready · id={id} · OTP {otp}")
                } else {
                    format!("RemoteLink ready · id={id}")
                }
            }
        };
        truncate_chars(&line, 127)
    }

    /// Multi-line console panel (logs / headless).
    pub fn console_panel(&self) -> String {
        let chrome = self.ux.chrome();
        let mut lines = vec![
            "── RemoteLink host tray ──".into(),
            format!("  name:    {}", self.display_name),
            format!(
                "  id:      {}",
                self.public_id.as_deref().unwrap_or("(not enrolled)")
            ),
            format!(
                "  chrome:  {} ({})",
                chrome.status_label(),
                self.ux.status_line()
            ),
        ];
        match (&self.otp_code, &self.otp_expires_at) {
            (Some(code), Some(exp)) => {
                lines.push(format!("  otp:     {code}  (expires {exp})"));
            }
            (Some(code), None) => lines.push(format!("  otp:     {code}")),
            _ => lines.push("  otp:     (none)".into()),
        }
        if let Some(ev) = &self.last_event {
            lines.push(format!("  event:   {ev}"));
        }
        lines.push("──────────────────────────".into());
        lines.join("\n")
    }

    /// JSON object for the status file (stable keys for tooling).
    pub fn to_status_json(&self) -> serde_json::Value {
        let chrome = self.ux.chrome();
        let (session_id, viewer_label, chrome_label) = match &chrome {
            SessionChrome::Active { session_id, label } => {
                (Some(session_id.clone()), label.clone(), "Active")
            }
            SessionChrome::Inactive => (None, None, "Inactive"),
        };
        serde_json::json!({
            "display_name": self.display_name,
            "public_id": self.public_id,
            "otp_code": self.otp_code,
            "otp_expires_at": self.otp_expires_at,
            "chrome": chrome_label,
            "session_id": session_id,
            "viewer_label": viewer_label,
            "connected": self.ux.indicator().connected,
            "active": self.ux.indicator().active,
            "last_event": self.last_event,
            "tooltip": self.tooltip(),
        })
    }
}

/// Shared host tray: console + status file + optional Windows NotifyIcon.
#[derive(Debug)]
pub struct HostTray {
    state: Arc<Mutex<TrayState>>,
    commands: TrayCommands,
    /// When true, print [`TrayState::console_panel`] on each update.
    console: bool,
    status_path: PathBuf,
    #[cfg(windows)]
    win: Option<win::WinNotifyTray>,
}

impl HostTray {
    /// Create a tray. When `enable_os_tray` is false, only console + status file.
    pub fn new(
        display_name: impl Into<String>,
        status_path: PathBuf,
        console: bool,
        enable_os_tray: bool,
    ) -> Self {
        let state = Arc::new(Mutex::new(TrayState {
            display_name: display_name.into(),
            ..TrayState::default()
        }));
        let commands = TrayCommands::default();
        #[cfg(windows)]
        let win = if enable_os_tray {
            match win::WinNotifyTray::spawn(Arc::clone(&state), commands.clone()) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("ws-host: Windows tray unavailable ({e}); console+file only");
                    None
                }
            }
        } else {
            None
        };
        #[cfg(not(windows))]
        let _ = enable_os_tray;

        let tray = Self {
            state,
            commands,
            console,
            status_path,
            #[cfg(windows)]
            win,
        };
        tray.publish("tray_start");
        tray
    }

    /// Console + file only (tests / CI).
    pub fn console_only(display_name: impl Into<String>, status_path: PathBuf) -> Self {
        Self::new(display_name, status_path, true, false)
    }

    /// Path of the JSON status file.
    pub fn status_path(&self) -> &Path {
        &self.status_path
    }

    /// Clone of current state (tests).
    pub fn snapshot(&self) -> TrayState {
        self.state.lock().expect("tray mutex").clone()
    }

    /// Set enrolled device identity.
    pub fn set_identity(&self, public_id: &str, display_name: Option<&str>) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.public_id = Some(public_id.into());
            if let Some(n) = display_name {
                if !n.is_empty() {
                    g.display_name = n.into();
                }
            }
        }
        self.publish("identity");
    }

    /// Publish a freshly minted Mode A OTP (plaintext stays host-local).
    pub fn set_otp(&self, code: &str, expires_at: impl Into<String>) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.otp_code = Some(code.into());
            g.otp_expires_at = Some(expires_at.into());
        }
        self.publish("otp_minted");
        #[cfg(windows)]
        if let Some(ref w) = self.win {
            w.balloon_otp(code);
        }
    }

    /// Clear OTP after it should no longer be shown (optional).
    pub fn clear_otp(&self) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.otp_code = None;
            g.otp_expires_at = None;
        }
        self.publish("otp_cleared");
    }

    /// Session bound (accepted); chrome not yet Active until media starts.
    pub fn begin_session(&self, session_id: &str, viewer_label: Option<String>) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            let _ = g.ux.begin_session(session_id, viewer_label);
        }
        self.publish("session_begin");
    }

    /// Media/control live — mandatory Active chrome.
    pub fn mark_session_active(&self) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.ux.mark_active();
        }
        self.publish("session_active");
    }

    /// Session ended (detach / complete / kill).
    pub fn end_session(&self) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.ux.end_session();
        }
        self.publish("session_end");
    }

    /// Local kill-switch: clear session chrome.
    pub fn apply_kill(&self) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.ux.apply_kill();
        }
        self.publish("kill_switch");
    }

    /// Shared command flags (tray menu → service loop).
    pub fn commands(&self) -> &TrayCommands {
        &self.commands
    }

    /// Consume tray "End session" (true once).
    pub fn take_end_session(&self) -> bool {
        self.commands.take_end_session()
    }

    /// Consume tray "Exit" (true once).
    pub fn take_exit(&self) -> bool {
        self.commands.take_exit()
    }

    /// Test helper: request end session as if the menu was clicked.
    pub fn request_end_session(&self) {
        self.commands.request_end_session();
    }

    /// Test helper: copy OTP text using the same path as the menu (no OS clipboard).
    pub fn otp_for_clipboard(&self) -> Option<String> {
        self.state.lock().ok().and_then(|g| g.otp_code.clone())
    }

    fn publish(&self, event: &str) {
        {
            let mut g = self.state.lock().expect("tray mutex");
            g.last_event = Some(event.into());
            if self.console {
                println!("{}", g.console_panel());
            }
            if let Err(e) = write_status_file(&self.status_path, &g) {
                eprintln!("ws-host: tray status file: {e}");
            }
        }
        #[cfg(windows)]
        if let Some(ref w) = self.win {
            w.refresh_tooltip();
        }
    }
}

impl Drop for HostTray {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            self.win.take();
        }
    }
}

fn write_status_file(path: &Path, state: &TrayState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    let body = serde_json::to_string_pretty(&state.to_status_json()).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!(
        "tmp-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Default status file next to credential file (or CWD).
pub fn default_status_path(creds_path: &Path) -> PathBuf {
    creds_path
        .parent()
        .map(|p| p.join(".remotelink-host-status.json"))
        .unwrap_or_else(|| PathBuf::from(".remotelink-host-status.json"))
}

#[cfg(windows)]
mod win {
    //! Minimal Shell_NotifyIcon tray (tooltip + OTP balloon).

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};

    use super::{TrayCommands, TrayState};

    const WM_APP_REFRESH: u32 = 0x8000 + 40;
    const WM_APP_BALLOON: u32 = 0x8000 + 41;
    const WM_APP_QUIT: u32 = 0x8000 + 42;
    const WM_TRAYICON: u32 = 0x0400 + 1; // WM_USER+1
    const WM_COMMAND: u32 = 0x0111;
    const WM_RBUTTONUP: isize = 0x0205;
    const WM_LBUTTONDBLCLK: isize = 0x0203;
    const WM_CONTEXTMENU: isize = 0x007B;
    const TRAY_UID: u32 = 1;
    const NIM_ADD: u32 = 0x0000_0000;
    const NIM_MODIFY: u32 = 0x0000_0001;
    const NIM_DELETE: u32 = 0x0000_0002;
    const NIF_MESSAGE: u32 = 0x0000_0001;
    const NIF_ICON: u32 = 0x0000_0002;
    const NIF_TIP: u32 = 0x0000_0004;
    const NIF_INFO: u32 = 0x0000_0010;
    const NIIF_INFO: u32 = 0x0000_0001;
    const IDI_APPLICATION: isize = 32512;
    const IMAGE_ICON: u32 = 1;
    const LR_SHARED: u32 = 0x0000_8000;
    const WS_EX_NOACTIVATE: u32 = 0x0800_0000;
    const HWND_MESSAGE: isize = -3;
    const GWLP_USERDATA: i32 = -21;
    const WM_DESTROY: u32 = 0x0002;
    const IDM_COPY_OTP: usize = 1001;
    const IDM_END_SESSION: usize = 1002;
    const IDM_EXIT: usize = 1003;
    const MF_STRING: u32 = 0x0000_0000;
    const MF_SEPARATOR: u32 = 0x0000_0800;
    const MF_GRAYED: u32 = 0x0000_0001;
    const TPM_RIGHTBUTTON: u32 = 0x0002;
    const TPM_RETURNCMD: u32 = 0x0100;
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[repr(C)]
    struct NotifyIconDataW {
        cb_size: u32,
        hwnd: *mut core::ffi::c_void,
        uid: u32,
        u_flags: u32,
        u_callback_message: u32,
        h_icon: *mut core::ffi::c_void,
        sz_tip: [u16; 128],
        dw_state: u32,
        dw_state_mask: u32,
        sz_info: [u16; 256],
        u_timeout_or_version: u32,
        sz_info_title: [u16; 64],
        dw_info_flags: u32,
        guid_item: [u8; 16],
        h_balloon_icon: *mut core::ffi::c_void,
    }

    #[repr(C)]
    struct WndClassW {
        style: u32,
        lpfn_wnd_proc:
            Option<unsafe extern "system" fn(*mut core::ffi::c_void, u32, usize, isize) -> isize>,
        cb_cls_extra: i32,
        cb_wnd_extra: i32,
        h_instance: *mut core::ffi::c_void,
        h_icon: *mut core::ffi::c_void,
        h_cursor: *mut core::ffi::c_void,
        hbr_background: *mut core::ffi::c_void,
        lpsz_menu_name: *const u16,
        lpsz_class_name: *const u16,
    }

    #[repr(C)]
    struct Msg {
        hwnd: *mut core::ffi::c_void,
        message: u32,
        w_param: usize,
        l_param: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterClassW(lp_wnd_class: *const WndClassW) -> u16;
        fn CreateWindowExW(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            h_wnd_parent: *mut core::ffi::c_void,
            h_menu: *mut core::ffi::c_void,
            h_instance: *mut core::ffi::c_void,
            lp_param: *mut core::ffi::c_void,
        ) -> *mut core::ffi::c_void;
        fn DefWindowProcW(
            h_wnd: *mut core::ffi::c_void,
            msg: u32,
            w_param: usize,
            l_param: isize,
        ) -> isize;
        fn GetMessageW(
            lp_msg: *mut Msg,
            h_wnd: *mut core::ffi::c_void,
            w_msg_filter_min: u32,
            w_msg_filter_max: u32,
        ) -> i32;
        fn TranslateMessage(lp_msg: *const Msg) -> i32;
        fn DispatchMessageW(lp_msg: *const Msg) -> isize;
        fn PostMessageW(
            h_wnd: *mut core::ffi::c_void,
            msg: u32,
            w_param: usize,
            l_param: isize,
        ) -> i32;
        fn DestroyWindow(h_wnd: *mut core::ffi::c_void) -> i32;
        fn LoadImageW(
            h_inst: *mut core::ffi::c_void,
            name: isize,
            type_: u32,
            cx: i32,
            cy: i32,
            fu_load: u32,
        ) -> *mut core::ffi::c_void;
        fn GetModuleHandleW(lp_module_name: *const u16) -> *mut core::ffi::c_void;
        fn GetWindowLongPtrW(h_wnd: *mut core::ffi::c_void, n_index: i32) -> isize;
        fn SetWindowLongPtrW(
            h_wnd: *mut core::ffi::c_void,
            n_index: i32,
            dw_new_long: isize,
        ) -> isize;
        fn PostQuitMessage(n_exit_code: i32);
        fn CreatePopupMenu() -> *mut core::ffi::c_void;
        fn DestroyMenu(h_menu: *mut core::ffi::c_void) -> i32;
        fn AppendMenuW(
            h_menu: *mut core::ffi::c_void,
            u_flags: u32,
            u_id_new_item: usize,
            lp_new_item: *const u16,
        ) -> i32;
        fn TrackPopupMenu(
            h_menu: *mut core::ffi::c_void,
            u_flags: u32,
            x: i32,
            y: i32,
            n_reserved: i32,
            h_wnd: *mut core::ffi::c_void,
            prc_rect: *const core::ffi::c_void,
        ) -> i32;
        fn GetCursorPos(lp_point: *mut Point) -> i32;
        fn SetForegroundWindow(h_wnd: *mut core::ffi::c_void) -> i32;
        fn OpenClipboard(h_wnd_new_owner: *mut core::ffi::c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(u_format: u32, h_mem: *mut core::ffi::c_void)
            -> *mut core::ffi::c_void;
    }

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }

    #[link(name = "shell32")]
    extern "system" {
        fn Shell_NotifyIconW(dw_message: u32, lp_data: *mut NotifyIconDataW) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalAlloc(u_flags: u32, dw_bytes: usize) -> *mut core::ffi::c_void;
        fn GlobalLock(h_mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
        fn GlobalUnlock(h_mem: *mut core::ffi::c_void) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        std::ffi::OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    fn copy_wide(dst: &mut [u16], src: &str) {
        let wide: Vec<u16> = src.encode_utf16().collect();
        let n = wide.len().min(dst.len().saturating_sub(1));
        dst[..n].copy_from_slice(&wide[..n]);
        if n < dst.len() {
            dst[n] = 0;
        }
        for x in dst.iter_mut().skip(n + 1) {
            *x = 0;
        }
    }

    struct TrayThreadCtx {
        state: Arc<Mutex<TrayState>>,
        commands: TrayCommands,
        balloon_otp: Arc<Mutex<Option<String>>>,
        hwnd: *mut core::ffi::c_void,
        h_icon: *mut core::ffi::c_void,
    }

    // SAFETY: hwnd/icon only touched on the tray UI thread after creation.
    unsafe impl Send for TrayThreadCtx {}

    unsafe extern "system" fn wnd_proc(
        hwnd: *mut core::ffi::c_void,
        msg: u32,
        w_param: usize,
        l_param: isize,
    ) -> isize {
        let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayThreadCtx;
        if ctx_ptr.is_null() {
            return DefWindowProcW(hwnd, msg, w_param, l_param);
        }
        let ctx = &*ctx_ptr;
        match msg {
            m if m == WM_APP_REFRESH => {
                modify_icon(ctx, false);
                0
            }
            m if m == WM_APP_BALLOON => {
                modify_icon(ctx, true);
                0
            }
            m if m == WM_APP_QUIT => {
                delete_icon(ctx);
                DestroyWindow(hwnd);
                0
            }
            m if m == WM_TRAYICON => {
                if l_param == WM_RBUTTONUP || l_param == WM_CONTEXTMENU {
                    show_context_menu(ctx);
                } else if l_param == WM_LBUTTONDBLCLK {
                    // Double-click: copy OTP when available.
                    let _ = copy_otp_to_clipboard(ctx);
                }
                0
            }
            m if m == WM_COMMAND => {
                handle_menu_command(ctx, w_param & 0xffff);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w_param, l_param),
        }
    }

    fn show_context_menu(ctx: &TrayThreadCtx) {
        let has_otp = ctx
            .state
            .lock()
            .map(|g| g.otp_code.is_some())
            .unwrap_or(false);
        let in_session = ctx
            .state
            .lock()
            .map(|g| g.ux.chrome().is_active() || g.ux.indicator().is_connected())
            .unwrap_or(false);

        let label_copy = to_wide("Copy OTP");
        let label_end = to_wide("End session");
        let label_exit = to_wide("Exit host");
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            let copy_flags = if has_otp {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            let end_flags = if in_session {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            AppendMenuW(menu, copy_flags, IDM_COPY_OTP, label_copy.as_ptr());
            AppendMenuW(menu, end_flags, IDM_END_SESSION, label_end.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, IDM_EXIT, label_exit.as_ptr());

            let mut pt = Point { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            SetForegroundWindow(ctx.hwnd);
            let cmd = TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD,
                pt.x,
                pt.y,
                0,
                ctx.hwnd,
                std::ptr::null(),
            ) as usize;
            DestroyMenu(menu);
            if cmd != 0 {
                handle_menu_command(ctx, cmd);
            }
        }
    }

    fn handle_menu_command(ctx: &TrayThreadCtx, cmd: usize) {
        match cmd {
            IDM_COPY_OTP => {
                if copy_otp_to_clipboard(ctx) {
                    // Balloon confirms copy.
                    if let Ok(g) = ctx.state.lock() {
                        if let Some(ref code) = g.otp_code {
                            if let Ok(mut b) = ctx.balloon_otp.lock() {
                                *b = Some(code.clone());
                            }
                            // Reuse balloon path with a friendly title via modify.
                        }
                    }
                    // Show a short tip via NIF_INFO.
                    show_info_balloon(ctx, "RemoteLink", "OTP copied to clipboard.");
                    ctx.commands.note_copy_otp_ok();
                }
            }
            IDM_END_SESSION => {
                ctx.commands.request_end_session();
                show_info_balloon(ctx, "RemoteLink", "Ending session…");
            }
            IDM_EXIT => {
                ctx.commands.request_exit();
                // Also request end so a live session tears down first.
                ctx.commands.request_end_session();
            }
            _ => {}
        }
    }

    fn copy_otp_to_clipboard(ctx: &TrayThreadCtx) -> bool {
        let code = match ctx.state.lock().ok().and_then(|g| g.otp_code.clone()) {
            Some(c) if !c.is_empty() => c,
            _ => return false,
        };
        set_clipboard_text(&code)
    }

    fn set_clipboard_text(text: &str) -> bool {
        let wide = to_wide(text);
        let bytes = wide.len() * 2;
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return false;
            }
            EmptyClipboard();
            let h_mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if h_mem.is_null() {
                CloseClipboard();
                return false;
            }
            let ptr = GlobalLock(h_mem);
            if ptr.is_null() {
                CloseClipboard();
                return false;
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, bytes);
            GlobalUnlock(h_mem);
            let ok = !SetClipboardData(CF_UNICODETEXT, h_mem).is_null();
            CloseClipboard();
            ok
        }
    }

    fn show_info_balloon(ctx: &TrayThreadCtx, title: &str, body: &str) {
        let tip = tip_of(ctx);
        let mut nid = fill_nid(ctx, &tip);
        nid.u_flags |= NIF_INFO;
        nid.dw_info_flags = NIIF_INFO;
        nid.u_timeout_or_version = 8_000;
        copy_wide(&mut nid.sz_info_title, title);
        copy_wide(&mut nid.sz_info, body);
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &mut nid);
        }
    }

    fn tip_of(ctx: &TrayThreadCtx) -> String {
        ctx.state
            .lock()
            .map(|g| g.tooltip())
            .unwrap_or_else(|_| "RemoteLink".into())
    }

    fn fill_nid(ctx: &TrayThreadCtx, tip: &str) -> NotifyIconDataW {
        let mut nid: NotifyIconDataW = unsafe { std::mem::zeroed() };
        nid.cb_size = std::mem::size_of::<NotifyIconDataW>() as u32;
        nid.hwnd = ctx.hwnd;
        nid.uid = TRAY_UID;
        nid.u_flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.u_callback_message = WM_TRAYICON;
        nid.h_icon = ctx.h_icon;
        copy_wide(&mut nid.sz_tip, tip);
        nid
    }

    fn modify_icon(ctx: &TrayThreadCtx, with_balloon: bool) {
        let tip = tip_of(ctx);
        let mut nid = fill_nid(ctx, &tip);
        if with_balloon {
            if let Ok(mut g) = ctx.balloon_otp.lock() {
                if let Some(code) = g.take() {
                    nid.u_flags |= NIF_INFO;
                    nid.dw_info_flags = NIIF_INFO;
                    nid.u_timeout_or_version = 15_000;
                    copy_wide(&mut nid.sz_info_title, "RemoteLink OTP");
                    copy_wide(
                        &mut nid.sz_info,
                        &format!("Mode A code: {code}\nEnter this in the viewer."),
                    );
                }
            }
        }
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &mut nid);
        }
    }

    fn delete_icon(ctx: &TrayThreadCtx) {
        let mut nid = fill_nid(ctx, "");
        unsafe {
            Shell_NotifyIconW(NIM_DELETE, &mut nid);
        }
    }

    fn add_icon(ctx: &TrayThreadCtx) {
        let tip = tip_of(ctx);
        let mut nid = fill_nid(ctx, &tip);
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &mut nid);
        }
    }

    /// Handle to a live notification-area icon + message pump thread.
    ///
    /// `hwnd` is stored as `usize` so the handle is `Send + Sync` (required for
    /// holding the tray across `await` points in the WSS service loop).
    pub struct WinNotifyTray {
        hwnd: usize,
        join: Option<JoinHandle<()>>,
        alive: Arc<AtomicBool>,
        balloon_otp: Arc<Mutex<Option<String>>>,
    }

    impl std::fmt::Debug for WinNotifyTray {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("WinNotifyTray")
                .field("alive", &self.alive.load(Ordering::SeqCst))
                .finish()
        }
    }

    impl WinNotifyTray {
        pub fn spawn(state: Arc<Mutex<TrayState>>, commands: TrayCommands) -> Result<Self, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
            let balloon_otp = Arc::new(Mutex::new(None));
            let balloon_t = Arc::clone(&balloon_otp);
            let alive = Arc::new(AtomicBool::new(true));

            let join = thread::Builder::new()
                .name("remotelink-tray".into())
                .spawn(move || {
                    let class_name =
                        to_wide(&format!("RemoteLinkHostTrayClass-{}", std::process::id()));
                    let h_instance = unsafe { GetModuleHandleW(std::ptr::null()) };
                    let wc = WndClassW {
                        style: 0,
                        lpfn_wnd_proc: Some(wnd_proc),
                        cb_cls_extra: 0,
                        cb_wnd_extra: 0,
                        h_instance,
                        h_icon: std::ptr::null_mut(),
                        h_cursor: std::ptr::null_mut(),
                        hbr_background: std::ptr::null_mut(),
                        lpsz_menu_name: std::ptr::null(),
                        lpsz_class_name: class_name.as_ptr(),
                    };
                    unsafe {
                        let _ = RegisterClassW(&wc);
                    }

                    let window_title = to_wide("RemoteLink");
                    let hwnd = unsafe {
                        CreateWindowExW(
                            WS_EX_NOACTIVATE,
                            class_name.as_ptr(),
                            window_title.as_ptr(),
                            0,
                            0,
                            0,
                            0,
                            0,
                            HWND_MESSAGE as *mut core::ffi::c_void,
                            std::ptr::null_mut(),
                            h_instance,
                            std::ptr::null_mut(),
                        )
                    };
                    if hwnd.is_null() {
                        let _ = tx.send(Err("CreateWindowExW failed for tray".into()));
                        return;
                    }

                    let h_icon = unsafe {
                        LoadImageW(
                            std::ptr::null_mut(),
                            IDI_APPLICATION,
                            IMAGE_ICON,
                            0,
                            0,
                            LR_SHARED,
                        )
                    };

                    let ctx = Box::into_raw(Box::new(TrayThreadCtx {
                        state,
                        commands,
                        balloon_otp: balloon_t,
                        hwnd,
                        h_icon,
                    }));
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);
                    }
                    add_icon(unsafe { &*ctx });
                    let _ = tx.send(Ok(hwnd as usize));

                    let mut msg: Msg = unsafe { std::mem::zeroed() };
                    loop {
                        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
                        if r == 0 || r == -1 {
                            break;
                        }
                        unsafe {
                            TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }
                    // Free ctx after message loop ends.
                    unsafe {
                        drop(Box::from_raw(ctx));
                    }
                })
                .map_err(|e| e.to_string())?;

            let hwnd = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(|_| "tray thread did not start".to_string())??;

            Ok(Self {
                hwnd,
                join: Some(join),
                alive,
                balloon_otp,
            })
        }

        pub fn refresh_tooltip(&self) {
            if self.alive.load(Ordering::SeqCst) && self.hwnd != 0 {
                unsafe {
                    PostMessageW(self.hwnd as *mut core::ffi::c_void, WM_APP_REFRESH, 0, 0);
                }
            }
        }

        pub fn balloon_otp(&self, code: &str) {
            if let Ok(mut g) = self.balloon_otp.lock() {
                *g = Some(code.into());
            }
            if self.alive.load(Ordering::SeqCst) && self.hwnd != 0 {
                unsafe {
                    PostMessageW(self.hwnd as *mut core::ffi::c_void, WM_APP_BALLOON, 0, 0);
                }
            }
        }
    }

    impl Drop for WinNotifyTray {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
            if self.hwnd != 0 {
                unsafe {
                    PostMessageW(self.hwnd as *mut core::ffi::c_void, WM_APP_QUIT, 0, 0);
                }
            }
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
            self.hwnd = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_status() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("rl-tray-status-{n}.json"))
    }

    #[test]
    fn tray_otp_and_session_update_status_file() {
        let path = tmp_status();
        let tray = HostTray::console_only("test-host", path.clone());
        tray.set_identity("1234567890", Some("test-host"));
        tray.set_otp("654321", "2099-01-01T00:00:00Z");
        let snap = tray.snapshot();
        assert_eq!(snap.public_id.as_deref(), Some("1234567890"));
        assert_eq!(snap.otp_code.as_deref(), Some("654321"));
        assert!(snap.tooltip().contains("654321"));
        assert!(snap.console_panel().contains("654321"));

        tray.begin_session("sess-1", Some("viewer".into()));
        tray.mark_session_active();
        let snap = tray.snapshot();
        assert!(snap.ux.chrome().is_active());
        assert!(snap.tooltip().contains("IN SESSION"));

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("654321"));
        assert!(body.contains("Active"));
        assert!(body.contains("sess-1"));

        tray.end_session();
        assert!(!tray.snapshot().ux.chrome().is_active());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn tooltip_truncated_to_127() {
        let s = TrayState {
            display_name: "x".into(),
            public_id: Some("1".into()),
            otp_code: Some("9".repeat(200)),
            ..TrayState::default()
        };
        assert!(s.tooltip().chars().count() <= 127);
    }

    #[test]
    fn tray_commands_end_session_and_exit() {
        let path = tmp_status();
        let tray = HostTray::console_only("cmd-host", path.clone());
        tray.set_otp("111222", "soon");
        assert_eq!(tray.otp_for_clipboard().as_deref(), Some("111222"));
        assert!(!tray.take_end_session());
        tray.request_end_session();
        assert!(tray.take_end_session());
        assert!(!tray.take_end_session());
        tray.commands().request_exit();
        assert!(tray.take_exit());
        let _ = fs::remove_file(&path);
    }
}

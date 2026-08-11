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
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::chrome::{HostSessionUx, SessionChrome};

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
        #[cfg(windows)]
        let win = if enable_os_tray {
            match win::WinNotifyTray::spawn(Arc::clone(&state)) {
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

    use super::TrayState;

    const WM_APP_REFRESH: u32 = 0x8000 + 40;
    const WM_APP_BALLOON: u32 = 0x8000 + 41;
    const WM_APP_QUIT: u32 = 0x8000 + 42;
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
        lpfn_wnd_proc: Option<
            unsafe extern "system" fn(*mut core::ffi::c_void, u32, usize, isize) -> isize,
        >,
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
    }

    #[link(name = "shell32")]
    extern "system" {
        fn Shell_NotifyIconW(dw_message: u32, lp_data: *mut NotifyIconDataW) -> i32;
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
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w_param, l_param),
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
        nid.u_callback_message = 0x0400 + 1;
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
        pub fn spawn(state: Arc<Mutex<TrayState>>) -> Result<Self, String> {
            let (tx, rx) = std::sync::mpsc::channel::<Result<usize, String>>();
            let balloon_otp = Arc::new(Mutex::new(None));
            let balloon_t = Arc::clone(&balloon_otp);
            let alive = Arc::new(AtomicBool::new(true));

            let join = thread::Builder::new()
                .name("remotelink-tray".into())
                .spawn(move || {
                    let class_name = to_wide(&format!(
                        "RemoteLinkHostTrayClass-{}",
                        std::process::id()
                    ));
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

                    let hwnd = unsafe {
                        CreateWindowExW(
                            WS_EX_NOACTIVATE,
                            class_name.as_ptr(),
                            to_wide("RemoteLink").as_ptr(),
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
                    PostMessageW(
                        self.hwnd as *mut core::ffi::c_void,
                        WM_APP_REFRESH,
                        0,
                        0,
                    );
                }
            }
        }

        pub fn balloon_otp(&self, code: &str) {
            if let Ok(mut g) = self.balloon_otp.lock() {
                *g = Some(code.into());
            }
            if self.alive.load(Ordering::SeqCst) && self.hwnd != 0 {
                unsafe {
                    PostMessageW(
                        self.hwnd as *mut core::ffi::c_void,
                        WM_APP_BALLOON,
                        0,
                        0,
                    );
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
}

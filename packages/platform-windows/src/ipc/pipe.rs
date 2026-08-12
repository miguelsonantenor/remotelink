//! Windows named-pipe backend for control IPC (KD5).
//!
//! Security posture (v1):
//! - **Local only**: `PIPE_REJECT_REMOTE_CLIENTS` so the pipe cannot be opened over SMB.
//! - **Restrictive DACL** (SDDL): Local System, Builtin Administrators, and the
//!   pipe owner (creating process) get full access; everyone else is denied.
//! - Same length-prefixed JSON framing as the TCP backend (byte-mode pipe).
//!
//! Boot-secret binding (optional second factor in the first control frame) can
//! layer on later without changing the transport API.

#![cfg(windows)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{FromRawHandle, IntoRawHandle, RawHandle};
use std::ptr;
use std::time::Duration;

use super::transport::TransportError;

/// Default pipe leaf name (full path is `\\.\pipe\<name>`).
pub const DEFAULT_PIPE_NAME: &str = "remotelink-host-control";

/// SDDL: protected DACL — SYSTEM + Builtin Admins + Owner only (Generic All).
///
/// `D:P` = protected DACL (no inherited ACEs). This is the production default
/// for a service↔agent control pipe on a single machine.
pub const CONTROL_PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

// --- Win32 constants ---

const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

const SDDL_REVISION_1: u32 = 1;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const INVALID_HANDLE_VALUE: isize = -1;

#[repr(C)]
struct SecurityAttributes {
    n_length: u32,
    lp_security_descriptor: *mut core::ffi::c_void,
    b_inherit_handle: i32,
}

#[repr(C)]
struct CommTimeouts {
    read_interval_timeout: u32,
    read_total_timeout_multiplier: u32,
    read_total_timeout_constant: u32,
    write_total_timeout_multiplier: u32,
    write_total_timeout_constant: u32,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        lp_name: *const u16,
        dw_open_mode: u32,
        dw_pipe_mode: u32,
        n_max_instances: u32,
        n_out_buffer_size: u32,
        n_in_buffer_size: u32,
        n_default_time_out: u32,
        lp_security_attributes: *const SecurityAttributes,
    ) -> *mut core::ffi::c_void;

    fn ConnectNamedPipe(
        h_named_pipe: *mut core::ffi::c_void,
        lp_overlapped: *mut core::ffi::c_void,
    ) -> i32;

    fn CreateFileW(
        lp_file_name: *const u16,
        dw_desired_access: u32,
        dw_share_mode: u32,
        lp_security_attributes: *const SecurityAttributes,
        dw_creation_disposition: u32,
        dw_flags_and_attributes: u32,
        h_template_file: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;

    fn WaitNamedPipeW(lp_named_pipe_name: *const u16, n_time_out: u32) -> i32;

    fn CloseHandle(h_object: *mut core::ffi::c_void) -> i32;

    fn SetCommTimeouts(
        h_file: *mut core::ffi::c_void,
        lp_comm_timeouts: *const CommTimeouts,
    ) -> i32;

    fn GetCommTimeouts(h_file: *mut core::ffi::c_void, lp_comm_timeouts: *mut CommTimeouts) -> i32;

    fn FlushFileBuffers(h_file: *mut core::ffi::c_void) -> i32;

    fn GetLastError() -> u32;
}

#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        string_security_descriptor: *const u16,
        string_sd_revision: u32,
        security_descriptor: *mut *mut core::ffi::c_void,
        security_descriptor_size: *mut u32,
    ) -> i32;

    fn LocalFree(h_mem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
}

/// Normalize a user-supplied pipe name into `\\.\pipe\…`.
pub fn normalize_pipe_path(name_or_path: &str) -> String {
    let s = name_or_path.trim().replace('/', "\\");
    if s.to_ascii_lowercase().starts_with(r"\\.\pipe\") {
        s
    } else if let Some(rest) = s.strip_prefix(r"\\.\pipe\") {
        format!(r"\\.\pipe\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\.\PIPE\") {
        format!(r"\\.\pipe\{rest}")
    } else {
        // Bare leaf name.
        let leaf = s.trim_start_matches('\\');
        format!(r"\\.\pipe\{leaf}")
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// RAII security descriptor from SDDL (LocalFree on drop).
struct SddlDescriptor {
    ptr: *mut core::ffi::c_void,
}

impl SddlDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = to_wide(sddl);
        let mut sd: *mut core::ffi::c_void = ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            return Err(io::Error::from_raw_os_error(
                unsafe { GetLastError() } as i32
            ));
        }
        Ok(Self { ptr: sd })
    }

    fn as_ptr(&self) -> *mut core::ffi::c_void {
        self.ptr
    }
}

impl Drop for SddlDescriptor {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                LocalFree(self.ptr);
            }
            self.ptr = ptr::null_mut();
        }
    }
}

/// Create one duplex server pipe instance (not yet connected).
fn create_server_instance(path: &str, first_instance: bool) -> io::Result<*mut core::ffi::c_void> {
    let wide = to_wide(path);
    let sd = SddlDescriptor::from_sddl(CONTROL_PIPE_SDDL)?;
    let sa = SecurityAttributes {
        n_length: std::mem::size_of::<SecurityAttributes>() as u32,
        lp_security_descriptor: sd.as_ptr(),
        b_inherit_handle: 0,
    };

    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first_instance {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            256 * 1024, // out buffer
            256 * 1024, // in buffer
            5_000,      // default client timeout ms
            &sa,
        )
    };
    if handle as isize == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return Err(io::Error::other(format!(
            "CreateNamedPipeW({path}) failed: Win32 error {err} \
             (is another process holding the first instance?)"
        )));
    }
    // Keep SD alive until CreateNamedPipeW returns — drop here is fine.
    drop(sd);
    Ok(handle)
}

fn connect_server_instance(handle: *mut core::ffi::c_void) -> io::Result<()> {
    let ok = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) };
    if ok != 0 {
        return Ok(());
    }
    let err = unsafe { GetLastError() };
    // Client already connected between CreateNamedPipe and ConnectNamedPipe.
    if err == ERROR_PIPE_CONNECTED {
        return Ok(());
    }
    Err(io::Error::from_raw_os_error(err as i32))
}

fn handle_to_file(handle: *mut core::ffi::c_void) -> File {
    unsafe { File::from_raw_handle(handle as RawHandle) }
}

/// Server-side multi-accept state for one named-pipe path.
pub struct NamedPipeListener {
    path: String,
    /// Next server instance waiting for `ConnectNamedPipe`.
    pending: Option<*mut core::ffi::c_void>,
    first_created: bool,
}

// SAFETY: handles are exclusively owned by this listener / moved into streams.
unsafe impl Send for NamedPipeListener {}

impl NamedPipeListener {
    /// Bind the first pipe instance (fails if another first-instance exists).
    pub fn bind(path: &str) -> Result<Self, TransportError> {
        let path = normalize_pipe_path(path);
        let handle = create_server_instance(&path, true)?;
        Ok(Self {
            path,
            pending: Some(handle),
            first_created: true,
        })
    }

    /// Bound pipe path (`\\.\pipe\…`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Block until a client connects; recreate the next server instance.
    pub fn accept(&mut self) -> Result<File, TransportError> {
        let handle = match self.pending.take() {
            Some(h) => h,
            None => create_server_instance(&self.path, !self.first_created)?,
        };
        self.first_created = true;

        if let Err(e) = connect_server_instance(handle) {
            unsafe {
                CloseHandle(handle);
            }
            // Recreate pending for the next accept attempt.
            self.pending = create_server_instance(&self.path, false).ok();
            return Err(e.into());
        }

        // Prepare next instance so a subsequent client can connect while we
        // serve the current one (classic Windows multi-instance pattern).
        // Best-effort; accept will recreate later if this fails.
        self.pending = create_server_instance(&self.path, false).ok();

        Ok(handle_to_file(handle))
    }
}

impl Drop for NamedPipeListener {
    fn drop(&mut self) {
        if let Some(h) = self.pending.take() {
            unsafe {
                CloseHandle(h);
            }
        }
    }
}

/// Connect as a client to an existing server pipe.
pub fn connect_named_pipe(path: &str) -> Result<File, TransportError> {
    let path = normalize_pipe_path(path);
    let wide = to_wide(&path);

    // Retry on ERROR_PIPE_BUSY (server instance not yet free).
    for attempt in 0..40 {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle as isize != INVALID_HANDLE_VALUE {
            return Ok(handle_to_file(handle));
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY || err == ERROR_FILE_NOT_FOUND {
            // Wait up to 1s for a server instance, then retry.
            let _ = unsafe { WaitNamedPipeW(wide.as_ptr(), 1_000) };
            if attempt + 1 < 40 {
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
        }
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("CreateFileW({path}) failed: Win32 error {err}"),
        )
        .into());
    }
    Err(TransportError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("timed out connecting to named pipe {path}"),
    )))
}

fn duration_to_ms(d: Option<Duration>) -> u32 {
    d.map(|dur| u32::try_from(dur.as_millis()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

fn get_comm_timeouts(file: &File) -> io::Result<CommTimeouts> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut timeouts = CommTimeouts {
        read_interval_timeout: 0,
        read_total_timeout_multiplier: 0,
        read_total_timeout_constant: 0,
        write_total_timeout_multiplier: 0,
        write_total_timeout_constant: 0,
    };
    let ok = unsafe { GetCommTimeouts(handle, &mut timeouts) };
    if ok == 0 {
        // Some handles return failure before first Set; start from zeros.
        let _ = unsafe { GetLastError() };
    }
    Ok(timeouts)
}

/// Set **read** total timeout on a pipe `File` (preserves write timeout).
pub fn set_pipe_read_timeout(file: &File, read: Option<Duration>) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut timeouts = get_comm_timeouts(file)?;
    timeouts.read_interval_timeout = 0;
    timeouts.read_total_timeout_multiplier = 0;
    timeouts.read_total_timeout_constant = duration_to_ms(read);
    let ok = unsafe { SetCommTimeouts(handle, &timeouts) };
    if ok == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(())
}

/// Set **write** total timeout on a pipe `File` (preserves read timeout).
pub fn set_pipe_write_timeout(file: &File, write: Option<Duration>) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut timeouts = get_comm_timeouts(file)?;
    timeouts.write_total_timeout_multiplier = 0;
    timeouts.write_total_timeout_constant = duration_to_ms(write);
    let ok = unsafe { SetCommTimeouts(handle, &timeouts) };
    if ok == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(())
}

/// Flush pipe buffers (best-effort after framed writes).
pub fn flush_pipe(file: &File) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let ok = unsafe { FlushFileBuffers(handle) };
    if ok == 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { GetLastError() } as i32
        ));
    }
    Ok(())
}

/// Helper used by tests: unique pipe path for this process.
pub fn unique_test_pipe_path() -> String {
    format!(
        r"\\.\pipe\remotelink-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

// Silence unused import when File helpers aren't all used externally.
#[allow(dead_code)]
fn _into_raw(file: File) -> RawHandle {
    file.into_raw_handle()
}

/// Read/Write façade so control framing stays backend-agnostic.
pub struct PipeStream {
    file: File,
}

impl PipeStream {
    /// Wrap an already-connected pipe handle.
    pub fn from_file(file: File) -> Self {
        Self { file }
    }

    /// Borrow the underlying pipe file (timeouts / diagnostics).
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Mutable borrow of the underlying pipe file.
    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Read for PipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for PipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()?;
        // Named pipes often need FlushFileBuffers for peer visibility.
        let _ = flush_pipe(&self.file);
        Ok(())
    }
}

//! Kill-switch registration stub.
//!
//! Production Windows: service registers a global hotkey and tray action; the
//! handler emits [`crate::ipc::message::KillSwitch`] to the session agent and
//! tears down signaling. This module is a process-local stub suitable for
//! unit tests and early service wiring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::ipc::message::{KillSwitch, KillSwitchSource};

/// Kill-switch registration errors.
#[derive(Debug, Error)]
pub enum KillSwitchError {
    /// A registrar was already armed in this process (v1 single registration).
    #[error("kill-switch already registered")]
    AlreadyRegistered,
    /// Callback panics are not allowed to unwind across FFI later; stub path.
    #[error("kill-switch callback failed: {0}")]
    Callback(String),
}

/// Handle that keeps the kill-switch registration alive.
///
/// Dropping the handle disarms the stub registrar (clears the armed flag).
#[derive(Debug)]
pub struct KillSwitchHandle {
    armed: Arc<AtomicBool>,
}

impl KillSwitchHandle {
    /// Whether the registration is still armed.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }
}

impl Drop for KillSwitchHandle {
    fn drop(&mut self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

/// Process-local kill-switch registrar (stub).
///
/// On Windows this will later call `RegisterHotKey` / tray menu hooks. For now
/// it stores a callback invoked via [`KillSwitchRegistrar::trigger`].
#[derive(Clone, Default)]
pub struct KillSwitchRegistrar {
    inner: Arc<Mutex<RegistrarState>>,
}

struct RegistrarState {
    armed: Arc<AtomicBool>,
    callback: Option<Arc<dyn Fn(KillSwitch) + Send + Sync>>,
}

impl Default for RegistrarState {
    fn default() -> Self {
        Self {
            armed: Arc::new(AtomicBool::new(false)),
            callback: None,
        }
    }
}

impl std::fmt::Debug for KillSwitchRegistrar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let armed = self
            .inner
            .lock()
            .map(|s| s.armed.load(Ordering::SeqCst))
            .unwrap_or(false);
        f.debug_struct("KillSwitchRegistrar")
            .field("armed", &armed)
            .finish()
    }
}

impl KillSwitchRegistrar {
    /// Create a new registrar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the kill-switch callback. Returns a handle that must be kept.
    pub fn register<F>(&self, callback: F) -> Result<KillSwitchHandle, KillSwitchError>
    where
        F: Fn(KillSwitch) + Send + Sync + 'static,
    {
        let mut state = self
            .inner
            .lock()
            .expect("kill-switch registrar mutex poisoned");
        if state.armed.load(Ordering::SeqCst) {
            return Err(KillSwitchError::AlreadyRegistered);
        }
        let armed = Arc::new(AtomicBool::new(true));
        state.armed = Arc::clone(&armed);
        state.callback = Some(Arc::new(callback));
        Ok(KillSwitchHandle { armed })
    }

    /// Simulate hotkey / tray activation (tests and early service loop).
    pub fn trigger(&self, event: KillSwitch) -> Result<(), KillSwitchError> {
        let cb = {
            let state = self
                .inner
                .lock()
                .expect("kill-switch registrar mutex poisoned");
            if !state.armed.load(Ordering::SeqCst) {
                return Ok(());
            }
            state.callback.clone()
        };
        if let Some(cb) = cb {
            cb(event);
        }
        Ok(())
    }

    /// Convenience: trigger with hotkey defaults (all sessions, disable unattended).
    pub fn trigger_hotkey(&self) -> Result<(), KillSwitchError> {
        self.trigger(KillSwitch {
            session_id: None,
            disable_unattended: true,
            source: KillSwitchSource::Hotkey,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn register_and_trigger() {
        let reg = KillSwitchRegistrar::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits2 = Arc::clone(&hits);
        let handle = reg
            .register(move |ev| {
                assert_eq!(ev.source, KillSwitchSource::Hotkey);
                assert!(ev.disable_unattended);
                hits2.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();
        assert!(handle.is_armed());
        reg.trigger_hotkey().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn double_register_fails_while_armed() {
        let reg = KillSwitchRegistrar::new();
        let _h = reg.register(|_| {}).unwrap();
        assert!(matches!(
            reg.register(|_| {}),
            Err(KillSwitchError::AlreadyRegistered)
        ));
    }
}

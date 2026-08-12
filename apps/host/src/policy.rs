//! Host-local session policy: OTP mint window + unattended Mode B gate.
//!
//! Aligns with DESIGN Mode A/B:
//! - **Mode A:** host mints OTP, keeps an active [`OtpRecord`] window, shows code
//!   in CLI/tray (v1: log only). Server may store hash for prefilter.
//! - **Mode B:** host-only [`HostSecret`]; rejected unless `unattended_enabled`.
//! - **Local confirm:** when `confirm_sessions` is set, incoming sessions need
//!   an explicit accept (stubbed as CLI log + [`local_confirm`] helper).

use std::time::{SystemTime, UNIX_EPOCH};

use remotelink_auth::{
    authorize_mode_a, authorize_mode_b, hash_otp, mint_otp_record, AuthChallenge, HostSecret,
    OtpCode, OtpHash, OtpRecord, SessionBindKey, OTP_DEFAULT_DIGITS,
};
use remotelink_protocol::{OtpPrefilterStatus, RejectReason, SessionMode};
use serde::{Deserialize, Serialize};

use crate::session::{Result as SessionResult, SessionError, SessionManager};

/// Default OTP pepper when none is configured (tests / single-node demo).
/// Must match server [`DEFAULT_OTP_PEPPER`] when posting hashes for prefilter.
pub const DEFAULT_HOST_OTP_PEPPER: &[u8] = b"remotelink-otp-server-pepper-v1!";

/// Default OTP TTL in seconds (15 minutes).
pub const DEFAULT_OTP_TTL_SECS: u64 = 900;

/// Host-local configuration (tray / service).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostLocalConfig {
    /// When false, Mode B (unattended) session auth is rejected.
    #[serde(default)]
    pub unattended_enabled: bool,
    /// When true, require local accept UI before session_accept (stub for v1).
    #[serde(default)]
    pub confirm_sessions: bool,
    /// OTP digit length (6–8).
    #[serde(default = "default_otp_digits")]
    pub otp_digits: usize,
    /// OTP window lifetime in seconds.
    #[serde(default = "default_otp_ttl")]
    pub otp_ttl_secs: u64,
}

fn default_otp_digits() -> usize {
    OTP_DEFAULT_DIGITS
}

fn default_otp_ttl() -> u64 {
    DEFAULT_OTP_TTL_SECS
}

impl Default for HostLocalConfig {
    fn default() -> Self {
        Self {
            unattended_enabled: false,
            confirm_sessions: false,
            otp_digits: OTP_DEFAULT_DIGITS,
            otp_ttl_secs: DEFAULT_OTP_TTL_SECS,
        }
    }
}

/// Snapshot of the active OTP window (no plaintext after mint UI display).
#[derive(Debug)]
pub struct ActiveOtpWindow {
    record: OtpRecord,
    /// Hash material for optional server store.
    hash: OtpHash,
    expires_at_unix: u64,
}

impl ActiveOtpWindow {
    /// Stored hash (for server POST body).
    pub fn hash(&self) -> &OtpHash {
        &self.hash
    }

    /// Absolute expiry as unix seconds.
    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    /// Whether the window is past its TTL.
    pub fn is_expired(&self, now_unix: u64) -> bool {
        self.record.is_expired(now_unix)
    }

    /// Whether the OTP was already consumed on this host.
    pub fn is_consumed(&self) -> bool {
        self.record.is_consumed()
    }
}

/// Outcome of a local confirm decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    /// Auto-accept (confirm_sessions = false).
    AutoAccept,
    /// User/tray accepted.
    Accepted,
    /// User/tray denied.
    Denied,
    /// Waiting for UI (confirm_sessions = true, not yet answered).
    Pending,
}

/// Host-side auth + policy service (tray/API stub).
pub struct HostAuthService {
    config: HostLocalConfig,
    /// Pepper for keyed OTP hashes (shared with server for prefilter).
    otp_pepper: Vec<u8>,
    /// Mode B host-only secret (never sent to server).
    host_secret: Option<HostSecret>,
    /// Active Mode A OTP window.
    active_otp: Option<ActiveOtpWindow>,
    /// Last minted plaintext (for tray + Mode A bind). Cleared on remint.
    last_otp_code: Option<String>,
}

impl Default for HostAuthService {
    fn default() -> Self {
        Self::new(HostLocalConfig::default(), DEFAULT_HOST_OTP_PEPPER.to_vec())
    }
}

impl HostAuthService {
    /// Create with config and OTP pepper.
    pub fn new(config: HostLocalConfig, otp_pepper: impl Into<Vec<u8>>) -> Self {
        Self {
            config,
            otp_pepper: otp_pepper.into(),
            host_secret: None,
            active_otp: None,
            last_otp_code: None,
        }
    }

    /// Borrow local config.
    pub fn config(&self) -> &HostLocalConfig {
        &self.config
    }

    /// Mutable config (tray settings).
    pub fn config_mut(&mut self) -> &mut HostLocalConfig {
        &mut self.config
    }

    /// Enable or disable Mode B unattended access.
    pub fn set_unattended_enabled(&mut self, enabled: bool) {
        self.config.unattended_enabled = enabled;
    }

    /// Enable or disable the local accept UI requirement.
    pub fn set_confirm_sessions(&mut self, enabled: bool) {
        self.config.confirm_sessions = enabled;
    }

    /// Install or replace Mode B host secret.
    pub fn set_host_secret(&mut self, secret: HostSecret) {
        self.host_secret = Some(secret);
    }

    /// Generate a fresh Mode B secret and enable unattended.
    pub fn enable_unattended_with_generated_secret(&mut self) -> &HostSecret {
        let secret = HostSecret::generate();
        self.host_secret = Some(secret);
        self.config.unattended_enabled = true;
        self.host_secret.as_ref().expect("just set")
    }

    /// Borrow the host-only Mode B secret, if configured.
    pub fn host_secret(&self) -> Option<&HostSecret> {
        self.host_secret.as_ref()
    }

    /// OTP pepper used for keyed hashing.
    pub fn otp_pepper(&self) -> &[u8] {
        &self.otp_pepper
    }

    /// Active Mode A OTP window, if any.
    pub fn active_otp(&self) -> Option<&ActiveOtpWindow> {
        self.active_otp.as_ref()
    }

    /// Whether the given session mode is allowed by local policy.
    pub fn policy_allows_mode(&self, mode: SessionMode) -> Result<(), PolicyError> {
        match mode {
            SessionMode::Otp | SessionMode::Password => Ok(()),
            SessionMode::Unattended => {
                if self.config.unattended_enabled {
                    if self.host_secret.is_some() {
                        Ok(())
                    } else {
                        Err(PolicyError::UnattendedSecretMissing)
                    }
                } else {
                    Err(PolicyError::UnattendedDisabled)
                }
            }
        }
    }

    /// Local confirm gate for an incoming session.
    ///
    /// When `confirm_sessions` is false, returns [`ConfirmDecision::AutoAccept`].
    /// When true, `user_accepted` must be provided by the tray/CLI stub.
    pub fn decide_confirm(&self, user_accepted: Option<bool>) -> ConfirmDecision {
        if !self.config.confirm_sessions {
            return ConfirmDecision::AutoAccept;
        }
        match user_accepted {
            Some(true) => ConfirmDecision::Accepted,
            Some(false) => ConfirmDecision::Denied,
            None => ConfirmDecision::Pending,
        }
    }

    /// Mint a Mode A OTP: store active window, return plaintext for UI display.
    ///
    /// v1: caller logs via [`OtpCode::to_ui_string`] (CLI). Does **not** contact
    /// the server; use [`ActiveOtpWindow::hash`] for `POST /v1/devices/{id}/otp`.
    pub fn mint_otp(&mut self) -> Result<OtpCode, PolicyError> {
        let now = now_unix();
        let expires = now.saturating_add(self.config.otp_ttl_secs.max(1));
        let digits = self.config.otp_digits;
        let (code, rec) = mint_otp_record(digits, &self.otp_pepper, expires)
            .map_err(|e| PolicyError::Auth(e.to_string()))?;
        let hash = rec.hash().clone();
        self.last_otp_code = Some(code.to_ui_string());
        self.active_otp = Some(ActiveOtpWindow {
            record: rec,
            hash,
            expires_at_unix: expires,
        });
        Ok(code)
    }

    /// Last minted OTP digits (plaintext shown on the host).
    pub fn last_otp_code(&self) -> Option<&str> {
        self.last_otp_code.as_deref()
    }

    /// Hash material for a plaintext code (server mint POST helper).
    pub fn hash_otp_for_server(&self, code: &str) -> Result<OtpHash, PolicyError> {
        hash_otp(code, &self.otp_pepper).map_err(|e| PolicyError::Auth(e.to_string()))
    }

    /// Hex fields suitable for `OtpMintRequest` JSON.
    pub fn otp_hash_wire(hash: &OtpHash) -> (String, String) {
        (hex::encode(hash.digest), hex::encode(hash.salt))
    }

    /// Verify + consume active OTP (Mode A). Clears window on success.
    pub fn consume_otp(
        &mut self,
        code: &str,
        now_unix: u64,
    ) -> Result<SessionBindKey, PolicyError> {
        let window = self.active_otp.as_mut().ok_or(PolicyError::NoActiveOtp)?;
        let key = authorize_mode_a(&mut window.record, code, &self.otp_pepper, now_unix)
            .map_err(|e| PolicyError::Auth(e.to_string()))?;
        self.active_otp = None;
        Ok(key)
    }

    /// Apply Mode A authorization on a [`SessionManager`] using the active window.
    pub fn authorize_session_mode_a(
        &mut self,
        mgr: &mut SessionManager,
        code: &str,
        now_unix: u64,
    ) -> SessionResult<()> {
        let window = self
            .active_otp
            .as_mut()
            .ok_or_else(|| SessionError::InvalidState("no active OTP window".into()))?;
        mgr.authorize_mode_a(&mut window.record, code, &self.otp_pepper, now_unix)?;
        self.active_otp = None;
        Ok(())
    }

    /// Mode B: policy gate + challenge-response on the session manager.
    pub fn authorize_session_mode_b(
        &self,
        mgr: &mut SessionManager,
        challenge: &AuthChallenge,
        fingerprint_host: &[u8],
        fingerprint_viewer: &[u8],
        mac: &[u8],
    ) -> SessionResult<()> {
        self.policy_allows_mode(SessionMode::Unattended)
            .map_err(|e| SessionError::InvalidState(e.to_string()))?;
        let secret = self
            .host_secret
            .as_ref()
            .ok_or_else(|| SessionError::InvalidState("host secret missing".into()))?;
        mgr.authorize_mode_b(secret, challenge, fingerprint_host, fingerprint_viewer, mac)
    }

    /// Direct Mode B authorize without SessionManager (policy + MAC only).
    pub fn verify_mode_b(
        &self,
        challenge: &AuthChallenge,
        session_id: &str,
        fingerprint_host: &[u8],
        fingerprint_viewer: &[u8],
        mac: &[u8],
    ) -> Result<SessionBindKey, PolicyError> {
        self.policy_allows_mode(SessionMode::Unattended)?;
        let secret = self
            .host_secret
            .as_ref()
            .ok_or(PolicyError::UnattendedSecretMissing)?;
        authorize_mode_b(
            secret,
            challenge,
            session_id,
            fingerprint_host,
            fingerprint_viewer,
            mac,
        )
        .map_err(|e| PolicyError::Auth(e.to_string()))
    }

    /// Host re-validation gate before emitting `session_accept`.
    ///
    /// Call when the service/agent receives [`SignalMessage::SessionIncoming`]
    /// (or is about to accept). Mode A requires the viewer OTP for local
    /// consume-once re-check; Mode B requires `unattended_enabled` + secret.
    pub fn decide_session_accept(
        &mut self,
        mode: SessionMode,
        otp_prefilter: OtpPrefilterStatus,
        otp_code: Option<&str>,
        user_accepted: Option<bool>,
        now_unix: u64,
    ) -> SessionAcceptDecision {
        match self.decide_confirm(user_accepted) {
            ConfirmDecision::Denied => {
                return SessionAcceptDecision::Deny {
                    reason: RejectReason::Policy,
                };
            }
            ConfirmDecision::Pending => return SessionAcceptDecision::NeedConfirm,
            ConfirmDecision::AutoAccept | ConfirmDecision::Accepted => {}
        }

        match mode {
            SessionMode::Unattended => match self.policy_allows_mode(SessionMode::Unattended) {
                Ok(()) => SessionAcceptDecision::Allow { bind_key: None },
                Err(PolicyError::UnattendedDisabled)
                | Err(PolicyError::UnattendedSecretMissing) => SessionAcceptDecision::Deny {
                    reason: RejectReason::Policy,
                },
                Err(_) => SessionAcceptDecision::Deny {
                    reason: RejectReason::Auth,
                },
            },
            SessionMode::Password => SessionAcceptDecision::Allow { bind_key: None },
            SessionMode::Otp => {
                // Server prefilter is advisory; host must re-validate active window.
                let _ = otp_prefilter;
                let Some(code) = otp_code.map(str::trim).filter(|s| !s.is_empty()) else {
                    // Soft check: active window must still exist even before code entry.
                    match self.active_otp.as_ref() {
                        None => {
                            return SessionAcceptDecision::Deny {
                                reason: RejectReason::Auth,
                            };
                        }
                        Some(w) if w.is_expired(now_unix) => {
                            return SessionAcceptDecision::Deny {
                                reason: RejectReason::Auth,
                            };
                        }
                        Some(_) => return SessionAcceptDecision::NeedOtpCode,
                    }
                };
                match self.consume_otp(code, now_unix) {
                    Ok(key) => SessionAcceptDecision::Allow {
                        bind_key: Some(key),
                    },
                    Err(_) => SessionAcceptDecision::Deny {
                        reason: RejectReason::Auth,
                    },
                }
            }
        }
    }
}

/// Outcome of [`HostAuthService::decide_session_accept`].
#[derive(Debug)]
pub enum SessionAcceptDecision {
    /// Host may send `session_accept` (Mode A may include bind key after consume).
    Allow {
        /// Present after Mode A host-side OTP consume.
        bind_key: Option<SessionBindKey>,
    },
    /// Waiting for local confirm UI (`--confirm-sessions`).
    NeedConfirm,
    /// Mode A needs viewer OTP plaintext for host re-validate/consume.
    NeedOtpCode,
    /// Host must send `session_reject` with this reason.
    Deny {
        /// Reject reason for the wire message.
        reason: RejectReason,
    },
}

impl PartialEq for SessionAcceptDecision {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::NeedConfirm, Self::NeedConfirm) | (Self::NeedOtpCode, Self::NeedOtpCode) => true,
            (Self::Allow { bind_key: a }, Self::Allow { bind_key: b }) => {
                a.is_some() == b.is_some()
            }
            (Self::Deny { reason: a }, Self::Deny { reason: b }) => a == b,
            _ => false,
        }
    }
}

impl Eq for SessionAcceptDecision {}

/// Policy / OTP UX errors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    /// Mode B rejected because `unattended_enabled` is false.
    #[error("unattended Mode B is disabled (set unattended_enabled)")]
    UnattendedDisabled,
    /// Mode B enabled but no [`HostSecret`] is installed.
    #[error("unattended enabled but host secret is not configured")]
    UnattendedSecretMissing,
    /// Mode A consume without a live mint window.
    #[error("no active OTP window")]
    NoActiveOtp,
    /// Underlying auth helper failure (verify, format, crypto).
    #[error("auth: {0}")]
    Auth(String),
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Log-oriented tray stub: print OTP for v1 (no real tray).
pub fn log_otp_to_cli(code: &OtpCode, expires_at_unix: u64) {
    // Intentional secret boundary: host UI / CLI display only.
    println!(
        "host: Mode A OTP (show on tray): {} (expires_at_unix={expires_at_unix})",
        code.to_ui_string()
    );
}

/// Log-oriented local accept stub.
pub fn log_confirm_prompt(session_id: &str) {
    println!(
        "host: local confirm required for session {session_id} \
         (tray accept/deny; --confirm-sessions)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_auth::{mode_b_viewer_response, HostSecret};

    #[test]
    fn mint_and_consume_once() {
        let mut svc = HostAuthService::default();
        let code = svc.mint_otp().unwrap();
        assert_eq!(code.len(), 6);
        let now = now_unix();
        let key = svc.consume_otp(code.as_str(), now).unwrap();
        assert!(!key.as_bytes().is_empty());
        // Second consume: window cleared.
        assert!(matches!(
            svc.consume_otp(code.as_str(), now),
            Err(PolicyError::NoActiveOtp)
        ));
    }

    #[test]
    fn last_otp_code_tracks_mint() {
        let mut svc = HostAuthService::default();
        assert!(svc.last_otp_code().is_none());
        let code = svc.mint_otp().unwrap();
        assert_eq!(svc.last_otp_code(), Some(code.as_str()));
        assert!(svc.active_otp().is_some());
    }

    #[test]
    fn mode_b_rejected_when_disabled() {
        let mut svc = HostAuthService::default();
        assert!(!svc.config().unattended_enabled);
        let secret = HostSecret::try_new(b"host-local-secret!!".to_vec()).unwrap();
        svc.set_host_secret(secret.clone());
        // Still disabled.
        assert_eq!(
            svc.policy_allows_mode(SessionMode::Unattended),
            Err(PolicyError::UnattendedDisabled)
        );

        let challenge = AuthChallenge::issue();
        let mac = mode_b_viewer_response(&secret, "s1", challenge.nonce.as_bytes(), b"h", b"v");
        assert!(matches!(
            svc.verify_mode_b(&challenge, "s1", b"h", b"v", &mac),
            Err(PolicyError::UnattendedDisabled)
        ));
    }

    #[test]
    fn mode_b_ok_when_enabled() {
        let mut svc = HostAuthService::default();
        let secret = HostSecret::try_new(b"host-local-secret!!".to_vec()).unwrap();
        svc.set_host_secret(secret.clone());
        svc.set_unattended_enabled(true);
        let challenge = AuthChallenge::issue();
        let mac = mode_b_viewer_response(&secret, "s1", challenge.nonce.as_bytes(), b"h", b"v");
        svc.verify_mode_b(&challenge, "s1", b"h", b"v", &mac)
            .unwrap();
    }

    #[test]
    fn confirm_sessions_gate() {
        let mut svc = HostAuthService::default();
        assert_eq!(svc.decide_confirm(None), ConfirmDecision::AutoAccept);
        svc.set_confirm_sessions(true);
        assert_eq!(svc.decide_confirm(None), ConfirmDecision::Pending);
        assert_eq!(svc.decide_confirm(Some(true)), ConfirmDecision::Accepted);
        assert_eq!(svc.decide_confirm(Some(false)), ConfirmDecision::Denied);
    }

    #[test]
    fn session_manager_mode_a_via_policy() {
        let mut svc = HostAuthService::default();
        let code = svc.mint_otp().unwrap();
        let mut mgr = SessionManager::new_mock();
        mgr.attach("sess-pol");
        svc.authorize_session_mode_a(&mut mgr, code.as_str(), now_unix())
            .unwrap();
        assert!(mgr.identity().session_authorized);
    }

    #[test]
    fn session_manager_mode_b_policy_reject() {
        let mut svc = HostAuthService::default();
        let secret = HostSecret::try_new(b"host-local-secret!!".to_vec()).unwrap();
        svc.set_host_secret(secret.clone());
        // unattended disabled
        let mut mgr = SessionManager::new_mock();
        mgr.attach("sess-b");
        let challenge = AuthChallenge::issue();
        let mac = mode_b_viewer_response(&secret, "sess-b", challenge.nonce.as_bytes(), b"", b"");
        assert!(svc
            .authorize_session_mode_b(&mut mgr, &challenge, b"", b"", &mac)
            .is_err());
        assert!(!mgr.identity().session_authorized);

        svc.set_unattended_enabled(true);
        svc.authorize_session_mode_b(&mut mgr, &challenge, b"", b"", &mac)
            .unwrap();
        assert!(mgr.identity().session_authorized);
    }

    #[test]
    fn decide_accept_mode_a_requires_code_and_consumes() {
        let mut svc = HostAuthService::default();
        let code = svc.mint_otp().unwrap();
        let now = now_unix();
        assert_eq!(
            svc.decide_session_accept(SessionMode::Otp, OtpPrefilterStatus::Ok, None, None, now),
            SessionAcceptDecision::NeedOtpCode
        );
        assert!(matches!(
            svc.decide_session_accept(
                SessionMode::Otp,
                OtpPrefilterStatus::Ok,
                Some(code.as_str()),
                None,
                now
            ),
            SessionAcceptDecision::Allow { bind_key: Some(_) }
        ));
        // Second accept: window gone.
        assert_eq!(
            svc.decide_session_accept(
                SessionMode::Otp,
                OtpPrefilterStatus::Ok,
                Some(code.as_str()),
                None,
                now
            ),
            SessionAcceptDecision::Deny {
                reason: RejectReason::Auth
            }
        );
    }

    #[test]
    fn decide_accept_mode_b_disabled_denies() {
        let mut svc = HostAuthService::default();
        let secret = HostSecret::try_new(b"host-local-secret!!".to_vec()).unwrap();
        svc.set_host_secret(secret);
        assert_eq!(
            svc.decide_session_accept(
                SessionMode::Unattended,
                OtpPrefilterStatus::None,
                None,
                None,
                now_unix()
            ),
            SessionAcceptDecision::Deny {
                reason: RejectReason::Policy
            }
        );
        svc.set_unattended_enabled(true);
        assert_eq!(
            svc.decide_session_accept(
                SessionMode::Unattended,
                OtpPrefilterStatus::None,
                None,
                None,
                now_unix()
            ),
            SessionAcceptDecision::Allow { bind_key: None }
        );
    }

    #[test]
    fn decide_accept_confirm_sessions() {
        let mut svc = HostAuthService::default();
        svc.set_confirm_sessions(true);
        let code = svc.mint_otp().unwrap();
        assert_eq!(
            svc.decide_session_accept(
                SessionMode::Otp,
                OtpPrefilterStatus::Skipped,
                Some(code.as_str()),
                None,
                now_unix()
            ),
            SessionAcceptDecision::NeedConfirm
        );
        assert!(matches!(
            svc.decide_session_accept(
                SessionMode::Otp,
                OtpPrefilterStatus::Skipped,
                Some(code.as_str()),
                Some(true),
                now_unix()
            ),
            SessionAcceptDecision::Allow { .. }
        ));
    }
}

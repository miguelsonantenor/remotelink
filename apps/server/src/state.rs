//! Shared application state.

use std::sync::Arc;

use crate::ice::IceConfig;
use crate::otp::{MemoryOtpStore, DEFAULT_OTP_PEPPER};
use crate::repo::DeviceRepository;
use crate::security::{
    AuditStore, AuthAttemptTracker, BlocklistStore, ClientIpConfig, MemoryAuditStore,
    MemoryBlocklist, RateLimiters,
};
use crate::session::{SessionRegistry, SharedSessionRegistry};

/// Axum state: repository + sessions + security controls + OTP store.
#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn DeviceRepository>,
    pub sessions: SharedSessionRegistry,
    pub rate_limits: Arc<RateLimiters>,
    pub auth_attempts: Arc<AuthAttemptTracker>,
    pub audit: Arc<dyn AuditStore>,
    pub blocklist: Arc<dyn BlocklistStore>,
    /// Mode A OTP hash store (host-minted).
    pub otp: Arc<MemoryOtpStore>,
    /// Pepper for keyed OTP verification (must match host mint pepper).
    pub otp_pepper: Arc<Vec<u8>>,
    /// Client IP trust policy (`TRUST_PROXY`).
    pub client_ip: ClientIpConfig,
    /// Operator admin token (`ADMIN_TOKEN`). `None` / empty disables admin routes.
    pub admin_token: Option<Arc<str>>,
    /// STUN/TURN advertised on `hello_ok`.
    pub ice: IceConfig,
}

impl AppState {
    pub fn new(repo: Arc<dyn DeviceRepository>) -> Self {
        Self {
            repo,
            sessions: Arc::new(SessionRegistry::new()),
            rate_limits: Arc::new(RateLimiters::new()),
            auth_attempts: Arc::new(AuthAttemptTracker::with_defaults()),
            audit: Arc::new(MemoryAuditStore::new()),
            blocklist: Arc::new(MemoryBlocklist::new()),
            otp: Arc::new(MemoryOtpStore::new()),
            otp_pepper: Arc::new(DEFAULT_OTP_PEPPER.to_vec()),
            client_ip: ClientIpConfig::default(),
            admin_token: None,
            ice: IceConfig::default(),
        }
    }

    /// Construct with an explicit session registry (tests / multi-node later).
    pub fn with_sessions(repo: Arc<dyn DeviceRepository>, sessions: SharedSessionRegistry) -> Self {
        Self {
            repo,
            sessions,
            rate_limits: Arc::new(RateLimiters::new()),
            auth_attempts: Arc::new(AuthAttemptTracker::with_defaults()),
            audit: Arc::new(MemoryAuditStore::new()),
            blocklist: Arc::new(MemoryBlocklist::new()),
            otp: Arc::new(MemoryOtpStore::new()),
            otp_pepper: Arc::new(DEFAULT_OTP_PEPPER.to_vec()),
            client_ip: ClientIpConfig::default(),
            admin_token: None,
            ice: IceConfig::default(),
        }
    }

    /// Full constructor for tests that inject security backends.
    pub fn with_security(
        repo: Arc<dyn DeviceRepository>,
        sessions: SharedSessionRegistry,
        rate_limits: Arc<RateLimiters>,
        auth_attempts: Arc<AuthAttemptTracker>,
        audit: Arc<dyn AuditStore>,
        blocklist: Arc<dyn BlocklistStore>,
    ) -> Self {
        Self {
            repo,
            sessions,
            rate_limits,
            auth_attempts,
            audit,
            blocklist,
            otp: Arc::new(MemoryOtpStore::new()),
            otp_pepper: Arc::new(DEFAULT_OTP_PEPPER.to_vec()),
            client_ip: ClientIpConfig::default(),
            admin_token: None,
            ice: IceConfig::default(),
        }
    }

    /// Minimum accepted length for `ADMIN_TOKEN` (weak secrets are rejected).
    pub const ADMIN_TOKEN_MIN_LEN: usize = 16;

    /// Set operator admin token (`ADMIN_TOKEN`).
    ///
    /// Empty or shorter than [`Self::ADMIN_TOKEN_MIN_LEN`] clears the token
    /// (admin routes stay disabled / unauthorized).
    pub fn with_admin_token(mut self, token: impl Into<String>) -> Self {
        let t = token.into();
        self.admin_token = if t.len() >= Self::ADMIN_TOKEN_MIN_LEN {
            Some(Arc::from(t))
        } else {
            None
        };
        self
    }

    /// Load `ADMIN_TOKEN` from the environment when present and long enough.
    pub fn with_admin_token_from_env(self) -> Self {
        match std::env::var("ADMIN_TOKEN") {
            Ok(t) if t.len() >= Self::ADMIN_TOKEN_MIN_LEN => self.with_admin_token(t),
            Ok(t) if !t.is_empty() => {
                tracing::warn!(
                    len = t.len(),
                    min = Self::ADMIN_TOKEN_MIN_LEN,
                    "ADMIN_TOKEN too short; admin routes disabled"
                );
                self
            }
            _ => self,
        }
    }

    /// Override proxy-trust policy (production: from `TRUST_PROXY` env).
    pub fn with_client_ip(mut self, client_ip: ClientIpConfig) -> Self {
        self.client_ip = client_ip;
        self
    }

    /// Override OTP pepper (tests / shared host config).
    pub fn with_otp_pepper(mut self, pepper: impl Into<Vec<u8>>) -> Self {
        self.otp_pepper = Arc::new(pepper.into());
        self
    }

    /// Inject OTP store (tests).
    pub fn with_otp_store(mut self, otp: Arc<MemoryOtpStore>) -> Self {
        self.otp = otp;
        self
    }

    /// Advertise STUN/TURN on `hello_ok`.
    pub fn with_ice(mut self, ice: IceConfig) -> Self {
        self.ice = ice;
        self
    }
}

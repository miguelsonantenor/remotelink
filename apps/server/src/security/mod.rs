//! Rate limiting, auth-attempt lockout, audit log, and blocklist.
//!
//! In-memory implementations are production-ready for single-node; Redis/Postgres
//! multi-node backends can implement the same traits later.

mod audit;
mod auth_attempts;
mod blocklist;
mod client_ip;
mod rate_limit;

pub use audit::{
    audit_best_effort, AuditError, AuditEvent, AuditEventType, AuditStore, MemoryAuditStore,
    NewAuditEvent,
};
pub use auth_attempts::{AuthAttemptConfig, AuthAttemptTracker, LockoutActive};
pub use blocklist::{
    any_blocked, hash_subject, BlockSubjectType, BlocklistEntry, BlocklistError, BlocklistStore,
    MemoryBlocklist, NewBlocklistEntry,
};
pub use client_ip::{
    client_ip_from_headers, peer_from_parts, resolve_client_ip, resolve_client_ip_from_parts,
    ClientIpConfig, OptionalPeer,
};
pub use rate_limit::{
    default_admin_config, default_refresh_config, default_register_config,
    default_session_intent_config, RateLimitConfig, RateLimitExceeded, RateLimiter, RateLimiters,
};

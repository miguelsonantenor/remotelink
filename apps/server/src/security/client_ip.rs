//! Client IP resolution for rate-limit / auth-attempt / blocklist keys.
//!
//! # Trust model
//!
//! - **Default:** use the TCP peer address (`ConnectInfo`). Proxy headers are
//!   **ignored** so clients cannot spoof rate-limit or lockout keys.
//! - **`trust_proxy = true`:** honor leftmost `X-Forwarded-For` hop, then
//!   `X-Real-IP`, then fall back to the peer address. Only enable when a
//!   reverse proxy is configured to **overwrite** (not append unchecked)
//!   these headers.
//!
//! Deploy: set `TRUST_PROXY=1` only behind a trusted reverse proxy.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::HeaderMap;

/// Whether proxy headers may override the socket peer IP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientIpConfig {
    /// When true, prefer `X-Forwarded-For` / `X-Real-IP` over the peer address.
    pub trust_proxy: bool,
}

/// Optional TCP peer from `ConnectInfo` (absent in oneshot unit tests).
///
/// Never rejects: missing ConnectInfo → `None` (callers fall back via
/// [`resolve_client_ip`]).
#[derive(Debug, Clone, Copy)]
pub struct OptionalPeer(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for OptionalPeer
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(peer_from_parts(parts)))
    }
}

impl ClientIpConfig {
    pub fn from_env() -> Self {
        Self {
            trust_proxy: env_truthy("TRUST_PROXY"),
        }
    }
}

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

/// Resolve client IP from optional peer + headers under `config`.
///
/// Priority when `trust_proxy`:
/// 1. first `X-Forwarded-For` hop
/// 2. `X-Real-IP`
/// 3. peer IP
/// 4. `"unknown"` only if nothing else is available
///
/// When not trusting proxy: peer first, then `"unknown"`.
pub fn resolve_client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    config: ClientIpConfig,
) -> String {
    if config.trust_proxy {
        if let Some(ip) = proxy_client_ip(headers) {
            return ip;
        }
    }
    if let Some(addr) = peer {
        return ip_key(addr.ip());
    }
    "unknown".to_string()
}

/// Convenience for tests / legacy call sites.
///
/// When `trust_proxy` is true, honors proxy headers then `fallback`.
/// When false, uses `fallback` only (headers ignored).
pub fn client_ip_from_headers(
    headers: &HeaderMap,
    fallback: Option<&str>,
    trust_proxy: bool,
) -> String {
    let peer = fallback.and_then(|s| {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // Synthetic SocketAddr for resolve path (port unused).
        format!("{s}:0").parse::<SocketAddr>().ok().or_else(|| {
            // bare IP without port
            s.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 0))
        })
    });
    resolve_client_ip(headers, peer, ClientIpConfig { trust_proxy })
}

/// Read peer `SocketAddr` from request extensions (set by ConnectInfo make-service).
pub fn peer_from_parts(parts: &Parts) -> Option<SocketAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
}

/// Resolve IP using ConnectInfo extension when present.
pub fn resolve_client_ip_from_parts(parts: &Parts, config: ClientIpConfig) -> String {
    resolve_client_ip(&parts.headers, peer_from_parts(parts), config)
}

fn proxy_client_ip(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-forwarded-for").and_then(|h| h.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            let ip = first.trim();
            if !ip.is_empty() && is_plausible_ip_token(ip) {
                return Some(ip.to_string());
            }
        }
    }
    if let Some(v) = headers.get("x-real-ip").and_then(|h| h.to_str().ok()) {
        let ip = v.trim();
        if !ip.is_empty() && is_plausible_ip_token(ip) {
            return Some(ip.to_string());
        }
    }
    None
}

fn is_plausible_ip_token(s: &str) -> bool {
    // Accept literal IPs; reject empty/garbage header injections for keying.
    s.parse::<IpAddr>().is_ok()
}

fn ip_key(ip: IpAddr) -> String {
    // Normalize IPv4-mapped IPv6 for stable keys.
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                v4.to_string()
            } else {
                v6.to_string()
            }
        }
        IpAddr::V4(v4) => v4.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn ignores_proxy_headers_without_trust() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.1, 10.0.0.1"),
        );
        let peer: SocketAddr = "127.0.0.1:54321".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&h, Some(peer), ClientIpConfig { trust_proxy: false }),
            "127.0.0.1"
        );
    }

    #[test]
    fn honors_forwarded_for_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.1, 10.0.0.1"),
        );
        h.insert("x-real-ip", HeaderValue::from_static("10.0.0.2"));
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&h, Some(peer), ClientIpConfig { trust_proxy: true }),
            "203.0.113.1"
        );
    }

    #[test]
    fn falls_back_to_peer_when_trusted_but_no_headers() {
        let empty = HeaderMap::new();
        let peer: SocketAddr = "198.51.100.4:9".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&empty, Some(peer), ClientIpConfig { trust_proxy: true }),
            "198.51.100.4"
        );
    }

    #[test]
    fn peer_preferred_over_unknown() {
        let empty = HeaderMap::new();
        let peer: SocketAddr = "10.0.0.5:80".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&empty, Some(peer), ClientIpConfig::default()),
            "10.0.0.5"
        );
        assert_eq!(
            resolve_client_ip(&empty, None, ClientIpConfig::default()),
            "unknown"
        );
    }

    #[test]
    fn rejects_non_ip_proxy_header() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("not-an-ip"));
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        assert_eq!(
            resolve_client_ip(&h, Some(peer), ClientIpConfig { trust_proxy: true }),
            "127.0.0.1"
        );
    }

    #[test]
    fn client_ip_from_headers_compat() {
        let mut h = HeaderMap::new();
        h.insert("x-real-ip", HeaderValue::from_static("198.51.100.4"));
        // untrusted: ignore header, use fallback peer
        assert_eq!(
            client_ip_from_headers(&h, Some("127.0.0.1"), false),
            "127.0.0.1"
        );
        // trusted: use header
        assert_eq!(
            client_ip_from_headers(&h, Some("127.0.0.1"), true),
            "198.51.100.4"
        );
    }
}

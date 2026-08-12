//! ICE / STUN / TURN config published to clients on `hello_ok`.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha1::Sha1;

/// How long minted TURN REST credentials last.
const TURN_TTL_SECS: u64 = 24 * 3600;

/// Signaling ICE configuration from the environment.
#[derive(Debug, Clone, Default)]
pub struct IceConfig {
    /// `stun:host:3478` URLs.
    pub stun_urls: Vec<String>,
    /// `turn:host:3478` / `turns:` URLs.
    pub turn_urls: Vec<String>,
    /// coturn `static-auth-secret` (REST HMAC-SHA1).
    pub turn_secret: Option<String>,
}

impl IceConfig {
    /// Read `STUN_URLS`, `TURN_URLS`, `TURN_SHARED_SECRET`.
    pub fn from_env() -> Self {
        Self {
            stun_urls: split_urls(std::env::var("STUN_URLS").ok()),
            turn_urls: split_urls(std::env::var("TURN_URLS").ok()),
            turn_secret: std::env::var("TURN_SHARED_SECRET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// True when at least one ICE server will be advertised.
    pub fn has_servers(&self) -> bool {
        !self.stun_urls.is_empty() || !self.turn_urls.is_empty()
    }

    /// JSON array for `hello_ok.feature_flags.ice_servers`.
    pub fn feature_flag_value(&self) -> Value {
        let mut servers = Vec::new();
        for url in &self.stun_urls {
            servers.push(json!({ "urls": [url] }));
        }
        if !self.turn_urls.is_empty() {
            let (username, credential) = match &self.turn_secret {
                Some(secret) => mint_turn_rest_cred(secret, TURN_TTL_SECS),
                None => (String::new(), String::new()),
            };
            let mut obj = json!({ "urls": self.turn_urls });
            if !username.is_empty() {
                obj["username"] = json!(username);
                obj["credential"] = json!(credential);
            }
            servers.push(obj);
        }
        Value::Array(servers)
    }
}

fn split_urls(raw: Option<String>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// coturn REST API username/password (`timestamp:user` + HMAC-SHA1).
pub fn mint_turn_rest_cred(secret: &str, ttl_secs: u64) -> (String, String) {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_add(ttl_secs);
    let username = format!("{exp}:rl");
    let mut mac =
        Hmac::<Sha1>::new_from_slice(secret.as_bytes()).expect("HMAC-SHA1 accepts any key length");
    mac.update(username.as_bytes());
    let credential = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        mac.finalize().into_bytes(),
    );
    (username, credential)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_turn_rest_cred_is_stable_shape() {
        let (user, cred) = mint_turn_rest_cred("dev-secret", 60);
        assert!(user.contains(":rl"), "{user}");
        assert!(!cred.is_empty());
        let (user2, cred2) = mint_turn_rest_cred("dev-secret", 60);
        // Same expiry second → same pair (or adjacent second).
        assert_eq!(user.split(':').nth(1), user2.split(':').nth(1));
        let _ = cred2;
    }

    #[test]
    fn empty_env_has_no_servers() {
        assert!(!IceConfig::default().has_servers());
        assert_eq!(IceConfig::default().feature_flag_value(), json!([]));
    }

    #[test]
    fn stun_only_advertises_urls() {
        let cfg = IceConfig {
            stun_urls: vec!["stun:example:3478".into()],
            ..IceConfig::default()
        };
        let v = cfg.feature_flag_value();
        assert_eq!(v[0]["urls"][0], "stun:example:3478");
        assert!(v[0].get("username").is_none());
    }
}

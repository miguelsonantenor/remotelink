//! HTTP helpers: token refresh + OTP hash mint against the signaling server.

use serde::Deserialize;

use crate::register::{DeviceRegistration, RegisterError};

/// Result of posting an OTP hash for server prefilter.
#[derive(Debug, Clone, Deserialize)]
pub struct OtpMintHttpResponse {
    /// When the OTP hash expires (RFC3339).
    pub expires_at: String,
    /// Opaque server row id.
    pub otp_id: i64,
}

/// Token pair from refresh.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenHttpResponse {
    /// New access token.
    pub access_token: String,
    /// New refresh token (rotated).
    pub refresh_token: String,
    /// Access expiry (RFC3339).
    pub expires_at: String,
    /// Token type (`Bearer`) when present.
    #[serde(default)]
    pub token_type: Option<String>,
}

fn http_client() -> Result<reqwest::Client, RegisterError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| RegisterError::Http(e.to_string()))
}

/// `POST /v1/devices/{id}/token/refresh` — rotate access/refresh tokens.
pub async fn refresh_device_token(
    http_base: &str,
    public_id: &str,
    refresh_token: &str,
) -> Result<TokenHttpResponse, RegisterError> {
    let base = http_base.trim_end_matches('/');
    let url = format!("{base}/v1/devices/{public_id}/token/refresh");
    let client = http_client()?;
    let res = client
        .post(&url)
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .await
        .map_err(|e| RegisterError::Http(e.to_string()))?;
    let status = res.status().as_u16();
    let text = res
        .text()
        .await
        .map_err(|e| RegisterError::Http(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(RegisterError::Status {
            status,
            body: text.chars().take(256).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| RegisterError::Parse(e.to_string()))
}

/// `POST /v1/devices/{id}/otp` — store a host-computed OTP hash for viewer prefilter.
///
/// Plaintext code is **not** sent; only `digest_hex` + `salt_hex` from
/// [`remotelink_auth::hash_otp`] / `mint_otp`.
pub async fn post_otp_hash(
    http_base: &str,
    public_id: &str,
    access_token: &str,
    digest_hex: &str,
    salt_hex: &str,
    keyed: bool,
    expires_in_secs: Option<u64>,
) -> Result<OtpMintHttpResponse, RegisterError> {
    let base = http_base.trim_end_matches('/');
    let url = format!("{base}/v1/devices/{public_id}/otp");
    let client = http_client()?;
    let body = serde_json::json!({
        "digest_hex": digest_hex,
        "salt_hex": salt_hex,
        "keyed": keyed,
        "expires_in_secs": expires_in_secs,
    });
    let res = client
        .post(&url)
        .header("authorization", format!("Bearer {access_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| RegisterError::Http(e.to_string()))?;
    let status = res.status().as_u16();
    let text = res
        .text()
        .await
        .map_err(|e| RegisterError::Http(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(RegisterError::Status {
            status,
            body: text.chars().take(256).collect(),
        });
    }
    serde_json::from_str(&text).map_err(|e| RegisterError::Parse(e.to_string()))
}

/// Apply refreshed tokens onto a [`DeviceRegistration`]-like triple for callers.
pub fn apply_refresh(
    public_id: &str,
    display_name: Option<String>,
    tokens: TokenHttpResponse,
) -> DeviceRegistration {
    DeviceRegistration {
        public_id: public_id.to_string(),
        display_name,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
    }
}

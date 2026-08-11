//! HTTP device enrollment against `POST /v1/devices/register`.

use base64::Engine;
use serde::Deserialize;
use thiserror::Error;

/// Errors from HTTP registration.
#[derive(Debug, Error)]
pub enum RegisterError {
    /// HTTP or transport failure.
    #[error("http: {0}")]
    Http(String),
    /// Server returned a non-success status.
    #[error("register failed ({status}): {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Response body snippet.
        body: String,
    },
    /// JSON parse failure.
    #[error("parse: {0}")]
    Parse(String),
}

/// Successful device registration.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceRegistration {
    /// Host public id (with check digits).
    pub public_id: String,
    /// Optional display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Access token for WSS `hello` (host).
    pub access_token: String,
    /// Refresh token.
    pub refresh_token: String,
    /// Access expiry (RFC3339).
    pub expires_at: String,
}

/// Register a new host device with a 32-byte ed25519 verifying key.
///
/// `http_base` is e.g. `http://127.0.0.1:8080` (no trailing path).
pub async fn register_device(
    http_base: &str,
    public_key_raw: &[u8; 32],
    display_name: Option<&str>,
) -> Result<DeviceRegistration, RegisterError> {
    let base = http_base.trim_end_matches('/');
    let url = format!("{base}/v1/devices/register");
    let public_key = base64::engine::general_purpose::STANDARD.encode(public_key_raw);
    let body = serde_json::json!({
        "public_key": public_key,
        "display_name": display_name,
        "protocol_version": remotelink_protocol::PROTOCOL_VERSION,
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| RegisterError::Http(e.to_string()))?;

    let res = client
        .post(&url)
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

/// Derive a `ws://` / `wss://` `/v1/ws` URL from an HTTP(S) base URL.
///
/// Examples:
/// - `http://127.0.0.1:8080` → `ws://127.0.0.1:8080/v1/ws`
/// - `https://sig.example` → `wss://sig.example/v1/ws`
/// - already `ws://…/v1/ws` is returned unchanged if path ends with `/v1/ws`
pub fn http_to_ws_url(http_base: &str) -> Result<String, RegisterError> {
    let s = http_base.trim();
    if s.starts_with("ws://") || s.starts_with("wss://") {
        if s.contains("/v1/ws") {
            return Ok(s.to_string());
        }
        let base = s.trim_end_matches('/');
        return Ok(format!("{base}/v1/ws"));
    }
    let url = url::Url::parse(s).map_err(|e| RegisterError::Parse(format!("url: {e}")))?;
    let mut out = url;
    match out.scheme() {
        "http" => {
            out.set_scheme("ws")
                .map_err(|_| RegisterError::Parse("cannot set ws scheme".into()))?;
        }
        "https" => {
            out.set_scheme("wss")
                .map_err(|_| RegisterError::Parse("cannot set wss scheme".into()))?;
        }
        other => {
            return Err(RegisterError::Parse(format!(
                "unsupported scheme `{other}` (use http/https/ws/wss)"
            )));
        }
    }
    out.set_path("/v1/ws");
    out.set_query(None);
    out.set_fragment(None);
    Ok(out.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_to_ws_localhost() {
        assert_eq!(
            http_to_ws_url("http://127.0.0.1:8080").unwrap(),
            "ws://127.0.0.1:8080/v1/ws"
        );
        assert_eq!(
            http_to_ws_url("https://sig.example/").unwrap(),
            "wss://sig.example/v1/ws"
        );
        assert_eq!(
            http_to_ws_url("ws://127.0.0.1:9/v1/ws").unwrap(),
            "ws://127.0.0.1:9/v1/ws"
        );
    }
}

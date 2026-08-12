//! Apply `hello_ok.feature_flags.ice_servers` to process env for webrtc-rs.

use remotelink_protocol::SignalMessage;

/// Copy advertised STUN/TURN URLs into `REMOTELINK_WEBRTC_*` env vars.
///
/// Called after `hello` so the next `WebrtcPeerTransport` picks them up.
pub fn apply_ice_servers_from_hello(msg: &SignalMessage) {
    let SignalMessage::HelloOk { feature_flags, .. } = msg else {
        return;
    };
    let Some(servers) = feature_flags.get("ice_servers").and_then(|v| v.as_array()) else {
        return;
    };
    let mut urls = Vec::new();
    let mut user = String::new();
    let mut pass = String::new();
    for server in servers {
        if let Some(list) = server.get("urls").and_then(|v| v.as_array()) {
            for u in list {
                if let Some(s) = u.as_str() {
                    if !s.is_empty() {
                        urls.push(s.to_string());
                    }
                }
            }
        }
        if let Some(s) = server.get("username").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                user = s.to_string();
            }
        }
        if let Some(s) = server.get("credential").and_then(|v| v.as_str()) {
            if !s.is_empty() {
                pass = s.to_string();
            }
        }
    }
    if urls.is_empty() {
        return;
    }
    std::env::set_var("REMOTELINK_WEBRTC_STUN", urls.join(","));
    if !user.is_empty() {
        std::env::set_var("REMOTELINK_WEBRTC_TURN_USER", user);
        std::env::set_var("REMOTELINK_WEBRTC_TURN_PASS", pass);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remotelink_protocol::SignalMessage;
    use serde_json::json;

    #[test]
    fn apply_sets_stun_env() {
        let msg = SignalMessage::HelloOk {
            server_time: "t".into(),
            feature_flags: json!({
                "ice_servers": [{ "urls": ["stun:example:3478"] }]
            }),
        };
        apply_ice_servers_from_hello(&msg);
        assert!(std::env::var("REMOTELINK_WEBRTC_STUN")
            .unwrap_or_default()
            .contains("stun:example:3478"));
    }
}

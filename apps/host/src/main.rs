//! RemoteLink host binary.
//!
//! Single binary with `--role=service|agent|colocate` (DESIGN KD5):
//! - **service**: enrollment, signaling WS, policy, kill-switch orchestration
//! - **agent**: session manager + PeerTransport (synthetic A/V without real display)
//! - **colocate**: CI/test mode — service control sequence + agent in-process
//! - **`--kill-switch`**: G9 demo — active session indicator then local kill
//!
//! Transport: `--transport=mock|live|webrtc|auto` (or `REMOTELINK_TRANSPORT`; default **mock**).
//!
//! Control IPC is length-prefixed JSON (no media bytes).

use std::env;

use remotelink_host::agent;
use remotelink_host::service;
use remotelink_host::{run_ws_host_blocking, WsHostConfig};
use remotelink_net::TransportConfig;
#[cfg(test)]
use remotelink_net::TransportMode;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let transport = parse_transport(&args);
    apply_transport_env(&transport);

    let role = parse_role(&args);
    match role {
        HostRole::Service => {
            println!(
                "remotelink-host {} role=service transport={}",
                remotelink_common::VERSION,
                transport.mode.as_str()
            );
            service::run();
        }
        HostRole::Agent => {
            println!(
                "remotelink-host {} role=agent transport={}",
                remotelink_common::VERSION,
                transport.mode.as_str()
            );
            agent::run_with_transport(transport.mode);
        }
        HostRole::Ws => {
            println!(
                "remotelink-host {} role=ws transport={}",
                remotelink_common::VERSION,
                transport.mode.as_str()
            );
            let cfg = parse_ws_host_config(&args, transport.mode);
            match run_ws_host_blocking(cfg) {
                Ok(summary) => println!("{summary}"),
                Err(e) => {
                    eprintln!("ws-host: failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        HostRole::Colocate => {
            println!(
                "remotelink-host {} role=colocate transport={}",
                remotelink_common::VERSION,
                transport.mode.as_str()
            );
            match service::run_colocate_synthetic("colocate-cli-session") {
                Ok(summary) => println!("colocate: {summary}"),
                Err(e) => {
                    eprintln!("colocate: failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        HostRole::KillSwitchDemo => {
            println!(
                "remotelink-host {} role=kill-switch-demo transport={}",
                remotelink_common::VERSION,
                transport.mode.as_str()
            );
            match service::run_kill_switch_demo("kill-switch-cli-session") {
                Ok(summary) => println!("kill-switch: {summary}"),
                Err(e) => {
                    eprintln!("kill-switch: failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        HostRole::Help => print_usage(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRole {
    Service,
    Agent,
    /// Multi-process WSS: register + accept + SessionManager media.
    Ws,
    /// In-process service control + agent synthetic session (CI / dogfood).
    Colocate,
    /// G9: attach session, show Active indicator, fire local kill-switch.
    KillSwitchDemo,
    Help,
}

fn parse_role(args: &[String]) -> HostRole {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return HostRole::Help,
            "--kill-switch" => return HostRole::KillSwitchDemo,
            "--role" => {
                let value = iter.next().map(|s| s.as_str()).unwrap_or("");
                return role_from_str(value);
            }
            flag if flag.starts_with("--role=") => {
                return role_from_str(&flag["--role=".len()..]);
            }
            _ => {}
        }
    }
    HostRole::Service
}

fn role_from_str(s: &str) -> HostRole {
    match s {
        "service" => HostRole::Service,
        "agent" => HostRole::Agent,
        "ws" | "ws-agent" | "signaling" => HostRole::Ws,
        "colocate" | "synthetic" => HostRole::Colocate,
        "kill-switch" | "kill_switch" => HostRole::KillSwitchDemo,
        "help" => HostRole::Help,
        other => {
            eprintln!(
                "unknown --role `{other}` (expected service|agent|ws|colocate|kill-switch)"
            );
            HostRole::Help
        }
    }
}

fn parse_ws_host_config(args: &[String], transport: remotelink_net::TransportMode) -> WsHostConfig {
    let mut cfg = WsHostConfig {
        transport,
        ..WsHostConfig::default()
    };
    if let Some(s) = flag_value(args, "--server") {
        cfg.server = s;
    } else if let Ok(s) = env::var("REMOTELINK_SERVER") {
        if !s.is_empty() {
            cfg.server = s;
        }
    }
    if let Some(n) = flag_value(args, "--display-name") {
        cfg.display_name = n;
    }
    if let Some(n) = flag_value(args, "--frames") {
        if let Ok(v) = n.parse() {
            cfg.video_frames = v;
        }
    }
    cfg
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.next().cloned();
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// CLI `--transport=` overrides env; unset CLI falls back to [`TransportConfig::from_env`].
fn parse_transport(args: &[String]) -> TransportConfig {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--transport" {
            if let Some(v) = iter.next() {
                return TransportConfig::parse(v).unwrap_or_else(|e| {
                    eprintln!("warning: {e}; using mock");
                    TransportConfig::default()
                });
            }
        } else if let Some(rest) = arg.strip_prefix("--transport=") {
            return TransportConfig::parse(rest).unwrap_or_else(|e| {
                eprintln!("warning: {e}; using mock");
                TransportConfig::default()
            });
        }
    }
    TransportConfig::from_env()
}

fn apply_transport_env(cfg: &TransportConfig) {
    // Ensure library code that only reads env sees the CLI choice.
    // SAFETY: single-threaded main before any other threads spawn transport work.
    env::set_var("REMOTELINK_TRANSPORT", cfg.mode.as_str());
}

fn print_usage() {
    eprintln!(
        "remotelink-host {} — Windows host service / session agent\n\n\
         Usage:\n  \
         remotelink-host [--role=service|agent|ws|colocate|kill-switch] [--transport=mock|live|webrtc|auto]\n  \
         remotelink-host --role=ws --server=http://127.0.0.1:8080 --transport=live\n  \
         remotelink-host --kill-switch\n\n\
         Roles (KD5 agent-media):\n  \
         service      Enrollment, signaling, policy, kill-switch (default)\n  \
         agent        Session manager + PeerTransport synthetic A/V (in-process demos)\n  \
         ws           Multi-process: register + WSS hello/accept + SDP/ICE + media\n  \
         colocate     CI/test: in-process service control + agent synthetic session\n  \
         kill-switch  G9 demo: mandatory session indicator + local kill-switch\n\n\
         WSS host flags (role=ws):\n  \
         --server URL     Signaling base (default http://127.0.0.1:8080; or REMOTELINK_SERVER)\n  \
         --display-name N Enrollment display name\n  \
         --frames N       Synthetic video frames to pump after connect (default 5)\n\n\
         Transport (also REMOTELINK_TRANSPORT; default mock — CI-safe):\n  \
         mock         In-process MockPeerTransport (default; ws role upgrades to live)\n  \
         live         TCP length-prefixed PeerTransport (multi-process demos)\n  \
         webrtc       webrtc-rs PeerConnection (requires remotelink-net feature webrtc-rs)\n  \
         auto         Prefer webrtc (if feature on) → live → mock\n",
        remotelink_common::VERSION
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_role_is_service() {
        assert_eq!(parse_role(&[]), HostRole::Service);
    }

    #[test]
    fn parses_role_equals() {
        assert_eq!(parse_role(&["--role=agent".into()]), HostRole::Agent);
    }

    #[test]
    fn parses_role_space() {
        assert_eq!(
            parse_role(&["--role".into(), "agent".into()]),
            HostRole::Agent
        );
    }

    #[test]
    fn parses_colocate_role() {
        assert_eq!(parse_role(&["--role=colocate".into()]), HostRole::Colocate);
        assert_eq!(parse_role(&["--role=synthetic".into()]), HostRole::Colocate);
    }

    #[test]
    fn parses_kill_switch_flag_and_role() {
        assert_eq!(
            parse_role(&["--kill-switch".into()]),
            HostRole::KillSwitchDemo
        );
        assert_eq!(
            parse_role(&["--role=kill-switch".into()]),
            HostRole::KillSwitchDemo
        );
    }

    #[test]
    fn parses_transport_flag() {
        let cfg = parse_transport(&["--transport=live".into()]);
        assert_eq!(cfg.mode, TransportMode::Live);
        let cfg = parse_transport(&["--transport".into(), "auto".into()]);
        assert_eq!(cfg.mode, TransportMode::Auto);
        let cfg = parse_transport(&[]);
        // from_env without var → mock
        assert_eq!(cfg.mode, TransportMode::Mock);
    }
}

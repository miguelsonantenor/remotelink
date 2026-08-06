//! RemoteLink host binary.
//!
//! Single binary with `--role=service|agent|colocate` (DESIGN KD5):
//! - **service**: enrollment, signaling WS, policy, kill-switch orchestration
//! - **agent**: session manager + PeerTransport (synthetic A/V without real display)
//! - **colocate**: CI/test mode — service control sequence + agent in-process
//!
//! PR 14 CLI stubs:
//! - `--mint-otp` — mint Mode A OTP, store active window, print code to CLI
//! - `--unattended-enabled` / `--unattended-secret=...` — Mode B policy
//! - `--confirm-sessions` — local accept UI stub
//! - `--test-authorize-otp CODE` / `--test-mode-b` — wire SessionManager auth for tests

use std::env;

use remotelink_auth::{mode_b_viewer_response, AuthChallenge, HostSecret};
use remotelink_host::agent;
use remotelink_host::policy::{
    log_confirm_prompt, log_otp_to_cli, HostAuthService, HostLocalConfig,
};
use remotelink_host::service;
use remotelink_host::session::SessionManager;
use remotelink_protocol::SessionMode;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return;
    }

    // Policy / OTP stubs run for any role when flags present (service default).
    if let Err(e) = run_policy_cli(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    // Pure policy demo exits without starting the role skeleton when only policy flags.
    if policy_only_exit(&args) {
        return;
    }

    let role = parse_role(args.iter().cloned());
    match role {
        HostRole::Service => service::run(),
        HostRole::Agent => agent::run(),
        HostRole::Colocate => {
            println!(
                "remotelink-host {} role=colocate",
                remotelink_common::VERSION
            );
            match service::run_colocate_synthetic("colocate-cli-session") {
                Ok(summary) => println!("colocate: {summary}"),
                Err(e) => {
                    eprintln!("colocate: failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        HostRole::Help => print_usage(),
    }
}

/// Build host auth service from CLI flags and run mint / authorize demos.
fn run_policy_cli(args: &[String]) -> Result<(), String> {
    let mint = args.iter().any(|a| a == "--mint-otp");
    let confirm = args.iter().any(|a| a == "--confirm-sessions");
    let unattended = args.iter().any(|a| a == "--unattended-enabled");
    let test_mode_b = args.iter().any(|a| a == "--test-mode-b");
    let test_otp = flag_value(args, "--test-authorize-otp");
    let secret_flag = flag_value(args, "--unattended-secret");

    if !mint
        && !confirm
        && !unattended
        && !test_mode_b
        && test_otp.is_none()
        && secret_flag.is_none()
    {
        return Ok(());
    }

    let mut config = HostLocalConfig {
        confirm_sessions: confirm,
        unattended_enabled: unattended || test_mode_b || secret_flag.is_some(),
        ..HostLocalConfig::default()
    };
    if let Some(ttl) = flag_value(args, "--otp-ttl") {
        config.otp_ttl_secs = ttl
            .parse()
            .map_err(|_| format!("invalid --otp-ttl `{ttl}`"))?;
    }

    let mut auth = HostAuthService::new(config, remotelink_host::DEFAULT_HOST_OTP_PEPPER.to_vec());

    if let Some(s) = secret_flag {
        let secret = HostSecret::try_new(s.into_bytes()).map_err(|e| e.to_string())?;
        auth.set_host_secret(secret);
        auth.set_unattended_enabled(true);
        println!("host: Mode B secret installed (host-only; not logged)");
    } else if unattended || test_mode_b {
        let _ = auth.enable_unattended_with_generated_secret();
        println!("host: unattended enabled with generated HostSecret");
    }

    if confirm {
        println!("host: confirm_sessions=true (local accept UI stub armed)");
        log_confirm_prompt("pending-session-stub");
    }

    if mint {
        let code = auth.mint_otp().map_err(|e| e.to_string())?;
        let exp = auth.active_otp().map(|w| w.expires_at_unix()).unwrap_or(0);
        log_otp_to_cli(&code, exp);
        if let Some(w) = auth.active_otp() {
            let (digest_hex, salt_hex) = HostAuthService::otp_hash_wire(w.hash());
            println!(
                "host: OTP hash for POST /v1/devices/{{id}}/otp: \
                 digest_hex={digest_hex} salt_hex={salt_hex} keyed=true expires_at_unix={exp}"
            );
        }
        // Keep code for authorize test when both flags set.
        if test_otp.is_none() {
            // Code already shown; for mint-only we still hold the window in process
            // until exit (demo).
            let _ = code;
        }
    }

    if let Some(code) = test_otp {
        // Ensure there is an OTP window to consume (mint if needed).
        if auth.active_otp().is_none() {
            let minted = auth.mint_otp().map_err(|e| e.to_string())?;
            if minted.as_str() != code {
                // Mint a window then try the provided code (expect fail unless match).
                println!(
                    "host: minted fresh OTP for window (provided --test-authorize-otp may mismatch)"
                );
            }
        }
        // Prefer re-mint with known code path: consume the provided code against window.
        // If user passed mint+code, window holds minted code.
        let mut mgr = SessionManager::new_mock();
        mgr.attach("cli-auth-mode-a");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        match auth.authorize_session_mode_a(&mut mgr, &code, now) {
            Ok(()) => {
                println!(
                    "host: authorize_mode_a ok session_authorized={} input_allowed={}",
                    mgr.identity().session_authorized,
                    mgr.input_allowed()
                );
            }
            Err(e) => {
                // If mint produced a different code, try consume of mint path only.
                return Err(format!("authorize_mode_a failed: {e}"));
            }
        }
    }

    if test_mode_b {
        auth.policy_allows_mode(SessionMode::Unattended)
            .map_err(|e| e.to_string())?;
        let secret = auth
            .host_secret()
            .ok_or_else(|| "host secret missing".to_string())?;
        let challenge = AuthChallenge::issue();
        let mac = mode_b_viewer_response(
            secret,
            "cli-auth-mode-b",
            challenge.nonce.as_bytes(),
            b"",
            b"",
        );
        let mut mgr = SessionManager::new_mock();
        mgr.attach("cli-auth-mode-b");
        auth.authorize_session_mode_b(&mut mgr, &challenge, b"", b"", &mac)
            .map_err(|e| e.to_string())?;
        println!(
            "host: authorize_mode_b ok session_authorized={}",
            mgr.identity().session_authorized
        );
    }

    // Demo: unattended disabled reject path when flag `--test-mode-b-disabled`.
    if args.iter().any(|a| a == "--test-mode-b-disabled") {
        let mut disabled = HostAuthService::default();
        let secret =
            HostSecret::try_new(b"host-local-secret!!".to_vec()).map_err(|e| e.to_string())?;
        disabled.set_host_secret(secret.clone());
        let challenge = AuthChallenge::issue();
        let mac = mode_b_viewer_response(
            &secret,
            "cli-disabled",
            challenge.nonce.as_bytes(),
            b"",
            b"",
        );
        match disabled.verify_mode_b(&challenge, "cli-disabled", b"", b"", &mac) {
            Err(e) => println!("host: Mode B correctly rejected when disabled: {e}"),
            Ok(_) => return Err("expected Mode B reject when unattended disabled".into()),
        }
    }

    Ok(())
}

fn policy_only_exit(args: &[String]) -> bool {
    let has_policy = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--mint-otp"
                | "--confirm-sessions"
                | "--unattended-enabled"
                | "--test-mode-b"
                | "--test-mode-b-disabled"
        ) || a.starts_with("--test-authorize-otp")
            || a.starts_with("--unattended-secret")
            || a.starts_with("--otp-ttl")
    });
    if !has_policy {
        return false;
    }
    // If an explicit role is set, continue into role skeleton.
    !args
        .iter()
        .any(|a| a == "--role" || a.starts_with("--role="))
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == name {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRole {
    Service,
    Agent,
    /// In-process service control + agent synthetic session (CI / dogfood).
    Colocate,
    Help,
}

fn parse_role(args: impl IntoIterator<Item = String>) -> HostRole {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return HostRole::Help,
            "--role" => {
                let value = iter.next().unwrap_or_default();
                return role_from_str(&value);
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
        "colocate" | "synthetic" => HostRole::Colocate,
        "help" => HostRole::Help,
        other => {
            eprintln!("unknown --role `{other}` (expected service|agent|colocate)");
            HostRole::Help
        }
    }
}

fn print_usage() {
    eprintln!(
        "remotelink-host {} — Windows host service / session agent\n\n\
         Usage:\n  \
         remotelink-host [--role=service|agent|colocate]\n  \
         remotelink-host --mint-otp [--otp-ttl=300]\n  \
         remotelink-host --unattended-enabled [--unattended-secret=BYTES]\n  \
         remotelink-host --confirm-sessions\n  \
         remotelink-host --test-authorize-otp CODE\n  \
         remotelink-host --test-mode-b | --test-mode-b-disabled\n\n\
         Roles (KD5 agent-media):\n  \
         service   Enrollment, signaling, policy, kill-switch (default)\n  \
         agent     Session manager + mock PeerTransport synthetic A/V\n  \
         colocate  CI/test: in-process service control + agent synthetic session\n\n\
         Policy (PR 14):\n  \
         --mint-otp              Mint Mode A OTP; print code to CLI (v1 tray)\n  \
         --unattended-enabled    Allow Mode B challenge-response\n  \
         --confirm-sessions      Local accept UI stub for incoming sessions\n",
        remotelink_common::VERSION
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_role_is_service() {
        assert_eq!(parse_role(Vec::<String>::new()), HostRole::Service);
    }

    #[test]
    fn parses_role_equals() {
        assert_eq!(parse_role(vec!["--role=agent".into()]), HostRole::Agent);
    }

    #[test]
    fn parses_role_space() {
        assert_eq!(
            parse_role(vec!["--role".into(), "agent".into()]),
            HostRole::Agent
        );
    }

    #[test]
    fn parses_colocate_role() {
        assert_eq!(
            parse_role(vec!["--role=colocate".into()]),
            HostRole::Colocate
        );
        assert_eq!(
            parse_role(vec!["--role=synthetic".into()]),
            HostRole::Colocate
        );
    }

    #[test]
    fn policy_only_exit_without_role() {
        assert!(policy_only_exit(&["--mint-otp".into()]));
        assert!(!policy_only_exit(&[
            "--mint-otp".into(),
            "--role=service".into()
        ]));
        assert!(!policy_only_exit(&[]));
    }

    #[test]
    fn flag_value_otp() {
        let args = vec!["--test-authorize-otp".into(), "123456".into()];
        assert_eq!(
            flag_value(&args, "--test-authorize-otp").as_deref(),
            Some("123456")
        );
    }
}

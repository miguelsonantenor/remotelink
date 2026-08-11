//! RemoteLink host binary.
//!
//! Single binary with `--role=service|agent|colocate` (DESIGN KD5):
//! - **service**: enrollment, signaling WS, policy, kill-switch orchestration
//! - **agent**: session manager + PeerTransport (synthetic A/V without real display)
//! - **colocate**: CI/test mode — service control sequence + agent in-process
//! - **`--kill-switch`**: G9 demo — active session indicator then local kill
//!
//! Control IPC is length-prefixed JSON (no media bytes).

use std::env;

use remotelink_host::agent;
use remotelink_host::service;

fn main() {
    let role = parse_role(env::args().skip(1));
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
        HostRole::KillSwitchDemo => {
            println!(
                "remotelink-host {} role=kill-switch-demo",
                remotelink_common::VERSION
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
    /// In-process service control + agent synthetic session (CI / dogfood).
    Colocate,
    /// G9: attach session, show Active indicator, fire local kill-switch.
    KillSwitchDemo,
    Help,
}

fn parse_role(args: impl IntoIterator<Item = String>) -> HostRole {
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return HostRole::Help,
            "--kill-switch" => return HostRole::KillSwitchDemo,
            "--role" => {
                let value = iter.next().unwrap_or_default();
                return role_from_str(&value);
            }
            flag if flag.starts_with("--role=") => {
                return role_from_str(&flag["--role=".len()..]);
            }
            _ => {
                // Unknown flags ignored in skeleton; default remains service.
            }
        }
    }
    HostRole::Service
}

fn role_from_str(s: &str) -> HostRole {
    match s {
        "service" => HostRole::Service,
        "agent" => HostRole::Agent,
        "colocate" | "synthetic" => HostRole::Colocate,
        "kill-switch" | "kill_switch" => HostRole::KillSwitchDemo,
        "help" => HostRole::Help,
        other => {
            eprintln!("unknown --role `{other}` (expected service|agent|colocate|kill-switch)");
            HostRole::Help
        }
    }
}

fn print_usage() {
    eprintln!(
        "remotelink-host {} — Windows host service / session agent\n\n\
         Usage:\n  \
         remotelink-host [--role=service|agent|colocate|kill-switch]\n  \
         remotelink-host --kill-switch\n\n\
         Roles (KD5 agent-media):\n  \
         service      Enrollment, signaling, policy, kill-switch (default)\n  \
         agent        Session manager + mock PeerTransport synthetic A/V\n  \
         colocate     CI/test: in-process service control + agent synthetic session\n  \
         kill-switch  G9 demo: mandatory session indicator + local kill-switch\n",
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
    fn parses_kill_switch_flag_and_role() {
        assert_eq!(
            parse_role(vec!["--kill-switch".into()]),
            HostRole::KillSwitchDemo
        );
        assert_eq!(
            parse_role(vec!["--role=kill-switch".into()]),
            HostRole::KillSwitchDemo
        );
    }
}

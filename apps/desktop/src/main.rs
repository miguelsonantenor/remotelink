//! RemoteLink product shell — single window for “this PC” + connect.
//!
//! ```text
//! cargo run -p remotelink-desktop
//! # binary: remotelink-app
//! ```
//!
//! Needs a signaling server (lab: `remotelink-server` on :18080). Host enrolls
//! automatically when “Allow remote access” is on; OTP and public ID appear
//! on the home screen.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

mod app;
mod config;
mod host_worker;
mod startup;
mod status;
mod viewer_worker;

use app::RemoteLinkApp;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }

    // Optional: --server= overrides config for this run only (not persisted until Save).
    if let Some(server) = flag_value(&args, "--server") {
        std::env::set_var("REMOTELINK_SERVER", server);
    }
    let autostart = args.iter().any(|a| a == "--autostart");
    if autostart {
        std::env::set_var("REMOTELINK_AUTOSTART", "1");
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 560.0])
            .with_min_inner_size([420.0, 420.0])
            .with_title("RemoteLink"),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "RemoteLink",
        options,
        Box::new(|cc| Ok(Box::new(RemoteLinkApp::new(cc)))),
    ) {
        eprintln!("remotelink-app: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    eprintln!(
        "remotelink-app {} — product shell (Phase 3 live session)\n\n\
         Usage:\n  \
         remotelink-app [--server=http://HOST:PORT] [--autostart]\n\n\
         Home screen:\n  \
         • This PC — public ID + OTP when host is enrolled (Copy ID + OTP)\n  \
         • Connect — remote ID + OTP (paste the pair into Remote ID)\n  \
         • Live session takes over the window; Fullscreen or Esc\n  \
         • Advanced — signaling URL, transport, Start with Windows, data folder\n\n\
         Lab:\n  \
         1. remotelink-server\n  \
         2. remotelink-app   (Allow remote access)\n  \
         3. On another machine/app instance: Connect with ID + OTP\n\n\
         Data: %LOCALAPPDATA%\\RemoteLink (or REMOTELINK_DATA_DIR)\n",
        remotelink_common::VERSION
    );
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

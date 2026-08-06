//! RemoteLink viewer binary.
//!
//! Default path is a **CLI connect shell** that drives toolkit-agnostic
//! `remotelink-viewer-core` (synthetic loopback or credential stubs).
//!
//! Optional egui shell: build with `--features gui` and pass `--gui`.

use std::env;

use remotelink_viewer_core::{
    connect_stub, run_synthetic_loopback, ConnectRequest, ViewerPhase, ViewerSession,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return;
    }

    #[cfg(feature = "gui")]
    if args.iter().any(|a| a == "--gui") {
        if let Err(e) = gui::run() {
            eprintln!("gui error: {e}");
            std::process::exit(1);
        }
        return;
    }

    #[cfg(not(feature = "gui"))]
    if args.iter().any(|a| a == "--gui") {
        eprintln!(
            "gui support not compiled in; rebuild with --features gui \
             or use the CLI shell (default)"
        );
        std::process::exit(2);
    }

    if let Err(e) = run_cli(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "remotelink-viewer {} — connect shell (viewer-core)\n\n\
         Usage:\n  \
         remotelink-viewer [--synthetic] [--host ID] [--password PW | --otp CODE | --unattended SECRET]\n  \
         remotelink-viewer --connect-stub --host ID --otp CODE\n  \
         remotelink-viewer --gui          (requires --features gui)\n\n\
         Defaults:\n  \
         --synthetic   run mock host→viewer media loopback and print stats\n  \
         --otp CODE    Mode A session_intent with prefilter.otp\n  \
         --unattended  Mode B session_intent (secret not in prefilter)\n",
        remotelink_common::VERSION
    );
}

fn run_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let host = flag_value(args, "--host").unwrap_or_else(|| "demo-host".into());
    let password = flag_value(args, "--password");
    let otp = flag_value(args, "--otp");
    let unattended = flag_value(args, "--unattended");
    let connect_stub_only = args.iter().any(|a| a == "--connect-stub");
    let want_synthetic = args.iter().any(|a| a == "--synthetic");
    // Default with no args: synthetic demo. Credentials / --connect-stub → stub path.
    let has_creds = password.is_some() || otp.is_some() || unattended.is_some();
    let run_stub = connect_stub_only || (has_creds && !want_synthetic);
    let run_synthetic = want_synthetic || args.is_empty() || (!run_stub && !want_synthetic);

    if run_stub {
        let req = build_request(
            &host,
            password.as_deref(),
            otp.as_deref(),
            unattended.as_deref(),
        )?;
        let stub = connect_stub(&req)?;
        println!(
            "remotelink-viewer {} phase=connect_stub session_id={} host={} mode={:?}",
            remotelink_common::VERSION,
            stub.session_id,
            req.host_public_id,
            req.mode()
        );
        println!("intent={}", req.session_intent_stub(&stub.session_id, 1)?);
        let mut session = ViewerSession::new();
        session.begin_connect(&req)?;
        println!("viewer_phase={}", session.phase().as_str());
        return Ok(());
    }

    if run_synthetic {
        println!(
            "remotelink-viewer {} synthetic loopback (MockPeerTransport answerer)",
            remotelink_common::VERSION
        );
        let req = build_request(
            &host,
            password.as_deref(),
            otp.as_deref().or(Some("123456")),
            unattended.as_deref(),
        )?;
        println!(
            "connect host={} mode={:?} (stub ok)",
            req.host_public_id,
            req.mode()
        );
        let (session, stats) = run_synthetic_loopback(5, 4)?;
        println!(
            "phase={} video_frames={} audio_packets={} recorded_nalus={} recorded_audio={}",
            session.phase().as_str(),
            stats.video_frames,
            stats.audio_packets,
            session.recorded_video_nalus().len(),
            session.recorded_audio_packets().len()
        );
        if session.phase() != &ViewerPhase::Streaming {
            return Err(format!("expected streaming, got {}", session.phase().as_str()).into());
        }
        return Ok(());
    }

    print_usage();
    Ok(())
}

fn build_request(
    host: &str,
    password: Option<&str>,
    otp: Option<&str>,
    unattended: Option<&str>,
) -> Result<ConnectRequest, Box<dyn std::error::Error>> {
    let req = if let Some(secret) = unattended {
        ConnectRequest::unattended(host, secret)
    } else if let Some(pw) = password {
        ConnectRequest::password(host, pw)
    } else if let Some(code) = otp {
        ConnectRequest::otp(host, code)
    } else {
        ConnectRequest::otp(host, "123456")
    };
    req.validate()?;
    Ok(req)
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

#[cfg(feature = "gui")]
mod gui {
    use eframe::egui;
    use remotelink_viewer_core::{
        connect_stub, run_synthetic_loopback, ConnectRequest, ViewerPhase,
    };

    pub fn run() -> eframe::Result<()> {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([420.0, 320.0])
                .with_title("RemoteLink Viewer"),
            ..Default::default()
        };
        eframe::run_native(
            "RemoteLink Viewer",
            options,
            Box::new(|_cc| Ok(Box::new(ConnectApp::default()))),
        )
    }

    #[derive(Default)]
    struct ConnectApp {
        host_public_id: String,
        secret: String,
        use_otp: bool,
        status: String,
        last_phase: String,
    }

    impl eframe::App for ConnectApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("RemoteLink — Connect");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Host public ID");
                    ui.text_edit_singleline(&mut self.host_public_id);
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.use_otp, "OTP mode");
                    ui.label(if self.use_otp { "OTP" } else { "Password" });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.secret)
                            .password(!self.use_otp)
                            .desired_width(180.0),
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Connect (stub)").clicked() {
                        self.do_connect_stub();
                    }
                    if ui.button("Synthetic media").clicked() {
                        self.do_synthetic();
                    }
                });
                ui.separator();
                ui.label(&self.status);
                if !self.last_phase.is_empty() {
                    ui.label(format!("phase: {}", self.last_phase));
                }
            });
        }
    }

    impl ConnectApp {
        fn do_connect_stub(&mut self) {
            let req = if self.use_otp {
                ConnectRequest::otp(&self.host_public_id, &self.secret)
            } else {
                ConnectRequest::password(&self.host_public_id, &self.secret)
            };
            match connect_stub(&req) {
                Ok(stub) => {
                    self.status = format!("session {}", stub.session_id);
                    self.last_phase = ViewerPhase::Connecting.as_str().into();
                }
                Err(e) => {
                    self.status = format!("error: {e}");
                    self.last_phase.clear();
                }
            }
        }

        fn do_synthetic(&mut self) {
            match run_synthetic_loopback(3, 2) {
                Ok((session, stats)) => {
                    self.status = format!(
                        "synthetic ok: video={} audio={}",
                        stats.video_frames, stats.audio_packets
                    );
                    self.last_phase = session.phase().as_str().into();
                }
                Err(e) => {
                    self.status = format!("synthetic error: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_value_space_and_eq() {
        let args = vec!["--host".into(), "abc".into()];
        assert_eq!(flag_value(&args, "--host").as_deref(), Some("abc"));
        let args = vec!["--otp=654321".into()];
        assert_eq!(flag_value(&args, "--otp").as_deref(), Some("654321"));
    }

    #[test]
    fn build_request_password() {
        let r = build_request("h1", Some("pw"), None, None).unwrap();
        assert!(matches!(
            r.secret,
            remotelink_viewer_core::ConnectSecret::Password(_)
        ));
    }

    #[test]
    fn build_request_otp_mode() {
        let r = build_request("h1", None, Some("654321"), None).unwrap();
        assert_eq!(r.mode(), remotelink_protocol::SessionMode::Otp);
    }

    #[test]
    fn build_request_unattended() {
        let r = build_request("h1", None, None, Some("host-local-secret!!")).unwrap();
        assert_eq!(r.mode(), remotelink_protocol::SessionMode::Unattended);
    }
}

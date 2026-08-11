//! RemoteLink viewer binary.
//!
//! Default path is a **CLI connect shell** that drives toolkit-agnostic
//! `remotelink-viewer-core` (synthetic / mock-codec loopback or credential stubs).
//!
//! Beta HUD (G3): prints required skew stats every N frames (`--hud-interval`).
//!
//! Input (PR 19): `--inject-input` sends a demo burst on the mock DataChannel;
//! `--always-capture` enables input while unfocused (default for headless).
//!
//! Optional egui shell: build with `--features gui` and pass `--gui`.
//!
//! Multi-process WSS: `--ws-connect --server=… --host PUBLIC_ID --transport=live`.

use std::env;

use remotelink_net::{TransportConfig, TransportMode};
use remotelink_viewer::{run_ws_viewer_blocking, WsViewerConfig};
use remotelink_viewer_core::{
    connect_stub, run_mock_codec_loopback_ex, run_synthetic_loopback_ex, ConnectRequest,
    SessionStats, ViewerPhase, ViewerSession,
};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return;
    }

    let transport = parse_transport(&args);
    // SAFETY: single-threaded main; library paths that read env see CLI choice.
    env::set_var("REMOTELINK_TRANSPORT", transport.mode.as_str());

    let dump_metrics = args.iter().any(|a| a == "--metrics");

    #[cfg(feature = "gui")]
    if args.iter().any(|a| a == "--gui") {
        if let Err(e) = gui::run() {
            eprintln!("gui error: {e}");
            std::process::exit(1);
        }
        if dump_metrics {
            print!("{}", remotelink_common::encode_process_metrics());
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

    // Metrics-only dump (no media loop).
    if dump_metrics
        && !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "--synthetic" | "--mock-codec" | "--connect-stub" | "--inject-input" | "--gui"
            ) || a.starts_with("--host")
                || a.starts_with("--otp")
                || a.starts_with("--password")
                || a.starts_with("--unattended")
        })
        && args.iter().all(|a| a == "--metrics")
    {
        print!("{}", remotelink_common::encode_process_metrics());
        return;
    }

    if let Err(e) = run_cli(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }

    if dump_metrics {
        print!("{}", remotelink_common::encode_process_metrics());
    }
}

fn print_usage() {
    eprintln!(
        "remotelink-viewer {} — connect shell (viewer-core)\n\n\
         Usage:\n  \
         remotelink-viewer [--synthetic | --mock-codec | --live-demo | --webrtc-demo | --ws-connect] [--host ID] \
         [--password PW | --otp CODE | --unattended SECRET]\n  \
         remotelink-viewer --ws-connect --server=http://127.0.0.1:8080 --host PUBLIC_ID --otp 123456 --transport=live\n  \
         remotelink-viewer --connect-stub --host ID --otp CODE\n  \
         remotelink-viewer --gui          (requires --features gui)\n\n\
         Transport (also REMOTELINK_TRANSPORT; default mock — CI-safe):\n  \
         --transport=mock|live|webrtc|auto\n  \
         --live-demo       localhost TCP PeerTransport answerer demo (real sockets)\n  \
         --webrtc-demo     webrtc-rs PeerConnection demo (requires --features webrtc-rs)\n  \
         --ws-connect      Multi-process: WSS intent/accept + SDP/ICE + media RX\n  \
         --server URL      Signaling base for --ws-connect (or REMOTELINK_SERVER)\n\n\
         Input (PR 19):\n  \
         --inject-input     after synthetic/mock media, send demo mouse/key events\n\
                            on DataChannel \"input\" (capture + scancode path)\n  \
         --always-capture   opt-in: send input even when the window is unfocused\n\
                            (default is focused-only per DESIGN; demos set focus)\n\
                            Continuous pointer streams need poll_input_capture each frame\n\n\
         Beta HUD (G3 required skew stats):\n  \
         --hud-interval N   print stats every N video frames (default 1 for mock-codec,\n\
                            0 = only final snapshot)\n  \
         --hud-block        print multi-line HUD block instead of one line\n\n\
         Observability (PR 21):\n  \
         --metrics          dump Prometheus text registry to stdout (alone or after run)\n\n\
         Defaults:\n  \
         --synthetic   run mock host→viewer media loopback and print stats\n  \
         --mock-codec  MH264 + MOPU roundtrip with skew HUD (PR 17)\n  \
         --otp CODE    Mode A session_intent with prefilter.otp\n  \
         --unattended  Mode B session_intent (secret not in prefilter)\n",
        remotelink_common::VERSION
    );
}

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
    // Demo flags imply the matching transport for logging / factory defaults.
    if args.iter().any(|a| a == "--live-demo") {
        return TransportConfig {
            mode: TransportMode::Live,
        };
    }
    if args.iter().any(|a| a == "--webrtc-demo") {
        return TransportConfig {
            mode: TransportMode::Webrtc,
        };
    }
    TransportConfig::from_env()
}

fn run_cli(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let host = flag_value(args, "--host").unwrap_or_else(|| "demo-host".into());
    let password = flag_value(args, "--password");
    let otp = flag_value(args, "--otp");
    let unattended = flag_value(args, "--unattended");
    let connect_stub_only = args.iter().any(|a| a == "--connect-stub");
    let want_synthetic = args.iter().any(|a| a == "--synthetic");
    let want_mock_codec = args.iter().any(|a| a == "--mock-codec");
    let resolved = parse_transport(args).resolved_mode();
    let want_live_demo = args.iter().any(|a| a == "--live-demo")
        || (resolved == TransportMode::Live
            && !want_synthetic
            && !want_mock_codec
            && !connect_stub_only
            && (args
                .iter()
                .any(|a| a == "--transport" || a.starts_with("--transport="))));
    let want_webrtc_demo = args.iter().any(|a| a == "--webrtc-demo")
        || (resolved == TransportMode::Webrtc
            && !want_synthetic
            && !want_mock_codec
            && !connect_stub_only
            && !args.iter().any(|a| a == "--ws-connect")
            && (args
                .iter()
                .any(|a| a == "--transport" || a.starts_with("--transport="))));
    let want_ws_connect = args.iter().any(|a| a == "--ws-connect");
    let inject_input = args.iter().any(|a| a == "--inject-input");
    let always_capture = args.iter().any(|a| a == "--always-capture");
    let hud_block = args.iter().any(|a| a == "--hud-block");
    let hud_interval = flag_value(args, "--hud-interval")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(if want_mock_codec { 1 } else { 0 });

    if want_ws_connect {
        let mut cfg = WsViewerConfig {
            host_public_id: host.clone(),
            transport: resolved,
            ..WsViewerConfig::default()
        };
        if let Some(s) = flag_value(args, "--server") {
            cfg.server = s;
        } else if let Ok(s) = env::var("REMOTELINK_SERVER") {
            if !s.is_empty() {
                cfg.server = s;
            }
        }
        if let Some(o) = otp.as_deref() {
            cfg.otp = o.to_string();
        }
        return match run_ws_viewer_blocking(cfg) {
            Ok(summary) => {
                println!("{summary}");
                Ok(())
            }
            Err(e) => Err(e.into()),
        };
    }
    if want_webrtc_demo {
        return run_webrtc_demo();
    }
    if want_live_demo {
        return run_live_demo();
    }

    // Default with no args: synthetic demo. Credentials / --connect-stub → stub path.
    let has_creds = password.is_some() || otp.is_some() || unattended.is_some();
    let run_stub = connect_stub_only || (has_creds && !want_synthetic && !want_mock_codec);
    let run_media = want_synthetic
        || want_mock_codec
        || args.is_empty()
        || (!run_stub && !want_synthetic && !want_mock_codec);

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
        print_hud(session.stats(), hud_block);
        return Ok(());
    }

    if run_media {
        let mode = if want_mock_codec {
            "mock-codec (MH264 + MOPU)"
        } else {
            "synthetic loopback"
        };
        println!(
            "remotelink-viewer {} {mode} (MockPeerTransport answerer)",
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

        let video_n = 5usize;
        let audio_n = 4usize;
        // Inject demo input while the host mock peer is still live (before pair drop).
        let (session, stats, demo_sent) = if want_mock_codec {
            let (s, st, n) = run_mock_codec_loopback_ex(video_n, audio_n, inject_input)?;
            if hud_interval > 0 {
                for i in 1..=st.video_frames {
                    if i % hud_interval == 0 || i == st.video_frames {
                        println!("[hud frame {i}/{}] {}", st.video_frames, st.hud_line());
                    }
                }
            }
            (s, st, n)
        } else {
            let (s, st, n) = run_synthetic_loopback_ex(video_n, audio_n, inject_input)?;
            if hud_interval > 0 {
                for i in 1..=st.video_frames {
                    if i % hud_interval == 0 || i == st.video_frames {
                        println!("[hud frame {i}/{}] {}", st.video_frames, st.hud_line());
                    }
                }
            }
            (s, st, n)
        };

        if always_capture {
            // Demo loopbacks apply focus for inject; always_capture is the DESIGN
            // opt-in for a long-lived GUI session (focused-only is the default).
            println!("input: always_capture=true (opt-in; session default is focused-only)");
        }
        if inject_input {
            println!(
                "input: injected {demo_sent} demo events via capture path (input_events={})",
                stats.input_events
            );
        }

        println!(
            "phase={} video_frames={} audio_packets={} mock_h264={} mopu={} recorded_nalus={} recorded_audio={} input_events={}",
            session.phase().as_str(),
            stats.video_frames,
            stats.audio_packets,
            stats.mock_h264_frames,
            stats.mock_opus_packets,
            session.recorded_video_nalus().len(),
            session.recorded_audio_packets().len(),
            stats.input_events
        );
        print_hud(&stats, hud_block);

        // Export G3 skew + ICE placeholder into the process metrics registry.
        let reg = remotelink_common::process_registry();
        reg.set_skew_ms(stats.skew_ms);
        reg.inc_ice_path(remotelink_common::IcePath::Host);
        for _ in 0..stats.input_events {
            reg.inc_input_event();
        }

        if session.phase() != &ViewerPhase::Streaming {
            return Err(format!("expected streaming, got {}", session.phase().as_str()).into());
        }
        if !stats.has_required_skew_metric() {
            return Err("G3: skew metric missing from stats export".into());
        }
        return Ok(());
    }

    print_usage();
    Ok(())
}

fn print_hud(stats: &SessionStats, block: bool) {
    if block {
        print!("{}", stats.hud_block());
    } else {
        println!("hud {}", stats.hud_line());
    }
}

/// webrtc-rs PeerTransport demo (answerer + in-process offerer).
///
/// Requires building with `--features webrtc-rs`.
fn run_webrtc_demo() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "webrtc-rs")]
    {
        use std::time::Duration;

        use remotelink_net::{
            webrtc_handshake, AudioPacket, DataMessage, IncomingTrackData, NaluFormat, PeerRole,
            PeerTransport, SharedRecording, VideoNalu, WebrtcPeerConfig, WebrtcPeerTransport,
            LABEL_INPUT,
        };

        println!(
            "remotelink-viewer {} webrtc-rs PeerTransport demo (answerer)",
            remotelink_common::VERSION
        );

        let mut offerer =
            WebrtcPeerTransport::new(PeerRole::Offerer, WebrtcPeerConfig::default())?;
        let mut answerer =
            WebrtcPeerTransport::new(PeerRole::Answerer, WebrtcPeerConfig::default())?;
        let rec = SharedRecording::new();
        answerer.set_callbacks(Box::new(rec.clone()));

        webrtc_handshake(&mut offerer, &mut answerer, Duration::from_secs(15))?;
        offerer.wait_data_channels_open(Duration::from_secs(5))?;

        offerer.send_video_nalu(VideoNalu {
            pts_host_mono: Duration::from_millis(16),
            rtp_ts: Some(1440),
            keyframe: true,
            format: NaluFormat::AnnexB,
            data: vec![0, 0, 0, 1, 0x65],
        })?;
        offerer.send_audio(AudioPacket {
            pts_host_mono: Duration::from_millis(16),
            rtp_ts: Some(768),
            sample_rate: 48_000,
            channels: 2,
            data: vec![9, 8, 7, 6],
        })?;
        offerer.send_data(DataMessage {
            label: LABEL_INPUT.into(),
            data: b"{}".to_vec(),
            unordered: false,
        })?;

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            answerer.poll()?;
            let snap = rec.snapshot();
            if snap.tracks.len() >= 2 && !snap.data.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err("webrtc demo: timeout waiting for frames".into());
            }
            std::thread::sleep(Duration::from_millis(15));
        }

        let snap = rec.snapshot();
        let video_n = snap
            .tracks
            .iter()
            .filter(|t| matches!(t, IncomingTrackData::Video(_)))
            .count();
        let audio_n = snap
            .tracks
            .iter()
            .filter(|t| matches!(t, IncomingTrackData::Audio(_)))
            .count();
        let fp = answerer
            .remote_fingerprint()?
            .ok_or("missing remote fingerprint")?;

        println!(
            "webrtc ok: video_rx={video_n} audio_rx={audio_n} data_rx={} remote_fp={}",
            snap.data.len(),
            fp.as_sign_material()
        );

        offerer.close()?;
        answerer.close()?;
        Ok(())
    }
    #[cfg(not(feature = "webrtc-rs"))]
    {
        Err(
            "webrtc demo requires --features webrtc-rs \
             (cargo run -p remotelink-viewer --features webrtc-rs -- --webrtc-demo)"
                .into(),
        )
    }
}

/// Localhost TCP PeerTransport demo (answerer + in-process offerer).
fn run_live_demo() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Duration;

    use remotelink_net::{
        live_handshake, AudioPacket, DataMessage, IncomingTrackData, LivePeerConfig,
        LivePeerTransport, NaluFormat, PeerRole, PeerTransport, SharedRecording, VideoNalu,
    };

    println!(
        "remotelink-viewer {} live TCP PeerTransport demo (answerer)",
        remotelink_common::VERSION
    );

    let mut offerer = LivePeerTransport::new(PeerRole::Offerer, LivePeerConfig::default())?;
    let mut answerer = LivePeerTransport::new(PeerRole::Answerer, LivePeerConfig::default())?;
    let rec = SharedRecording::new();
    answerer.set_callbacks(Box::new(rec.clone()));

    live_handshake(&mut offerer, &mut answerer)?;

    offerer.send_video_nalu(VideoNalu {
        pts_host_mono: Duration::from_millis(16),
        rtp_ts: Some(1440),
        keyframe: true,
        format: NaluFormat::AnnexB,
        data: vec![0, 0, 0, 1, 0x65],
    })?;
    offerer.send_audio(AudioPacket {
        pts_host_mono: Duration::from_millis(16),
        rtp_ts: Some(768),
        sample_rate: 48_000,
        channels: 2,
        data: vec![9, 8, 7, 6],
    })?;
    offerer.send_data(DataMessage {
        label: "input".into(),
        data: b"{}".to_vec(),
        unordered: true,
    })?;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        answerer.poll()?;
        let snap = rec.snapshot();
        if snap.tracks.len() >= 2 && !snap.data.is_empty() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            return Err("live demo: timeout waiting for frames".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let snap = rec.snapshot();
    let video_n = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, IncomingTrackData::Video(_)))
        .count();
    let audio_n = snap
        .tracks
        .iter()
        .filter(|t| matches!(t, IncomingTrackData::Audio(_)))
        .count();
    let fp = answerer
        .remote_fingerprint()?
        .ok_or("missing remote fingerprint")?;

    println!(
        "live ok: video_rx={video_n} audio_rx={audio_n} data_rx={} remote_fp={}",
        snap.data.len(),
        fp.as_sign_material()
    );

    offerer.close()?;
    answerer.close()?;
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
        connect_stub, run_mock_codec_loopback_ex, run_synthetic_loopback_ex, ConnectRequest,
        ViewerPhase,
    };

    pub fn run() -> eframe::Result<()> {
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([480.0, 420.0])
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
        /// Beta HUD overlay text (G3 skew stats).
        hud_text: String,
        show_hud: bool,
        /// When true, input is sent even if the viewport is unfocused.
        always_capture: bool,
        /// Last demo input event count from synthetic/mock inject.
        last_input_events: u64,
    }

    impl eframe::App for ConnectApp {
        fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            // When a long-lived ViewerSession is held here: drive
            // set_focused(ctx.input(|i| i.focused)), set_always_capture(self.always_capture),
            // push_raw_input(...), and poll_input_capture() every frame for continuous moves.
            let _focused = ctx.input(|i| i.focused);

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
                    if ui.button("Mock codec + skew").clicked() {
                        self.do_mock_codec();
                    }
                });
                ui.checkbox(&mut self.show_hud, "Show beta HUD (G3 skew)");
                ui.checkbox(
                    &mut self.always_capture,
                    "Always capture input (even unfocused)",
                );
                if self.always_capture {
                    ui.colored_label(
                        egui::Color32::from_rgb(200, 140, 40),
                        "Warning: input may leave the app while unfocused",
                    );
                }
                ui.separator();
                ui.label(&self.status);
                if !self.last_phase.is_empty() {
                    ui.label(format!("phase: {}", self.last_phase));
                }
                if self.last_input_events > 0 {
                    ui.label(format!("demo input_events: {}", self.last_input_events));
                }
                if self.show_hud && !self.hud_text.is_empty() {
                    ui.separator();
                    ui.heading("Beta stats");
                    ui.monospace(&self.hud_text);
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
                    self.hud_text.clear();
                    self.last_input_events = 0;
                }
                Err(e) => {
                    self.status = format!("error: {e}");
                    self.last_phase.clear();
                }
            }
        }

        fn do_synthetic(&mut self) {
            match run_synthetic_loopback_ex(3, 2, true) {
                Ok((session, stats, n)) => {
                    // Batch demo uses inject_demo_input (sets focus). always_capture
                    // applies when a long-lived ViewerSession is wired to the frame loop.
                    let _ = self.always_capture;
                    self.last_input_events = session.stats().input_events;
                    self.status = format!(
                        "synthetic ok: video={} audio={} skew_ms={:.2} input={n}",
                        stats.video_frames, stats.audio_packets, stats.skew_ms
                    );
                    self.last_phase = session.phase().as_str().into();
                    self.hud_text = session.stats().hud_block();
                    self.show_hud = true;
                }
                Err(e) => {
                    self.status = format!("synthetic error: {e}");
                }
            }
        }

        fn do_mock_codec(&mut self) {
            match run_mock_codec_loopback_ex(5, 4, true) {
                Ok((session, stats, n)) => {
                    let _ = self.always_capture;
                    self.last_input_events = session.stats().input_events;
                    self.status = format!(
                        "mock-codec ok: h264={} mopu={} skew_ms={:.2} input={n}",
                        stats.mock_h264_frames, stats.mock_opus_packets, stats.skew_ms
                    );
                    self.last_phase = session.phase().as_str().into();
                    self.hud_text = session.stats().hud_block();
                    self.show_hud = true;
                }
                Err(e) => {
                    self.status = format!("mock-codec error: {e}");
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

    #[test]
    fn mock_codec_loopback_exports_skew_hud() {
        let (_s, stats, _) = run_mock_codec_loopback_ex(2, 2, false).unwrap();
        assert!(stats.has_required_skew_metric());
        let line = stats.hud_line();
        assert!(line.contains("skew_ms="), "{line}");
        assert!(line.contains("bind="), "{line}");
    }

    #[test]
    fn inject_demo_input_during_synthetic() {
        let (session, stats, n) = run_synthetic_loopback_ex(1, 1, true).unwrap();
        assert_eq!(n, 6);
        assert_eq!(session.stats().input_events, 6);
        assert_eq!(stats.input_events, 6);
    }
}

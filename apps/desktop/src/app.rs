//! RemoteLink product shell — home screen (this PC + connect).

use std::str::FromStr;

use eframe::egui;

use crate::config::{ensure_parent, AppConfig};
use crate::host_worker::HostWorker;
use crate::status::{read_status, status_age_secs, HostStatusSnapshot};
use crate::viewer_worker::{ConnectOutcome, ViewerWorker};
use remotelink_net::TransportMode;

/// Main eframe application.
pub struct RemoteLinkApp {
    config: AppConfig,
    show_advanced: bool,
    /// Connect form: remote host public id.
    remote_id: String,
    /// Connect form: OTP / password.
    remote_secret: String,
    /// Status line under Connect.
    connect_status: String,
    /// Host allow-access desired state (UI toggle).
    allow_access: bool,
    host: Option<HostWorker>,
    viewer: Option<ViewerWorker>,
    host_status: HostStatusSnapshot,
    host_error: Option<String>,
    footer_note: String,
    last_status_poll: f64,
}

impl RemoteLinkApp {
    /// Build app; optionally auto-start host from config.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Dark-ish product chrome.
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);

        let config = AppConfig::load();
        let allow_access = config.auto_start_host;
        let mut app = Self {
            config,
            show_advanced: false,
            remote_id: String::new(),
            remote_secret: String::new(),
            connect_status: String::new(),
            allow_access,
            host: None,
            viewer: None,
            host_status: HostStatusSnapshot::default(),
            host_error: None,
            footer_note: format!(
                "RemoteLink {} · product shell (Phase 1)",
                remotelink_common::VERSION
            ),
            last_status_poll: 0.0,
        };
        if allow_access {
            app.start_host();
        }
        app
    }

    fn transport_mode(&self) -> TransportMode {
        TransportMode::from_str(&self.config.transport).unwrap_or(TransportMode::Live)
    }

    fn start_host(&mut self) {
        if self.host.as_ref().is_some_and(|h| h.is_running()) {
            return;
        }
        let status_path = AppConfig::status_path();
        let creds_path = AppConfig::creds_path();
        if let Err(e) = ensure_parent(&status_path) {
            self.host_error = Some(e);
            return;
        }
        if let Err(e) = ensure_parent(&creds_path) {
            self.host_error = Some(e);
            return;
        }
        self.host_error = None;
        self.host = Some(HostWorker::start(
            self.config.server.clone(),
            self.config.display_name.clone(),
            self.transport_mode(),
            status_path,
            creds_path,
        ));
        self.connect_status.clear();
    }

    fn stop_host_ui(&mut self) {
        // Cooperative host stop is not wired in the service loop yet.
        // Dropping the handle detaches the thread; full stop needs process exit
        // or a future kill-switch IPC. Mark UI as disabled for now.
        self.host = None;
        self.allow_access = false;
        self.host_status = HostStatusSnapshot::default();
        self.footer_note =
            "Host stop is best-effort in Phase 1 — quit the app to fully end the host.".into();
    }

    fn poll_host_status(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if now - self.last_status_poll < 0.4 {
            return;
        }
        self.last_status_poll = now;
        let path = AppConfig::status_path();
        if let Some(snap) = read_status(&path) {
            self.host_status = snap;
        }
        if let Some(ref h) = self.host {
            if let Some(e) = h.take_error() {
                self.host_error = Some(e);
            }
            if !h.is_running() && self.allow_access {
                // Host thread exited unexpectedly.
                if self.host_error.is_none() {
                    self.host_error = Some(
                        "Host service stopped. Check signaling server, then toggle Allow access."
                            .into(),
                    );
                }
                self.allow_access = false;
            }
        }
        // Keep animating while host/viewer work.
        if self.host.as_ref().is_some_and(|h| h.is_running())
            || self.viewer.as_ref().is_some_and(|v| !v.is_finished())
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
    }

    fn poll_viewer(&mut self) {
        let Some(ref worker) = self.viewer else {
            return;
        };
        match worker.poll() {
            ConnectOutcome::Running => {
                self.connect_status = "Connecting…".into();
            }
            ConnectOutcome::Ok(s) => {
                self.connect_status = s;
                let id = self.remote_id.trim().to_string();
                if !id.is_empty() {
                    self.config.push_recent(&id);
                    let _ = self.config.save();
                }
                self.viewer = None;
            }
            ConnectOutcome::Err(e) => {
                self.connect_status = format!("Connect failed: {e}");
                self.viewer = None;
            }
        }
    }

    fn start_connect(&mut self) {
        let host = self.remote_id.trim().to_string();
        let otp = self.remote_secret.trim().to_string();
        if host.is_empty() {
            self.connect_status = "Enter a remote ID.".into();
            return;
        }
        if otp.is_empty() {
            self.connect_status = "Enter the OTP shown on the remote PC.".into();
            return;
        }
        if self.viewer.as_ref().is_some_and(|v| !v.is_finished()) {
            self.connect_status = "A connection is already in progress.".into();
            return;
        }
        self.connect_status = format!("Connecting to {host}…");
        self.viewer = Some(ViewerWorker::start(
            self.config.server.clone(),
            host,
            otp,
            self.transport_mode(),
        ));
    }

    fn copy_to_clipboard(ctx: &egui::Context, text: &str) {
        ctx.copy_text(text.to_string());
    }
}

impl eframe::App for RemoteLinkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_host_status(ctx);
        self.poll_viewer();

        egui::TopBottomPanel::top("title_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RemoteLink");
                ui.label(
                    egui::RichText::new("remote desktop")
                        .weak()
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .selectable_label(self.show_advanced, "Advanced")
                        .clicked()
                    {
                        self.show_advanced = !self.show_advanced;
                    }
                });
            });
        });

        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(&self.footer_note)
                        .small()
                        .weak(),
                );
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);

            // ── This PC ──────────────────────────────────────────────
            egui::Frame::group(ui.style())
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading("This PC");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut allow = self.allow_access;
                            if ui
                                .checkbox(&mut allow, "Allow remote access")
                                .on_hover_text(
                                    "Enrolls this PC and waits for a viewer (needs signaling server).",
                                )
                                .changed()
                            {
                                if allow {
                                    self.allow_access = true;
                                    self.start_host();
                                } else {
                                    self.stop_host_ui();
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);

                    let public_id = self
                        .host_status
                        .public_id
                        .clone()
                        .unwrap_or_else(|| {
                            if self.allow_access {
                                "… enrolling".into()
                            } else {
                                "—".into()
                            }
                        });

                    ui.horizontal(|ui| {
                        ui.label("Your ID");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&public_id)
                                    .monospace()
                                    .size(22.0)
                                    .strong(),
                            )
                            .selectable(true),
                        );
                        if public_id != "—"
                            && !public_id.starts_with('…')
                            && ui.button("Copy").clicked()
                        {
                            Self::copy_to_clipboard(ctx, &public_id);
                            self.footer_note = "ID copied.".into();
                        }
                    });

                    let otp = self
                        .host_status
                        .otp_code
                        .clone()
                        .unwrap_or_else(|| "—".into());
                    ui.horizontal(|ui| {
                        ui.label("OTP");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&otp)
                                    .monospace()
                                    .size(20.0)
                                    .color(egui::Color32::from_rgb(120, 200, 140)),
                            )
                            .selectable(true),
                        );
                        if otp != "—" && ui.button("Copy").clicked() {
                            Self::copy_to_clipboard(ctx, &otp);
                            self.footer_note = "OTP copied.".into();
                        }
                        if let Some(exp) = &self.host_status.otp_expires_at {
                            ui.label(
                                egui::RichText::new(format!("expires {exp}"))
                                    .small()
                                    .weak(),
                            );
                        }
                    });

                    ui.add_space(4.0);
                    let chrome = if self.host_status.chrome.is_empty() {
                        if self.allow_access {
                            "Starting…"
                        } else {
                            "Off"
                        }
                    } else {
                        self.host_status.chrome.as_str()
                    };
                    let host_alive = self.host.as_ref().is_some_and(|h| h.is_running());
                    ui.label(format!(
                        "Status: {chrome} · host {}",
                        if host_alive { "running" } else { "stopped" }
                    ));
                    if let Some(sid) = &self.host_status.session_id {
                        let who = self
                            .host_status
                            .viewer_label
                            .as_deref()
                            .unwrap_or("viewer");
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 60),
                            format!("In session with {who} ({sid})"),
                        );
                    }
                    if let Some(err) = &self.host_error {
                        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
                    }
                    let age = status_age_secs(&AppConfig::status_path());
                    if host_alive && age.is_none() {
                        ui.label(
                            egui::RichText::new("Waiting for host status file…")
                                .small()
                                .weak(),
                        );
                    }
                });

            ui.add_space(12.0);

            // ── Connect ──────────────────────────────────────────────
            egui::Frame::group(ui.style())
                .inner_margin(12.0)
                .show(ui, |ui| {
                    ui.heading("Connect to remote PC");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Remote ID");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.remote_id)
                                .desired_width(200.0)
                                .hint_text("10-digit ID"),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("OTP");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.remote_secret)
                                .desired_width(120.0)
                                .hint_text("code"),
                        );
                        let connecting = self
                            .viewer
                            .as_ref()
                            .is_some_and(|v| !v.is_finished());
                        ui.add_enabled_ui(!connecting, |ui| {
                            if ui
                                .add_sized([100.0, 28.0], egui::Button::new("Connect"))
                                .clicked()
                            {
                                self.start_connect();
                            }
                        });
                    });

                    if !self.config.recent_hosts.is_empty() {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new("Recent").small().weak());
                        ui.horizontal_wrapped(|ui| {
                            for id in self.config.recent_hosts.clone() {
                                if ui.small_button(&id).clicked() {
                                    self.remote_id = id;
                                }
                            }
                        });
                    }

                    if !self.connect_status.is_empty() {
                        ui.add_space(6.0);
                        ui.label(&self.connect_status);
                    }
                });

            if self.show_advanced {
                ui.add_space(12.0);
                egui::Frame::group(ui.style())
                    .inner_margin(12.0)
                    .show(ui, |ui| {
                        ui.heading("Advanced");
                        ui.label(
                            egui::RichText::new(
                                "Signaling server is hidden for normal use. Lab default is localhost.",
                            )
                            .small()
                            .weak(),
                        );
                        ui.horizontal(|ui| {
                            ui.label("Server");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.server)
                                    .desired_width(280.0)
                                    .hint_text("http://127.0.0.1:8080"),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Display name");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.config.display_name)
                                    .desired_width(180.0),
                            );
                        });
                        ui.horizontal(|ui| {
                            ui.label("Transport");
                            egui::ComboBox::from_id_salt("transport")
                                .selected_text(&self.config.transport)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.config.transport,
                                        "live".into(),
                                        "live",
                                    );
                                    ui.selectable_value(
                                        &mut self.config.transport,
                                        "webrtc".into(),
                                        "webrtc",
                                    );
                                    ui.selectable_value(
                                        &mut self.config.transport,
                                        "mock".into(),
                                        "mock",
                                    );
                                });
                        });
                        ui.checkbox(
                            &mut self.config.auto_start_host,
                            "Auto-start host when app opens",
                        );
                        ui.horizontal(|ui| {
                            if ui.button("Save settings").clicked() {
                                match self.config.save() {
                                    Ok(()) => {
                                        self.footer_note = format!(
                                            "Saved {}",
                                            AppConfig::config_path().display()
                                        );
                                    }
                                    Err(e) => self.footer_note = e,
                                }
                            }
                            if ui.button("Open data folder").clicked() {
                                let dir = AppConfig::data_dir();
                                let _ = std::fs::create_dir_all(&dir);
                                #[cfg(windows)]
                                {
                                    let _ = std::process::Command::new("explorer")
                                        .arg(&dir)
                                        .spawn();
                                }
                                #[cfg(not(windows))]
                                {
                                    self.footer_note = format!("Data: {}", dir.display());
                                }
                            }
                        });
                        ui.label(
                            egui::RichText::new(format!(
                                "Data dir: {}",
                                AppConfig::data_dir().display()
                            ))
                            .small()
                            .weak(),
                        );
                    });
            }

            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(
                    "Tip: start the signaling server once (remotelink-server), then share Your ID + OTP with the other person.",
                )
                .small()
                .weak(),
            );
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.config.save();
    }
}

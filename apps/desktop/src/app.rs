//! RemoteLink product shell — home screen (this PC + connect).

use std::str::FromStr;

use eframe::egui;

use crate::config::{ensure_parent, AppConfig};
use crate::startup;
use crate::host_worker::HostWorker;
use crate::status::{read_status, status_age_secs, HostStatusSnapshot};
use crate::viewer_worker::ViewerWorker;
use remotelink_net::TransportMode;
use remotelink_viewer::{named_key_from_name, MouseButtonKind, RawInput};

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
    minimize_on_start: bool,
}

impl RemoteLinkApp {
    /// Build app; optionally auto-start host from config.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Dark-ish product chrome.
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);

        let mut config = AppConfig::load();
        let login_start = std::env::var("REMOTELINK_AUTOSTART")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if login_start {
            config.auto_start_host = true;
        }
        if config.start_with_windows {
            if let Err(e) = startup::apply(true) {
                eprintln!("remotelink-app: refresh Start with Windows: {e}");
            }
        }
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
                "RemoteLink {} · live session{}",
                remotelink_common::VERSION,
                if login_start { " · started at login" } else { "" }
            ),
            last_status_poll: 0.0,
            minimize_on_start: login_start,
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
        if self.host.as_mut().is_some_and(|h| h.poll()) {
            return;
        }
        // Clear previous worker (kills old child if any).
        self.host = None;
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
        // Stale status confuses the UI until the new host rewrites it.
        let _ = std::fs::remove_file(&status_path);
        self.host_error = None;
        match HostWorker::start(
            self.config.server.clone(),
            self.config.display_name.clone(),
            self.transport_mode(),
            status_path,
            creds_path,
        ) {
            Ok(w) => {
                self.footer_note = format!("Host started ({})", w.host_exe().display());
                self.host = Some(w);
                self.allow_access = true;
            }
            Err(e) => {
                self.host_error = Some(e);
                self.allow_access = false;
            }
        }
        self.connect_status.clear();
    }

    fn stop_host_ui(&mut self) {
        if let Some(mut h) = self.host.take() {
            h.stop();
        }
        self.allow_access = false;
        self.host_status = HostStatusSnapshot::default();
        let _ = std::fs::remove_file(AppConfig::status_path());
        self.footer_note = "Remote access stopped.".into();
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
        if let Some(ref mut h) = self.host {
            let alive = h.poll();
            if let Some(e) = h.take_error() {
                self.host_error = Some(e);
            }
            if !alive && self.allow_access {
                if self.host_error.is_none() {
                    self.host_error = Some(
                        "Host service stopped. Check the signaling server URL, then toggle Allow access."
                            .into(),
                    );
                }
                self.allow_access = false;
                self.host = None;
            }
        }
        // Keep animating while host/viewer work.
        let viewer_live = self.viewer.as_ref().is_some_and(|v| !v.is_finished());
        if self.allow_access || viewer_live {
            let interval = if viewer_live {
                std::time::Duration::from_millis(16)
            } else {
                std::time::Duration::from_millis(250)
            };
            ctx.request_repaint_after(interval);
        }
    }

    fn poll_viewer(&mut self) {
        let Some(ref worker) = self.viewer else {
            return;
        };
        let snap = worker.snapshot();
        if let Some(err) = &snap.error {
            self.connect_status = format!("Connect failed: {err}");
        } else if !snap.status.is_empty() {
            self.connect_status = snap.status.clone();
        }
        if worker.is_finished() {
            let id = self.remote_id.trim().to_string();
            if snap.error.is_none() && !id.is_empty() {
                self.config.push_recent(&id);
                let _ = self.config.save();
            }
            if snap.error.is_none() && !self.connect_status.starts_with("Connect failed") {
                self.connect_status = snap.status;
            }
            self.viewer = None;
        }
    }

    fn disconnect_viewer(&mut self) {
        if let Some(v) = self.viewer.take() {
            v.request_stop();
        }
        self.connect_status = "Disconnected.".into();
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

    fn draw_live_session(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(snap) = self.viewer.as_ref().map(|w| w.snapshot()) else {
            return;
        };

        egui::Frame::group(ui.style())
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Remote session");
                    ui.label(
                        egui::RichText::new(&snap.phase)
                            .small()
                            .weak(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Disconnect").clicked() {
                            self.disconnect_viewer();
                        }
                    });
                });
                ui.label(format!(
                    "{} · {}×{} · video {} · audio {} · bind {}",
                    snap.hud,
                    snap.width,
                    snap.height,
                    snap.video_rx,
                    snap.audio_rx,
                    if snap.identity_bound {
                        "ready"
                    } else {
                        "pending"
                    }
                ));

                if let (Some(rgba), w, h) = (snap.rgba.as_ref(), snap.width, snap.height) {
                    if w > 0 && h > 0 && rgba.len() >= (w as usize * h as usize * 4) {
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [w as usize, h as usize],
                            rgba,
                        );
                        let tex = ctx.load_texture(
                            "remotelink-live-frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        let avail = ui.available_size();
                        let aspect = w as f32 / h.max(1) as f32;
                        let max_w = avail.x.max(320.0);
                        let max_h = (avail.y - 8.0).max(240.0);
                        let mut size = egui::vec2(max_w, max_w / aspect);
                        if size.y > max_h {
                            size = egui::vec2(max_h * aspect, max_h);
                        }
                        let response = ui.add(
                            egui::Image::new((tex.id(), size)).sense(egui::Sense::click_and_drag()),
                        );
                        self.forward_session_input(&response, ctx, size);
                    }
                } else {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Waiting for first video frame…")
                            .italics()
                            .weak(),
                    );
                }
            });
    }

    fn forward_session_input(
        &self,
        response: &egui::Response,
        ctx: &egui::Context,
        size: egui::Vec2,
    ) {
        let Some(worker) = self.viewer.as_ref() else {
            return;
        };
        let rect = response.rect;
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        if let Some(pos) = pointer.filter(|_| response.hovered()) {
            // UNIT capture rect: send normalized 0..1 (pixel coords were clamped to 1).
            let px = ((pos.x - rect.min.x) / size.x).clamp(0.0, 1.0);
            let py = ((pos.y - rect.min.y) / size.y).clamp(0.0, 1.0);
            worker.send_input(RawInput::MouseMove { px, py });
            if ctx.input(|i| i.pointer.primary_pressed()) {
                worker.send_input(RawInput::MouseButton {
                    button: MouseButtonKind::Left,
                    pressed: true,
                    px,
                    py,
                });
            }
            if ctx.input(|i| i.pointer.primary_released()) {
                worker.send_input(RawInput::MouseButton {
                    button: MouseButtonKind::Left,
                    pressed: false,
                    px,
                    py,
                });
            }
            if ctx.input(|i| i.pointer.secondary_pressed()) {
                worker.send_input(RawInput::MouseButton {
                    button: MouseButtonKind::Right,
                    pressed: true,
                    px,
                    py,
                });
            }
            if ctx.input(|i| i.pointer.secondary_released()) {
                worker.send_input(RawInput::MouseButton {
                    button: MouseButtonKind::Right,
                    pressed: false,
                    px,
                    py,
                });
            }
            let scroll = ctx.input(|i| i.raw_scroll_delta);
            if scroll != egui::Vec2::ZERO {
                worker.send_input(RawInput::MouseWheel {
                    delta_x: scroll.x,
                    delta_y: scroll.y,
                    precise: true,
                    px,
                    py,
                });
            }
        }

        if response.has_focus() || response.hovered() {
            ctx.input(|i| {
                for ev in &i.events {
                    if let egui::Event::Key {
                        key,
                        pressed,
                        modifiers,
                        ..
                    } = ev
                    {
                        if let Some(named) = named_key_from_name(format!("{key:?}").as_str()) {
                            let mut mods = 0u32;
                            if modifiers.ctrl {
                                mods |= remotelink_protocol::modifiers::CTRL;
                            }
                            if modifiers.alt {
                                mods |= remotelink_protocol::modifiers::ALT;
                            }
                            if modifiers.shift {
                                mods |= remotelink_protocol::modifiers::SHIFT;
                            }
                            if modifiers.command {
                                mods |= remotelink_protocol::modifiers::META;
                            }
                            worker.send_input(RawInput::KeyNamed {
                                key: named,
                                pressed: *pressed,
                                modifiers: mods,
                            });
                        }
                    }
                }
            });
        }
    }
}

impl eframe::App for RemoteLinkApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.minimize_on_start {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.minimize_on_start = false;
        }
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

        if self.viewer.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1280.0, 860.0)));
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);

            if self.viewer.is_some() {
                self.draw_live_session(ui, ctx);
                ui.add_space(10.0);
            }

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
                    let host_alive = self.allow_access
                        && self.host.is_some()
                        && self.host_error.is_none();
                    let online_hint = if host_alive {
                        if self.host_status.public_id.is_some() {
                            "online (waiting for viewers)"
                        } else {
                            "starting…"
                        }
                    } else {
                        "stopped"
                    };
                    ui.label(format!("Status: {chrome} · host {online_hint}"));
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
                        if connecting
                            && ui
                                .add_sized([100.0, 28.0], egui::Button::new("Disconnect"))
                                .clicked()
                        {
                            self.disconnect_viewer();
                        }
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
                                    .hint_text("http://127.0.0.1:18080"),
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
                        let mut start_win = self.config.start_with_windows;
                        if ui
                            .checkbox(&mut start_win, "Start with Windows")
                            .on_hover_text(
                                "Launch RemoteLink at login (minimized) and allow remote access.",
                            )
                            .changed()
                        {
                            match startup::apply(start_win) {
                                Ok(()) => {
                                    self.config.start_with_windows = start_win;
                                    let _ = self.config.save();
                                    self.footer_note = if start_win {
                                        "Start with Windows enabled.".into()
                                    } else {
                                        "Start with Windows disabled.".into()
                                    };
                                }
                                Err(e) => {
                                    self.footer_note = e;
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Save settings").clicked() {
                                if let Err(e) = startup::apply(self.config.start_with_windows) {
                                    self.footer_note = e;
                                } else {
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

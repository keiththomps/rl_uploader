use crate::{
    Error, Result,
    api::Ballchasing,
    config::Settings,
    platform,
    protocol::EpicAuth,
    proxy::CaptureProxy,
    proxy_certs,
    worker::{Command, Worker},
};
use eframe::egui::{self, Color32, RichText};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

enum Event {
    Message(String),
    Device(u64, String, String, u64),
    AuthDone(u64, Result<()>),
    Certificate(Result<Value>),
    Installed(Result<()>),
}
struct SignIn {
    generation: u64,
    cancel: Arc<AtomicBool>,
    code: String,
    uri: String,
    deadline: Option<Instant>,
    message: String,
    pending: bool,
}
pub struct App {
    dir: PathBuf,
    settings: Settings,
    worker: Worker,
    proxy: Arc<CaptureProxy>,
    token: Zeroizing<String>,
    message: String,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    sign_in: Option<SignIn>,
    generation: u64,
    certificate: Option<Value>,
    setup_busy: bool,
    confirm_import: bool,
}
impl App {
    fn new(dir: PathBuf, settings: Settings, paused: bool) -> Self {
        let proxy = CaptureProxy::new(&dir);
        let worker = Worker::start(&dir, settings.clone(), proxy.clone(), paused);
        let (tx, rx) = mpsc::channel();
        Self {
            dir,
            settings,
            worker,
            proxy,
            token: Zeroizing::new(String::new()),
            message: String::new(),
            tx,
            rx,
            sign_in: None,
            generation: 0,
            certificate: None,
            setup_busy: false,
            confirm_import: false,
        }
    }
    fn connect(&mut self) {
        if let Some(s) = &self.sign_in {
            s.cancel.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in = Some(SignIn {
            generation,
            cancel: cancel.clone(),
            code: String::new(),
            uri: String::new(),
            deadline: None,
            message: "Requesting an Epic sign-in code…".into(),
            pending: true,
        });
        let tx = self.tx.clone();
        let auth = EpicAuth::new(&self.dir);
        std::thread::spawn(move || {
            let result = (|| {
                let device = auth.begin()?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::new("Sign-in cancelled."));
                }
                let _ = tx.send(Event::Device(
                    generation,
                    device["user_code"].as_str().unwrap_or_default().into(),
                    device["verification_uri"]
                        .as_str()
                        .unwrap_or_default()
                        .into(),
                    device["expires_in"].as_u64().unwrap_or(600).clamp(1, 1800),
                ));
                auth.finish(&device, &cancel)
            })();
            let _ = tx.send(Event::AuthDone(generation, result));
        });
    }
    fn save(&mut self) -> Result<()> {
        self.settings.validate()?;
        // Validate archive separation before saving or pausing a working configuration.
        let folder = self.settings.folder();
        let archive = platform::normalized_path(&self.dir.join("archive"));
        if folder.starts_with(&archive) || archive.starts_with(&folder) {
            return Err(Error::new("Replay and archive folders must be separate."));
        }
        let changed_token = !self.token.is_empty();
        if changed_token {
            platform::save_token(&self.dir, &self.token)?;
            self.token.clear();
        }
        self.settings.save(&self.dir)?;
        self.worker
            .send(Command::Apply(self.settings.clone(), changed_token));
        self.message = "Settings saved. Replay capture and upload queue are running.".into();
        Ok(())
    }
    fn events(&mut self) {
        for event in self.rx.try_iter() {
            match event {
                Event::Message(s) => self.message = s,
                Event::Device(g, code, uri, seconds) => {
                    if let Some(s) = self.sign_in.as_mut().filter(|s| s.generation == g) {
                        s.code = code;
                        s.uri = uri;
                        s.deadline = Some(Instant::now() + Duration::from_secs(seconds));
                        s.message="Enter this code on Epic's sign-in page. This window stays open while you sign in.".into();
                    }
                }
                Event::AuthDone(g, result) => {
                    if let Some(s) = self.sign_in.as_mut().filter(|s| s.generation == g) {
                        s.pending = false;
                        s.message = match result {
                            Ok(()) => {
                                self.worker.send(Command::Reconnect);
                                "Epic connected. You can close this window.".into()
                            }
                            Err(e) => e.message,
                        };
                    }
                }
                Event::Certificate(result) => {
                    self.setup_busy = false;
                    match result {
                        Ok(v) => self.certificate = Some(v),
                        Err(e) => self.message = e.message,
                    }
                }
                Event::Installed(result) => {
                    self.setup_busy = false;
                    self.message=match result { Ok(())=>"Certificate installed. Select periodic mode, then Save & start. Windows will ask for routing permission.".into(),Err(e)=>e.message };
                }
            }
        }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(250));
        self.events();
        if ctx.input(|i| i.viewport().close_requested()) && !self.proxy.close_allowed() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.message="Close Rocket League before exiting so its captured connection stays intact. You can minimize this window.".into();
        }
        let snapshot = self.worker.snapshot.lock().unwrap().clone();
        egui::CentralPanel::default().show(ctx,|ui| {
            egui::ScrollArea::vertical().show(ui,|ui| {
                ui.add_space(10.0); ui.heading(RichText::new("RL Uploader").size(30.0));
                ui.label(RichText::new("Save your replays. Keep your sessions.").color(Color32::from_rgb(159,180,200))); ui.add_space(14.0);
                ui.horizontal_wrapped(|ui| { for (label,status) in [("Saved",None),("Queued",Some("queued")),("Uploaded",Some("uploaded")),("Already uploaded",Some("duplicate")),("Need attention",Some("failed"))] {
                    let count = status.map(|s| *snapshot.summary.counts.get(s).unwrap_or(&0)).unwrap_or_else(|| snapshot.summary.counts.values().sum());
                    ui.group(|ui| { ui.set_min_width(118.0); ui.label(label); ui.label(RichText::new(count.to_string()).size(26.0).strong()); });
                } });
                ui.add_space(12.0);
                ui.group(|ui| { ui.set_width(ui.available_width()); ui.label(RichText::new("Automatic replay saving").size(19.0).strong());
                    ui.checkbox(&mut self.settings.cloud_capture,"Automatically download my recent Match History replays");
                    ui.horizontal_wrapped(|ui| {
                        ui.radio_value(&mut self.settings.capture_mode,"after_session".into(),"After Rocket League closes");
                        ui.radio_value(&mut self.settings.capture_mode,"periodic".into(),"Periodically while playing");
                        egui::ComboBox::from_id_salt("interval").selected_text(format!("Every {} minutes",self.settings.capture_interval_minutes)).show_ui(ui,|ui| { for n in [15,30] { ui.selectable_value(&mut self.settings.capture_interval_minutes,n,format!("Every {n} minutes")); } });
                    });
                    ui.label("Periodic capture reuses the game's connection. After play, capture uses your connected Epic account.");
                    ui.label(RichText::new("Recent history holds 20 matches. Longer sessions can exceed that window.").color(Color32::from_rgb(238,190,112)));
                    ui.horizontal(|ui| {
                        if ui.button(if EpicAuth::new(&self.dir).connected() { "Reconnect Epic" } else { "Connect Epic" }).clicked() { self.connect(); }
                        if ui.add_enabled(!self.setup_busy,egui::Button::new("Set up periodic capture")).clicked() {
                            if platform::game_running() != Some(false) { self.message="Close Rocket League before setting up periodic capture.".into(); }
                            else { self.setup_busy=true; let dir=self.dir.clone(); let tx=self.tx.clone(); std::thread::spawn(move || { let _ = tx.send(Event::Certificate(proxy_certs::prepare(&dir))); }); }
                        }
                    });
                    ui.label(&snapshot.cloud);
                    if self.settings.capture_mode == "periodic" { ui.label(self.proxy.detail()); }
                    ui.add_space(4.0); ui.small("Experimental unofficial interface; no game injection. Epic/EAC approval has not been verified.");
                });
                ui.add_space(10.0);
                ui.group(|ui| { ui.set_width(ui.available_width()); ui.label(RichText::new("Local replays & ballchasing").size(19.0).strong());
                    ui.label("Rocket League replay folder"); ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.settings.replay_dir).desired_width(ui.available_width()-90.0));
                        if ui.button("Browse…").clicked() && let Some(p) = rfd::FileDialog::new().set_directory(&self.settings.replay_dir).pick_folder() { self.settings.replay_dir=p.to_string_lossy().into(); }
                    });
                    ui.horizontal(|ui| { ui.label("Ballchasing token"); ui.add(egui::TextEdit::singleline(&mut *self.token).password(true).hint_text(if snapshot.has_token { "Saved securely — enter a replacement" } else { "Optional: paste your API token" }).desired_width(330.0));
                        ui.hyperlink_to("Get token","https://ballchasing.com/upload");
                        if ui.button("Test token").clicked() { let dir=self.dir.clone(); let token=self.token.clone(); let tx=self.tx.clone(); std::thread::spawn(move || { let result=(|| { let value=if token.is_empty() { platform::load_token(&dir)? } else { token }; Ballchasing::new(value)?.ping()?; Ok::<_,crate::Error>(()) })(); let _=tx.send(Event::Message(match result { Ok(())=>"Ballchasing token accepted.".into(), Err(e)=>e.message })); }); }
                    });
                    ui.horizontal(|ui| { ui.label("New upload visibility"); egui::ComboBox::from_id_salt("visibility").selected_text(&self.settings.visibility).show_ui(ui,|ui| { for v in ["private","unlisted","public"] { ui.selectable_value(&mut self.settings.visibility,v.into(),v); } }); });
                    ui.small("Replays are saved and archived even without a ballchasing token. Existing files are imported only when requested.");
                });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(RichText::new("Save & start").strong()).clicked() && let Err(e)=self.save() { self.message=e.message; }
                    if ui.button(if snapshot.paused {"Resume"} else {"Pause"}).clicked() { self.worker.send(if snapshot.paused {Command::Resume} else {Command::Pause}); }
                    if ui.button("Import existing replays").clicked() { self.confirm_import=true; }
                    if ui.button("Retry failed uploads").clicked() { self.worker.send(Command::Retry); }
                    if ui.button("Open archive").clicked() { let _=webbrowser::open(&self.dir.join("archive").to_string_lossy()); }
                });
                if !self.message.is_empty() { ui.label(&self.message); }
                if !snapshot.error.is_empty() { ui.colored_label(Color32::LIGHT_RED,&snapshot.error); }
                if !snapshot.summary.blocked.is_empty() { ui.colored_label(Color32::LIGHT_RED,&snapshot.summary.blocked); }
                if snapshot.summary.cooldown > crate::now() { ui.label(format!("Uploads resume in about {} minute(s). Saved files remain in the archive.",((snapshot.summary.cooldown-crate::now())/60.0).ceil() as u64)); }
                ui.label(if snapshot.paused {"Capture and uploads paused."} else {&snapshot.scan});
                ui.add_space(10.0); ui.separator(); ui.label(RichText::new("Recent replays").size(19.0).strong());
                egui::Grid::new("replays").striped(true).min_col_width(110.0).show(ui,|ui| {
                    ui.strong("Replay"); ui.strong("Status"); ui.strong("Visibility"); ui.strong("Details"); ui.end_row();
                    for job in &snapshot.summary.recent {
                        let name = if job.filename.chars().count()>32 { format!("{}…",job.filename.chars().take(29).collect::<String>()) } else {job.filename.clone()};
                        if job.replay_id.is_empty() { ui.label(name).on_hover_text(&job.filename); } else { ui.hyperlink_to(name,format!("https://ballchasing.com/replay/{}",job.replay_id)).on_hover_text(&job.filename); }
                        ui.label(&job.status); ui.label(&job.visibility); ui.label(&job.message); ui.end_row();
                    }
                });
            });
        });
        let mut renew = false;
        let mut close = false;
        if let Some(sign) = &self.sign_in {
            egui::Window::new("Connect Epic")
                .collapsible(false)
                .resizable(false)
                .default_width(470.0)
                .show(ctx, |ui| {
                    ui.label(&sign.message);
                    if !sign.code.is_empty() {
                        ui.add_space(12.0);
                        ui.label(RichText::new(&sign.code).size(34.0).strong());
                        ui.horizontal(|ui| {
                            if ui.button("Copy code").clicked() {
                                ui.ctx().copy_text(sign.code.clone());
                            }
                            ui.hyperlink_to("Open Epic sign-in page", &sign.uri);
                        });
                        if let Some(deadline) = sign.deadline {
                            let remain =
                                deadline.saturating_duration_since(Instant::now()).as_secs();
                            ui.label(format!(
                                "Code expires in {}:{:02}",
                                remain / 60,
                                remain % 60
                            ));
                        }
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        renew = ui.button("Get a new code").clicked();
                        close = ui
                            .button(if sign.pending { "Cancel" } else { "Close" })
                            .clicked();
                    });
                });
        }
        if renew {
            self.connect();
        }
        if close && let Some(s) = self.sign_in.take() {
            s.cancel.store(true, Ordering::Relaxed);
        }
        let mut install = false;
        let mut dismiss = false;
        if let Some(cert) = &self.certificate {
            egui::Window::new("Periodic capture certificate").collapsible(false).resizable(false).default_width(510.0).show(ctx,|ui| {
            ui.label("Periodic capture runs a local TLS proxy for ws.rlpp.psynet.gg. Windows must trust its certificate to let it reuse the game connection.");
            ui.label("This trust persists until you remove it or the certificate expires. The CA is constrained to this hostname and its signing key is discarded.");
            ui.label(format!("Expires: {}",cert["expires"].as_str().unwrap_or_default()));
            ui.label(format!("SHA-1: {}",cert["sha1"].as_str().unwrap_or_default()));
            ui.label("Starting periodic mode separately asks for administrator access to route this hostname and install a startup recovery task. Close the game before exiting the proxy.");
            if proxy_certs::trusted(&self.dir) { ui.label("This certificate is already trusted."); dismiss=ui.button("Done").clicked(); }
            else { ui.horizontal(|ui| { install=ui.button("Install this certificate for my Windows user").clicked(); dismiss=ui.button("Cancel").clicked(); }); }
        });
        }
        if install && let Some(cert) = self.certificate.take() {
            self.setup_busy = true;
            let dir = self.dir.clone();
            let tx = self.tx.clone();
            let hash = cert["sha1"].as_str().unwrap_or_default().to_string();
            std::thread::spawn(move || {
                let _ = tx.send(Event::Installed(proxy_certs::install(&dir, &hash)));
            });
        }
        if dismiss {
            self.certificate = None;
        }
        if self.confirm_import {
            egui::Window::new("Import existing replays?")
                .collapsible(false).resizable(false).show(ctx, |ui| {
                    ui.label("This queues existing files in the replay folder using the saved visibility setting.");
                    ui.horizontal(|ui| {
                        if ui.button("Import").clicked() {
                            self.worker.send(Command::Import);
                            self.confirm_import = false;
                        }
                        if ui.button("Cancel").clicked() { self.confirm_import = false; }
                    });
                });
        }
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(sign) = &self.sign_in {
            sign.cancel.store(true, Ordering::Relaxed);
        }
        self.worker.stop();
        self.proxy.close();
    }
}
pub fn run(dir: PathBuf, settings: Settings, minimized: bool, paused: bool) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1060.0, 900.0])
            .with_min_inner_size([850.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "RL Uploader — Rust",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::Dark);
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            cc.egui_ctx.style_mut(|style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(15.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
                style.spacing.item_spacing = egui::vec2(8.0, 7.0);
                style.spacing.button_padding = egui::vec2(9.0, 5.0);
            });
            if minimized {
                cc.egui_ctx
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
            Ok(Box::new(App::new(dir, settings, paused)))
        }),
    )
    .map_err(|_| {
        Error::new(
            "Could not open the desktop window. Try --headless or update your graphics driver.",
        )
    })
}

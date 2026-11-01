use crate::{
    Error, Result,
    api::Ballchasing,
    config::Settings,
    ipc::Frontend,
    platform,
    protocol::EpicAuth,
    worker::{Command, Snapshot},
};
use eframe::egui::{self, Color32, RichText};
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

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Activity,
    Settings,
}
enum Event {
    TokenTest(Result<()>),
    Device(u64, String, String, u64),
    AuthDone(u64, Result<()>),
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
#[derive(Clone, Copy)]
enum Tone {
    Good,
    Attention,
    Neutral,
}
fn status(ui: &mut egui::Ui, text: &str, tone: Tone) {
    let dark = ui.visuals().dark_mode;
    let color = match tone {
        Tone::Good => {
            if dark {
                Color32::from_rgb(126, 215, 150)
            } else {
                Color32::from_rgb(20, 110, 55)
            }
        }
        Tone::Attention => {
            if dark {
                Color32::from_rgb(245, 193, 112)
            } else {
                Color32::from_rgb(145, 76, 0)
            }
        }
        Tone::Neutral => ui.visuals().weak_text_color(),
    };
    ui.label(RichText::new(text).color(color));
}
fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).strong().size(16.0));
            ui.add_space(8.0);
            body(ui);
        });
    ui.add_space(10.0);
}
fn visibility_label(value: &str) -> &str {
    match value {
        "private" => "Private",
        "unlisted" => "Unlisted",
        "public" => "Public",
        _ => value,
    }
}
fn job_status(value: &str) -> &str {
    match value {
        "uploaded" => "Uploaded",
        "duplicate" => "Already uploaded",
        "queued" => "Queued",
        "uploading" => "Uploading",
        "failed" => "Needs attention",
        _ => value,
    }
}
fn replay_label(name: &str) -> String {
    let stem = name.strip_suffix(".replay").unwrap_or(name);
    if stem.len() >= 16 && stem.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        format!("Match {}", stem.chars().take(8).collect::<String>())
    } else if stem.chars().count() > 32 {
        format!("{}…", stem.chars().take(31).collect::<String>())
    } else {
        stem.to_owned()
    }
}

pub struct App {
    dir: PathBuf,
    settings: Settings,
    saved: Settings,
    worker: Frontend,
    page: Page,
    startup: bool,
    saved_startup: bool,
    token: Zeroizing<String>,
    message: String,
    message_error: bool,
    testing: bool,
    token_result: Option<(bool, String)>,
    epic_ready: bool,
    epic_saved: bool,
    folder_ready: bool,
    game_state: Option<bool>,
    next_refresh: Instant,
    tx: mpsc::Sender<Event>,
    rx: mpsc::Receiver<Event>,
    sign_in: Option<SignIn>,
    generation: u64,
    confirm_import: bool,
    confirm_close: bool,
}
impl App {
    fn new(dir: PathBuf, settings: Settings, worker: Frontend) -> Self {
        let (tx, rx) = mpsc::channel();
        let startup = crate::install::startup_enabled();
        let auth = EpicAuth::new(&dir);
        let ready = auth.credentials_ready();
        let initial = if !ready || !settings.folder().is_dir() {
            Page::Settings
        } else {
            Page::Activity
        };
        Self {
            epic_ready: ready,
            epic_saved: auth.connected(),
            folder_ready: settings.folder().is_dir(),
            game_state: platform::game_running(),
            next_refresh: Instant::now() + Duration::from_secs(5),
            dir,
            saved: settings.clone(),
            settings,
            worker,
            page: initial,
            startup,
            saved_startup: startup,
            token: Zeroizing::new(String::new()),
            message: String::new(),
            message_error: false,
            testing: false,
            token_result: None,
            tx,
            rx,
            sign_in: None,
            generation: 0,
            confirm_import: false,
            confirm_close: false,
        }
    }
    fn dirty(&self) -> bool {
        self.settings != self.saved || self.startup != self.saved_startup || !self.token.is_empty()
    }
    fn notify(&mut self, result: Result<()>, success: &str) {
        self.message_error = result.is_err();
        self.message = match result {
            Ok(()) => success.into(),
            Err(e) => e.message,
        };
    }
    fn connect(&mut self) {
        if platform::game_running() != Some(false) {
            self.notify(
                Err(Error::new("Close Rocket League before connecting Epic.")),
                "",
            );
            return;
        }
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
            message: "Requesting a sign-in code…".into(),
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
    fn test_token(&mut self) {
        if platform::game_running() != Some(false) {
            self.token_result = Some((
                false,
                "Close Rocket League before testing the connection.".into(),
            ));
            return;
        }
        self.testing = true;
        self.token_result = None;
        let dir = self.dir.clone();
        let token = self.token.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let value = if token.is_empty() {
                    platform::load_token(&dir)?
                } else {
                    token
                };
                Ballchasing::new(value)?.ping()?;
                Ok(())
            })();
            let _ = tx.send(Event::TokenTest(result));
        });
    }
    fn save(&mut self) -> Result<()> {
        self.settings.validate()?;
        let folder = self.settings.folder();
        let archive = platform::normalized_path(&self.dir.join("archive"));
        if folder.starts_with(&archive) || archive.starts_with(&folder) {
            return Err(Error::new("Replay and archive folders must be separate."));
        }
        if !folder.is_dir() {
            return Err(Error::new(
                "The replay folder was not found. Choose an existing folder.",
            ));
        }
        let changed_token = !self.token.is_empty();
        if changed_token {
            platform::save_token(&self.dir, &self.token)?;
        }
        self.settings.save(&self.dir)?;
        if crate::install::installed() {
            crate::install::set_startup(self.startup)?;
        }
        self.token.clear();
        self.saved = self.settings.clone();
        self.saved_startup = self.startup;
        self.folder_ready = true;
        self.worker
            .send(Command::Apply(self.saved.clone(), changed_token));
        Ok(())
    }
    fn events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                Event::TokenTest(result) => {
                    self.testing = false;
                    self.token_result = Some(match result {
                        Ok(()) => (
                            true,
                            if self.token.is_empty() {
                                "Connection successful.".into()
                            } else {
                                "Token accepted. Click Apply to save it.".into()
                            },
                        ),
                        Err(e) => (false, e.message),
                    });
                }
                Event::Device(g, code, uri, seconds) => {
                    if let Some(s) = self.sign_in.as_mut().filter(|s| s.generation == g) {
                        s.code = code;
                        s.uri = uri;
                        s.deadline = Some(Instant::now() + Duration::from_secs(seconds));
                        s.message = "Open Epic's sign-in page and enter this code.".into();
                    }
                }
                Event::AuthDone(g, result) => {
                    if let Some(s) = self.sign_in.as_mut().filter(|s| s.generation == g) {
                        s.pending = false;
                        s.message = match result {
                            Ok(()) => {
                                self.epic_ready = true;
                                self.epic_saved = true;
                                self.worker.send(Command::Reconnect);
                                "Epic connected. You can close this dialog.".into()
                            }
                            Err(e) => e.message,
                        };
                    }
                }
            }
        }
        if Instant::now() >= self.next_refresh {
            let auth = EpicAuth::new(&self.dir);
            self.epic_ready = auth.credentials_ready();
            self.epic_saved = auth.connected();
            self.folder_ready = self.saved.folder().is_dir();
            self.game_state = platform::game_running();
            self.next_refresh = Instant::now() + Duration::from_secs(5);
        }
    }
    fn overview(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui.heading("Activity");
        ui.label("Your recent replays are saved after Rocket League closes.");
        ui.add_space(14.0);
        section(ui, "Current status", |ui| {
            let (headline, tone) = if snapshot.paused {
                ("Paused", Tone::Neutral)
            } else if !snapshot.error.is_empty() {
                ("Needs attention", Tone::Attention)
            } else if snapshot.game_running == Some(true) {
                ("Waiting for Rocket League to close", Tone::Good)
            } else if !self.saved.cloud_capture {
                ("Automatic saving is off", Tone::Neutral)
            } else if !self.epic_ready || !self.folder_ready || !snapshot.capture_error.is_empty() {
                ("Setup needs attention", Tone::Attention)
            } else {
                ("Ready to save after your next session", Tone::Good)
            };
            status(ui, headline, tone);
            if snapshot.paused {
                ui.label("Replay saving and uploads are paused. Resume to continue.");
            } else if !snapshot.cloud.is_empty() {
                ui.label(&snapshot.cloud);
            }
            if !snapshot.error.is_empty() {
                status(ui, &snapshot.error, Tone::Attention);
            }
            if !snapshot.capture_error.is_empty() {
                status(ui, &snapshot.capture_error, Tone::Attention);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(if snapshot.paused { "Resume" } else { "Pause" })
                    .clicked()
                {
                    self.worker.send(if snapshot.paused {
                        Command::Resume
                    } else {
                        Command::Pause
                    });
                }
                if ui.button("Open replay folder").clicked() {
                    let _ = webbrowser::open(&self.saved.replay_dir);
                }
                if ui.button("Open archive").clicked() {
                    let _ = webbrowser::open(&self.dir.join("archive").to_string_lossy());
                }
            });
        });
        section(ui, "Configuration", |ui| {
            egui::Grid::new("configuration_status")
                .num_columns(2)
                .min_row_height(18.0)
                .spacing([28.0, 5.0])
                .show(ui, |ui| {
                    ui.label("Automatic saving");
                    status(
                        ui,
                        if self.saved.cloud_capture {
                            "On · after the game closes"
                        } else {
                            "Off"
                        },
                        if self.saved.cloud_capture {
                            Tone::Good
                        } else {
                            Tone::Neutral
                        },
                    );
                    ui.end_row();
                    ui.label("Epic account");
                    status(
                        ui,
                        if !snapshot.capture_error.is_empty() {
                            "Needs attention"
                        } else if self.epic_ready {
                            "Connected"
                        } else if self.epic_saved {
                            "Reconnect required"
                        } else {
                            "Not connected"
                        },
                        if self.epic_ready && snapshot.capture_error.is_empty() {
                            Tone::Good
                        } else {
                            Tone::Attention
                        },
                    );
                    ui.end_row();
                    ui.label("Replay folder");
                    status(
                        ui,
                        if self.folder_ready {
                            "Folder found"
                        } else {
                            "Folder not found"
                        },
                        if self.folder_ready {
                            Tone::Good
                        } else {
                            Tone::Attention
                        },
                    );
                    ui.end_row();
                    ui.label("Ballchasing uploads");
                    let (label, tone) = if !self.saved.uploads_enabled {
                        ("Off · replays stay on this PC", Tone::Neutral)
                    } else if !snapshot.summary.blocked.is_empty() {
                        ("Needs attention", Tone::Attention)
                    } else if snapshot.has_token {
                        ("Configured · token saved", Tone::Good)
                    } else {
                        ("Not configured · token needed", Tone::Neutral)
                    };
                    status(ui, label, tone);
                    ui.end_row();
                    ui.label("Windows startup");
                    status(
                        ui,
                        if self.saved_startup { "On" } else { "Off" },
                        Tone::Neutral,
                    );
                    ui.end_row();
                });
            ui.add_space(10.0);
            if ui.button("Change settings").clicked() {
                self.page = Page::Settings;
            }
        });
        ui.horizontal_wrapped(|ui| {
            let saved: u64 = snapshot.summary.counts.values().sum();
            let uploaded = snapshot.summary.counts.get("uploaded").unwrap_or(&0)
                + snapshot.summary.counts.get("duplicate").unwrap_or(&0);
            let queued = snapshot.summary.counts.get("queued").copied().unwrap_or(0);
            ui.label(format!("{saved} saved locally"));
            ui.separator();
            ui.label(format!("{uploaded} on ballchasing"));
            ui.separator();
            ui.label(format!("{queued} queued"));
        });
        if !snapshot.summary.blocked.is_empty() {
            status(ui, &snapshot.summary.blocked, Tone::Attention);
        }
        if self.saved.uploads_enabled
            && snapshot.summary.cooldown > crate::now()
            && snapshot.summary.counts.get("queued").copied().unwrap_or(0) > 0
        {
            ui.label(format!(
                "Uploads retry in about {} minute(s). Your local copies are safe.",
                ((snapshot.summary.cooldown - crate::now()) / 60.0).ceil() as u64
            ));
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Recent replays").strong());
        if snapshot.summary.recent.is_empty() {
            ui.add_space(12.0);
            ui.label("No replays saved yet.");
            ui.label("Play a match, close Rocket League, and leave RL Uploader running.");
        } else {
            egui::Grid::new("recent_replays")
                .striped(true)
                .num_columns(3)
                .min_row_height(18.0)
                .spacing([20.0, 5.0])
                .show(ui, |ui| {
                    ui.strong("Replay");
                    ui.strong("Status");
                    ui.strong("Visibility");
                    ui.end_row();
                    for job in &snapshot.summary.recent {
                        let name = replay_label(&job.filename);
                        if job.replay_id.is_empty() {
                            ui.label(name).on_hover_text(&job.filename);
                        } else {
                            ui.hyperlink_to(
                                name,
                                format!("https://ballchasing.com/replay/{}", job.replay_id),
                            )
                            .on_hover_text(&job.filename);
                        }
                        ui.label(job_status(&job.status))
                            .on_hover_text(&job.message);
                        ui.label(visibility_label(&job.visibility));
                        ui.end_row();
                    }
                });
        }
        ui.add_space(10.0);
        if snapshot.summary.counts.get("failed").copied().unwrap_or(0) > 0
            && ui.button("Retry failed uploads").clicked()
        {
            self.worker.send(Command::Retry);
        }
    }
    fn preferences(&mut self, ui: &mut egui::Ui, snapshot: &Snapshot) {
        ui.heading("Settings");
        ui.label("Changes take effect when you click Apply. Account sign-in is saved immediately.");
        ui.add_space(14.0);
        section(ui, "Replay saving", |ui| {
            ui.checkbox(
                &mut self.settings.cloud_capture,
                "Save recent replays automatically after Rocket League closes",
            );
            ui.label(
                "Saving starts after 30 seconds. Match History contains up to 20 recent matches.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("Epic account:");
                status(
                    ui,
                    if self.epic_ready {
                        "Connected"
                    } else if self.epic_saved {
                        "Reconnect required"
                    } else {
                        "Not connected"
                    },
                    if self.epic_ready {
                        Tone::Good
                    } else {
                        Tone::Attention
                    },
                );
                if ui
                    .add_enabled(
                        self.game_state == Some(false),
                        egui::Button::new(if self.epic_saved {
                            "Reconnect…"
                        } else {
                            "Connect Epic…"
                        }),
                    )
                    .clicked()
                {
                    self.connect();
                }
            });
            ui.small("Use the Epic account linked to your Steam Rocket League account.");
            if self.game_state != Some(false) {
                ui.small("Close Rocket League before connecting an account or testing a token.");
            }
            ui.add_space(10.0);
            ui.label("Rocket League replay folder");
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 90.0).max(180.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.replay_dir).desired_width(width),
                );
                if ui.button("Browse…").clicked()
                    && let Some(folder) = rfd::FileDialog::new()
                        .set_directory(&self.settings.replay_dir)
                        .pick_folder()
                {
                    self.settings.replay_dir = folder.to_string_lossy().into();
                }
            });
            let exists = self.settings.folder().is_dir();
            status(
                ui,
                if exists {
                    "Folder found"
                } else {
                    "Folder not found — choose your Rocket League Demos folder."
                },
                if exists { Tone::Good } else { Tone::Attention },
            );
            ui.small("Downloaded replays also have a separate local archive. Manually saved replays are processed after you exit the game.");
        });
        section(ui, "Ballchasing uploads (optional)", |ui| {
            ui.checkbox(
                &mut self.settings.uploads_enabled,
                "Upload saved replays to ballchasing",
            );
            ui.horizontal(|ui| {
                ui.label("API token:");
                status(
                    ui,
                    if !self.token.is_empty() {
                        "Replacement not applied"
                    } else if snapshot.has_token {
                        "Saved on this PC"
                    } else {
                        "Not configured"
                    },
                    if snapshot.has_token && self.token.is_empty() {
                        Tone::Good
                    } else {
                        Tone::Neutral
                    },
                );
            });
            ui.horizontal(|ui| {
                let edit = egui::TextEdit::singleline(&mut *self.token)
                    .password(true)
                    .desired_width((ui.available_width() - 115.0).max(160.0))
                    .hint_text(if snapshot.has_token {
                        "Enter a replacement token"
                    } else {
                        "Paste your ballchasing API token"
                    });
                if ui.add_enabled(!self.testing, edit).changed() {
                    self.token_result = None;
                }
                ui.hyperlink_to("Get API token", "https://ballchasing.com/upload");
            });
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !self.testing
                            && self.game_state == Some(false)
                            && (snapshot.has_token || !self.token.is_empty()),
                        egui::Button::new(if self.testing {
                            "Testing…"
                        } else {
                            "Test connection"
                        }),
                    )
                    .clicked()
                {
                    self.test_token();
                }
                if let Some((ok, message)) = &self.token_result {
                    status(ui, message, if *ok { Tone::Good } else { Tone::Attention });
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label("New upload visibility:");
                egui::ComboBox::from_id_salt("visibility")
                    .selected_text(visibility_label(&self.settings.visibility))
                    .show_ui(ui, |ui| {
                        for (value, label) in [
                            ("private", "Private"),
                            ("unlisted", "Unlisted"),
                            ("public", "Public"),
                        ] {
                            ui.selectable_value(&mut self.settings.visibility, value.into(), label);
                        }
                    });
            });
            ui.small(match self.settings.visibility.as_str() {
                "public" => "Public: anyone can find and view the replay.",
                "unlisted" => "Unlisted: anyone with the replay link can view it.",
                _ => "Private: only your ballchasing account can view the replay.",
            });
            ui.small("Saving works without uploads. Turning uploads off keeps your token and queue for later.");
        });
        section(ui, "Application", |ui| {
            ui.add_enabled(
                crate::install::installed(),
                egui::Checkbox::new(
                    &mut self.startup,
                    "Start in the system tray when I sign in to Windows",
                ),
            );
            ui.small(
                "Closing the window keeps saving enabled. Use the tray menu to pause or quit.",
            );
            ui.add_space(8.0);
            if ui.button("Import existing local replays…").clicked() {
                self.confirm_import = true;
            }
            ui.small("Existing files are left alone until you import them. Imports use the saved upload settings.");
        });
        egui::CollapsingHeader::new("About RL Uploader").show(ui, |ui| {
            ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
            ui.label("Automatic saving uses an unofficial Match History interface. Epic/EAC approval has not been verified.");
            ui.label("This app does not intercept game traffic, read game memory, or change the game.");
        });
    }
    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut renew = false;
        let mut close = false;
        if let Some(sign) = &self.sign_in {
            egui::Window::new("Connect Epic account").collapsible(false).resizable(false).default_width(430.0).anchor(egui::Align2::CENTER_CENTER,[0.0,0.0]).show(ctx, |ui| {
                ui.label(&sign.message);
                if !sign.code.is_empty() {
                    ui.add_space(14.0); ui.label(RichText::new(&sign.code).monospace().size(30.0).strong());
                    ui.horizontal(|ui| { if ui.button("Copy code").clicked() { ui.ctx().copy_text(sign.code.clone()); } ui.hyperlink_to("Open Epic sign-in page",&sign.uri); });
                    if sign.pending && let Some(deadline)=sign.deadline {
                        let seconds=deadline.saturating_duration_since(Instant::now()).as_secs();
                        ui.label(format!("Code expires in {}:{:02}",seconds/60,seconds%60));
                    }
                    ui.small("This dialog stays open if the code expires. Request a new code to try again.");
                }
                ui.add_space(12.0); ui.horizontal(|ui| {
                    renew=ui.button("Get a new code").clicked();
                    close=ui.button(if sign.pending { "Cancel" } else { "Close" }).clicked();
                });
            });
        }
        if renew {
            self.connect();
        }
        if close && let Some(sign) = self.sign_in.take() {
            sign.cancel.store(true, Ordering::Relaxed);
        }
        if self.confirm_import {
            egui::Window::new("Import existing replays")
                .collapsible(false)
                .resizable(false)
                .default_width(400.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Add existing replay files to your archive and upload queue?");
                    ui.label(format!(
                        "Upload visibility: {}",
                        visibility_label(&self.saved.visibility)
                    ));
                    ui.label(if self.saved.uploads_enabled {
                        "Uploads will run after Rocket League closes."
                    } else {
                        "Uploads are off. Replays will be saved locally."
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Import").clicked() {
                            self.worker.send(Command::Import);
                            self.confirm_import = false;
                            self.message =
                                "Import requested. Files are processed after Rocket League closes."
                                    .into();
                            self.message_error = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_import = false;
                        }
                    });
                });
        }
        if self.confirm_close {
            egui::Window::new("Unsaved changes")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label("Apply your changes before closing this window?");
                    ui.horizontal(|ui| {
                        if ui.button("Apply and close").clicked() {
                            let result = self.save();
                            let ok = result.is_ok();
                            self.notify(result, "Settings saved.");
                            if ok {
                                self.confirm_close = false;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        }
                        if ui.button("Discard and close").clicked() {
                            self.settings = self.saved.clone();
                            self.startup = self.saved_startup;
                            self.token.clear();
                            self.confirm_close = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_close = false;
                        }
                    });
                });
        }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_secs(1));
        self.events();
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_close = true;
        }
        let snapshot = self.worker.snapshot.lock().unwrap().clone();
        egui::TopBottomPanel::top("tabs")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(20, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.page, Page::Activity, "Activity");
                    let settings_title = if self.dirty() {
                        "Settings *"
                    } else {
                        "Settings"
                    };
                    ui.selectable_value(&mut self.page, Page::Settings, settings_title);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.small(if snapshot.paused {
                            "Paused"
                        } else {
                            "Running in the system tray"
                        });
                    });
                });
            });
        egui::TopBottomPanel::bottom("actions")
            .frame(
                egui::Frame::new()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(20, 12)),
            )
            .show(ctx, |ui| {
                if !self.message.is_empty() {
                    status(
                        ui,
                        &self.message,
                        if self.message_error {
                            Tone::Attention
                        } else {
                            Tone::Good
                        },
                    );
                    ui.add_space(6.0);
                }
                ui.horizontal(|ui| {
                    if self.page == Page::Settings {
                        if ui
                            .add_enabled(self.dirty() && !self.testing, egui::Button::new("Apply"))
                            .clicked()
                        {
                            let result = self.save();
                            self.notify(result, "Settings saved.");
                        }
                        if ui
                            .add_enabled(self.dirty(), egui::Button::new("Revert"))
                            .clicked()
                        {
                            self.settings = self.saved.clone();
                            self.startup = self.saved_startup;
                            self.token.clear();
                            self.token_result = None;
                            self.message.clear();
                        }
                        ui.small(if self.dirty() {
                            "Unsaved changes"
                        } else {
                            "All changes saved"
                        });
                    } else {
                        ui.small("Closing this window keeps RL Uploader in the system tray.");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(24.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(match self.page {
                        Page::Activity => "activity_scroll",
                        Page::Settings => "settings_scroll",
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.page {
                            Page::Activity => self.overview(ui, &snapshot),
                            Page::Settings => self.preferences(ui, &snapshot),
                        }
                    });
            });
        self.dialogs(ctx);
    }
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        if let Some(sign) = &self.sign_in {
            sign.cancel.store(true, Ordering::Relaxed);
        }
    }
}
pub fn run(dir: PathBuf, settings: Settings) -> Result<()> {
    let worker = Frontend::connect()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 790.0])
            .with_min_inner_size([720.0, 560.0])
            .with_icon(egui::IconData {
                rgba: crate::desktop::icon_rgba(),
                width: 32,
                height: 32,
            }),
        ..Default::default()
    };
    eframe::run_native(
        "RL Uploader",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_theme(egui::ThemePreference::System);
            #[cfg(windows)]
            if let Some(font) = std::env::var_os("WINDIR")
                .and_then(|root| std::fs::read(PathBuf::from(root).join("Fonts/segoeui.ttf")).ok())
            {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "Segoe UI".into(),
                    Arc::new(egui::FontData::from_owned(font)),
                );
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "Segoe UI".into());
                cc.egui_ctx.set_fonts(fonts);
            }
            for theme in [egui::Theme::Light, egui::Theme::Dark] {
                cc.egui_ctx.style_mut_of(theme, |style| {
                    style
                        .text_styles
                        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
                    style
                        .text_styles
                        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
                    style
                        .text_styles
                        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
                    style
                        .text_styles
                        .insert(egui::TextStyle::Heading, egui::FontId::proportional(23.0));
                    style.spacing.item_spacing = egui::vec2(9.0, 7.0);
                    style.spacing.button_padding = egui::vec2(12.0, 5.0);
                    style.spacing.interact_size = egui::vec2(40.0, 28.0);
                    let accent = if theme == egui::Theme::Light {
                        Color32::from_rgb(0, 103, 192)
                    } else {
                        Color32::from_rgb(96, 177, 245)
                    };
                    style.visuals.hyperlink_color = accent;
                    style.visuals.selection.bg_fill = if theme == egui::Theme::Light {
                        Color32::from_rgb(218, 235, 250)
                    } else {
                        Color32::from_rgb(35, 69, 96)
                    };
                    style.visuals.selection.stroke = egui::Stroke::new(1.0_f32, accent);
                    for widget in [
                        &mut style.visuals.widgets.inactive,
                        &mut style.visuals.widgets.hovered,
                        &mut style.visuals.widgets.active,
                    ] {
                        widget.corner_radius = egui::CornerRadius::same(4);
                    }
                });
            }
            Ok(Box::new(App::new(dir, settings, worker)))
        }),
    )
    .map_err(|_| {
        Error::new(
            "Could not open the desktop window. Try --headless or update your graphics driver.",
        )
    })
}

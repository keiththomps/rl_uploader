use crate::{
    Result,
    api::{Ballchasing, Upload},
    cloud::CloudCapture,
    config::Settings,
    engine::Engine,
    platform,
    state::Summary,
};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub summary: Summary,
    pub scan: String,
    pub cloud: String,
    pub error: String,
    pub paused: bool,
    pub has_token: bool,
    pub settings: Settings,
    pub game_running: Option<bool>,
    pub capture_error: String,
    pub scans: u64,
    pub cloud_checks: u64,
    pub summary_refreshes: u64,
}
#[derive(serde::Serialize, serde::Deserialize)]
pub enum Command {
    Apply(Settings, bool),
    Pause,
    Resume,
    Import,
    Retry,
    Reconnect,
    Stop,
    FilesChanged,
    WatchLost,
}
#[derive(Clone)]
pub struct WorkerHandle {
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub cancel: Arc<AtomicBool>,
    pub tx: mpsc::Sender<Command>,
}
impl WorkerHandle {
    pub fn send(&self, command: Command) {
        if matches!(command, Command::Pause | Command::Stop | Command::Apply(..)) {
            self.cancel.store(true, Ordering::Relaxed);
        }
        let _ = self.tx.send(command);
    }
}

pub struct Worker {
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub cancel: Arc<AtomicBool>,
    pub tx: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(dir: &Path, settings: Settings, paused: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let cancel = Arc::new(AtomicBool::new(paused));
        let out = snapshot.clone();
        let stop = cancel.clone();
        let dir = dir.to_path_buf();
        let worker_tx = tx.clone();
        let thread = std::thread::spawn(move || {
            let result = open_engine(&dir, settings).and_then(|engine| {
                run(
                    engine,
                    paused,
                    rx,
                    worker_tx,
                    &out,
                    stop,
                    Arc::new(platform::game_running),
                )
            });
            if let Err(e) = result {
                out.lock().unwrap().error = e.message;
            }
        });
        Self {
            snapshot,
            cancel,
            tx,
            thread: Some(thread),
        }
    }
    pub fn handle(&self) -> WorkerHandle {
        WorkerHandle {
            snapshot: self.snapshot.clone(),
            cancel: self.cancel.clone(),
            tx: self.tx.clone(),
        }
    }
    pub fn send(&self, command: Command) {
        self.handle().send(command);
    }
    pub fn stop(&mut self) {
        self.send(Command::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}
fn open_engine(dir: &Path, settings: Settings) -> Result<Engine> {
    let mut engine = Engine::new(dir, settings, None)?;
    match platform::load_token(dir) {
        Ok(token) if !token.is_empty() => match Ballchasing::new(token) {
            Ok(client) => engine.client = Some(Box::new(client) as Box<dyn Upload>),
            Err(e) => engine.state.set("blocked", e.message)?,
        },
        Ok(_) => (),
        Err(e) => engine.state.set("blocked", e.message)?,
    }
    Ok(engine)
}
#[allow(clippy::too_many_arguments)]
fn run(
    mut engine: Engine,
    mut paused: bool,
    rx: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Command>,
    snapshot: &Mutex<Snapshot>,
    stop: Arc<AtomicBool>,
    running: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
) -> Result<()> {
    let dir = engine.data_dir.clone();
    let mut capture = CloudCapture::new(&dir, stop.clone());
    capture.running = running.clone();
    let transfer_stop = stop.clone();
    let transfer_running = running.clone();
    let transfer_guard: crate::protocol::Guard = Arc::new(move || {
        !transfer_stop.load(Ordering::Relaxed) && transfer_running() == Some(false)
    });
    let mut game_idle = false;
    let dirty = Arc::new(AtomicBool::new(false));
    let mut watcher = crate::watch::Watcher::start(&engine.folder, tx.clone(), dirty.clone());
    let mut next_scan = crate::now();
    let mut next_cloud = crate::now();
    let mut revision = None;
    let mut command = None;
    loop {
        if let Some(command) = command.take() {
            let mut schedule_now = true;
            let result = match command {
                Command::Stop => return Ok(()),
                Command::Pause => {
                    paused = true;
                    Ok(())
                }
                Command::Resume => {
                    paused = false;
                    stop.store(false, Ordering::Relaxed);
                    capture.resume();
                    Ok(())
                }
                Command::Apply(settings, new_token) => {
                    // Complete/cancel the old pass before changing its transport or folder.
                    match open_engine(&dir, settings) {
                        Ok(value) => {
                            engine = value;
                            watcher = crate::watch::Watcher::start(
                                &engine.folder,
                                tx.clone(),
                                dirty.clone(),
                            );
                            revision = None;
                            if new_token {
                                engine.state.set("blocked", "")?;
                            }
                            capture = CloudCapture::new(&dir, stop.clone());
                            capture.running = running.clone();
                            stop.store(paused, Ordering::Relaxed);
                            Ok(())
                        }
                        Err(e) => {
                            paused = true;
                            Err(e)
                        }
                    }
                }
                Command::Import => engine.include_existing(),
                Command::Retry => engine.state.retry_failed(),
                Command::Reconnect => {
                    capture = CloudCapture::new(&dir, stop.clone());
                    capture.running = running.clone();
                    CloudCapture::reconnect(&engine)
                }
                Command::FilesChanged => {
                    dirty.store(false, Ordering::Release);
                    next_scan = crate::now();
                    schedule_now = false;
                    Ok(())
                }
                Command::WatchLost => {
                    watcher = None;
                    next_scan = crate::now();
                    schedule_now = false;
                    Ok(())
                }
            };
            snapshot.lock().unwrap().error = result.err().map(|e| e.message).unwrap_or_default();
            if schedule_now {
                next_scan = crate::now();
                next_cloud = crate::now();
            }
        }
        let now = crate::now();
        if !paused {
            let game_state = running();
            snapshot.lock().unwrap().game_running = game_state;
            let idle = game_state == Some(false);
            if idle != game_idle {
                next_scan = now;
                next_cloud = now;
            }
            game_idle = idle;
            let result = (|| {
                if game_idle && now >= next_scan {
                    if watcher.is_none() {
                        watcher =
                            crate::watch::Watcher::start(&engine.folder, tx.clone(), dirty.clone());
                    }
                    engine.scan(now)?;
                    snapshot.lock().unwrap().scans += 1;
                    // Active writes settle promptly. Reconcile infrequently to cover missed
                    // OneDrive/network-drive notifications without repeated directory scans.
                    next_scan = now
                        + if engine.settling() {
                            5.0
                        } else if watcher.is_none() {
                            10.0
                        } else {
                            300.0
                        };
                }
                if now >= next_cloud {
                    capture.tick(&mut engine, now)?;
                    snapshot.lock().unwrap().cloud_checks += 1;
                    next_cloud = now
                        + if engine.settings.cloud_capture {
                            capture.check_interval(now)
                        } else {
                            3600.0
                        };
                }
                if game_idle
                    && !stop.load(Ordering::Relaxed)
                    && engine.settings.uploads_enabled
                    && engine.client.is_some()
                    && engine.state.next_upload()?.is_some_and(|due| due <= now)
                {
                    engine.upload_one(now, &transfer_guard)?;
                }
                Ok::<_, crate::Error>(())
            })();
            if let Err(e) = result {
                snapshot.lock().unwrap().error = e.message;
                next_scan = next_scan.max(crate::now() + 10.0);
                next_cloud = next_cloud.max(crate::now() + 10.0);
            }
        }
        {
            let mut status = snapshot.lock().unwrap();
            let changes = engine.state.connection.total_changes();
            if revision != Some(changes) {
                status.summary = engine.state.summary()?;
                status.capture_error = engine.state.get("cloud_blocked")?;
                status.summary_refreshes += 1;
                revision = Some(changes);
            }
            status.scan = if !paused && !game_idle {
                "Replay processing and uploads are waiting until Rocket League is confirmed closed."
                    .into()
            } else {
                engine.message.clone()
            };
            status.cloud = capture.message.clone();
            status.paused = paused;
            status.has_token = engine.client.is_some();
            status.settings = engine.settings.clone();
        }
        let timeout = if paused {
            3600.0
        } else {
            let upload = if game_idle && engine.settings.uploads_enabled && engine.client.is_some()
            {
                engine.state.next_upload()?.unwrap_or(f64::INFINITY)
            } else {
                f64::INFINITY
            };
            let scan = if game_idle { next_scan } else { f64::INFINITY };
            // Check process state without spinning on deferred files or queued uploads.
            (scan.min(next_cloud).min(upload) - crate::now()).clamp(0.05, 5.0)
        };
        match rx.recv_timeout(Duration::from_secs_f64(timeout)) {
            Ok(c) => command = Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Upload, UploadResponse};
    use std::{fs, sync::atomic::AtomicUsize, time::Instant};

    struct RecordingUpload(Arc<AtomicUsize>);
    impl Upload for RecordingUpload {
        fn upload(
            &self,
            _: Vec<u8>,
            _: &str,
            guard: &crate::protocol::Guard,
        ) -> Result<UploadResponse> {
            crate::protocol::check(guard)?;
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(UploadResponse {
                status: 201,
                headers: Default::default(),
                value: serde_json::json!({"id":"synthetic-replay"}),
            })
        }
    }

    #[test]
    fn disabling_uploads_retains_queue_and_applying_settings_preserves_pause() {
        let root = tempfile::tempdir().unwrap();
        let demos = root.path().join("Demos");
        let data = root.path().join("data");
        fs::create_dir(&demos).unwrap();
        fs::create_dir(&data).unwrap();
        let config = Settings {
            replay_dir: demos.to_string_lossy().into(),
            uploads_enabled: false,
            ..Default::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new(
            &data,
            config.clone(),
            Some(Box::new(RecordingUpload(calls.clone()))),
        )
        .unwrap();
        let replay = vec![1, 0, 0, 0, 0, 0, 0, 0, 42, 1, 0, 0, 0, 0, 0, 0, 0, 42];
        engine
            .archive_replay(&replay, "test.replay", "test", "test", crate::now())
            .unwrap();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let out = snapshot.clone();
        let (tx, rx) = mpsc::channel();
        let worker_tx = tx.clone();
        let handle = std::thread::spawn(move || {
            run(
                engine,
                false,
                rx,
                worker_tx,
                &out,
                Arc::new(AtomicBool::new(false)),
                Arc::new(|| Some(false)),
            )
        });
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !condition() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let result = std::panic::catch_unwind(|| {
            wait(&|| snapshot.lock().unwrap().scans > 0);
            std::thread::sleep(Duration::from_millis(200));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(snapshot.lock().unwrap().summary.counts["queued"], 1);
            tx.send(Command::Pause).unwrap();
            wait(&|| snapshot.lock().unwrap().paused);
            let mut changed = config;
            changed.uploads_enabled = true;
            tx.send(Command::Apply(changed, false)).unwrap();
            wait(&|| snapshot.lock().unwrap().settings.uploads_enabled);
            assert!(snapshot.lock().unwrap().paused);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        });
        let _ = tx.send(Command::Stop);
        handle.join().unwrap().unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    fn gameplay_and_unknown_state_defer_files_and_uploads_then_resume_without_busy_polling() {
        let root = tempfile::tempdir().unwrap();
        let demos = root.path().join("Demos");
        let data = root.path().join("data");
        fs::create_dir(&demos).unwrap();
        fs::create_dir(&data).unwrap();
        let settings = Settings {
            replay_dir: demos.to_string_lossy().into(),
            ..Default::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new(
            &data,
            settings,
            Some(Box::new(RecordingUpload(calls.clone()))),
        )
        .unwrap();
        engine.scan(crate::now()).unwrap(); // Establish the original empty replay folder.
        let mut replay = vec![1, 0, 0, 0, 0, 0, 0, 0, 42, 1, 0, 0, 0, 0, 0, 0, 0, 42];
        engine
            .archive_replay(&replay, "old.replay", "old", "old", crate::now())
            .unwrap();
        replay[17] = 43;
        fs::write(demos.join("during-game.replay"), &replay).unwrap();
        let state = Arc::new(Mutex::new(Some(true)));
        let game = state.clone();
        let checks = Arc::new(AtomicUsize::new(0));
        let count = checks.clone();
        let running = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            *game.lock().unwrap()
        });
        let (tx, rx) = mpsc::channel();
        let worker_tx = tx.clone();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let out = snapshot.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = stop.clone();
        let handle =
            std::thread::spawn(move || run(engine, false, rx, worker_tx, &out, stop, running));
        let result = std::panic::catch_unwind(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while snapshot.lock().unwrap().scan.is_empty() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(snapshot.lock().unwrap().scans, 0);
            *state.lock().unwrap() = None;
            tx.send(Command::FilesChanged).unwrap();
            std::thread::sleep(Duration::from_millis(250));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(snapshot.lock().unwrap().scans, 0);
            assert!(
                checks.load(Ordering::SeqCst) <= 4,
                "queued work must not cause a busy loop"
            );
            *state.lock().unwrap() = Some(false);
            let deadline = Instant::now() + Duration::from_secs(20);
            while calls.load(Ordering::SeqCst) < 2 {
                assert!(
                    Instant::now() < deadline,
                    "queued and newly saved replays did not resume"
                );
                std::thread::sleep(Duration::from_millis(30));
            }
            assert_eq!(
                fs::read(
                    data.join("archive")
                        .join(format!("{}.replay", crate::engine::digest(&replay)))
                )
                .unwrap(),
                replay
            );
        });
        cancel.store(true, Ordering::Relaxed);
        let _ = tx.send(Command::Stop);
        handle.join().unwrap().unwrap();
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

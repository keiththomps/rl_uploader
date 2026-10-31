use crate::{
    Result,
    api::{Ballchasing, Upload},
    cloud::CloudCapture,
    config::Settings,
    engine::Engine,
    platform,
    proxy::CaptureProxy,
    state::Summary,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub summary: Summary,
    pub scan: String,
    pub cloud: String,
    pub error: String,
    pub paused: bool,
    pub has_token: bool,
}
pub enum Command {
    Apply(Settings, bool),
    Pause,
    Resume,
    Import,
    Retry,
    Reconnect,
    Stop,
}
pub struct Worker {
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub cancel: Arc<AtomicBool>,
    pub tx: mpsc::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(dir: &Path, settings: Settings, proxy: Arc<CaptureProxy>, paused: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let cancel = Arc::new(AtomicBool::new(paused));
        let out = snapshot.clone();
        let stop = cancel.clone();
        let dir = dir.to_path_buf();
        let thread = std::thread::spawn(move || {
            if let Err(e) = run(dir, settings, proxy, paused, &rx, &out, stop) {
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
    pub fn send(&self, command: Command) {
        if matches!(command, Command::Pause | Command::Stop | Command::Apply(..)) {
            self.cancel.store(true, Ordering::Relaxed);
        }
        let _ = self.tx.send(command);
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
fn configure_proxy(proxy: &Arc<CaptureProxy>, settings: &Settings) {
    let enabled = settings.cloud_capture && settings.capture_mode == "periodic";
    proxy.enabled.store(enabled, Ordering::Relaxed);
    if enabled && !proxy.started.load(Ordering::Relaxed) {
        let p = proxy.clone();
        std::thread::spawn(move || {
            let _ = p.start();
        });
    }
}
fn run(
    dir: PathBuf,
    settings: Settings,
    proxy: Arc<CaptureProxy>,
    mut paused: bool,
    rx: &mpsc::Receiver<Command>,
    snapshot: &Mutex<Snapshot>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut engine = open_engine(&dir, settings)?;
    let mut capture = CloudCapture::new(&dir, stop.clone(), Some(proxy.clone()));
    if !paused {
        configure_proxy(&proxy, &engine.settings);
    }
    let mut next = std::time::Instant::now();
    loop {
        for command in rx.try_iter() {
            let result = match command {
                Command::Stop => return Ok(()),
                Command::Pause => {
                    paused = true;
                    Ok(())
                }
                Command::Resume => {
                    paused = false;
                    stop.store(false, Ordering::Relaxed);
                    configure_proxy(&proxy, &engine.settings);
                    Ok(())
                }
                Command::Apply(settings, new_token) => {
                    // Complete/cancel the old pass before changing its transport or folder.
                    match open_engine(&dir, settings) {
                        Ok(value) => {
                            engine = value;
                            if new_token {
                                engine.state.set("blocked", "")?;
                            }
                            capture = CloudCapture::new(&dir, stop.clone(), Some(proxy.clone()));
                            paused = false;
                            stop.store(false, Ordering::Relaxed);
                            configure_proxy(&proxy, &engine.settings);
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
                Command::Reconnect => CloudCapture::reconnect(&engine),
            };
            snapshot.lock().unwrap().error = result.err().map(|e| e.message).unwrap_or_default();
            next = std::time::Instant::now();
        }
        if std::time::Instant::now() >= next {
            if !paused {
                let result = (|| {
                    engine.scan(crate::now())?;
                    capture.tick(&mut engine, crate::now())?;
                    if !stop.load(Ordering::Relaxed) {
                        engine.upload_one(crate::now())?;
                    }
                    Ok::<_, crate::Error>(())
                })();
                if let Err(e) = result {
                    snapshot.lock().unwrap().error = e.message;
                }
            }
            let mut status = snapshot.lock().unwrap();
            status.summary = engine.state.summary()?;
            status.scan = engine.message.clone();
            status.cloud = capture.message.clone();
            status.paused = paused;
            status.has_token = engine.client.is_some();
            next = std::time::Instant::now() + Duration::from_secs(2);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

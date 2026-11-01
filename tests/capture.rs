use rl_uploader::{
    Error, Result,
    cloud::{CloudCapture, History},
    config::Settings,
    engine::{Engine, digest},
    protocol::{self, Guard},
};
use serde_json::{Value, json};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

fn replay(content: &[u8]) -> Vec<u8> {
    let mut d = vec![];
    d.extend(1u32.to_le_bytes());
    d.extend([0; 4]);
    d.push(1);
    d.extend((content.len() as u32).to_le_bytes());
    d.extend([0; 4]);
    d.extend(content);
    d
}
struct Mock {
    calls: Arc<AtomicUsize>,
    entries: Vec<Value>,
    fail: bool,
}
impl History for Mock {
    fn history(&self, guard: &Guard) -> Result<(String, Vec<Value>)> {
        protocol::check(guard)?;
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail {
            return Err(Error::permanent("Synthetic access failure."));
        }
        Ok(("synthetic-account".into(), self.entries.clone()))
    }
}
fn downloaded(url: &str, guard: &Guard) -> Result<Vec<u8>> {
    protocol::check(guard)?;
    if url == "missing" {
        return Err(Error::new("Not ready."));
    }
    Ok(replay(url.as_bytes()))
}
struct Fixture {
    _root: tempfile::TempDir,
    engine: Engine,
    capture: CloudCapture,
    running: Arc<std::sync::Mutex<Option<bool>>>,
    offline: Arc<AtomicUsize>,
}
impl Fixture {
    fn new(running: Option<bool>, entries: Vec<Value>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let folder = root.path().join("Demos");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(
            data.join("epic.dpapi"),
            b"fake marker; offline mock never decrypts it",
        )
        .unwrap();
        let settings = Settings {
            replay_dir: folder.to_string_lossy().into(),
            cloud_capture: true,
            ..Default::default()
        };
        let engine = Engine::new(&data, settings, None).unwrap();
        let offline = Arc::new(AtomicUsize::new(0));
        let mut capture = CloudCapture::new(&data, Arc::new(AtomicBool::new(false)));
        capture.offline = Arc::new(Mock {
            calls: offline.clone(),
            entries,
            fail: false,
        });
        capture.downloader = downloaded;
        let running = Arc::new(std::sync::Mutex::new(running));
        let check = running.clone();
        capture.running = Arc::new(move || *check.lock().unwrap());
        Self {
            _root: root,
            engine,
            capture,
            running,
            offline,
        }
    }
    fn tick(&mut self, now: f64) {
        self.capture.tick(&mut self.engine, now).unwrap();
    }
}
fn entry(id: &str, url: &str) -> Value {
    json!({"Match":{"MatchGUID":id},"ReplayUrl":url})
}
#[test]
fn disabled_capture_does_not_access_history() {
    let mut f = Fixture::new(Some(true), vec![]);
    f.engine.settings.cloud_capture = false;
    f.tick(0.0);
    f.tick(5000.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
}
#[test]
fn unknown_process_state_fails_closed() {
    let mut f = Fixture::new(None, vec![]);
    f.tick(0.0);
    f.tick(5000.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
    assert!(f.capture.message.contains("Cannot verify"));
}
#[test]
fn after_session_waits_for_exit_and_full_thirty_seconds() {
    let mut f = Fixture::new(Some(true), vec![]);
    f.tick(0.0);
    f.tick(5000.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
    *f.running.lock().unwrap() = Some(false);
    f.tick(5001.0);
    f.tick(5030.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
    f.tick(5031.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 1);
}
#[test]
fn capture_saves_demos_archive_and_durable_queue_without_upload_token() {
    let mut f = Fixture::new(Some(false), vec![entry("01234567-89ab-cdef", "one")]);
    f.tick(0.0);
    f.tick(30.0);
    assert!(f.engine.folder.join("01234567-89ab-cdef.replay").exists());
    assert!(
        f.engine
            .archive
            .join(format!("{}.replay", digest(&replay(b"one"))))
            .exists()
    );
    assert_eq!(f.engine.state.summary().unwrap().counts["queued"], 1);
    assert!(f.engine.client.is_none());
    f.tick(901.0);
    assert_eq!(f.engine.state.summary().unwrap().counts["queued"], 1);
}
#[test]
fn missing_replay_does_not_block_other_available_replays() {
    let mut f = Fixture::new(
        Some(false),
        vec![
            entry("11111111-11111111", "one"),
            entry("22222222-22222222", "missing"),
            entry("33333333-33333333", "three"),
        ],
    );
    f.tick(0.0);
    f.tick(30.0);
    assert_eq!(f.engine.state.summary().unwrap().counts["queued"], 2);
    assert!(f.capture.message.contains("1 unavailable"));
    assert_eq!(f.engine.state.number("cloud_next").unwrap(), 90.0);
}
#[test]
fn account_quota_never_stops_local_capture() {
    let mut f = Fixture::new(Some(false), vec![entry("01234567-89ab-cdef", "one")]);
    f.engine.state.set("blocked", "Upload quota").unwrap();
    f.engine.state.set("cooldown", 99999999999u64).unwrap();
    f.tick(0.0);
    f.tick(30.0);
    assert_eq!(f.engine.state.summary().unwrap().counts["queued"], 1);
}
#[test]
fn repeated_match_ids_are_saved_once() {
    let m = entry("01234567-89ab-cdef", "one");
    let mut f = Fixture::new(Some(false), vec![m.clone(), m]);
    f.tick(0.0);
    f.tick(30.0);
    assert_eq!(fs::read_dir(&f.engine.folder).unwrap().count(), 1);
    assert_eq!(f.engine.state.summary().unwrap().counts["queued"], 1);
}
#[test]
fn cancelled_capture_does_not_fetch_history() {
    let mut f = Fixture::new(Some(false), vec![]);
    f.capture.stop.store(true, Ordering::Relaxed);
    f.tick(0.0);
    f.tick(30.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
}
#[test]
fn game_start_mid_capture_prevents_offline_auth_even_with_credentials_missing() {
    let temp = tempfile::tempdir().unwrap();
    let auth = protocol::EpicAuth::new(temp.path());
    let blocked: Guard = Arc::new(|| false);
    let error = protocol::offline_history(&auth, &blocked).unwrap_err();
    assert!(error.message.contains("Game state changed"));
}
#[test]
fn unknown_state_resets_exit_grace_period() {
    let mut f = Fixture::new(Some(false), vec![]);
    f.tick(0.0);
    *f.running.lock().unwrap() = None;
    f.tick(29.0);
    *f.running.lock().unwrap() = Some(false);
    f.tick(30.0);
    f.tick(59.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 0);
    f.tick(60.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 1);
}
#[test]
fn reconnect_clears_auth_block_without_clearing_upload_quota() {
    let f = Fixture::new(Some(true), vec![]);
    f.engine.state.set("cloud_blocked", "denied").unwrap();
    f.engine.state.set("cloud_next", 99999).unwrap();
    f.engine.state.set("cooldown", 99999).unwrap();
    CloudCapture::reconnect(&f.engine).unwrap();
    assert!(f.engine.state.get("cloud_blocked").unwrap().is_empty());
    assert_eq!(f.engine.state.number("cloud_next").unwrap(), 0.0);
    assert_eq!(f.engine.state.number("cooldown").unwrap(), 99999.0);
}

#[test]
fn completed_capture_does_not_poll_again_until_another_session_ends() {
    let mut f = Fixture::new(Some(false), vec![]);
    f.tick(0.0);
    f.tick(30.0);
    f.tick(100000.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 1);
    *f.running.lock().unwrap() = Some(true);
    f.tick(100001.0);
    *f.running.lock().unwrap() = Some(false);
    f.tick(100002.0);
    f.tick(100032.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 2);
}

#[test]
fn resume_checks_for_a_session_missed_while_paused_after_a_new_grace_period() {
    let mut f = Fixture::new(Some(false), vec![]);
    f.tick(0.0);
    f.tick(30.0);
    f.capture.resume();
    f.tick(1000.0);
    f.tick(1029.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 1);
    f.tick(1030.0);
    f.tick(100000.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 2);
}

#[test]
fn unknown_state_after_completion_allows_a_fresh_check_when_closed_again() {
    let mut f = Fixture::new(Some(false), vec![]);
    f.tick(0.0);
    f.tick(30.0);
    *f.running.lock().unwrap() = None;
    f.tick(40.0);
    *f.running.lock().unwrap() = Some(false);
    f.tick(50.0);
    f.tick(79.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 1);
    f.tick(80.0);
    assert_eq!(f.offline.load(Ordering::Relaxed), 2);
}

use rl_uploader::{
    config::Settings,
    desktop,
    engine::{Engine, digest},
    install, platform,
    worker::{Command, Worker},
};
use std::{
    fs,
    time::{Duration, Instant},
};

fn replay() -> Vec<u8> {
    vec![1, 0, 0, 0, 0, 0, 0, 0, 42, 1, 0, 0, 0, 0, 0, 0, 0, 42]
}
fn settings(folder: &std::path::Path) -> Settings {
    Settings {
        replay_dir: folder.to_string_lossy().into(),
        ..Default::default()
    }
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "background operation did not complete"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(windows)]
#[test]
fn idle_worker_does_not_rescan_or_reload_history_every_tick() {
    let root = tempfile::tempdir().unwrap();
    let demos = root.path().join("Demos");
    let data = root.path().join("data");
    fs::create_dir(&demos).unwrap();
    fs::create_dir(&data).unwrap();
    let mut config = settings(&demos);
    config.cloud_capture = true;
    let mut worker = Worker::start(&data, config, false);
    wait_until(|| worker.snapshot.lock().unwrap().scans == 1);
    std::thread::sleep(Duration::from_secs(6));
    let status = worker.snapshot.lock().unwrap().clone();
    assert_eq!(status.scans, 1);
    assert_eq!(status.summary_refreshes, 1);
    assert!((1..=2).contains(&status.cloud_checks));
    let stop = Instant::now();
    worker.stop();
    assert!(stop.elapsed() < Duration::from_secs(1));
}
#[cfg(windows)]
#[test]
fn directory_notification_saves_new_replay_and_pause_retains_worker() {
    let root = tempfile::tempdir().unwrap();
    let demos = root.path().join("Demos");
    let data = root.path().join("data");
    fs::create_dir(&demos).unwrap();
    fs::create_dir(&data).unwrap();
    let mut worker = Worker::start(&data, settings(&demos), false);
    wait_until(|| worker.snapshot.lock().unwrap().scans == 1);
    fs::write(demos.join("new.replay"), replay()).unwrap();
    wait_until(|| worker.snapshot.lock().unwrap().summary.counts.get("queued") == Some(&1));
    assert_eq!(
        fs::read(
            data.join("archive")
                .join(format!("{}.replay", digest(&replay())))
        )
        .unwrap(),
        replay()
    );
    worker.send(Command::Pause);
    wait_until(|| worker.snapshot.lock().unwrap().paused);
    let scans = worker.snapshot.lock().unwrap().scans;
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(worker.snapshot.lock().unwrap().scans, scans);
    worker.send(Command::Resume);
    wait_until(|| !worker.snapshot.lock().unwrap().paused);
    worker.stop();
}
#[test]
fn migration_keeps_source_and_transfers_committed_wal_and_archive() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    let demos = root.path().join("Demos");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir(&demos).unwrap();
    let config = settings(&demos);
    config.save(&source).unwrap();
    let mut engine = Engine::new(&source, config, None).unwrap();
    let hash = engine
        .archive_replay(&replay(), "test.replay", "source", "signature", 123.0)
        .unwrap();
    engine.state.set("migration-sentinel", "preserve").unwrap();
    assert!(install::migrate_data(&source, &target).unwrap());
    let copied = rl_uploader::state::State::open(&target).unwrap();
    assert_eq!(copied.get("migration-sentinel").unwrap(), "preserve");
    assert_eq!(copied.summary().unwrap().counts["queued"], 1);
    assert_eq!(
        fs::read(source.join("archive").join(format!("{hash}.replay"))).unwrap(),
        fs::read(target.join("archive").join(format!("{hash}.replay"))).unwrap()
    );
    assert!(source.join("settings.json").exists());
}
#[test]
fn migration_refuses_running_source_and_never_merges_existing_profile() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::create_dir(&source).unwrap();
    settings(root.path()).save(&source).unwrap();
    let lock = platform::InstanceLock::acquire(&source).unwrap();
    assert!(install::migrate_data(&source, &target).is_err());
    assert!(!target.exists());
    drop(lock);
    fs::create_dir(&target).unwrap();
    fs::write(target.join("keep-me"), b"original").unwrap();
    assert!(!install::migrate_data(&source, &target).unwrap());
    assert_eq!(fs::read(target.join("keep-me")).unwrap(), b"original");
}
#[test]
fn native_icon_and_instance_identity_are_deterministic() {
    let ico = desktop::icon_ico();
    assert_eq!(&ico[..6], &[0, 0, 1, 0, 1, 0]);
    assert_eq!(ico.len(), 22 + 40 + 4096 + 128);
    assert_eq!(ico, include_bytes!("../assets/app.ico"));
    let root = tempfile::tempdir().unwrap();
    let name = desktop::window_name(root.path());
    assert_eq!(name, desktop::window_name(root.path()));
    assert_ne!(name, desktop::window_name(&root.path().join("other")));
}

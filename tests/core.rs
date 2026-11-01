use rl_uploader::{
    api::{self, Upload, UploadResponse},
    cloud,
    config::Settings,
    engine::{self, Engine},
    platform, protocol,
    state::State,
};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

fn upload_guard(allowed: bool) -> protocol::Guard {
    Arc::new(move || allowed)
}

fn replay(content: &[u8]) -> Vec<u8> {
    let mut d = vec![];
    d.extend(1u32.to_le_bytes());
    d.extend([0u8; 4]);
    d.push(42);
    d.extend((content.len() as u32).to_le_bytes());
    d.extend([0u8; 4]);
    d.extend(content);
    d
}
struct Fixture {
    _root: tempfile::TempDir,
    folder: std::path::PathBuf,
    data: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("Demos");
        let data = root.path().join("data");
        fs::create_dir(&folder).unwrap();
        fs::create_dir(&data).unwrap();
        Self {
            _root: root,
            folder,
            data,
        }
    }
    fn settings(&self) -> Settings {
        Settings {
            replay_dir: self.folder.to_string_lossy().into(),
            ..Default::default()
        }
    }
    fn engine(&self) -> Engine {
        Engine::new(&self.data, self.settings(), None).unwrap()
    }
}
#[test]
fn legacy_settings_migrate_without_losing_choices() {
    let f = Fixture::new();
    fs::write(
        f.data.join("settings.json"),
        serde_json::to_vec(
            &json!({"replay_dir":f.folder,"visibility":"unlisted","cloud_capture":true}),
        )
        .unwrap(),
    )
    .unwrap();
    let s = Settings::load(&f.data).unwrap();
    assert!(s.uploads_enabled);
    assert_eq!(s.visibility, "unlisted");
    assert!(s.cloud_capture);
    s.save(&f.data).unwrap();
    assert_eq!(Settings::load(&f.data).unwrap(), s);
}
#[test]
fn settings_drop_retired_fields_without_losing_active_preferences() {
    let f = Fixture::new();
    fs::write(f.data.join("settings.json"), json!({"replay_dir":f.folder,"visibility":"unlisted","cloud_capture":true,"capture_mode":"periodic","capture_interval_minutes":30}).to_string()).unwrap();
    let settings = Settings::load(&f.data).unwrap();
    assert!(settings.cloud_capture && settings.uploads_enabled);
    settings.save(&f.data).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(f.data.join("settings.json")).unwrap()).unwrap();
    assert!(saved.get("capture_mode").is_none());
    assert!(saved.get("capture_interval_minutes").is_none());
    assert_eq!(saved["visibility"], "unlisted");
    let mut invalid = settings;
    invalid.visibility = "friends".into();
    assert!(invalid.validate().is_err());
}
#[test]
fn container_framing_rejects_partial_and_extra_bytes() {
    let d = replay(b"data");
    assert!(engine::complete_replay(&d));
    for n in 0..d.len() {
        assert!(!engine::complete_replay(&d[..n]));
    }
    let mut extra = d;
    extra.push(1);
    assert!(!engine::complete_replay(&extra));
    assert!(!engine::complete_replay(&[255; 20]));
}
#[test]
fn initial_files_are_excluded_then_explicitly_importable() {
    let f = Fixture::new();
    fs::write(f.folder.join("old.replay"), replay(b"old")).unwrap();
    let mut e = f.engine();
    assert_eq!(e.scan(0.0).unwrap(), 0);
    assert_eq!(e.state.summary().unwrap().ignored, 1);
    e.include_existing().unwrap();
    e.scan(1.0).unwrap();
    assert_eq!(e.scan(7.0).unwrap(), 1);
    assert_eq!(e.state.summary().unwrap().counts["queued"], 1);
}
#[test]
fn new_replays_settle_and_partial_files_never_upload() {
    let f = Fixture::new();
    let mut e = f.engine();
    e.scan(0.0).unwrap();
    let path = f.folder.join("new.replay");
    let data = replay(b"new");
    fs::write(&path, &data[..8]).unwrap();
    e.scan(1.0).unwrap();
    assert_eq!(e.scan(7.0).unwrap(), 0);
    fs::write(&path, &data).unwrap();
    e.scan(8.0).unwrap();
    assert_eq!(e.scan(12.0).unwrap(), 0);
    assert_eq!(e.scan(14.0).unwrap(), 1);
    assert_eq!(fs::read(path).unwrap(), data);
}
#[test]
fn archive_survives_source_deletion_and_deduplicates_renames() {
    let f = Fixture::new();
    let mut e = f.engine();
    let d = replay(b"identical");
    let first = e.archive_replay(&d, "one.replay", "one", "1", 1.0).unwrap();
    let second = e.archive_replay(&d, "two.replay", "two", "2", 2.0).unwrap();
    assert_eq!(first, second);
    assert_eq!(e.state.summary().unwrap().counts["queued"], 1);
    assert_eq!(
        fs::read(e.archive.join(format!("{first}.replay"))).unwrap(),
        d
    );
    drop(e);
    assert_eq!(f.engine().state.summary().unwrap().counts["queued"], 1);
}
#[test]
fn archive_repair_preserves_hash_and_rejects_invalid_input() {
    let f = Fixture::new();
    let mut e = f.engine();
    let d = replay(b"repair");
    let hash = e.archive_replay(&d, "one", "src", "sig", 1.0).unwrap();
    fs::write(e.archive.join(format!("{hash}.replay")), b"corrupt").unwrap();
    e.archive_replay(&d, "two", "src2", "sig2", 2.0).unwrap();
    assert_eq!(
        fs::read(e.archive.join(format!("{hash}.replay"))).unwrap(),
        d
    );
    assert!(e.archive_replay(b"bad", "bad", "bad", "bad", 3.0).is_err());
}
#[test]
fn archive_folder_cannot_be_used_as_source_via_dotdot() {
    let f = Fixture::new();
    let mut s = f.settings();
    s.replay_dir = f
        .data
        .join("archive")
        .join("..")
        .join("archive")
        .to_string_lossy()
        .into();
    assert!(Engine::new(&f.data, s, None).is_err());
}
#[test]
fn interrupted_upload_recovers_without_resetting_quota_or_privacy() {
    let f = Fixture::new();
    let mut e = f.engine();
    e.settings.visibility = "unlisted".into();
    e.archive_replay(&replay(b"recover"), "one", "src", "sig", 1.0)
        .unwrap();
    let mut j = e.state.due(2.0).unwrap().unwrap();
    j.status = "uploading".into();
    e.state.update(&j).unwrap();
    e.state.set("cooldown", 12345).unwrap();
    drop(e);
    let s = State::open(&f.data).unwrap();
    let j = s.due(3.0).unwrap().unwrap();
    assert_eq!(j.status, "queued");
    assert_eq!(j.visibility, "unlisted");
    assert_eq!(s.number("cooldown").unwrap(), 12345.0);
}
struct FakeUpload {
    status: u16,
    id: Option<&'static str>,
    retry: Option<&'static str>,
}
impl Upload for FakeUpload {
    fn upload(
        &self,
        data: Vec<u8>,
        visibility: &str,
        guard: &protocol::Guard,
    ) -> rl_uploader::Result<UploadResponse> {
        protocol::check(guard)?;
        assert!(engine::complete_replay(&data));
        assert_eq!(visibility, "private");
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(r) = self.retry {
            headers.insert("retry-after", r.parse().unwrap());
        }
        Ok(UploadResponse {
            status: self.status,
            headers,
            value: json!({"id":self.id}),
        })
    }
}
fn queued(
    f: &Fixture,
    status: u16,
    id: Option<&'static str>,
    retry: Option<&'static str>,
) -> Engine {
    let mut e = f.engine();
    e.client = Some(Box::new(FakeUpload { status, id, retry }));
    e.archive_replay(&replay(b"upload"), "one", "src", "sig", 1.0)
        .unwrap();
    e
}
#[test]
fn gameplay_interruptions_keep_upload_queued_without_consuming_retry_attempts() {
    use std::sync::atomic::AtomicBool;
    struct InterruptedUpload(Arc<AtomicBool>);
    impl Upload for InterruptedUpload {
        fn upload(
            &self,
            _: Vec<u8>,
            _: &str,
            _: &protocol::Guard,
        ) -> rl_uploader::Result<UploadResponse> {
            self.0.store(false, Ordering::SeqCst);
            Err(rl_uploader::Error::new(
                "Synthetic game launch during transfer",
            ))
        }
    }
    let f = Fixture::new();
    let mut e = queued(&f, 201, Some("saved"), None);
    let allowed = Arc::new(AtomicBool::new(false));
    let active = allowed.clone();
    let guard: protocol::Guard = Arc::new(move || active.load(Ordering::SeqCst));
    assert!(!e.upload_one(2.0, &guard).unwrap());
    assert_eq!(e.state.due(2.0).unwrap().unwrap().attempts, 0);
    allowed.store(true, Ordering::SeqCst);
    e.client = Some(Box::new(InterruptedUpload(allowed.clone())));
    assert!(e.upload_one(2.0, &guard).unwrap());
    let job = e.state.due(2.0).unwrap().unwrap();
    assert_eq!(job.status, "queued");
    assert_eq!(job.attempts, 0);
    assert_eq!(e.state.number("cooldown").unwrap(), 0.0);
    allowed.store(true, Ordering::SeqCst);
    e.client = Some(Box::new(FakeUpload {
        status: 201,
        id: Some("saved"),
        retry: None,
    }));
    assert!(e.upload_one(2.0, &guard).unwrap());
    assert_eq!(e.state.summary().unwrap().counts["uploaded"], 1);
}

#[test]
fn upload_guard_blocks_new_requests_and_interrupts_a_streaming_body() {
    use std::sync::atomic::AtomicBool;
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    server.set_nonblocking(true).unwrap();
    let client = api::Ballchasing::with_root(
        zeroize::Zeroizing::new("synthetic-test-token".into()),
        &format!("http://{}", server.local_addr().unwrap()),
    )
    .unwrap();
    assert!(
        client
            .upload(vec![42; 1024], "private", &upload_guard(false))
            .is_err()
    );
    assert_eq!(
        server.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    server.set_nonblocking(false).unwrap();
    let allowed = Arc::new(AtomicBool::new(true));
    let active = allowed.clone();
    let guard: protocol::Guard = Arc::new(move || active.load(Ordering::SeqCst));
    let peer = std::thread::spawn(move || {
        let (mut socket, _) = server.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (_, _, headers) = read_http_headers(&mut socket).unwrap();
        allowed.store(false, Ordering::SeqCst);
        let expected = headers["content-length"].parse::<usize>().unwrap();
        let mut received = Vec::new();
        let _ = socket.read_to_end(&mut received);
        assert!(
            received.len() < expected,
            "interrupted upload sent the whole replay"
        );
    });
    assert!(
        client
            .upload(vec![42; 32 * 1024 * 1024], "private", &guard)
            .is_err()
    );
    peer.join().unwrap();
}
#[test]
fn uploads_and_conflicts_are_terminal_only_with_valid_id() {
    for status in [201, 409] {
        let f = Fixture::new();
        let mut e = queued(&f, status, Some("abc-123"), None);
        assert!(e.upload_one(2.0, &upload_guard(true)).unwrap());
        let summary = e.state.summary().unwrap();
        assert_eq!(
            summary.recent[0].status,
            if status == 201 {
                "uploaded"
            } else {
                "duplicate"
            }
        );
        assert_eq!(summary.recent[0].replay_id, "abc-123");
    }
}
#[test]
fn missing_and_malformed_upload_ids_retry_instead_of_losing_replay() {
    for id in [None, Some("../../evil"), Some("")] {
        let f = Fixture::new();
        let mut e = queued(&f, 201, id, None);
        e.upload_one(2.0, &upload_guard(true)).unwrap();
        assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
        assert!(e.state.number("cooldown").unwrap() > rl_uploader::now());
    }
}
#[test]
fn auth_errors_block_uploads_but_keep_archive_and_queue() {
    for status in [401, 403] {
        let f = Fixture::new();
        let mut e = queued(&f, status, None, None);
        e.upload_one(2.0, &upload_guard(true)).unwrap();
        assert!(!e.state.get("blocked").unwrap().is_empty());
        assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
        assert!(!e.upload_one(f64::MAX, &upload_guard(true)).unwrap());
        assert_eq!(fs::read_dir(&e.archive).unwrap().count(), 1);
    }
}
#[test]
fn quota_429_uses_hour_fallback_and_preserves_queue() {
    let f = Fixture::new();
    let mut e = queued(&f, 429, None, None);
    e.upload_one(2.0, &upload_guard(true)).unwrap();
    let delay = e.state.number("cooldown").unwrap() - rl_uploader::now();
    assert!((3595.0..=3601.0).contains(&delay));
    assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
}
#[test]
fn transient_error_respects_retry_after() {
    let f = Fixture::new();
    let mut e = queued(&f, 503, None, Some("7200"));
    e.upload_one(2.0, &upload_guard(true)).unwrap();
    let d = e.state.number("cooldown").unwrap() - rl_uploader::now();
    assert!((7195.0..=7201.0).contains(&d));
}
#[test]
fn rejected_replay_stays_archived_and_can_be_retried() {
    for status in [400, 413, 415, 422] {
        let f = Fixture::new();
        let mut e = queued(&f, status, None, None);
        e.upload_one(2.0, &upload_guard(true)).unwrap();
        assert_eq!(e.state.summary().unwrap().recent[0].status, "failed");
        assert_eq!(fs::read_dir(&e.archive).unwrap().count(), 1);
        e.state.retry_failed().unwrap();
        assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
    }
}
#[test]
fn corrupted_archive_never_goes_to_service() {
    let f = Fixture::new();
    let mut e = queued(&f, 201, Some("valid"), None);
    let j = e.state.due(1.0).unwrap().unwrap();
    fs::write(e.archive.join(format!("{}.replay", j.digest)), b"corrupt").unwrap();
    e.upload_one(2.0, &upload_guard(true)).unwrap();
    assert_eq!(e.state.summary().unwrap().recent[0].status, "failed");
    assert_eq!(e.state.summary().unwrap().recent[0].attempts, 0);
}
#[test]
fn retry_after_dates_seconds_and_bounds() {
    assert_eq!(api::retry_after(Some("120"), 0.0, 5.0), 120.0);
    assert_eq!(
        api::retry_after(Some("Thu, 01 Jan 1970 00:02:00 GMT"), 60.0, 5.0),
        60.0
    );
    assert_eq!(api::retry_after(Some("-5"), 0.0, 5.0), 1.0);
    assert_eq!(api::retry_after(Some("NaN"), 0.0, 5.0), 5.0);
    assert_eq!(api::retry_after(Some("99999999"), 0.0, 5.0), 604800.0);
}
#[test]
fn cloud_url_allowlist_rejects_credentials_ports_other_buckets_and_downgrade() {
    let first = cloud::replay_url(
        "http://api.rlpp.psynet.gg/Match.replay?secret=signed",
        false,
    )
    .unwrap();
    assert_eq!(
        first.as_str(),
        "https://api.rlpp.psynet.gg/Match.replay?secret=signed"
    );
    for raw in [
        "https://evil.test/Match.replay",
        "https://user@api.rlpp.psynet.gg/Match.replay",
        "https://api.rlpp.psynet.gg:444/Match.replay",
        "https://api.rlpp.psynet.gg/other",
    ] {
        assert!(cloud::replay_url(raw, false).is_err());
    }
    let good = "https://storage.googleapis.com/psyonix-rl-replays/Prod/matches/20261031/120000/0123456789abcdef.replay?sig=x";
    assert!(cloud::replay_url(good, true).is_ok());
    for raw in [
        good.replace("https:", "http:"),
        good.replace("psyonix-rl-replays", "attacker"),
        good.replace("storage.googleapis.com", "evil.test"),
        good.replace("/Prod/", "/../"),
    ] {
        assert!(cloud::replay_url(&raw, true).is_err());
    }
}
#[test]
fn collision_safe_download_publishing_keeps_originals() {
    let f = Fixture::new();
    let id = "01234567-89ab-cdef";
    let a = replay(b"a");
    let b = replay(b"b");
    let first = cloud::save_download(&f.folder, id, &a).unwrap();
    let second = cloud::save_download(&f.folder, id, &b).unwrap();
    assert_ne!(first, second);
    assert_eq!(fs::read(first).unwrap(), a);
    assert_eq!(fs::read(&second).unwrap(), b);
    assert_eq!(cloud::save_download(&f.folder, id, &b).unwrap(), second);
    assert!(cloud::save_download(&f.folder, "../../escape", &a).is_err());
}
#[test]
fn build_signature_matches_python_protocol() {
    assert_eq!(protocol::build_id(protocol::GAME_VERSION), -1885667689);
    assert_eq!(
        protocol::sign(b"{}"),
        "fMPMoP62q5HjQXDLS6U5vH0oiWh2Y5Ji8nJDVOPJH9U="
    );
}
#[test]
fn epic_pending_namespaced_errors_are_not_terminal() {
    for code in ["authorization_pending", "slow_down"] {
        assert_eq!(protocol::machine_error(&json!({"error":code})), code);
        assert_eq!(
            protocol::machine_error(
                &json!({"errorCode":format!("errors.com.epicgames.account.oauth.{code}")})
            ),
            code
        );
    }
    assert_eq!(
        protocol::machine_error(&json!({"error":"access-token-secret"})),
        ""
    );
}
#[test]
fn match_parser_rejects_invalid_structure_and_responses_over_limit() {
    assert!(protocol::parse_matches("{}").is_err());
    assert!(
        protocol::parse_matches(r#"{"Error":{"message":"secret"}}"#)
            .unwrap_err()
            .permanent
    );
    assert_eq!(
        protocol::parse_matches(r#"{"Result":{"Matches":[]}}"#)
            .unwrap()
            .len(),
        0
    );
    assert!(
        protocol::parse_matches(&json!({"Result":{"Matches":vec![json!({});101]}}).to_string())
            .is_err()
    );
}
#[cfg(windows)]
#[test]
fn dpapi_roundtrip_and_token_storage_remain_compatible() {
    let f = Fixture::new();
    let secret = b"synthetic-test-token-only";
    let encrypted = platform::dpapi(secret, true).unwrap();
    assert_ne!(encrypted.as_slice(), secret);
    assert_eq!(
        platform::dpapi(&encrypted, false).unwrap().as_slice(),
        secret
    );
    platform::save_token(&f.data, "synthetic-test-token-only").unwrap();
    assert_eq!(
        platform::load_token(&f.data).unwrap().as_str(),
        "synthetic-test-token-only"
    );
    assert!(
        !fs::read(f.data.join("token.dpapi"))
            .unwrap()
            .windows(secret.len())
            .any(|v| v == secret)
    );
}
#[test]
fn instance_lock_excludes_second_process_owner() {
    let f = Fixture::new();
    let lock = platform::InstanceLock::acquire(&f.data).unwrap();
    assert!(platform::InstanceLock::acquire(&f.data).is_err());
    drop(lock);
    assert!(platform::InstanceLock::acquire(&f.data).is_ok());
}
#[test]
fn http_upload_uses_multipart_and_never_follows_auth_redirects() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = received.clone();
    let t = std::thread::spawn(move || {
        let (mut socket, _) = server.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (raw, first, h) = read_http_headers(&mut socket).unwrap();
        assert!(first.starts_with("POST /v2/upload?visibility=private"));
        assert_eq!(h["authorization"], "synthetic-test-token");
        let n = h["content-length"].parse::<usize>().unwrap();
        let mut body = vec![0; n];
        socket.read_exact(&mut body).unwrap();
        let mut all = raw;
        all.extend(body);
        *captured.lock().unwrap() = all;
        write!(socket,"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/stolen\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
    });
    let client = api::Ballchasing::with_root(
        zeroize::Zeroizing::new("synthetic-test-token".into()),
        &format!("http://{addr}"),
    )
    .unwrap();
    let response = client
        .upload(replay(b"wire"), "private", &upload_guard(true))
        .unwrap();
    assert_eq!(response.status, 302);
    t.join().unwrap();
    let bytes = received.lock().unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("name=\"file\"; filename=\"replay.replay\""));
    assert!(bytes.windows(4).any(|s| s == b"wire"));
}

// Test-only HTTP framing; the application only makes outgoing requests.
fn read_http_headers(
    reader: &mut impl Read,
) -> std::io::Result<(Vec<u8>, String, std::collections::HashMap<String, String>)> {
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        if raw.len() > 65536 {
            return Err(std::io::Error::other("headers too large"));
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        raw.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&raw);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap().to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_owned()))
        .collect();
    Ok((raw, first, headers))
}

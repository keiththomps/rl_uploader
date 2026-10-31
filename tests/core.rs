use rl_uploader::{
    api::{self, Upload, UploadResponse},
    cloud,
    config::Settings,
    engine::{self, Engine},
    platform, protocol, proxy, proxy_certs,
    proxy_wire::{self, Broker, RelayFrames, Session},
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
    root: tempfile::TempDir,
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
        Self { root, folder, data }
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
    assert_eq!(s.capture_mode, "after_session");
    assert_eq!(s.capture_interval_minutes, 15);
    assert_eq!(s.visibility, "unlisted");
    assert!(s.cloud_capture);
    s.save(&f.data).unwrap();
    assert_eq!(Settings::load(&f.data).unwrap(), s);
}
#[test]
fn settings_reject_invalid_intervals_modes_and_boolean_interval() {
    let f = Fixture::new();
    for bad in [json!(true), json!(0), json!(20), json!("15")] {
        let v = json!({"replay_dir":f.folder,"capture_interval_minutes":bad});
        fs::write(f.data.join("settings.json"), v.to_string()).unwrap();
        assert!(Settings::load(&f.data).is_err());
    }
    let mut s = f.settings();
    s.capture_mode = "something".into();
    assert!(s.validate().is_err());
    s = f.settings();
    s.visibility = "friends".into();
    assert!(s.validate().is_err());
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
    fn upload(&self, data: Vec<u8>, visibility: &str) -> rl_uploader::Result<UploadResponse> {
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
fn uploads_and_conflicts_are_terminal_only_with_valid_id() {
    for status in [201, 409] {
        let f = Fixture::new();
        let mut e = queued(&f, status, Some("abc-123"), None);
        assert!(e.upload_one(2.0).unwrap());
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
        e.upload_one(2.0).unwrap();
        assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
        assert!(e.state.number("cooldown").unwrap() > rl_uploader::now());
    }
}
#[test]
fn auth_errors_block_uploads_but_keep_archive_and_queue() {
    for status in [401, 403] {
        let f = Fixture::new();
        let mut e = queued(&f, status, None, None);
        e.upload_one(2.0).unwrap();
        assert!(!e.state.get("blocked").unwrap().is_empty());
        assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
        assert!(!e.upload_one(f64::MAX).unwrap());
        assert_eq!(fs::read_dir(&e.archive).unwrap().count(), 1);
    }
}
#[test]
fn quota_429_uses_hour_fallback_and_preserves_queue() {
    let f = Fixture::new();
    let mut e = queued(&f, 429, None, None);
    e.upload_one(2.0).unwrap();
    let delay = e.state.number("cooldown").unwrap() - rl_uploader::now();
    assert!((3595.0..=3601.0).contains(&delay));
    assert_eq!(e.state.summary().unwrap().recent[0].status, "queued");
}
#[test]
fn transient_error_respects_retry_after() {
    let f = Fixture::new();
    let mut e = queued(&f, 503, None, Some("7200"));
    e.upload_one(2.0).unwrap();
    let d = e.state.number("cooldown").unwrap() - rl_uploader::now();
    assert!((7195.0..=7201.0).contains(&d));
}
#[test]
fn rejected_replay_stays_archived_and_can_be_retried() {
    for status in [400, 413, 415, 422] {
        let f = Fixture::new();
        let mut e = queued(&f, status, None, None);
        e.upload_one(2.0).unwrap();
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
    e.upload_one(2.0).unwrap();
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
fn frame(raw: Vec<u8>, masked: bool) -> proxy_wire::Frame {
    let mut b = raw;
    let f = proxy_wire::next_frame(&mut b, masked).unwrap().unwrap();
    assert!(b.is_empty());
    f
}
#[test]
fn websocket_raw_frames_roundtrip_all_length_encodings_and_masks() {
    for size in [0, 1, 125, 126, 65535, 65536] {
        for mask in [true, false] {
            let data = vec![0xa5; size];
            let raw = proxy_wire::encode_frame(&data, 2, true, mask);
            let f = frame(raw.clone(), mask);
            assert_eq!(f.raw, raw);
            assert_eq!(f.payload, data);
        }
    }
}
#[test]
fn websocket_partial_frames_wait_and_invalid_frames_fail() {
    let raw = proxy_wire::encode_frame(b"hello", 1, true, true);
    for n in 0..raw.len() {
        assert!(
            proxy_wire::next_frame(&mut raw[..n].to_vec(), true)
                .unwrap()
                .is_none()
        );
    }
    for mut bad in [
        vec![0xc1, 0],
        vec![0x89, 126, 0, 126],
        vec![0x81, 126, 0, 1],
        vec![0x83, 0],
        vec![0x81, 127, 127, 255, 255, 255, 255, 255, 255, 255],
    ] {
        assert!(proxy_wire::next_frame(&mut bad, false).is_err());
    }
    assert!(proxy_wire::next_frame(&mut raw.clone(), false).is_err());
}
#[test]
fn relay_preserves_invalid_utf8_and_fragmentation_with_control_frames() {
    let (s, _) = Session::new();
    let mut relay = RelayFrames::default();
    let first = proxy_wire::encode_frame(&[255], 1, false, true);
    assert_eq!(relay.from_game(frame(first.clone(), true)).unwrap(), first);
    assert!(!relay.can_inject());
    let ping = proxy_wire::encode_frame(b"ping", 9, true, true);
    assert_eq!(relay.from_game(frame(ping.clone(), true)).unwrap(), ping);
    assert!(!relay.can_inject());
    let end = proxy_wire::encode_frame(b"finish", 0, true, true);
    assert_eq!(relay.from_game(frame(end.clone(), true)).unwrap(), end);
    assert!(relay.can_inject());
    let a = proxy_wire::encode_frame(b"game", 1, false, false);
    let b = proxy_wire::encode_frame(b"data", 0, true, false);
    assert!(
        relay
            .from_server(frame(a.clone(), false), &s)
            .unwrap()
            .is_none()
    );
    let ping = proxy_wire::encode_frame(b"ping", 9, true, false);
    assert_eq!(
        relay
            .from_server(frame(ping.clone(), false), &s)
            .unwrap()
            .unwrap(),
        ping
    );
    let mut both = a;
    both.extend(b.clone());
    assert_eq!(
        relay.from_server(frame(b, false), &s).unwrap().unwrap(),
        both
    );
}
#[test]
fn own_history_reply_is_consumed_and_game_reply_is_preserved() {
    let (s, rx) = Session::new();
    let session = s.clone();
    let worker = std::thread::spawn(move || {
        session.history(
            "Steam|12345678901234567|0",
            &(Arc::new(|| true) as protocol::Guard),
            Duration::from_secs(2),
        )
    });
    let req = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let decoded = frame(req.payload, true);
    let text = String::from_utf8(decoded.payload).unwrap();
    assert!(text.contains("Matches/GetMatchHistory"));
    assert!(!text.contains("AuthPlayer"));
    let body = format!(
        "PsyResponseID: {}\r\n\r\n{{\"Result\":{{\"Matches\":[]}}}}",
        req.id
    );
    let mut relay = RelayFrames::default();
    assert!(
        relay
            .from_server(
                frame(
                    proxy_wire::encode_frame(body.as_bytes(), 1, true, false),
                    false
                ),
                &s
            )
            .unwrap()
            .is_none()
    );
    assert!(worker.join().unwrap().unwrap().is_empty());
    let game = proxy_wire::encode_frame(b"PsyResponseID: game\r\n\r\n{}", 1, true, false);
    assert_eq!(
        relay
            .from_server(frame(game.clone(), false), &s)
            .unwrap()
            .unwrap(),
        game
    );
}
#[test]
fn timed_out_live_requests_keep_session_alive_and_hide_late_replies() {
    let (s, rx) = Session::new();
    assert!(
        s.history(
            "Epic|123|0",
            &(Arc::new(|| true) as protocol::Guard),
            Duration::from_millis(10)
        )
        .is_err()
    );
    let req = rx.recv().unwrap();
    assert!(!req.active.load(Ordering::Relaxed));
    assert!(s.alive.load(Ordering::Relaxed));
    assert!(s.consume(format!("PsyResponseID: {}\r\n\r\n{{}}", req.id).as_bytes()));
}
#[test]
fn session_disconnect_cancels_waiting_requests() {
    let (s, rx) = Session::new();
    let worker = s.clone();
    let t = std::thread::spawn(move || {
        worker.history(
            "player",
            &(Arc::new(|| true) as protocol::Guard),
            Duration::from_secs(5),
        )
    });
    rx.recv_timeout(Duration::from_secs(1)).unwrap();
    s.close();
    assert!(t.join().unwrap().is_err());
}
#[test]
fn stale_session_detach_does_not_clear_replacement() {
    let b = Broker::default();
    let (a, _) = Session::new();
    let (c, _) = Session::new();
    b.attach(a.clone());
    b.attach(c.clone());
    b.detach(&a);
    assert!(Arc::ptr_eq(&b.session().unwrap(), &c));
    c.close();
    assert!(b.session().is_none());
}
#[test]
fn game_identity_requires_primary_logged_in_account() {
    let good = "HandleLocalPlayerLoginStatusChanged PlayerID=Steam|12345678901234567|0 LoginStatus=LS_LoggedIn IsPrimary=True";
    assert_eq!(
        proxy::parse_player_id(good).as_deref(),
        Some("Steam|12345678901234567|0")
    );
    assert!(proxy::parse_player_id(&good.replace("True", "False")).is_none());
    assert!(proxy::parse_player_id(&good.replace("LS_LoggedIn", "LS_NotLoggedIn")).is_none());
}
#[test]
fn websocket_handshake_is_bound_to_game_host_and_path() {
    let raw=b"GET /ws/gc2 HTTP/1.1\r\nHost: ws.rlpp.psynet.gg\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nPsyToken: fake\r\nPsySessionID: fake\r\n\r\n";
    let (_, first, mut h) = proxy::read_headers(&mut std::io::Cursor::new(raw)).unwrap();
    proxy::validate_handshake(&first, &h).unwrap();
    assert!(proxy::validate_handshake("GET /evil HTTP/1.1", &h).is_err());
    h.insert("host".into(), "evil.test".into());
    assert!(proxy::validate_handshake(&first, &h).is_err());
    assert!(
        proxy::read_headers(&mut std::io::Cursor::new(
            b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n"
        ))
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
#[cfg(windows)]
#[test]
fn certificates_are_constrained_expiring_and_ca_key_is_never_saved() {
    let f = Fixture::new();
    let m = proxy_certs::prepare(&f.data).unwrap();
    assert_eq!(m["host"], protocol::HOST);
    assert!(!proxy_certs::trusted(&f.data));
    let der = fs::read(f.data.join("proxy/root.cer")).unwrap();
    let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
    let constraints = cert.name_constraints().unwrap().unwrap();
    assert!(constraints.critical);
    assert_eq!(
        cert.validity().not_after.timestamp() - cert.validity().not_before.timestamp(),
        30 * 86400 + 300
    );
    assert_eq!(
        cert.basic_constraints()
            .unwrap()
            .unwrap()
            .value
            .path_len_constraint,
        Some(0)
    );
    assert_eq!(fs::read_dir(f.data.join("proxy")).unwrap().count(), 5);
    proxy_certs::server_config(&f.data).unwrap();
    assert_eq!(proxy_certs::prepare(&f.data).unwrap(), m);
    assert!(!f.root.path().join("ca-key.pem").exists());
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
        let (raw, first, h) = proxy::read_headers(&mut socket).unwrap();
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
    let response = client.upload(replay(b"wire"), "private").unwrap();
    assert_eq!(response.status, 302);
    t.join().unwrap();
    let bytes = received.lock().unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("name=\"file\"; filename=\"replay.replay\""));
    assert!(bytes.windows(4).any(|s| s == b"wire"));
}

use crate::{
    Error, Result, api,
    engine::{Engine, MAX_REPLAY, complete_replay, digest, read_replay, signature},
    io_error, net_error, platform,
    protocol::{self, EpicAuth, Guard, check},
};
use regex::Regex;
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub fn valid_match_id(id: &str) -> bool {
    (16..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}
pub fn replay_url(raw: &str, storage: bool) -> Result<url::Url> {
    let mut url =
        url::Url::parse(raw).map_err(|_| Error::new("Invalid replay download address."))?;
    let valid = url.username().is_empty()
        && url.password().is_none()
        && if storage {
            url.scheme() == "https" && url.host_str() == Some("storage.googleapis.com") && url.port_or_known_default() == Some(443) && Regex::new(r"^/psyonix-rl-replays/Prod/matches/[0-9]{8}/[0-9]{6}/[a-fA-F0-9-]{16,64}\.replay$").unwrap().is_match(url.path())
        } else {
            ["http", "https"].contains(&url.scheme())
                && url.host_str() == Some("api.rlpp.psynet.gg")
                && url.path() == "/Match.replay"
                && [Some(80), Some(443)].contains(&url.port_or_known_default())
        };
    if !valid {
        return Err(Error::new(
            "Unexpected replay download destination. Download deferred.",
        ));
    }
    url.set_scheme("https")
        .map_err(|_| Error::new("Invalid download scheme."))?;
    let _ = url.set_port(None);
    url.set_fragment(None);
    Ok(url)
}
pub fn download(raw: &str, guard: &Guard) -> Result<Vec<u8>> {
    let first = replay_url(raw, false)?;
    check(guard)?;
    let client = api::client(20)?;
    let mut response = client.get(first).send().map_err(net_error)?;
    if [301, 302, 303, 307, 308].contains(&response.status().as_u16()) {
        let target = replay_url(
            response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            true,
        )?;
        check(guard)?;
        response = client.get(target).send().map_err(net_error)?;
    }
    if response.status().as_u16() != 200 {
        return Err(Error::new(format!(
            "Replay download returned HTTP {}. Will retry.",
            response.status().as_u16()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_REPLAY as u64)
    {
        return Err(Error::new("Replay exceeds the 64 MiB limit."));
    }
    let mut data = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        check(guard)?;
        let n = response.read(&mut chunk).map_err(net_error)?;
        if n == 0 {
            break;
        }
        if data.len() + n > MAX_REPLAY {
            return Err(Error::new("Replay exceeds the 64 MiB limit."));
        }
        data.extend_from_slice(&chunk[..n]);
    }
    if !complete_replay(&data) {
        return Err(Error::new(
            "Downloaded replay is incomplete or unavailable. Will retry.",
        ));
    }
    Ok(data)
}
pub fn save_download(folder: &Path, id: &str, data: &[u8]) -> Result<PathBuf> {
    if !valid_match_id(id) || data.len() > MAX_REPLAY || !complete_replay(data) {
        return Err(Error::new("Refusing to save an invalid replay."));
    }
    let hash = digest(data);
    for target in [
        folder.join(format!("{id}.replay")),
        folder.join(format!("cloud_{id}_{}.replay", &hash[..12])),
    ] {
        if target.exists() {
            if read_replay(&target)
                .map(|v| digest(&v) == hash)
                .unwrap_or(false)
            {
                return Ok(target);
            }
            continue;
        }
        let mut temp = tempfile::NamedTempFile::new_in(folder).map_err(io_error)?;
        temp.write_all(data).map_err(io_error)?;
        temp.as_file().sync_all().map_err(io_error)?;
        match temp.persist_noclobber(&target) {
            Ok(_) => return Ok(target),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_replay(&target)
                    .map(|v| digest(&v) == hash)
                    .unwrap_or(false)
                {
                    return Ok(target);
                }
            }
            Err(e) => return Err(io_error(e)),
        }
    }
    Err(Error::new(
        "A different replay already uses this filename. Original kept.",
    ))
}
pub trait History: Send + Sync {
    fn history(&self, guard: &Guard) -> Result<(String, Vec<Value>)>;
}
impl History for EpicAuth {
    fn history(&self, guard: &Guard) -> Result<(String, Vec<Value>)> {
        protocol::offline_history(self, guard)
    }
}
pub struct CloudCapture {
    pub stop: Arc<AtomicBool>,
    pub auth: EpicAuth,
    pub live: Option<Arc<dyn History>>,
    pub message: String,
    pub running: Arc<dyn Fn() -> Option<bool> + Send + Sync>,
    pub offline: Arc<dyn History>,
    pub downloader: fn(&str, &Guard) -> Result<Vec<u8>>,
    idle_since: Option<f64>,
    was_running: Option<bool>,
}
impl CloudCapture {
    pub fn new(dir: &Path, stop: Arc<AtomicBool>, live: Option<Arc<dyn History>>) -> Self {
        let auth = EpicAuth::new(dir);
        Self {
            offline: Arc::new(auth.clone()),
            auth,
            stop,
            live,
            message: "Automatic capture is off.".into(),
            running: Arc::new(platform::game_running),
            downloader: download,
            idle_since: None,
            was_running: None,
        }
    }
    pub fn reconnect(engine: &Engine) -> Result<()> {
        for prefix in ["cloud", "live"] {
            engine.state.set(&format!("{prefix}_blocked"), "")?;
            engine.state.set(&format!("{prefix}_next"), 0)?;
            engine.state.set(&format!("{prefix}_attempts"), 0)?;
        }
        Ok(())
    }
    pub fn tick(&mut self, engine: &mut Engine, now: f64) -> Result<()> {
        if !engine.settings.cloud_capture {
            self.message = "Automatic capture is off.".into();
            return Ok(());
        }
        let Some(running) = (self.running)() else {
            self.idle_since = None;
            self.message = "Cannot verify Rocket League's state. Capture deferred.".into();
            return Ok(());
        };
        let (client, prefix) = if running {
            self.idle_since = None;
            if self.was_running != Some(true) {
                engine.state.set("cloud_next", 0)?;
                engine.state.set("live_next", 0)?;
            }
            self.was_running = Some(true);
            if engine.settings.capture_mode == "after_session" {
                self.message = "Waiting for Rocket League to close.".into();
                return Ok(());
            }
            let Some(live) = self.live.as_ref() else {
                self.message = "Periodic capture needs a captured game session. No second login will be created.".into();
                return Ok(());
            };
            (live.clone(), "live")
        } else {
            if self.was_running == Some(true) {
                engine.state.set("cloud_next", 0)?;
            }
            self.was_running = Some(false);
            if !self.auth.connected() {
                self.message = "Connect Epic to enable automatic replay saving after play.".into();
                return Ok(());
            }
            if now - *self.idle_since.get_or_insert(now) < 30.0 {
                self.message =
                    "Rocket League is closed. Waiting 30 seconds before cloud capture.".into();
                return Ok(());
            }
            (self.offline.clone(), "cloud")
        };
        let blocked = engine.state.get(&format!("{prefix}_blocked"))?;
        if !blocked.is_empty() {
            self.message = blocked;
            return Ok(());
        }
        if now < engine.state.number(&format!("{prefix}_next"))? {
            self.message = engine.state.get(&format!("{prefix}_message"))?;
            return Ok(());
        }
        let stop = self.stop.clone();
        let process = self.running.clone();
        let guard: Guard =
            Arc::new(move || !stop.load(Ordering::Relaxed) && process() == Some(running));
        let pass = self.capture(engine, client.as_ref(), &guard, now);
        match pass {
            Ok((saved, total)) => {
                self.message = format!(
                    "{} capture complete: {saved} replay(s) saved; {total} in recent history.",
                    if running { "Live session" } else { "Cloud" }
                );
                if total >= 20 {
                    self.message.push_str(
                        " History is at the 20-match limit; earlier matches may be missing.",
                    );
                }
                engine.state.set(&format!("{prefix}_attempts"), 0)?;
                engine.state.set(
                    &format!("{prefix}_next"),
                    now + f64::from(engine.settings.capture_interval_minutes) * 60.0,
                )?;
            }
            Err(e) => {
                self.message = e.message;
                if e.permanent {
                    engine
                        .state
                        .set(&format!("{prefix}_blocked"), &self.message)?;
                } else {
                    let attempts = (engine.state.number(&format!("{prefix}_attempts"))? as u32)
                        .saturating_add(1);
                    let maximum = if running {
                        engine.settings.capture_interval_minutes * 60
                    } else {
                        3600
                    };
                    let delay = (60 * 2u32.pow(attempts.saturating_sub(1).min(6))).min(maximum);
                    engine.state.set(&format!("{prefix}_attempts"), attempts)?;
                    engine
                        .state
                        .set(&format!("{prefix}_next"), now + f64::from(delay))?;
                }
            }
        }
        engine
            .state
            .set(&format!("{prefix}_message"), &self.message)
    }
    fn capture(
        &self,
        engine: &mut Engine,
        client: &dyn History,
        guard: &Guard,
        now: f64,
    ) -> Result<(usize, usize)> {
        if !engine.folder.is_dir() {
            return Err(Error::new(
                "Replay folder unavailable. Cloud capture will retry.",
            ));
        }
        check(guard)?;
        let (account, matches) = client.history(guard)?;
        let mut seen = HashSet::new();
        let mut saved = 0;
        let mut missing = 0;
        for entry in matches.iter().rev() {
            check(guard)?;
            let id = entry["Match"]["MatchGUID"]
                .as_str()
                .filter(|id| valid_match_id(id))
                .ok_or_else(|| Error::new("Invalid match identifier from Match History."))?;
            if !seen.insert(id) {
                continue;
            }
            let key = format!("cloud_saved:{account}:{id}");
            let previous = engine.state.get(&key)?;
            if previous.len() == 64
                && previous.bytes().all(|c| c.is_ascii_hexdigit())
                && engine.archive.join(format!("{previous}.replay")).exists()
            {
                continue;
            }
            let result = (self.downloader)(entry["ReplayUrl"].as_str().unwrap_or_default(), guard)
                .and_then(|data| {
                    check(guard)?;
                    let path = save_download(&engine.folder, id, &data)?;
                    Ok((data, path))
                });
            let (data, path) = match result {
                Ok(v) => v,
                Err(_) => {
                    missing += 1;
                    continue;
                }
            };
            let hash = engine.archive_replay(
                &data,
                &path.file_name().unwrap_or_default().to_string_lossy(),
                &path.to_string_lossy(),
                &signature(&path)?,
                now,
            )?;
            engine.state.set(&key, hash)?;
            saved += 1;
        }
        if missing > 0 {
            return Err(Error::new(format!(
                "Saved {saved} replay(s); {missing} unavailable or incomplete. Will retry."
            )));
        }
        Ok((saved, matches.len()))
    }
}

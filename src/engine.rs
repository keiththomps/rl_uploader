use crate::{
    Error, Result,
    api::Upload,
    config::Settings,
    io_error, platform,
    state::{Job, State},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub const MAX_REPLAY: usize = 64 * 1024 * 1024;
pub fn digest(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}
pub fn complete_replay(data: &[u8]) -> bool {
    if data.len() < 16 {
        return false;
    }
    let h = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if h == 0 || h > data.len() - 16 {
        return false;
    }
    let c = u32::from_le_bytes(data[8 + h..12 + h].try_into().unwrap()) as usize;
    c > 0 && (h as u64 + c as u64 + 16) == data.len() as u64
}
pub fn signature(path: &Path) -> Result<String> {
    let m = fs::metadata(path).map_err(io_error)?;
    let time = m
        .modified()
        .map_err(io_error)?
        .duration_since(UNIX_EPOCH)
        .map_err(io_error)?
        .as_nanos();
    Ok(format!("{}:{time}", m.len()))
}
pub fn read_replay(path: &Path) -> Result<Vec<u8>> {
    let mut f = fs::File::open(path).map_err(io_error)?;
    if f.metadata().map_err(io_error)?.len() > MAX_REPLAY as u64 {
        return Err(Error::new(
            "Replay exceeds the 64 MiB limit; original kept.",
        ));
    }
    let mut data = Vec::new();
    f.by_ref()
        .take(MAX_REPLAY as u64 + 1)
        .read_to_end(&mut data)
        .map_err(io_error)?;
    if data.len() > MAX_REPLAY {
        return Err(Error::new(
            "Replay exceeds the 64 MiB limit; original kept.",
        ));
    }
    Ok(data)
}
pub struct Engine {
    pub data_dir: PathBuf,
    pub settings: Settings,
    pub folder: PathBuf,
    pub archive: PathBuf,
    pub state: State,
    pub client: Option<Box<dyn Upload>>,
    pub message: String,
    pending: HashMap<String, (String, f64)>,
    include_first: bool,
}
impl Engine {
    pub fn settling(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn new(dir: &Path, settings: Settings, client: Option<Box<dyn Upload>>) -> Result<Self> {
        settings.validate()?;
        let folder = settings.folder();
        let archive =
            platform::normalized_path(&std::path::absolute(dir).map_err(io_error)?.join("archive"));
        if folder.starts_with(&archive) || archive.starts_with(&folder) {
            return Err(Error::new("Replay and archive folders must be separate."));
        }
        fs::create_dir_all(&archive).map_err(io_error)?;
        Ok(Self {
            data_dir: dir.into(),
            settings,
            folder,
            archive,
            state: State::open(dir)?,
            client,
            message: "Waiting for saved replays".into(),
            pending: HashMap::new(),
            include_first: false,
        })
    }
    pub fn archive_replay(
        &mut self,
        data: &[u8],
        filename: &str,
        source: &str,
        sig: &str,
        now: f64,
    ) -> Result<String> {
        if data.len() > MAX_REPLAY || !complete_replay(data) {
            return Err(Error::new("Invalid or incomplete replay."));
        }
        let hash = digest(data);
        let target = self.archive.join(format!("{hash}.replay"));
        if read_replay(&target)
            .map(|d| digest(&d) != hash)
            .unwrap_or(true)
        {
            platform::atomic_write(&target, data)?;
        }
        self.state.enqueue(
            &Job {
                digest: hash.clone(),
                filename: filename.into(),
                size: data.len() as u64,
                created: now,
                visibility: self.settings.visibility.clone(),
                ..Default::default()
            },
            source,
            sig,
        )?;
        Ok(hash)
    }
    pub fn include_existing(&mut self) -> Result<()> {
        self.state.include_existing(&self.folder)?;
        self.include_first = true;
        Ok(())
    }
    pub fn scan(&mut self, now: f64) -> Result<usize> {
        let entries = match fs::read_dir(&self.folder) {
            Ok(v) => v,
            Err(_) => {
                self.message = "Replay folder unavailable. Check the path or OneDrive.".into();
                return Ok(0);
            }
        };
        let files: Vec<_> = entries
            .filter_map(|r| r.ok())
            .map(|r| r.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("replay"))
            })
            .collect();
        let baseline = format!("initialized:{}", self.folder.display());
        if self.state.get(&baseline)?.is_empty() {
            let initial = if self.include_first {
                vec![]
            } else {
                files
                    .iter()
                    .filter_map(|p| {
                        signature(p)
                            .ok()
                            .map(|sig| (p.to_string_lossy().into_owned(), sig))
                    })
                    .collect()
            };
            self.state.baseline(&baseline, &initial)?;
        }
        self.include_first = false;
        let live: HashSet<_> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        self.pending.retain(|k, _| live.contains(k));
        let mut saved = 0;
        self.message = "Watching for saved replays".into();
        for path in files {
            let key = path.to_string_lossy().into_owned();
            let sig = match signature(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if self.state.observed(&key, &sig)? {
                self.pending.remove(&key);
                continue;
            }
            match self.pending.get(&key) {
                Some((old, since)) if old == &sig && now - since >= 5.0 => (),
                Some((old, _)) if old == &sig => continue,
                _ => {
                    self.pending.insert(key, (sig, now));
                    continue;
                }
            }
            let data = match read_replay(&path) {
                Ok(d) => d,
                Err(e) => {
                    self.message = e.message;
                    continue;
                }
            };
            if signature(&path).ok().as_ref() != Some(&sig) {
                self.pending.remove(&key);
                continue;
            }
            if !complete_replay(&data) {
                self.message =
                    "Waiting for a complete replay; invalid files are left untouched.".into();
                continue;
            }
            match self.archive_replay(
                &data,
                &path.file_name().unwrap_or_default().to_string_lossy(),
                &key,
                &sig,
                now,
            ) {
                Ok(_) => {
                    saved += 1;
                    self.pending.remove(&key);
                }
                Err(e) => self.message = e.message,
            }
        }
        Ok(saved)
    }
    fn retry(&self, job: &mut Job, now: f64, delay: f64, message: String) -> Result<()> {
        job.status = "queued".into();
        job.next_try = now + delay;
        job.message = message;
        self.state.update(job)?;
        self.state.set("cooldown", now + delay)
    }
    pub fn upload_one(&mut self, now: f64, guard: &crate::protocol::Guard) -> Result<bool> {
        if !guard() {
            return Ok(false);
        }
        let Some(client) = self.client.as_ref() else {
            return Ok(false);
        };
        if !self.state.get("blocked")?.is_empty() || self.state.number("cooldown")? > now {
            return Ok(false);
        }
        let Some(mut job) = self.state.due(now)? else {
            return Ok(false);
        };
        let data = match read_replay(&self.archive.join(format!("{}.replay", job.digest))) {
            Ok(d) if digest(&d) == job.digest => d,
            _ => {
                job.status = "failed".into();
                job.message =
                    "Archive missing or integrity check failed. Restore it, then retry.".into();
                self.state.update(&job)?;
                return Ok(true);
            }
        };
        if !guard() {
            return Ok(false);
        }
        let previous_attempts = job.attempts;
        job.attempts = job.attempts.saturating_add(1);
        job.status = "uploading".into();
        job.message = "Uploading".into();
        self.state.update(&job)?;
        let delay = (5 * 2u32.pow(job.attempts.saturating_sub(1).min(10))).min(3600) as f64;
        let response = match client.upload(data, &job.visibility, guard) {
            Ok(v) => v,
            Err(_) if !guard() => {
                job.status = "queued".into();
                job.attempts = previous_attempts;
                job.next_try = 0.0;
                job.message =
                    "Transfer paused. Waiting for Rocket League to close or capture to resume."
                        .into();
                self.state.update(&job)?;
                return Ok(true);
            }
            Err(_) => {
                self.retry(
                    &mut job,
                    crate::now(),
                    delay,
                    "Connection failed. Will retry automatically.".into(),
                )?;
                return Ok(true);
            }
        };
        let now = crate::now();
        self.state.set("cooldown", now + 3.0)?;
        match response.status {
            201 | 409 if response.replay_id().is_some() => {
                job.status = if response.status == 409 {
                    "duplicate"
                } else {
                    "uploaded"
                }
                .into();
                job.replay_id = response.replay_id().unwrap().into();
                job.next_try = 0.0;
                job.message = if response.status == 409 {
                    "Already on ballchasing"
                } else {
                    "Uploaded"
                }
                .into();
                self.state.update(&job)?;
            }
            401 | 403 => {
                job.status = "queued".into();
                job.message = format!(
                    "Account access rejected (HTTP {}). Save a valid token to resume.",
                    response.status
                );
                self.state.set("blocked", &job.message)?;
                self.state.update(&job)?;
            }
            429 => self.retry(
                &mut job,
                now,
                response.delay(now, 3600.0),
                "Upload rate or account quota reached. Waiting to retry.".into(),
            )?,
            400 | 413 | 415 | 422 => {
                job.status = "failed".into();
                job.message = format!(
                    "Replay rejected (HTTP {}). Local copy kept.",
                    response.status
                );
                self.state.update(&job)?;
            }
            500..=599 | 408 | 425 | 201 | 409 => self.retry(
                &mut job,
                now,
                response.delay(now, delay),
                format!(
                    "Temporary/invalid API response (HTTP {}). Will retry.",
                    response.status
                ),
            )?,
            _ => {
                job.status = "queued".into();
                job.message = format!(
                    "Unexpected API response (HTTP {}). Check service, then reconnect.",
                    response.status
                );
                self.state.set("blocked", &job.message)?;
                self.state.update(&job)?;
            }
        }
        Ok(true)
    }
}

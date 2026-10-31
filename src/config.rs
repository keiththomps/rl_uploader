use crate::{Error, Result, io_error, platform};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub replay_dir: String,
    pub visibility: String,
    pub cloud_capture: bool,
    pub capture_mode: String,
    pub capture_interval_minutes: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            replay_dir: platform::documents()
                .join("My Games/Rocket League/TAGame/Demos")
                .to_string_lossy()
                .into(),
            visibility: "private".into(),
            cloud_capture: false,
            capture_mode: "after_session".into(),
            capture_interval_minutes: 15,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !Path::new(&self.replay_dir).is_absolute() {
            return Err(Error::new("Choose an absolute replay folder path."));
        }
        if !["private", "unlisted", "public"].contains(&self.visibility.as_str())
            || !["after_session", "periodic"].contains(&self.capture_mode.as_str())
            || ![15, 30].contains(&self.capture_interval_minutes)
        {
            return Err(Error::new("Invalid capture settings."));
        }
        Ok(())
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let p = dir.join("settings.json");
        if !p.exists() {
            return Ok(Self::default());
        }
        let v: Self =
            serde_json::from_slice(&std::fs::read(p).map_err(io_error)?).map_err(|_| {
                Error::new("Settings are invalid; existing settings were left untouched.")
            })?;
        v.validate()?;
        Ok(v)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        self.validate()?;
        platform::atomic_write(
            &dir.join("settings.json"),
            &serde_json::to_vec_pretty(self).map_err(io_error)?,
        )
    }
    pub fn folder(&self) -> PathBuf {
        platform::normalized_path(Path::new(&self.replay_dir))
    }
}

pub mod api;
pub mod cloud;
pub mod config;
pub mod desktop;
pub mod engine;
pub mod gui;
mod icon;
pub mod install;
pub mod ipc;
pub mod platform;
pub mod protocol;
pub mod state;
pub mod watch;
pub mod worker;

use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};
/// Only local, curated messages belong here; never attach HTTP bodies or tokens.
#[derive(Clone, Debug)]
pub struct Error {
    pub message: String,
    pub code: String,
    pub permanent: bool,
}
impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: String::new(),
            permanent: false,
        }
    }
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            permanent: true,
            ..Self::new(message)
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn io_error(_: impl fmt::Display) -> Error {
    Error::new("Local file operation failed. Check permissions and available space.")
}
pub fn db_error(_: rusqlite::Error) -> Error {
    Error::new("Replay queue could not be accessed. Close other copies and check the data folder.")
}
pub fn net_error(_: impl fmt::Display) -> Error {
    Error::new("Service connection failed. Will retry automatically.")
}

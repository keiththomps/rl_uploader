//! Installed-app discovery, optional sign-in startup, and explicit profile import.
//! Windows Installer owns installing and removing application files.
use crate::{Error, Result, io_error, platform};
use std::{
    fs,
    path::{Path, PathBuf},
};
const APP: &str = "RL Uploader";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const PRODUCT: &str = r"Software\RLUploader";

pub fn install_dir() -> PathBuf {
    platform::data_dir()
        .parent()
        .unwrap_or(Path::new("."))
        .join("Programs")
        .join("RLUploader")
}
pub fn installed_exe() -> PathBuf {
    install_dir().join("rl-uploader.exe")
}
pub fn installed() -> bool {
    installed_exe().is_file()
        && registry::get(PRODUCT, "InstallDir").is_some_and(|p| {
            platform::normalized_path(Path::new(&p)) == platform::normalized_path(&install_dir())
        })
}

#[cfg(windows)]
mod registry {
    use super::*;
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::*,
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe {
                RegCloseKey(self.0);
            }
        }
    }
    fn open(path: &str, write: bool) -> Result<Key> {
        unsafe {
            let mut key = std::ptr::null_mut();
            let status = if write {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    wide(path).as_ptr(),
                    0,
                    std::ptr::null(),
                    0,
                    KEY_SET_VALUE | KEY_QUERY_VALUE,
                    std::ptr::null(),
                    &mut key,
                    std::ptr::null_mut(),
                )
            } else {
                RegOpenKeyExW(
                    HKEY_CURRENT_USER,
                    wide(path).as_ptr(),
                    0,
                    KEY_QUERY_VALUE,
                    &mut key,
                )
            };
            if status != ERROR_SUCCESS {
                return Err(Error::new(
                    "Windows application registration is unavailable.",
                ));
            }
            Ok(Key(key))
        }
    }
    pub fn get(path: &str, name: &str) -> Option<String> {
        unsafe {
            let key = open(path, false).ok()?;
            let mut kind = 0;
            let mut len = 0;
            if RegQueryValueExW(
                key.0,
                wide(name).as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut len,
            ) != ERROR_SUCCESS
                || kind != REG_SZ
                || len > 32768
            {
                return None;
            }
            let mut value = vec![0u16; (len as usize).div_ceil(2)];
            if RegQueryValueExW(
                key.0,
                wide(name).as_ptr(),
                std::ptr::null(),
                &mut kind,
                value.as_mut_ptr() as _,
                &mut len,
            ) != ERROR_SUCCESS
            {
                return None;
            }
            Some(String::from_utf16_lossy(
                &value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())],
            ))
        }
    }
    pub fn set(path: &str, name: &str, value: &str) -> Result<()> {
        unsafe {
            let key = open(path, true)?;
            let value = wide(value);
            if RegSetValueExW(
                key.0,
                wide(name).as_ptr(),
                0,
                REG_SZ,
                value.as_ptr() as _,
                (value.len() * 2) as u32,
            ) != ERROR_SUCCESS
            {
                return Err(Error::new("Windows could not save app registration."));
            }
            Ok(())
        }
    }
    pub fn remove_value(path: &str, name: &str) -> Result<()> {
        unsafe {
            let key = open(path, true)?;
            let status = RegDeleteValueW(key.0, wide(name).as_ptr());
            if ![ERROR_SUCCESS, ERROR_FILE_NOT_FOUND].contains(&status) {
                return Err(Error::new("Windows could not remove the startup entry."));
            }
            Ok(())
        }
    }
}
#[cfg(not(windows))]
mod registry {
    use super::*;
    pub fn get(_: &str, _: &str) -> Option<String> {
        None
    }
    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        Err(Error::new("Windows required."))
    }
    pub fn remove_value(_: &str, _: &str) -> Result<()> {
        Err(Error::new("Windows required."))
    }
}
pub fn startup_command() -> Result<String> {
    let exe = installed_exe();
    if exe.to_string_lossy().contains('"') {
        return Err(Error::new("Invalid installed application path."));
    }
    let value = format!("\"{}\" --tray", exe.display());
    if value.encode_utf16().count() > 259 {
        return Err(Error::new(
            "The installation path is too long for Windows startup registration.",
        ));
    }
    Ok(value)
}
pub fn startup_enabled() -> bool {
    startup_command().ok().is_some_and(|expected| {
        registry::get(RUN, APP).is_some_and(|actual| startup_matches(&actual, &expected))
    })
}
fn startup_matches(actual: &str, expected: &str) -> bool {
    // The prototype used mixed path separators; MSI writes a native Windows path.
    actual.replace('/', "\\") == expected.replace('/', "\\")
}
pub fn set_startup(enabled: bool) -> Result<()> {
    if enabled {
        if !installed() {
            return Err(Error::new("Install RL Uploader before enabling startup."));
        }
        registry::set(RUN, APP, &startup_command()?)
    } else {
        registry::remove_value(RUN, APP)
    }
}
/// Copy into a new staging directory and publish only after the complete copy succeeds.
/// The source is locked against either app and SQLite backup includes any committed WAL.
pub fn migrate_data(source: &Path, destination: &Path) -> Result<bool> {
    if platform::normalized_path(source) == platform::normalized_path(destination) {
        return Ok(false);
    }
    if !source.join("settings.json").exists() {
        return Err(Error::new(
            "Migration source does not contain RL Uploader settings.",
        ));
    }
    if destination.exists()
        && fs::read_dir(destination)
            .map_err(io_error)?
            .next()
            .is_some()
    {
        return Ok(false);
    }
    let _source_lock = platform::InstanceLock::acquire(source)?;
    crate::config::Settings::load(source)?;
    let parent = destination
        .parent()
        .ok_or_else(|| Error::new("Invalid data destination."))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let stage = tempfile::Builder::new()
        .prefix(".rlu-migrate-")
        .tempdir_in(parent)
        .map_err(io_error)?;
    for file in ["settings.json", "token.dpapi", "epic.dpapi"] {
        copy_regular(source.join(file), stage.path().join(file))?;
    }
    for sub in ["archive"] {
        let from = source.join(sub);
        if !from.exists() {
            continue;
        }
        if fs::symlink_metadata(&from)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::new("Migration refuses linked data directories."));
        }
        fs::create_dir(stage.path().join(sub)).map_err(io_error)?;
        for entry in fs::read_dir(from).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            let text = name.to_string_lossy();
            let allowed = text
                .strip_suffix(".replay")
                .is_some_and(|v| v.len() == 64 && v.bytes().all(|c| c.is_ascii_hexdigit()));
            if allowed {
                copy_regular(entry.path(), stage.path().join(sub).join(name))?;
            }
        }
    }
    if source.join("queue.sqlite3").exists() {
        let connection = rusqlite::Connection::open_with_flags(
            source.join("queue.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(crate::db_error)?;
        connection
            .backup("main", stage.path().join("queue.sqlite3"), None)
            .map_err(crate::db_error)?;
    }
    if destination.exists() {
        fs::remove_dir(destination).map_err(io_error)?;
    } // empty only; never recursive
    fs::rename(stage.path(), destination).map_err(io_error)?;
    Ok(true)
}
fn copy_regular(source: PathBuf, destination: PathBuf) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(&source).map_err(io_error)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::new("Migration refuses linked or non-file data."));
    }
    fs::copy(source, destination).map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_recognizes_msi_and_legacy_paths_but_rejects_other_commands() {
        let expected =
            r#""C:\Users\name\AppData\Local\Programs\RLUploader\rl-uploader.exe" --tray"#;
        assert!(startup_matches(expected, expected));
        assert!(startup_matches(
            &expected.replace("Programs\\RLUploader", "Programs/RLUploader"),
            expected
        ));
        assert!(!startup_matches(
            &expected.replace("--tray", "--headless"),
            expected
        ));
        assert!(!startup_matches(
            &expected.replace("rl-uploader.exe", "other.exe"),
            expected
        ));
    }
}

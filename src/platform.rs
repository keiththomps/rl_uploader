use crate::{Error, Result, io_error};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Duration,
};
use zeroize::Zeroizing;

pub fn documents() -> PathBuf {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::Shell::SHGetFolderPathW;
        let mut out = [0u16; 32768];
        if SHGetFolderPathW(
            std::ptr::null_mut(),
            5,
            std::ptr::null_mut(),
            0,
            out.as_mut_ptr(),
        ) == 0
        {
            return PathBuf::from(String::from_utf16_lossy(
                &out[..out.iter().position(|&c| c == 0).unwrap_or(0)],
            ));
        }
    }
    PathBuf::from(
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .unwrap_or_default(),
    )
    .join("Documents")
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RLUploader")
}
pub fn normalized_path(p: &Path) -> PathBuf {
    let canonical = fs::canonicalize(p).unwrap_or_else(|_| {
        let mut clean = PathBuf::new();
        for component in p.components() {
            match component {
                std::path::Component::CurDir => (),
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other.as_os_str()),
            }
        }
        clean
    });
    let s = canonical.to_string_lossy().replace('/', "\\");
    #[cfg(windows)]
    {
        PathBuf::from(s.strip_prefix("\\\\?\\").unwrap_or(&s).to_lowercase())
    }
    #[cfg(not(windows))]
    {
        let _ = s;
        canonical
    }
}
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new("Missing parent directory."))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temp.write_all(data).map_err(io_error)?;
    temp.as_file().sync_all().map_err(io_error)?;
    temp.persist(path).map_err(io_error)?;
    Ok(())
}
pub struct InstanceLock(File);
impl InstanceLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir).map_err(io_error)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("instance.lock"))
            .map_err(io_error)?;
        // LockFileEx byte range overlaps Python's msvcrt.locking first byte.
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
            Error::new("RL Uploader is already using this data folder. Close that copy first.")
        })?;
        Ok(Self(file))
    }
}
impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

pub fn dpapi(data: &[u8], encrypt: bool) -> Result<Zeroizing<Vec<u8>>> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::LocalFree,
            Security::Cryptography::{
                CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
            },
        };
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len().try_into().map_err(io_error)?,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out: CRYPT_INTEGER_BLOB = std::mem::zeroed();
        let ok = if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
        };
        if ok == 0 {
            return Err(Error::new(
                "Windows could not protect or unlock these credentials. Reconnect this account.",
            ));
        }
        let bytes =
            Zeroizing::new(std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec());
        std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
        LocalFree(out.pbData as _);
        Ok(bytes)
    }
    #[cfg(not(windows))]
    {
        let _ = (data, encrypt);
        Err(Error::new("Credential storage requires Windows DPAPI."))
    }
}
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 4096 && token.bytes().all(|c| (33..=126).contains(&c))
}
pub fn load_token(dir: &Path) -> Result<Zeroizing<String>> {
    if let Ok(v) = std::env::var("BALLCHASING_TOKEN") {
        if valid_token(&v) {
            return Ok(Zeroizing::new(v));
        }
        return Err(Error::new("BALLCHASING_TOKEN is invalid."));
    }
    if !dir.join("token.dpapi").exists() {
        return Ok(Zeroizing::new(String::new()));
    }
    let v = dpapi(&fs::read(dir.join("token.dpapi")).map_err(io_error)?, false)?;
    let s = Zeroizing::new(String::from_utf8(v.to_vec()).map_err(io_error)?);
    if !valid_token(&s) {
        return Err(Error::new(
            "Stored ballchasing token is invalid. Reconnect.",
        ));
    }
    Ok(s)
}
pub fn save_token(dir: &Path, token: &str) -> Result<()> {
    if !valid_token(token) {
        return Err(Error::new(
            "Enter a valid ballchasing token without spaces.",
        ));
    }
    atomic_write(&dir.join("token.dpapi"), &dpapi(token.as_bytes(), true)?)
}
pub fn game_running() -> Option<bool> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
            System::Diagnostics::ToolHelp::*,
        };
        let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        if Process32FirstW(handle, &mut entry) == 0 {
            CloseHandle(handle);
            return None;
        }
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            if ["rocketleague.exe", "rocketleague_eac.exe"].contains(&name.as_str()) {
                CloseHandle(handle);
                return Some(true);
            }
            if Process32NextW(handle, &mut entry) == 0 {
                let error = windows_sys::Win32::Foundation::GetLastError();
                CloseHandle(handle);
                return if error == 18 { Some(false) } else { None };
            }
        }
    }
    #[cfg(not(windows))]
    {
        None
    }
}
pub fn process_birth() -> Result<u64> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentProcess, GetProcessTimes},
        };
        let mut times: [FILETIME; 4] = std::mem::zeroed();
        let p = times.as_mut_ptr();
        if GetProcessTimes(GetCurrentProcess(), p, p.add(1), p.add(2), p.add(3)) == 0 {
            return Err(Error::new("Cannot verify process identity."));
        }
        Ok((u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }
    #[cfg(not(windows))]
    {
        Err(Error::new("Windows required."))
    }
}
pub fn hidden_command(program: &str) -> Command {
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    c
}
/// Bound child lifetime, discard sensitive stderr at call sites. Call only with fixed commands.
pub fn command_output(mut command: Command, timeout: Duration) -> Result<Output> {
    use std::process::Stdio;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = command.spawn().map_err(io_error)?;
    // Drain pipes concurrently so a verbose failed utility cannot deadlock.
    let out = child
        .stdout
        .take()
        .ok_or_else(|| Error::new("Missing command output."))?;
    let err = child
        .stderr
        .take()
        .ok_or_else(|| Error::new("Missing command output."))?;
    let read = |mut stream: Box<dyn std::io::Read + Send>| {
        use std::io::Read;
        let mut v = Vec::new();
        let _ = (&mut *stream).take(1024 * 1024).read_to_end(&mut v);
        v
    };
    let o = std::thread::spawn(move || read(Box::new(out)));
    let e = std::thread::spawn(move || read(Box::new(err)));
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().map_err(io_error)? {
            break s;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new("System operation timed out."));
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    Ok(Output {
        status,
        stdout: o.join().unwrap_or_default(),
        stderr: e.join().unwrap_or_default(),
    })
}

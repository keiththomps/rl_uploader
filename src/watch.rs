//! Sleep in the kernel until the replay directory changes or the watcher is stopped.
use crate::worker::Command;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

pub struct Watcher {
    #[cfg(windows)]
    stop: usize,
    thread: Option<JoinHandle<()>>,
}
impl Watcher {
    pub fn start(path: &Path, tx: mpsc::Sender<Command>, pending: Arc<AtomicBool>) -> Option<Self> {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::ffi::OsStrExt;
            use windows_sys::Win32::{
                Foundation::INVALID_HANDLE_VALUE, Storage::FileSystem::*, System::Threading::*,
            };
            let name: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let changed = FindFirstChangeNotificationW(
                name.as_ptr(),
                0,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
            );
            if changed == INVALID_HANDLE_VALUE {
                return None;
            }
            let stop = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
            if stop.is_null() {
                FindCloseChangeNotification(changed);
                return None;
            }
            let (signal, notification) = (stop as usize, changed as usize);
            let thread = std::thread::spawn(move || {
                let handles = [signal as _, notification as _];
                loop {
                    let event = WaitForMultipleObjects(2, handles.as_ptr(), 0, u32::MAX);
                    if event == 0 {
                        break;
                    }
                    if event != 1 {
                        let _ = tx.send(Command::WatchLost);
                        break;
                    }
                    // Rearm before telling the worker, so changes during its scan are retained.
                    if FindNextChangeNotification(notification as _) == 0 {
                        let _ = tx.send(Command::WatchLost);
                        break;
                    }
                    if !pending.swap(true, Ordering::AcqRel)
                        && tx.send(Command::FilesChanged).is_err()
                    {
                        break;
                    }
                }
                FindCloseChangeNotification(notification as _);
            });
            // Event belongs to the handle, notification belongs to its waiting thread.
            Some(Self {
                stop: signal,
                thread: Some(thread),
            })
        }
        #[cfg(not(windows))]
        {
            let _ = (path, tx, pending);
            None
        }
    }
}
impl Drop for Watcher {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.stop as _);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.stop as _);
        }
    }
}

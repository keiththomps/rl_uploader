//! Windows notification-area host. No graphics context or UI polling in this process.
use crate::{
    Error, Result,
    engine::digest,
    platform,
    worker::{Command, Worker, WorkerHandle},
};
use std::path::{Path, PathBuf};

pub fn window_name(dir: &Path) -> String {
    format!(
        "RLUploader.Tray.{}",
        &digest(platform::normalized_path(dir).to_string_lossy().as_bytes())[..24]
    )
}
pub use crate::icon::{icon_ico, icon_rgba};
#[cfg(windows)]
mod win {
    use super::*;
    use std::{
        cell::RefCell,
        io::BufReader,
        process::{Child, Stdio},
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::*,
        System::{LibraryLoader::GetModuleHandleW, Threading::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    };
    const CALLBACK: u32 = WM_APP + 1;
    const SHOW: u32 = WM_APP + 2;
    const CHILD_CLOSED: u32 = WM_APP + 3;
    const ICON_RETRY: usize = 1;
    const CLASS: &str = "RLUploader.NativeTray";
    pub fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    fn put<const N: usize>(out: &mut [u16; N], text: &str) {
        for (to, from) in out.iter_mut().take(N - 1).zip(text.encode_utf16()) {
            *to = from;
        }
    }
    pub fn activate(dir: &Path, show: bool) -> bool {
        unsafe {
            let hwnd = FindWindowW(wide(CLASS).as_ptr(), wide(&window_name(dir)).as_ptr());
            if hwnd.is_null() {
                false
            } else {
                if show {
                    PostMessageW(hwnd, SHOW, 0, 0);
                }
                true
            }
        }
    }
    pub fn request_exit(dir: &Path) -> Result<()> {
        unsafe {
            let hwnd = FindWindowW(wide(CLASS).as_ptr(), wide(&window_name(dir)).as_ptr());
            if hwnd.is_null() {
                return Ok(());
            }
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let process = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if process.is_null() {
                return Err(Error::new("Could not wait for the running app to close."));
            }
            let mut response = 0;
            let sent =
                SendMessageTimeoutW(hwnd, WM_CLOSE, 0, 0, SMTO_ABORTIFHUNG, 5000, &mut response);
            let result = if sent == 0 || response != 1 {
                Err(Error::new(
                    "Close Rocket League and the app before installing or uninstalling.",
                ))
            } else if WaitForSingleObject(process, 60000) != WAIT_OBJECT_0 {
                Err(Error::new(
                    "RL Uploader is still finishing a request. Try again when it exits.",
                ))
            } else {
                Ok(())
            };
            CloseHandle(process);
            result
        }
    }
    struct Host {
        dir: PathBuf,
        worker: WorkerHandle,
        child: Option<Child>,
        icon: HICON,
        taskbar_created: u32,
    }
    impl Host {
        unsafe fn notice(&self, hwnd: HWND, message: &str) {
            let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
            n.cbSize = std::mem::size_of_val(&n) as u32;
            n.hWnd = hwnd;
            n.uID = 1;
            n.uFlags = NIF_INFO;
            n.dwInfoFlags = NIIF_INFO;
            put(&mut n.szInfoTitle, "RL Uploader");
            put(&mut n.szInfo, message);
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &n);
            }
        }
        unsafe fn add_icon(&self, hwnd: HWND) -> bool {
            let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
            n.cbSize = std::mem::size_of_val(&n) as u32;
            n.hWnd = hwnd;
            n.uID = 1;
            n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            n.hIcon = self.icon;
            n.uCallbackMessage = CALLBACK;
            put(&mut n.szTip, "RL Uploader — double-click to open settings");
            unsafe { Shell_NotifyIconW(NIM_ADD, &n) != 0 }
        }
        fn show(&mut self, hwnd: HWND) {
            if let Some(child) = self.child.as_mut() {
                if matches!(child.try_wait(), Ok(None)) {
                    unsafe {
                        EnumWindows(Some(foreground_child), child.id() as isize);
                    }
                    return;
                }
                self.child = None;
            }
            let spawn = (|| {
                let exe = std::env::current_exe().map_err(crate::io_error)?;
                let mut command = platform::hidden_command(&exe.to_string_lossy());
                command
                    .arg("--ui-child")
                    .arg("--data-dir")
                    .arg(&self.dir)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                let mut child = command.spawn().map_err(crate::io_error)?;
                let input = child
                    .stdout
                    .take()
                    .ok_or_else(|| Error::new("Could not open settings channel."))?;
                let output = child
                    .stdin
                    .take()
                    .ok_or_else(|| Error::new("Could not open settings channel."))?;
                let worker = self.worker.clone();
                let handle = hwnd as usize;
                let child_pid = child.id();
                std::thread::spawn(move || {
                    crate::ipc::serve(BufReader::new(input), output, worker);
                    unsafe {
                        PostMessageW(handle as _, CHILD_CLOSED, child_pid as usize, 0);
                    }
                });
                self.child = Some(child);
                Ok::<_, Error>(())
            })();
            if let Err(e) = spawn {
                unsafe {
                    self.notice(hwnd, &e.message);
                }
            }
        }
        unsafe fn menu(&mut self, hwnd: HWND) {
            let menu = unsafe { CreatePopupMenu() };
            if menu.is_null() {
                return;
            }
            let paused = self.worker.snapshot.lock().unwrap().paused;
            unsafe {
                AppendMenuW(menu, MF_STRING, 1, wide("Open settings").as_ptr());
                AppendMenuW(
                    menu,
                    MF_STRING,
                    2,
                    wide(if paused {
                        "Resume capture and uploads"
                    } else {
                        "Pause capture and uploads"
                    })
                    .as_ptr(),
                );
                AppendMenuW(
                    menu,
                    MF_STRING
                        | if crate::install::startup_enabled() {
                            MF_CHECKED
                        } else {
                            MF_UNCHECKED
                        },
                    3,
                    wide("Start with Windows").as_ptr(),
                );
                AppendMenuW(menu, MF_STRING, 4, wide("Open replay archive").as_ptr());
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
                AppendMenuW(menu, MF_STRING, 5, wide("Quit RL Uploader").as_ptr());
                let mut pos: POINT = std::mem::zeroed();
                GetCursorPos(&mut pos);
                SetForegroundWindow(hwnd);
                let selected = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                    pos.x,
                    pos.y,
                    0,
                    hwnd,
                    null(),
                );
                DestroyMenu(menu);
                PostMessageW(hwnd, WM_NULL, 0, 0);
                match selected {
                    1 => self.show(hwnd),
                    2 => self.worker.send(if paused {
                        Command::Resume
                    } else {
                        Command::Pause
                    }),
                    3 => {
                        if let Err(e) =
                            crate::install::set_startup(!crate::install::startup_enabled())
                        {
                            self.notice(hwnd, &e.message)
                        }
                    }
                    4 => {
                        let _ = webbrowser::open(&self.dir.join("archive").to_string_lossy());
                    }
                    5 => {
                        PostMessageW(hwnd, WM_CLOSE, 0, 0);
                    }
                    _ => (),
                }
            }
        }
    }
    unsafe extern "system" fn foreground_child(hwnd: HWND, pid: isize) -> i32 {
        unsafe {
            let mut owner = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == pid as u32 && IsWindowVisible(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
                SetForegroundWindow(hwnd);
                return 0;
            }
            1
        }
    }
    unsafe extern "system" fn close_child(hwnd: HWND, pid: isize) -> i32 {
        unsafe {
            let mut owner = 0;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == pid as u32 {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
            1
        }
    }
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        w: WPARAM,
        l: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_NCCREATE {
                let create = &*(l as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            if message == WM_DESTROY {
                let mut n: NOTIFYICONDATAW = std::mem::zeroed();
                n.cbSize = std::mem::size_of_val(&n) as u32;
                n.hWnd = hwnd;
                n.uID = 1;
                Shell_NotifyIconW(NIM_DELETE, &n);
                PostQuitMessage(0);
                return 0;
            }
            let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Host>;
            if pointer.is_null() {
                return DefWindowProcW(hwnd, message, w, l);
            }
            // Native menu/foreground APIs can re-enter this window procedure.
            // Never create aliased mutable references to the host on those callbacks.
            let Ok(mut host) = (*pointer).try_borrow_mut() else {
                return if message == WM_CLOSE {
                    0
                } else {
                    DefWindowProcW(hwnd, message, w, l)
                };
            };
            if message == host.taskbar_created {
                if host.add_icon(hwnd) {
                    KillTimer(hwnd, ICON_RETRY);
                } else {
                    SetTimer(hwnd, ICON_RETRY, 5000, None);
                }
                return 0;
            }
            match message {
                WM_TIMER if w == ICON_RETRY => {
                    if host.add_icon(hwnd) {
                        KillTimer(hwnd, ICON_RETRY);
                    }
                    0
                }
                SHOW => {
                    host.show(hwnd);
                    0
                }
                CALLBACK => {
                    match l as u32 {
                        WM_LBUTTONDBLCLK => host.show(hwnd),
                        WM_RBUTTONUP | WM_CONTEXTMENU => host.menu(hwnd),
                        _ => (),
                    }
                    0
                }
                CHILD_CLOSED => {
                    if host.child.as_ref().is_some_and(|c| c.id() == w as u32)
                        && let Some(mut child) = host.child.take()
                    {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                    0
                }
                WM_CLOSE => {
                    if let Some(child) = &host.child {
                        EnumWindows(Some(close_child), child.id() as isize);
                    }
                    host.worker.send(Command::Stop);
                    DestroyWindow(hwnd);
                    1
                }
                _ => DefWindowProcW(hwnd, message, w, l),
            }
        }
    }
    pub fn run(
        dir: PathBuf,
        settings: crate::config::Settings,
        tray_only: bool,
        paused: bool,
    ) -> Result<()> {
        let mut worker = Worker::start(&dir, settings, paused);
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = wide(CLASS);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            if RegisterClassW(&wc) == 0 {
                return Err(Error::new("Could not register the tray window."));
            }
            let ico = icon_ico();
            let icon = CreateIconFromResourceEx(
                ico[22..].as_ptr(),
                (ico.len() - 22) as u32,
                1,
                0x00030000,
                32,
                32,
                0,
            );
            if icon.is_null() {
                UnregisterClassW(class.as_ptr(), instance);
                return Err(Error::new("Could not create the tray icon."));
            }
            let mut host = Box::new(RefCell::new(Host {
                dir: dir.clone(),
                worker: worker.handle(),
                child: None,
                icon,
                taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            }));
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                wide(&window_name(&dir)).as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                host.as_mut() as *mut _ as _,
            );
            if hwnd.is_null() {
                DestroyIcon(icon);
                UnregisterClassW(class.as_ptr(), instance);
                return Err(Error::new("Could not create the tray host."));
            }
            if !host.borrow().add_icon(hwnd) {
                // Run-at-sign-in can race Explorer initialization. Keep capture alive
                // and retry only until the notification area accepts the icon.
                SetTimer(hwnd, ICON_RETRY, 5000, None);
            }
            if !tray_only {
                PostMessageW(hwnd, SHOW, 0, 0);
            }
            let mut message: MSG = std::mem::zeroed();
            loop {
                let result = GetMessageW(&mut message, null_mut(), 0, 0);
                if result <= 0 {
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            worker.stop();
            if let Some(mut child) = host.borrow_mut().child.take() {
                // WM_CLOSE already asked the settings child to close. Keep our
                // process alive until it releases the executable during updates.
                let _ = child.wait();
            }
            DestroyIcon(icon);
            UnregisterClassW(class.as_ptr(), instance);
        }
        Ok(())
    }
}
#[cfg(windows)]
pub use win::{activate, request_exit, run};
#[cfg(not(windows))]
pub fn activate(_: &Path, _: bool) -> bool {
    false
}
#[cfg(not(windows))]
pub fn request_exit(_: &Path) -> Result<()> {
    Err(Error::new("Windows is required."))
}
#[cfg(not(windows))]
pub fn run(_: PathBuf, _: crate::config::Settings, _: bool, _: bool) -> Result<()> {
    Err(Error::new("Windows is required."))
}

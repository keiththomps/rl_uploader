#![cfg_attr(windows, windows_subsystem = "windows")]
use rl_uploader::{Error, Result, config::Settings, platform, worker::Worker};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn main() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Console::{
                AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                SetStdHandle,
            },
        };
        let saved = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE]
            .map(|which| (which, GetStdHandle(which)));
        AttachConsole(u32::MAX);
        // AttachConsole may overwrite redirected handles. Preserve pipes used by
        // --doctor, --help and headless callers while retaining Ctrl+C support.
        for (which, handle) in saved {
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetStdHandle(which, handle);
            }
        }
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Err(e) = run() {
        eprintln!("{}", e.message);
        if !std::env::args().any(|s| {
            matches!(
                s.as_str(),
                "--headless" | "--doctor" | "--help" | "--install" | "--quit"
            )
        }) {
            rfd::MessageDialog::new()
                .set_title("RL Uploader")
                .set_description(&e.message)
                .set_level(rfd::MessageLevel::Error)
                .show();
        }
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut dir = platform::data_dir();
    let mut headless = false;
    let mut tray_only = false;
    let mut ui_child = false;
    let mut paused = false;
    let mut action = String::new();
    let mut migrate_from = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                dir = PathBuf::from(
                    args.next()
                        .ok_or_else(|| Error::new("--data-dir needs a path."))?,
                )
            }
            "--headless" => headless = true,
            "--minimized" | "--tray" => tray_only = true,
            "--ui-child" => ui_child = true,
            "--migrate-from" => {
                migrate_from =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        Error::new("--migrate-from needs a data folder.")
                    })?))
            }
            "--paused" => paused = true,
            "--doctor" => {
                if !action.is_empty() {
                    return Err(Error::new("Choose only one maintenance action."));
                }
                action = arg;
            }
            "--install" | "--uninstall" => {
                return Err(Error::new(
                    "Install or remove RL Uploader with RLUploader-Setup.msi or Windows Installed apps.",
                ));
            }
            "--quit" => {
                if !action.is_empty() {
                    return Err(Error::new("Choose only one maintenance action."));
                }
                action = arg;
            }
            "--help" | "-h" => {
                println!(
                    "RL Uploader {}\n  --data-dir PATH     Settings, credentials and archive location\n  --tray              Start in the system tray\n  --headless          Run without a window or tray icon\n  --paused            Start with saving and uploads paused\n  --migrate-from PATH Import an existing profile into an empty data folder\n  --quit              Exit the running tray app\n  --doctor            Read-only configuration diagnostics",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => return Err(Error::new("Unknown command line option. Use --help.")),
        }
    }
    dir = std::path::absolute(dir).map_err(rl_uploader::io_error)?;
    if action == "--quit" {
        return rl_uploader::desktop::request_exit(&dir);
    }
    if let Some(source) = migrate_from {
        if !action.is_empty() || ui_child {
            return Err(Error::new(
                "Import a profile separately from maintenance actions.",
            ));
        }
        rl_uploader::install::migrate_data(&source, &dir)?;
    }
    if action == "--doctor" {
        let settings = Settings::load(&dir)?;
        let epic_readable = std::fs::read(dir.join("epic.dpapi"))
            .ok()
            .and_then(|v| platform::dpapi(&v, false).ok())
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .is_some_and(|v| {
                v["account_id"].as_str().is_some_and(|a| a.len() == 32)
                    && v["refresh_token"].as_str().is_some_and(|t| !t.is_empty())
            });
        let diagnostics = serde_json::json!({
            "version":env!("CARGO_PKG_VERSION"), "data_dir":dir, "replay_dir":settings.replay_dir,
            "installed":rl_uploader::install::installed(),"startup_enabled":rl_uploader::install::startup_enabled(),
            "replay_folder_exists":settings.folder().is_dir(), "game_running":platform::game_running(),
            "cloud_capture":settings.cloud_capture, "uploads_enabled":settings.uploads_enabled,
            "epic_connected":dir.join("epic.dpapi").exists(), "epic_credentials_readable":epic_readable,
            "ballchasing_token_saved":dir.join("token.dpapi").exists(),
            "ballchasing_token_readable":platform::load_token(&dir).is_ok_and(|v|!v.is_empty()),
            "game_memory_access":false, "game_file_patching":false
        });
        println!("{}", serde_json::to_string_pretty(&diagnostics).unwrap());
        return Ok(());
    }
    if ui_child {
        return rl_uploader::gui::run(dir.clone(), Settings::load(&dir)?);
    }
    if action.is_empty() && !headless && rl_uploader::desktop::activate(&dir, !tray_only) {
        return Ok(());
    }
    let _lock = match platform::InstanceLock::acquire(&dir) {
        Ok(lock) => lock,
        Err(e) => {
            if action.is_empty() && !headless && rl_uploader::desktop::activate(&dir, !tray_only) {
                return Ok(());
            }
            return Err(e);
        }
    };
    let settings = Settings::load(&dir)?;
    settings.save(&dir)?; // Normalize older profiles to the current settings schema.
    if !headless {
        return rl_uploader::desktop::run(dir, settings, tray_only, paused);
    }
    let mut worker = Worker::start(&dir, settings, paused);
    let quit = Arc::new(AtomicBool::new(false));
    let quit_signal = quit.clone();
    let cancel = worker.cancel.clone();
    ctrlc::set_handler(move || {
        cancel.store(true, Ordering::Relaxed);
        quit_signal.store(true, Ordering::Relaxed);
    })
    .map_err(rl_uploader::io_error)?;
    let mut previous = String::new();
    while !quit.load(Ordering::Relaxed) {
        let snapshot = worker.snapshot.lock().unwrap().clone();
        let status = format!("{} | {}", snapshot.cloud, snapshot.error);
        if status != previous {
            println!("{status}");
            previous = status;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    worker.stop();
    Ok(())
}

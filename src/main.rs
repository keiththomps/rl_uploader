#![cfg_attr(windows, windows_subsystem = "windows")]
use rl_uploader::{
    Error, Result, config::Settings, platform, proxy::CaptureProxy, proxy_certs, worker::Worker,
};
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
        if !std::env::args().any(|s| s == "--headless" || s == "--doctor" || s == "--help") {
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
    let mut minimized = false;
    let mut paused = false;
    let mut action = String::new();
    let mut fingerprint = String::new();
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
            "--minimized" => minimized = true,
            "--paused" => paused = true,
            "--doctor"
            | "--prepare-proxy"
            | "--install-proxy"
            | "--remove-proxy-trust"
            | "--cleanup-route" => {
                if !action.is_empty() {
                    return Err(Error::new("Choose only one maintenance action."));
                }
                action = arg;
            }
            "--fingerprint" => fingerprint = args.next().unwrap_or_default(),
            "--help" | "-h" => {
                println!(
                    "RL Uploader 0.4 (Rust)\n  --data-dir PATH     Settings, DPAPI credentials and archive location\n  --headless          Run without the desktop window\n  --minimized         Start the window minimized\n  --paused            Open without starting capture or uploads\n  --doctor            Read-only configuration diagnostics\n  --prepare-proxy     Generate local proxy certificate (does not trust it)\n  --install-proxy --fingerprint SHA1   Install the reviewed certificate\n  --remove-proxy-trust                Remove this proxy certificate\n  --cleanup-route                    Ask Windows to remove our hosts block"
                );
                return Ok(());
            }
            _ => return Err(Error::new("Unknown command line option. Use --help.")),
        }
    }
    dir = std::path::absolute(dir).map_err(rl_uploader::io_error)?;
    if action == "--doctor" {
        let settings = Settings::load(&dir)?;
        let cert = proxy_certs::manifest(&dir).ok();
        let epic_readable = std::fs::read(dir.join("epic.dpapi"))
            .ok()
            .and_then(|v| platform::dpapi(&v, false).ok())
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .is_some_and(|v| {
                v["account_id"].as_str().is_some_and(|a| a.len() == 32)
                    && v["refresh_token"].as_str().is_some_and(|t| !t.is_empty())
            });
        let diagnostics = serde_json::json!({
            "version":"0.4.0-rust", "data_dir":dir, "replay_dir":settings.replay_dir,
            "replay_folder_exists":settings.folder().is_dir(), "game_running":platform::game_running(),
            "cloud_capture":settings.cloud_capture, "capture_mode":settings.capture_mode,
            "capture_interval_minutes":settings.capture_interval_minutes,
            "epic_connected":dir.join("epic.dpapi").exists(), "epic_credentials_readable":epic_readable,
            "ballchasing_token_saved":dir.join("token.dpapi").exists(),
            "ballchasing_token_readable":platform::load_token(&dir).is_ok_and(|v|!v.is_empty()),
            "proxy_trusted":proxy_certs::trusted(&dir), "proxy_certificate":cert,
            "proxy_key_readable":proxy_certs::server_config(&dir).is_ok(),
            "game_memory_access":false, "game_file_patching":false
        });
        println!("{}", serde_json::to_string_pretty(&diagnostics).unwrap());
        return Ok(());
    }
    let _lock = platform::InstanceLock::acquire(&dir)?;
    match action.as_str() {
        "--prepare-proxy" => {
            if platform::game_running() != Some(false) {
                return Err(Error::new(
                    "Close Rocket League before preparing the proxy.",
                ));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&proxy_certs::prepare(&dir)?).unwrap()
            );
            return Ok(());
        }
        "--install-proxy" => {
            if fingerprint.is_empty() {
                return Err(Error::new(
                    "Review --prepare-proxy, then provide --fingerprint SHA1 to install that exact certificate.",
                ));
            }
            proxy_certs::install(&dir, &fingerprint)?;
            println!("Proxy certificate installed.");
            return Ok(());
        }
        "--remove-proxy-trust" => {
            proxy_certs::remove_trust(&dir)?;
            println!("Proxy certificate removed.");
            return Ok(());
        }
        "--cleanup-route" => {
            if platform::game_running() != Some(false) {
                return Err(Error::new(
                    "Close Rocket League before removing its proxy route.",
                ));
            }
            rl_uploader::proxy::elevate_script("-CleanupOnly")?;
            return Ok(());
        }
        _ => (),
    }
    let settings = Settings::load(&dir)?;
    if !headless {
        return rl_uploader::gui::run(dir, settings, minimized, paused);
    }
    let proxy = CaptureProxy::new(&dir);
    let mut worker = Worker::start(&dir, settings, proxy.clone(), paused);
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
    if !proxy.close_allowed() {
        println!("Waiting for Rocket League to close before releasing its proxy connection.");
    }
    while !proxy.close_allowed() {
        std::thread::sleep(Duration::from_millis(500));
    }
    proxy.close();
    Ok(())
}

# RL Uploader

A small Windows tray app that saves your recent Rocket League replays after the
game closes and optionally uploads them to ballchasing.

## Defender compatibility

On November 1, 2026, Microsoft Defender quarantined version 0.9.3 as
`Trojan:Win32/Bearfoos.A!ml`, despite earlier clean scans. The cause is unresolved;
a false positive has not been confirmed by Microsoft. Do not restore or allow
the flagged build or add antivirus exclusions to run it. Saving and uploading
stop when the executable is quarantined; existing replay archives and settings
are retained. No report has been submitted to Microsoft.

Version 0.9.4 replaces a duplicate executable in the installer's icon cache with
a real ICO file and synchronizes the embedded manifest with the app version.
Its executable and installer passed normal Defender scans with security
intelligence 1.463.76.0 on November 1. The installed app completed a live history
check and ran for ten minutes with real-time protection enabled, no new
detections, and a clean final scan. These packaging corrections do not prove
what caused the earlier detection or guarantee future antivirus verdicts.

## Install and configure

Run `dist\RLUploader-Setup.msi`, then open **RL Uploader** from Start. The app
installs for the current Windows user and starts in the tray at sign-in.

Open **Settings**:

1. Connect the Epic account linked to your Steam Rocket League account. The
   sign-in dialog stays open until you close it, including after a code expires.
2. Check the replay folder. Redirected Documents and OneDrive folders are supported.
3. Enable **Save recent replays automatically after Rocket League closes**.
4. Optionally enable ballchasing uploads and paste an API token. **Test connection**
   checks the token; **Apply** stores it. Choose Private, Unlisted, or Public for
   new uploads. Existing queued replays retain their original visibility.
5. Click **Apply**. Unsaved changes are marked; **Revert** restores saved settings.

**Activity** shows the saved configuration, account and folder status, current
work, and recent replays. Uploaded replays link to ballchasing. Closing the window
keeps the lightweight tray process running. Pause/resume and Quit are available
in the tray menu. Applying settings preserves a paused state.

Saving works without a ballchasing token. Turning uploads off preserves the
local archive and queue. Uploads resume when enabled with a valid token.

## After-session behavior

After Rocket League has been closed for 30 seconds, the app creates its own
account session, fetches Match History, and closes that connection. It downloads
available replays into the game's Demos folder, retains a separate archive, and
queues optional uploads. A successful check runs once per closed-game period;
another game session or an app restart allows a fresh check. Unavailable replays
and temporary failures retry with backoff.

Match History contains at most the latest **20 matches within 30 days**. A session
longer than 20 matches can exceed that window. The app also handles manually saved
replays. Existing local files are excluded initially; choose **Import existing
local replays** to include them.

Replay-file processing and uploads wait while Rocket League is running or its
process state is unknown. Upload requests and body chunks recheck that state.
Data already buffered by the network may still drain if the game starts during
a transfer. Process checks cannot eliminate the race where a game launch occurs
at the exact instant an outgoing login request starts.

The app uses an **unofficial Match History API**. Epic/Psyonix/EAC approval has not
been verified. It does not intercept game traffic, reuse the game's login,
modify game files, read game memory, or disable anti-cheat. There is no in-game
capture mode, listening server, routing helper, or certificate setup.

## Data and startup

- App: `%LOCALAPPDATA%\Programs\RLUploader`
- Settings, encrypted credentials, queue and archive: `%LOCALAPPDATA%\RLUploader`
- Replay folder: normally `Documents\My Games\Rocket League\TAGame\Demos`

Credentials use Windows DPAPI. Keep the data folder private. Downloads use
validated destinations; replay writes are atomic and do not replace a different
existing replay. The durable queue deduplicates replays, respects upload quotas,
and retains local files after upload errors.

The only startup entry is the optional tray app. Change it in Settings or the
tray menu. Windows Startup Apps can override it. Uninstall through Windows
**Installed apps**; the archive, credentials and settings are retained.

## Development

Install the Rust MSVC toolchain, Visual Studio C++ Build Tools and Windows SDK.
The project-local toolchain is used when available. `Cargo.lock` and
`rust-toolchain.toml` pin the build inputs.

```powershell
.\Build.ps1 -Check
```

This checks formatting, runs Clippy and tests, builds the release executable,
and creates the per-user MSI. Packaging uses the build-only WiX 4.0.6 tool under
`.tools\wix`. Rust, Python, WiX and .NET are not needed on end-user machines.
The development executable is unsigned; release signing requires a publisher's
code-signing certificate. Do not use antivirus exclusions.

Useful executable options:

```text
--tray                  Start quietly in the system tray
--paused                Start with capture and uploads paused
--headless              Run without a graphical window
--doctor                Read-only configuration diagnostics
--quit                  Ask the running app to exit
--data-dir PATH         Use a separate profile
--migrate-from PATH     Copy a profile into an empty data directory
```

`Start RL Uploader.cmd` opens the installed app, or the local release against
`.local` when the app is not installed. Migration preserves replay archives,
credentials, and a consistent SQLite snapshot; it never merges existing profiles.

`Measure-Background.ps1` samples the installed tray's CPU and memory. Close the
settings window before measuring; its disposable graphics process is separate.
Tests cover after-session timing, game-state guards, cancellation, archive
integrity, queue recovery, optional uploads, settings migration and local HTTP
uploads. See [implementation notes](docs/RUST_PORT.md).

# RL Uploader — Rust

A native Windows app that saves recent Rocket League replays, keeps a durable
local archive, and optionally uploads them to ballchasing. Automatic saving works
without a ballchasing token. Choose capture after play or every 15/30 minutes.

The executable includes the UI, cloud client, SQLite queue, TLS proxy and
certificate support. Python, Hebnix, and OpenSSL installations are not required.
Windows PowerShell and certutil provide OS routing and certificate integration.

**Automatic capture uses an unofficial API. Epic/Psyonix/EAC approval has not
been verified.** The app does not inject into the game, inspect game memory,
patch game files, or disable EAC. Periodic mode intercepts one game's TLS hostname
through a local proxy and requires explicit setup. It remains experimental.

## Build and start

For developers, install the [Rust MSVC toolchain](https://rust-lang.org/tools/install/)
and Visual Studio C++ Build Tools. A project-local Rust toolchain, when present
under `.tools/`, is detected automatically; it is not committed.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\Build.ps1 -Check
```

This runs formatting, static checks, tests and the release build. `Cargo.lock`
and `rust-toolchain.toml` pin the dependencies and compiler. The release statically
links the Windows C runtime.

Double-click **Start RL Uploader.cmd** to use this project's existing `.local`
data folder. The executable is `target\release\rl-uploader.exe`. When run directly
without `--data-dir`, it uses `%LOCALAPPDATA%\RLUploader` instead.

Close the Python app before launching Rust against its data folder. Settings,
encrypted credentials, archive, queue, and approved certificate are compatible.
The Python baseline is preserved on `main` at `bf37e53`; see
[port details](docs/RUST_PORT.md).

## Automatic saving after play

1. Confirm the replay folder. The app detects redirected Documents folders,
   including OneDrive; the normal suffix is `Rocket League\TAGame\Demos`.
2. Choose **Connect Epic**, open its sign-in link, and enter the displayed device
   code yourself. Use the Epic account linked to your Steam Rocket League account.
   The dialog stays open while you sign in and on errors or expiry. **Get a new
   code** starts a fresh attempt.
3. Enable automatic replay saving, select **After Rocket League closes**, and
   choose **Save & start**. Keep the app running while playing.
4. After the game closes and stays closed for 30 seconds, the app fetches recent
   Match History, closes its cloud session, saves replays into Demos, archives
   them, and queues uploads. It retries unavailable replays automatically.

Match History contains at most the latest **20 matches within 30 days**. The
first capture saves all available recent matches. Long sessions can exceed that
window before an after-session capture runs. The process check prevents planned
second logins while playing, but a game launch exactly as a login request is sent
remains a race. Let an active after-session capture finish before relaunching.
Unknown process state defers capture.

The app also watches for replays saved manually in game, waiting for five seconds
of unchanged size and modification time before archiving. Existing local files
are excluded on first watch; use **Import existing replays** to queue them.

## Periodic capture through the game's session

This mode runs every 15 or 30 minutes while the game is open, using the existing
captured WebSocket session. It never falls back to another account login during
play. When the game closes, it uses the connected Epic account after the same
30-second wait. A missing proxy session defers capture rather than logging in.

1. Close Rocket League and choose **Set up periodic capture**.
2. Review the certificate fingerprint and expiry before installing trust. The
   current-user CA is constrained to `ws.rlpp.psynet.gg` and expires in 30 days.
   Its signing key is discarded; only the leaf key is stored, protected by DPAPI.
   An existing valid, approved certificate is reused without a new trust prompt.
3. Select periodic mode and choose **Save & start**. Windows asks for administrator
   access to run the narrow hostname-routing watchdog. The app itself stays
   unelevated.
4. Wait until the proxy is ready, then launch Rocket League. If the game was
   already connected before routing started, close and relaunch it.

The proxy listens only on loopback port 443. A nonce-protected control endpoint
and public local revocation list use port 47639. The proxy verifies the real
upstream certificate, forwards game messages, and consumes only responses to its
own Match History requests. It does not persist or log game session headers,
raw frames, signed download URLs, or response bodies.

The elevated watchdog adds only its marked hosts block while the game and proxy
are ready. It removes that block on game exit or proxy failure. A fixed startup
recovery task also removes the owned block after a reboot. If both proxy and
watchdog are forcibly killed, reboot or use the cleanup command below. Other
hosts entries are preserved; conflicting or malformed mappings are rejected.

Close Rocket League before exiting RL Uploader while proxy connections are
active. The GUI blocks that exit to preserve the game connection; minimize it
instead. Pausing capture keeps the relay alive.

## Ballchasing uploads

Paste your API token from [ballchasing](https://ballchasing.com/upload) into the
masked field. It is encrypted with Windows DPAPI for your user. Choose Private,
Unlisted or Public, then **Save & start**. Private is the default. Visibility is
stored per queued replay; changing it affects new queued replays only.

Uploads are sequential with a minimum three-second gap. `Retry-After` is honored
(including date values). HTTP 429 without that header backs off for one hour;
network/server failures use increasing retries. Account quotas can be tighter
than request limits and are account-dependent. See the
[ballchasing API documentation](https://ballchasing.com/doc/_api).

Quota limits and upload failures never delete saved replays. Completed files are
validated for container framing and archived atomically as SHA-256 filenames.
Identical content is uploaded once across renames and restarts. Downloaded files
never overwrite different existing Demos files. Invalid/incomplete files stay
untouched. Uploads always read the archive; source files are never deleted.

## Commands and recovery

```powershell
# Read-only diagnostics: prints presence/validity, never tokens.
.\target\release\rl-uploader.exe --data-dir .local --doctor

# Start minimized or without the desktop window.
.\target\release\rl-uploader.exe --data-dir .local --minimized
.\target\release\rl-uploader.exe --data-dir .local --headless

# Open paused for inspection without starting capture/upload/proxy routing.
.\target\release\rl-uploader.exe --data-dir .local --paused

# Prepare local certificate files without installing trust.
.\target\release\rl-uploader.exe --data-dir .local --prepare-proxy

# Install only the exact certificate whose SHA-1 you reviewed above.
.\target\release\rl-uploader.exe --data-dir .local --install-proxy --fingerprint REVIEWED_SHA1

# Close the game and app first; remove only this certificate or owned hosts block.
.\target\release\rl-uploader.exe --data-dir .local --remove-proxy-trust
.\target\release\rl-uploader.exe --data-dir .local --cleanup-route
```

`--headless` stops capture on Ctrl+C, then waits for the game to close before
releasing an active proxy connection. `--help` lists options. Keep `.local`
backed up: it contains `settings.json`, `queue.sqlite3`, `archive/`, encrypted
account credentials and `proxy/`. Do not commit or share its contents. DPAPI
credentials cannot be decrypted by another Windows user or machine.

## Validation status

Rust regression tests and loopback TLS tests cover persistence, downloads,
upload retries, process guards, schedule selection and session relay behavior.
Read-only checks confirm compatibility with the existing Python credentials and
approved proxy key. The earlier Python capture downloaded 20 real replays.

A live Rocket League capture cycle using the Rust build, especially periodic
proxy mode, still needs validation. Protocol constants target the locally
installed October 2026 Rocket League build; future game updates may require
adapter changes. These limitations are not a claim of EAC approval.

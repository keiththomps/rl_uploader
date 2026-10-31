# Rust port

The Python baseline is commit `bf37e53` on `main`. Work on the native app lives on
`port/rust`. There are no calls to Python, Hebnix, OpenSSL executables, or another
application's credential store in the Rust runtime.

The original Python source and its tests remain on this branch as a comparison
reference until live Rust game-session validation is complete. All three app
launchers now use the Rust executable. `run.pyw` is the legacy Python entry point.

## Implementation map

| Component | Rust implementation |
| --- | --- |
| Desktop and persistent Epic sign-in dialog | `src/gui.rs`, egui/eframe |
| Worker and pause/resume | `src/worker.rs` |
| Replay discovery, validation, archive, upload retry | `src/engine.rs` |
| SQLite queue and migration compatibility | `src/state.rs` |
| Ballchasing HTTP client | `src/api.rs` |
| Epic device flow and offline Match History | `src/protocol.rs` |
| Capture schedules and replay downloads | `src/cloud.rs` |
| Existing-session TLS proxy | `src/proxy.rs`, `src/proxy_wire.rs` |
| Certificate provisioning and DPAPI leaf key | `src/proxy_certs.rs` |
| Process names, Windows Documents, locks, DPAPI | `src/platform.rs` |
| CLI / headless / diagnostics | `src/main.rs` |

`assets/proxy_route.ps1` is the original Windows routing/recovery helper, embedded
in the executable at compile time. It is invoked as fixed encoded PowerShell,
without extracting an executable script into the user's data directory. Its
startup recovery task also contains only the fixed cleanup code. This small OS
integration helper remains PowerShell; all application logic, networking, proxy
relay, credential handling, and UI are Rust.

## Compatibility contract

- `settings.json` keeps the same keys and defaults. Old settings default to
  `after_session`, 15 minutes, and private uploads.
- `queue.sqlite3` keeps the original tables, columns, and metadata keys. There is
  no destructive schema migration. Interrupted uploads return to the queue.
- `token.dpapi`, `epic.dpapi`, and `proxy/leaf-key.dpapi` use the same Windows
  current-user DPAPI format. Reconnecting accounts is not required merely to port.
- Archive filenames remain the SHA-256 digest followed by `.replay`.
- The Windows lock overlaps the Python app's first-byte lock; both implementations
  cannot own the same data directory simultaneously.
- Existing proxy certificates are reused. The port does not silently install or
  replace trusted certificates. New Rust-generated certificates use ECDSA; the
  original RSA certificate and encrypted key are also supported.
- Build tools, caches, credentials, certificates and replay data remain ignored by Git.

## Validation and remaining field checks

Rust tests cover replay framing and settlement, initial exclusion and import,
archive repair/deduplication, restart recovery, private/unlisted queue persistence,
upload auth failures, quota backoff, multipart requests and denied redirects,
download URL restrictions and collision handling, capture transport selection,
15/30-minute timing, unknown process state, guarded authentication, failed replay
isolation, WebSocket framing/fragmentation, private response consumption, late
responses, session replacement/disconnection, constrained certificates, DPAPI,
TLS hostname enforcement, large-frame backpressure, and pure hosts transformations.
Tests use synthetic tokens and loopback services. They do not install trust, change
hosts, register tasks, or authenticate to a live account.

Read-only diagnostics have verified the existing Python settings, both encrypted
account credentials, and the previously approved RSA proxy certificate/key.
The 54 Rust tests pass, along with `cargo fmt --check` and Clippy with warnings
denied. A temporary copy of the Python queue preserved all 20 uploaded jobs,
including visibility and replay IDs, when opened by Rust. Cross-process locking
was checked in both directions against Python's `msvcrt.locking`.

The release desktop was visually inspected in an isolated paused profile and
resume/close were exercised. The release executable's import table contains only
Windows system DLLs; it does not need a separate Visual C++ runtime installation.

The earlier Python after-session path downloaded 20 real replays successfully.
That result does **not** establish live compatibility of the Rust adapter or of
periodic proxy mode. A real game launch/play/exit capture cycle remains a field
test. Epic/EAC approval of this unofficial integration has not been verified.

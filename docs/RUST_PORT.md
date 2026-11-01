# Rust implementation notes

The main branch contains the maintained Rust-only app, version 0.9.4. The original
Python baseline remains in git history at `bf37e53`; it is no longer shipped or
retained as a second executable implementation.

## Runtime

The native Windows tray owns the worker, directory notification, encrypted
credentials and durable SQLite queue. It creates a separate egui/eframe settings
process only when the window is open, communicating over inherited private
pipes. Closing the window releases its graphics resources. The tray sleeps
between process-state checks and file notifications.

The cloud client uses an explicitly connected Epic account only after the game
has stayed closed for 30 seconds. Saving is independent of ballchasing uploads.
A completed history check waits for a new game session; incomplete downloads and
transient failures use backoff. The worker defers local file processing and
uploads while the game is open or its process state is unknown. Upload bodies
check a cancellation guard between chunks; network-buffered bytes can still drain.

## Removed implementation

The experimental periodic capture path has been deleted, including the local
HTTP/WebSocket relay, session-header capture, configuration-response rewriting,
certificate generation/trust commands, hosts routing, elevated helper, recovery
service, cache recovery startup command, secondary MSI, Python implementation,
and their dedicated tests and dependencies. No dead feature flag or hidden UI
remains. Old capture-mode/interval settings are ignored and removed on the next
normal settings save; active preferences and encrypted credentials are retained.
Profile migration copies only settings, credentials, archive and queue.

The protocol code that remains creates its own after-session login and reads
that account's Match History. It does not inspect game logs or game memory and
has no local listening sockets. This integration is unofficial; approval from
Epic/EAC remains unverified.

## Window UX

The window uses the Windows Segoe UI font when available, system light/dark
appearance, native window chrome, compact controls and separate Activity/Settings
pages. Configuration status uses text as well as color. Activity describes saved
settings; edits are drafts until Apply. Revert restores them, and closing with
unsaved changes offers Apply, Discard, or Cancel. Applying settings preserves
pause. Account sign-in saves immediately and its dialog stays open on expiry.

Ballchasing uploads have a separate on/off switch. The token field distinguishes
saved credentials from an unapplied replacement; testing the connection does not
save a replacement. Upload visibility applies to newly queued files. The UI does
not label a merely saved token as service-verified. Missing folders, missing
accounts and persistent service failures are surfaced separately.

## Upgrade and verification

On October 31, 2026, a real after-session cycle saved and uploaded a newly played
match about 33 seconds after Rocket League closed. It became the 21st verified
archive/upload record. No network interception was active during that match.
A packet-loss indicator reported by the user could not be attributed from the
available records; the app does not collect gameplay packet telemetry.

The 0.8.3 upload-guard regressions remain. The current suite also covers removal
of obsolete settings, once-per-session capture, interrupted transfer recovery,
DPAPI compatibility, durable archive migration and the directory watcher.
Synthetic checks do not establish Epic/EAC approval or eliminate all process
launch races. Production credentials and signed download URLs must never be
logged, committed, or copied into test fixtures.

Version 0.9.2 passed formatting, Clippy with warnings denied, and 50 tests.
The added regressions cover preserving pause when applying settings, disabled
uploads retaining their queue, and catching up after a session missed while
paused or while process state was unknown. The real Windows UI was reviewed in
an isolated paused profile and with the installed user's saved configuration.
Unsaved-change feedback, close confirmation and Apply preserving pause were
verified. A stale paused-state label was corrected during that review.

The final executable and MSI passed normal Defender scans with zero new
detections. The per-user upgrade preserved Epic and ballchasing credentials,
after-session saving, upload preference, startup and all 21 archive SHA-256s.
MSI inspection confirmed only the app, README and license notices, and one
optional tray startup entry. No recovery startup entry or nested routing MSI
remains. A 30.024-second idle tray sample measured 14.44 MiB working set,
2.71 MiB private memory and 93.75 ms CPU (0.3123% of one core).

On November 1, both retired roots were verified absent from the current user's
trusted store after the user approved their deletion. The old certificate/key
profiles and diagnostic were removed from the installed profile and the legacy
workspace profile. Account credentials remained readable; all 29 current replay
archives passed SHA-256 checks and all 29 had completed uploads. After-session
saving, uploads and sign-in startup remain enabled.

The separately installed routing component was removed through Programs and
Features on November 1. Its Windows Installer registration, service, install
directory, protocol registry key and helper process were verified absent.
Both retired certificate roots, temporary hosts routes and the recovery startup
entry are absent. The current app does not call or depend on the retired helper.

The current app was also accidentally uninstalled during that cleanup. The
verified 0.9.3 per-user MSI restored it successfully without recreating routing
components. The installed executable matches the previously Defender-scanned
build. Existing settings and both encrypted credentials remained readable;
after-session saving, uploads and sign-in startup remain enabled. All 29 replay
archives passed fresh SHA-256 checks and retained their completed upload records.

The final UI-only 0.9.3 refinement keeps each page's scroll position separate
and marks unapplied settings in the tab title. It passed formatting and Clippy;
the prior 50-test runtime suite remains unchanged. Its executable and installer
passed fresh Defender scans and the installed version was verified. After the
November 1 reinstall, the app was running in the system tray with after-session
saving and uploads enabled until the subsequent detection below.

## Defender detection on November 1

At 13:19 America/Indianapolis, Defender quarantined the installed 0.9.3 executable
as `Trojan:Win32/Bearfoos.A!ml`. Security intelligence was 1.463.72.0 and engine
1.1.26100.4. The verified executable SHA-256 before quarantine was
`3818B78E9B82BD268434DD72B637CA49EA2AA72D08B891636C01D1B51DF6F465`.
An ordinary file-hash read of the unexecuted release copy was then blocked by
real-time protection and that copy was quarantined as well. The diagnostic
command stopped before its planned on-demand scan could run.

Defender also flagged the Windows Installer AppIcon cache. The 0.9.3 MSI
uses the executable itself as its icon source, so that entry contains another
copy of the same executable; it is not evidence of another application component.
The logs do not identify a triggering code path or establish a false positive.
The earlier clean scans cannot be treated as a current compatibility guarantee.

The app stopped and automatic saving/uploading became unavailable. All 29 archive
files passed fresh SHA-256 verification and retained completed upload records;
settings and encrypted credential files remain present. Protection remains
enabled. No exclusions, allow-list entries, restoration, or replacement build
were used to bypass the detection. A developer review report was prepared locally;
the user requested that it remain local, so nothing was submitted to Microsoft.

## Packaging corrections in 0.9.4

The installer now uses a 4,286-byte ICO asset instead of caching a second copy of
the executable as its icon. Its size decreased from 17,600,512 to 4,837,376 bytes.
The shared asset is checked against the runtime icon generator in the existing
icon test. The embedded application manifest is generated from Cargo's version
instead of retaining 0.7.2.0. Packaging rejects a release executable whose version
differs from Cargo.toml.

Formatting, Clippy with warnings denied and all 50 tests passed. Inspection of
the built PE confirmed manifest version 0.9.4.0 and `asInvoker` permissions;
inspection of the MSI confirmed the ICO payload and product version 0.9.4.
Both artifacts passed normal Defender scans with remediation enabled after a
normal security-intelligence update from 1.463.72.0 to 1.463.76.0. No antivirus
configuration was weakened. These observations do not identify the cause of the
0.9.3 detection, and a clean scan is not a guarantee of future classification.

The per-user upgrade preserved the settings, DPAPI credentials and queue byte for
byte. Installed diagnostics confirmed both credentials were readable, saving and
uploads enabled, and startup registered. The live app completed a history check
30 seconds after launch, checking 20 matches with no new replays and retaining
29 completed upload records.

The same installed process then ran for 604.9 seconds with real-time protection
enabled and zero new Defender detections. A final normal scan of the installed
executable reported no threats. All 29 replay archives also passed SHA-256 checks.
At the end of observation the tray used 18.29 MiB working set and 3.84 MiB private
memory. A separate 30.017-second sample measured 78.125 ms CPU, or 0.2603% of one
core. No fresh match was played during this check, so no new upload was required.

The installed executable SHA-256 is
`150A49FB4CB0F0808B01E517579AEB5805222BF8A26185809B4E821A5EB5F643`.
The installed MSI icon cache was verified to contain the 4,286-byte ICO. A legacy
Start-menu shortcut still referred to the prototype's deleted icon file; its
icon reference was repaired to use the installed asset. The app remains running
in the tray, with automatic saving, uploads and sign-in startup enabled.

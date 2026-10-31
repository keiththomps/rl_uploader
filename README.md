# RL Uploader

A small Windows desktop app that saves recent Rocket League replays, keeps a
local archive, and uploads them to ballchasing. Choose capture after the game
closes, or experimental periodic capture through the game's existing session. It also
watches for replay files saved in game. Python 3.11+ with Tkinter is required.

**Automatic saving is experimental and opt-in.** It downloads your linked Epic
account's Match History through an unofficial API. A live test on October 31,
2026 saved all 20 available matches from the Steam-linked account into Demos and
the local archive, and successfully uploaded replays to ballchasing. This is not
an Epic/Psyonix/EAC-approved integration.

## Start

1. Double-click **Start RL Uploader.cmd**. `run.pyw` is a console-free alternative
   if Windows associates `.pyw` files with Python.
2. The app detects Windows' Documents folder, including OneDrive redirection.
   Check that the selected directory ends in `Rocket League\TAGame\Demos`.
3. Log in to [ballchasing](https://ballchasing.com/upload) and copy your upload
   token into the app's masked field. Do not paste it into chat or source code.
4. Choose **Private**, **Unlisted**, or **Public**. Private is the default.
5. Choose **Save & start**. Minimize the window while playing.
6. Set up cloud capture below for automatic saving. Files you save manually in
   Rocket League are also archived and queued once unchanged for five seconds.

### Automatic saving after play

1. Double-click **Setup cloud capture.cmd** once. It installs the pinned
   `websocket-client` dependency into this project's `.vendor/` directory.
2. Close Rocket League, then select **Connect Epic** in the app. Open the Epic
   sign-in page from the dialog and enter its device code. Sign in to the Epic
   account linked to your Steam Rocket League account. Never paste passwords or
   account tokens into chat. No Epic Games Launcher installation is required.
   The dialog shows the code's remaining lifetime (currently 10 minutes). It
   stays open on expiry or errors; **Get a new code** retries without closing it.
3. Select **Enable automatic replay saving**, choose **After game closes**, then **Save & start**.
4. Leave RL Uploader running while you play. Thirty seconds after Rocket League
   closes, it requests your recent history, closes the cloud session, downloads
   the available replays into your Demos folder, archives them, and queues uploads.
   Saving works even without a ballchasing token.

**Match History only retains the latest 20 matches within 30 days.** Longer
sessions can lose earlier matches before the app gets a chance to save them.
The first capture imports all available recent history, including matches from
before setup. That differs from the file watcher's initial local-file exclusion.
Allow capture to finish before reopening Rocket League. A second cloud login can
disrupt a game session: the app checks process names before login and during
capture, but a launch exactly as an authentication request is sent remains a race.
When game status cannot be verified, capture is deferred.

The app retries unavailable downloads and checks again every 15 minutes while
the game is closed. It remembers completed matches across restarts. An account
or protocol rejection pauses capture until you reconnect. Protocol constants
match the locally installed October 2026 build; a future update may require
changes to this experimental adapter.

### Periodic capture through the existing game session

This is a separate opt-in mode. It does not depend on Hebnix. **Live game
compatibility is not yet verified.** The relay and mode routing have automated
coverage, including byte-preserving frame forwarding and no second login.

1. Close Rocket League. Run **Setup periodic capture.cmd** once to install
   certificate support into `.vendor/`.
2. In the app, choose **Set up periodic capture**. Review the certificate and
   routing disclosure. Setup creates a 30-day certificate restricted to
   `ws.rlpp.psynet.gg`, trusts its root for your Windows user, and encrypts the
   leaf private key with DPAPI. The CA signing key is never saved.
3. Enable automatic replay saving, choose **Periodically**, select **15** or
   **30 minutes**, then **Save & start**. Approve the Windows administrator
   prompt for the routing watchdog. Start Rocket League only after the proxy
   reports ready. A game already running needs a restart to use the proxy.
4. While the game runs, only the captured WebSocket carries the Match History
   request. No extra AuthPlayer login is attempted. If the connection is missing
   or changes, capture defers. After the game exits, the normal Epic-authenticated
   path runs after 30 seconds. The two transports share the archive and queue.

Periodic mode redirects only `ws.rlpp.psynet.gg` to loopback while Rocket League
is present and the proxy is healthy. It does not intercept Epic OAuth, Steam,
or other hostnames. The upstream connection uses normal TLS verification and
the system's DNS servers, bypassing only our local hosts mapping. WebSocket
compression is not negotiated; game payloads and frame boundaries are preserved.
Our own history responses are consumed by the uploader and not sent to the game.

The local TLS listener uses port 443 (IPv4, and IPv6 if available). A loopback
health/CRL listener uses port 47639. Conflicting listeners stop setup before
routing is enabled. The hostname mapping is system-wide for this one hostname;
other programs using it can also be affected. Existing unrelated hosts entries
are preserved. Conflicting mappings are refused.

An elevated watchdog removes the mapping when Rocket League exits, the proxy
becomes unavailable, or the uploader process exits. A Windows startup task named
`RLUploaderRestoreWebSocketRoute` removes only the uploader's own hosts block
after a machine crash. That task contains fixed cleanup code; it never launches
code from this project. The task does not start the uploader or poll periodically.
Killing both the uploader and watchdog can leave routing until recovery runs.

**Keep the uploader open while the game uses the proxy.** Closing its window
minimizes it during play to avoid breaking that connection. Pausing capture or
switching modes keeps an existing relay alive. Quit Rocket League before exiting
the app. The proxy is not a general-purpose HTTP proxy and cannot authenticate
another account or send arbitrary service requests.

Certificate trust expires after 30 days. With the game closed, remove and repeat
setup when the app reports expiry. Manual recovery, from an administrator
PowerShell window, removes just this app's hostname block:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\rl_uploader\proxy_route.ps1 -CleanupOnly
```

With the game closed, remove the dedicated certificate from your Windows user's
trust store using `python -m rl_uploader --data-dir .local --remove-proxy-trust`.
These changes are not certified by Epic/EAC. Certificate pinning or game updates
may prevent this route from working; the app will not disable TLS checks or EAC.

Saving settings without a token is supported: local archiving continues and
uploads wait until a token is available. **Test token** makes a read-only account
API request; it does not upload a replay. Existing replay files are excluded on
the first scan of each folder. **Import existing replays** opts that folder's
excluded files into archiving/uploading. New files saved while the app is closed
are discovered next time it starts, after the initial baseline has been made.

Uploads continue while the window is minimized; closing the app stops the
watcher after any active request finishes. Reopening the app resumes saved
settings and queued uploads. Pause stops both discovery and uploads after the
current request. The app does not install a service or enable Windows startup.
To run at sign-in, you can place a shortcut to `run.pyw` in your Windows Startup
folder (`Win+R` → `shell:startup`).

## Saving replays and EAC

As checked on **October 31, 2026**:

- [Rocket League requires EAC for online play](https://www.rocketleague.com/news/easy-anti-cheat-comes-to-rocket-league-on-pc-today).
- Its [documented Stats API](https://www.rocketleague.com/developer/stats-api?lang=en)
  provides match events and replay playback commands, but lists no command to
  save a replay or enable saving every match.
- [Match History](https://www.rocketleague.com/news/introducing-rocket-league-match-history-and-player-profiles)
  lets you save replays from the last 20 matches within 30 days. In game, use
  **Profile → Match History → select a match → Save Replay**. You can also save
  from the post-match screen or use the game's Replay Auto-Save binding during
  a match, then complete any in-game save prompt. Check your bindings in-game;
  the name "Auto-Save" does not mean every match is automatically saved.

The file watcher and uploader use local files and the documented ballchasing
API. Optional cloud capture uses undocumented Epic/PsyNet services, adapted from
the MIT-licensed [dank/rlapi](https://github.com/dank/rlapi) protocol. See
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for attribution.

The app enumerates process names to select the appropriate capture transport.
It does not open game process memory, inject DLLs, extract launcher credentials,
send gameplay inputs, patch game files, edit game configuration, or disable EAC.
After-session mode does not install certificates, redirect hosts, or intercept
game traffic. Optional periodic mode uses the explicit proxy setup described
above. Neither mode disables upstream TLS verification. No Steam password or
Steam session ticket is used.
These choices reduce interaction with the game; they do **not** establish
publisher approval or guarantee account safety for an unofficial API.

## Reliability and privacy

- Uses the current [ballchasing upload API](https://ballchasing.com/doc/_api),
  `POST /api/v2/upload`, with a multipart `file` and explicit visibility.
- The upload token is stored with Windows DPAPI, scoped to your Windows user.
  It is never written to settings, queue entries, or logs. Other programs running
  as your user can still access your DPAPI secrets.
- Optional Epic access/refresh credentials are encrypted separately in
  `epic.dpapi`; refresh rotation is persisted. Passwords are entered only on
  Epic's website. Signed replay URLs and account tokens are not written to the
  queue, status messages, or logs. Cloud sessions request only your own history.
- `BALLCHASING_TOKEN`, if set in the environment, overrides the saved token.
- Authentication and upload requests never follow redirects. Replay GETs allow
  exactly one HTTPS redirect from PsyNet to Psyonix's `psyonix-rl-replays`
  Google Cloud Storage bucket, with a validated replay path. No account
  authorization headers are attached to replay downloads.
- Files must have complete replay container framing and settle before reading.
  SHA-256 identities prevent re-uploading byte-identical copies or renamed files.
  Ballchasing's duplicate response is treated as success.
- A persistent SQLite queue and local archive survive restarts, offline periods,
  and deletion of the original replay. Source files are never edited or deleted.
- Visibility is captured **when queued**. Changing the setting affects future
  queued files; it does not change queued or previously uploaded replays.
- Network/server failures back off. Rate/quota limits pause the entire uploader,
  honor `Retry-After` (bounded to seven days), and otherwise wait one hour.
  Cooldowns survive restarts.
- Bad credentials pause uploads until you save a replacement token, or
  successfully test the current token and choose Save & start. A rejected replay
  is retained locally and marked failed; other replays can continue.
- The archive keeps every queued replay, including successfully uploaded ones.
  There is no automatic retention/deletion policy. Review disk usage periodically;
  deleting pending archives prevents those uploads. Files larger than 64 MiB are
  left untouched and not queued. Only the selected folder is watched, not subfolders.
- [Ballchasing account quotas](https://ballchasing.com/doc/uploads) still apply.
  A queue can grow if you save more matches than your account can upload.

When launched with the included launcher, data lives in `.local/` beside this
README: `settings.json`, `token.dpapi`, optional `epic.dpapi`, `queue.sqlite3`, and `archive/`. These are
excluded from Git. Keep this data folder when updating the source. Deleting the
queue loses deduplication history. The archive filenames are content hashes;
the original filename is retained in the queue/history.

## Command line and verification

```powershell
# Read-only discovery: no uploads, game changes, or token access
python -m rl_uploader --doctor

# Desktop with project-local data
python -m rl_uploader --data-dir .local

# Console mode after saving settings in the desktop app
python -m rl_uploader --data-dir .local --headless

# Desktop minimized
python -m rl_uploader --data-dir .local --minimized

# Automated tests: temporary files and a loopback fake API, no live uploads
python -m unittest discover -s tests -v
```

Without `--data-dir`, the module uses `%LOCALAPPDATA%\RLUploader`. Only one
instance per data folder is allowed. Use the same folder for desktop/headless
mode to share history and avoid duplicate queue processing.

Tests cover partial/changing files, archive recovery, discovery while closed,
deduplication, server duplicates, authentication failures, retry delays and quota
cooldowns across restarts, multipart HTTP transport, redirect blocking, DPAPI,
and interrupted uploads. Cloud tests cover automatic download-to-file capture,
process guards, a full 30-second idle delay, restart deduplication, interrupted
downloads, preservation of existing files, account reconnection, URL restrictions,
TLS-only replay downloads, and encrypted refresh-token rotation. Tests use
synthetic credentials/replays and mocked cloud services. The live test additionally
verified Epic device sign-in, Steam-linked Match History retrieval, Psyonix storage
redirects, 20 saved replay files, and accepted ballchasing uploads. A subsequent
complete play/exit cycle has not yet been observed; process-transition behavior
is covered by tests. Neither kind of test can prove publisher approval.

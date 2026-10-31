"""Replay capture with separate live-session and after-exit transports."""

import hashlib
import os
from pathlib import Path
import re
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

from .api import NoRedirect
from .cloud_protocol import CloudError, EpicAuth, MatchHistoryClient
from .engine import complete_replay, MAX_REPLAY_BYTES, signature
from .platform import rocket_league_running


def replay_download_url(url: str, *, storage=False) -> str:
    try:
        parsed = urllib.parse.urlsplit(url)
        has_credentials = parsed.username is not None or parsed.password is not None
        port = parsed.port
    except (ValueError, TypeError):
        raise CloudError("Invalid replay download address.") from None
    if storage:
        if (parsed.scheme != "https" or parsed.hostname != "storage.googleapis.com"
                or port not in (None, 443) or has_credentials
                or not re.fullmatch(r"/psyonix-rl-replays/Prod/matches/[0-9]{8}/[0-9]{6}/[a-fA-F0-9-]{16,64}\.replay", parsed.path)):
            raise CloudError("Unexpected replay storage destination. Download deferred.")
        return urllib.parse.urlunsplit(("https", "storage.googleapis.com", parsed.path, parsed.query, ""))
    if (parsed.scheme not in ("http", "https") or parsed.hostname != "api.rlpp.psynet.gg"
            or parsed.path != "/Match.replay" or port not in (None, 80, 443) or has_credentials):
        raise CloudError("Unexpected replay download address. Capture paused for this attempt.")
    # Match History may return HTTP. Require TLS, keeping the signed query intact.
    return urllib.parse.urlunsplit(("https", "api.rlpp.psynet.gg", parsed.path, parsed.query, ""))


def download_replay(url: str, guard) -> bytes:
    secure_url = replay_download_url(url)
    if not guard():
        raise CloudError("Cloud capture interrupted; Rocket League may have started.")
    try:
        opener = urllib.request.build_opener(NoRedirect())
        try:
            response = opener.open(secure_url, timeout=20)
        except urllib.error.HTTPError as redirect:
            with redirect:
                if redirect.code not in (301, 302, 303, 307, 308):
                    raise CloudError(f"Replay download returned HTTP {redirect.code}. Will retry.") from None
                destination = replay_download_url(redirect.headers.get("Location", ""), storage=True)
            if not guard():
                raise CloudError("Cloud capture interrupted; Rocket League may have started.")
            # Exactly one validated redirect into Psyonix's replay bucket. No
            # Epic, Steam or PsyNet account headers are attached to either GET.
            response = opener.open(destination, timeout=20)
        with response:
            data = bytearray()
            while True:
                if not guard():
                    raise CloudError("Cloud capture interrupted; Rocket League may have started.")
                chunk = response.read(min(65536, MAX_REPLAY_BYTES + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > MAX_REPLAY_BYTES:
                    raise CloudError("Replay exceeds the 64 MiB limit.")
    except urllib.error.HTTPError as error:
        status = error.code
        error.close()
        raise CloudError(f"Replay storage returned HTTP {status}. Will retry.") from None
    except (OSError, urllib.error.URLError):
        raise CloudError("Replay download failed or is not available yet. Will retry.") from None
    if not complete_replay(data):
        raise CloudError("Downloaded replay is incomplete or has an unknown format. Will retry.")
    return bytes(data)


def save_download(folder: Path, match_id: str, data: bytes) -> Path:
    """Publish a complete replay atomically, without replacing an existing file."""
    if not re.fullmatch(r"[a-fA-F0-9-]{16,64}", match_id):
        raise CloudError("Invalid match identifier.")
    if not complete_replay(data) or len(data) > MAX_REPLAY_BYTES:
        raise CloudError("Refusing to save an incomplete replay.")
    digest = hashlib.sha256(data).hexdigest()
    candidates = [folder / (match_id + ".replay"), folder / (f"cloud_{match_id}_{digest[:12]}.replay")]
    for target in candidates:
        if target.exists():
            if target.stat().st_size <= MAX_REPLAY_BYTES and hashlib.sha256(target.read_bytes()).hexdigest() == digest:
                return target
            continue
        temp_name = None
        try:
            with tempfile.NamedTemporaryFile(dir=folder, prefix=".rl_uploader_", suffix=".tmp", delete=False) as stream:
                temp_name = Path(stream.name)
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            if os.name == "nt":
                os.rename(temp_name, target)  # Windows refuses if the target already exists.
            else:
                os.link(temp_name, target)  # POSIX rename would overwrite; publish without replacement.
            return target
        except FileExistsError:
            if target.stat().st_size <= MAX_REPLAY_BYTES and hashlib.sha256(target.read_bytes()).hexdigest() == digest:
                return target
        finally:
            if temp_name is not None:
                temp_name.unlink(missing_ok=True)
    raise CloudError("A different replay already uses this filename. Original file kept.")


class CloudCapture:
    def __init__(self, engine, stop, notify=lambda status: None, *, clock=time.time,
                 running=rocket_league_running, auth=None, client=None, live_client=None, download=download_replay):
        self.engine = engine
        self.state = engine.state
        self.stop = stop
        self.notify = notify
        self.clock = clock
        self.running = running
        self.auth = auth or EpicAuth(engine.data_dir)
        self.client = client or MatchHistoryClient(self.auth, self.guard)
        self.live_client = live_client
        self.download = download
        self.idle_since = None
        self.was_running = None
        self.message = "Cloud capture is off."

    def guard(self):
        return not self.stop.is_set() and self.running() is False

    def status(self, message):
        self.message = message
        self.notify(message)

    def reconnect(self):
        for prefix in ("cloud", "live"):
            self.state.set(prefix + "_blocked", "")
            self.state.set(prefix + "_next", 0)
            self.state.set(prefix + "_attempts", 0)

    def tick(self):
        if not self.engine.settings.cloud_capture:
            self.status("Automatic capture is off.")
            return
        running = self.running()
        now = self.clock()
        if running is None:
            self.idle_since = None
            self.status("Cannot verify Rocket League's state. Capture deferred.")
            return
        if running is True:
            self.idle_since = None
            if self.was_running is not True:
                self.state.set("cloud_next", 0)
                self.state.set("live_next", 0)
            self.was_running = True
            if self.engine.settings.capture_mode == "after_session":
                self.status("Waiting for Rocket League to close.")
                return
            if self.live_client is None:
                self.status("Periodic capture needs a captured game session. No second login will be created.")
                return
            client, prefix = self.live_client, "live"
        else:
            if self.was_running is True:
                self.state.set("cloud_next", 0)
            self.was_running = False
            if not self.auth.connected():
                self.status("Connect Epic to enable automatic replay saving after play.")
                return
            if self.idle_since is None:
                self.idle_since = now
            if now - self.idle_since < 30:
                self.status("Rocket League is closed. Waiting 30 seconds before cloud capture.")
                return
            client, prefix = self.client, "cloud"
        if self.state.get(prefix + "_blocked"):
            self.status(self.state.get(prefix + "_blocked"))
            return
        due = float(self.state.get(prefix + "_next", "0"))
        if now < due:
            self.status(self.state.get(prefix + "_message", "Replay capture is up to date."))
            return
        # The source must remain appropriate throughout this pass. The live
        # transport can never fall back to AuthPlayer, even when it disconnects.
        capture_guard = lambda: not self.stop.is_set() and self.running() is running
        saved = 0
        unavailable = 0
        first_error = ""
        try:
            if not self.engine.folder.is_dir():
                raise CloudError("Replay folder unavailable. Cloud capture will retry.")
            if not capture_guard():
                raise CloudError("Game state changed. Capture deferred.")
            self.status("Fetching Match History through the existing game session." if running else
                        "Fetching Match History. Please wait before relaunching Rocket League.")
            account, matches = client.history()
            seen = set()
            for entry in reversed(matches):
                if not capture_guard():
                    raise CloudError("Game state changed or capture paused. Remaining downloads deferred.")
                if not isinstance(entry, dict) or not isinstance(entry.get("Match"), dict):
                    raise CloudError("Unexpected match entry format.")
                match_id = entry["Match"].get("MatchGUID", "")
                if not isinstance(match_id, str) or not re.fullmatch(r"[a-fA-F0-9-]{16,64}", match_id):
                    raise CloudError("Invalid match identifier from Match History.")
                if match_id in seen:
                    continue
                seen.add(match_id)
                key = "cloud_saved:" + account + ":" + match_id
                previous = self.state.get(key)
                if re.fullmatch(r"[a-f0-9]{64}", previous) and (self.engine.archive / (previous + ".replay")).exists():
                    continue
                self.status(f"Saving replay {saved + 1} from Match History…")
                try:
                    data = self.download(entry.get("ReplayUrl", ""), capture_guard)
                    path = save_download(self.engine.folder, match_id, data)
                except CloudError as error:
                    # One replay can be unavailable while newer ones are ready.
                    # Keep harvesting the rest before scheduling another pass.
                    unavailable += 1
                    first_error = first_error or str(error)
                    continue
                digest = self.engine.archive_replay(data, path.name, str(path), signature(path))
                self.state.set(key, digest)
                saved += 1
            if unavailable:
                raise CloudError(f"Saved {saved} replay(s); {unavailable} unavailable or incomplete. Will retry. {first_error}")
            source = "Live session" if running else "Cloud"
            message = f"{source} capture complete: {saved} replay(s) saved this pass; {len(matches)} in recent history."
            if len(matches) >= 20:
                message += " History is at the 20-match limit; earlier matches may be missing."
            self.state.set(prefix + "_attempts", 0)
            self.state.set(prefix + "_next", self.clock() + self.engine.settings.capture_interval_minutes * 60)
        except CloudError as error:
            message = str(error)
            if error.permanent:
                self.state.set(prefix + "_blocked", message)
            else:
                attempts = int(self.state.get(prefix + "_attempts", "0")) + 1
                maximum = self.engine.settings.capture_interval_minutes * 60 if running else 3600
                delay = min(maximum, 60 * 2 ** min(attempts - 1, 6))
                self.state.set(prefix + "_attempts", attempts)
                self.state.set(prefix + "_next", self.clock() + delay)
        except Exception as error:
            message = f"Cloud capture failed ({type(error).__name__}). Files already saved are kept; retrying in 5 minutes."
            self.state.set(prefix + "_next", self.clock() + 300)
        self.state.set(prefix + "_message", message)
        self.status(message)

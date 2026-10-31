"""Read completed replay files, archive atomically, then upload from the archive."""

import hashlib
import http.client
import os
from pathlib import Path
import struct
import time
import urllib.error

from .api import retry_after
from .config import Settings
from .state import State

MAX_REPLAY_BYTES = 64 * 1024 * 1024


def signature(path: Path) -> str:
    info = path.stat()
    return f"{info.st_size}:{info.st_mtime_ns}"


def complete_replay(data: bytes) -> bool:
    """Check the two length-prefixed replay sections; partial writes must not upload.

    This is a container framing check, not a full replay parser. Ballchasing does
    the actual replay validation. CRCs and gameplay data are left untouched.
    """
    if len(data) < 16:
        return False
    header_size = struct.unpack_from("<I", data, 0)[0]
    if not 0 < header_size <= len(data) - 16:
        return False
    content_size = struct.unpack_from("<I", data, 8 + header_size)[0]
    return content_size > 0 and 16 + header_size + content_size == len(data)


class Engine:
    def __init__(self, data_dir: Path, settings: Settings, client=None, *,
                 clock=time.time, settle_seconds=5.0, include_existing=False):
        settings.validate()
        self.data_dir = data_dir.resolve()
        self.settings = settings
        self.folder = Path(os.path.normcase(str(Path(settings.replay_dir).resolve())))
        self.archive = self.data_dir / "archive"
        if self.folder == self.archive or self.archive.is_relative_to(self.folder) or self.folder.is_relative_to(self.archive):
            raise ValueError("The replay folder must be separate from the uploader's data folder.")
        self.archive.mkdir(parents=True, exist_ok=True)
        self.state = State(self.data_dir)
        self.client = client
        self.clock = clock
        self.settle_seconds = settle_seconds
        self.pending: dict[str, tuple[str, float]] = {}
        self.scan_message = "Waiting for saved replays"
        self.include_on_first_scan = include_existing

    def close(self):
        self.state.close()

    def archive_replay(self, data: bytes, filename: str, source: str, file_signature: str) -> str:
        """Archive a complete file and queue it; also used by cloud capture."""
        if len(data) > MAX_REPLAY_BYTES or not complete_replay(data):
            raise ValueError("Invalid or incomplete replay")
        digest = hashlib.sha256(data).hexdigest()
        target = self.archive / (digest + ".replay")
        if not target.exists() or hashlib.sha256(target.read_bytes()).hexdigest() != digest:
            temporary = target.with_suffix(".tmp")
            with temporary.open("wb") as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            temporary.replace(target)
        self.state.enqueue(digest, filename, len(data), self.clock(), self.settings.visibility,
                           source, file_signature)
        return digest

    def scan(self) -> int:
        now = self.clock()
        try:
            files = [p for p in self.folder.iterdir() if p.suffix.lower() == ".replay" and p.is_file()]
        except OSError:
            self.scan_message = "Replay folder unavailable. Check the path or OneDrive connection."
            return 0
        baseline_key = "initialized:" + str(self.folder)
        if not self.state.get(baseline_key):
            baseline = []
            if not self.include_on_first_scan:
                for path in files:
                    try:
                        baseline.append((str(path), signature(path)))
                    except OSError:
                        pass
            self.state.baseline(baseline_key, baseline)
        self.include_on_first_scan = False
        queued = 0
        warnings = []
        live_paths = {str(p) for p in files}
        self.pending = {k: v for k, v in self.pending.items() if k in live_paths}
        for path in files:
            key = str(path)
            try:
                current = signature(path)
                if self.state.observed(key, current):
                    self.pending.pop(key, None)
                    continue
                previous, since = self.pending.get(key, ("", now))
                if previous != current:
                    self.pending[key] = (current, now)
                    continue
                if now - since < self.settle_seconds:
                    continue
                size = int(current.split(":")[0])
                if size > MAX_REPLAY_BYTES:
                    warnings.append("A replay exceeds the 64 MiB limit; it was left untouched.")
                    continue
                with path.open("rb") as stream:
                    data = stream.read(MAX_REPLAY_BYTES + 1)
                if signature(path) != current or len(data) != size:
                    self.pending.pop(key, None)
                    continue
                if not complete_replay(data):
                    warnings.append("Waiting for a complete replay file; incomplete/invalid files are not uploaded.")
                    continue
                self.archive_replay(data, path.name, key, current)
                queued += 1
                self.pending.pop(key, None)
            except OSError:
                warnings.append("A replay could not be read or archived. Retrying automatically.")
        self.scan_message = warnings[0] if warnings else "Watching for saved replays"
        return queued

    def include_existing(self):
        self.state.include_existing(self.folder)
        self.include_on_first_scan = True

    def _retry(self, job, now: float, delay: float, message: str):
        self.state.update(job["digest"], status="queued", next_try=now + delay, message=message)
        # Global cooldown prevents a large backlog from hammering the same failure.
        self.state.set("cooldown", now + delay)

    def upload_one(self) -> bool:
        now = self.clock()
        if self.client is None or self.state.get("blocked") or float(self.state.get("cooldown", "0")) > now:
            return False
        job = self.state.due(now)
        if job is None:
            return False
        try:
            data = (self.archive / (job["digest"] + ".replay")).read_bytes()
        except OSError:
            self.state.update(job["digest"], status="failed", message="Archive missing or unreadable. Restore it, then retry.")
            return True
        if hashlib.sha256(data).hexdigest() != job["digest"]:
            self.state.update(job["digest"], status="failed", message="Archive integrity check failed. Restore it, then retry.")
            return True
        attempts = job["attempts"] + 1
        self.state.update(job["digest"], status="uploading", attempts=attempts, message="Uploading")
        delay = min(3600, 5 * 2 ** min(attempts - 1, 10))
        try:
            response = self.client.upload(data, job["visibility"])
        except (OSError, urllib.error.URLError, http.client.HTTPException):
            self._retry(job, self.clock(), delay, "Connection failed. Will retry automatically.")
            return True
        now = self.clock()
        self.state.set("cooldown", now + 3)
        if response.status in (201, 409) and response.replay_id():
            duplicate = response.status == 409
            self.state.update(job["digest"], status="duplicate" if duplicate else "uploaded",
                              replay_id=response.replay_id(), next_try=0,
                              message="Already on ballchasing" if duplicate else "Uploaded")
        elif response.status in (401, 403):
            message = ("Token rejected. Save a valid token to resume." if response.status == 401 else
                       "Account access denied (HTTP 403). Check ballchasing access, then reconnect.")
            self.state.set("blocked", message)
            self.state.update(job["digest"], status="queued", message=message)
        elif response.status == 429:
            self._retry(job, now, retry_after(response.headers, now, 3600),
                        "Upload rate or account quota reached. Waiting to retry.")
        elif response.status in (400, 413, 415, 422):
            self.state.update(job["digest"], status="failed",
                              message=f"Replay rejected (HTTP {response.status}). Local copy kept.")
        elif response.status >= 500 or response.status in (408, 425, 201, 409):
            self._retry(job, now, retry_after(response.headers, now, delay),
                        f"Temporary/invalid API response (HTTP {response.status}). Will retry.")
        else:
            message = f"Unexpected API response (HTTP {response.status}). Check service/API, then reconnect."
            self.state.set("blocked", message)
            self.state.update(job["digest"], status="queued", message=message)
        return True

    def snapshot(self) -> dict:
        return {**self.state.summary(), "scan_message": self.scan_message,
                "settling": len(self.pending), "has_token": self.client is not None}

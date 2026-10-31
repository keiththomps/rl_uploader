"""The documented ballchasing upload API. No game or Epic account access."""

from dataclasses import dataclass
from datetime import timezone
from email.utils import parsedate_to_datetime
import json
import re
import urllib.error
import urllib.request
import uuid

from .platform import validate_token

API_ROOT = "https://ballchasing.com/api"
MAX_RETRY_DELAY = 7 * 24 * 60 * 60


@dataclass(frozen=True)
class Response:
    status: int
    headers: dict
    body: bytes

    def replay_id(self) -> str:
        try:
            value = json.loads(self.body)
            replay_id = value.get("id", "") if isinstance(value, dict) else ""
            if isinstance(replay_id, str) and re.fullmatch(r"[a-zA-Z0-9-]{1,100}", replay_id):
                return replay_id
        except (ValueError, UnicodeError):
            pass
        return ""


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # Never forward the token to a redirected destination.


class BallchasingClient:
    def __init__(self, token: str):
        self.token = validate_token(token)
        self.opener = urllib.request.build_opener(NoRedirect())

    def _request(self, request: urllib.request.Request) -> Response:
        request.add_header("Authorization", self.token)
        request.add_header("User-Agent", "RLUploader/0.3 (standalone replay uploader)")
        try:
            with self.opener.open(request, timeout=30) as response:
                return Response(response.status, dict(response.headers.items()), response.read(1024 * 1024))
        except urllib.error.HTTPError as error:
            with error:
                return Response(error.code, dict(error.headers.items()), error.read(1024 * 1024))

    def ping(self) -> Response:
        return self._request(urllib.request.Request(API_ROOT + "/"))

    def upload(self, replay: bytes, visibility: str) -> Response:
        if visibility not in ("private", "unlisted", "public"):
            raise ValueError("Invalid upload visibility.")
        boundary = "RLUploader" + uuid.uuid4().hex
        prefix = (f"--{boundary}\r\n"
                  'Content-Disposition: form-data; name="file"; filename="replay.replay"\r\n'
                  "Content-Type: application/octet-stream\r\n\r\n").encode("ascii")
        body = prefix + replay + f"\r\n--{boundary}--\r\n".encode("ascii")
        return self._request(urllib.request.Request(
            API_ROOT + "/v2/upload?visibility=" + visibility,
            data=body, headers={"Content-Type": "multipart/form-data; boundary=" + boundary},
            method="POST"))


def retry_after(headers: dict, now: float, fallback: float) -> float:
    value = next((v for k, v in headers.items() if k.lower() == "retry-after"), "")
    try:
        delay = float(value)
        if 0 <= delay < float("inf"):
            return min(MAX_RETRY_DELAY, max(1, delay))
    except (TypeError, ValueError):
        try:
            date = parsedate_to_datetime(value)
            if date.tzinfo is None:
                date = date.replace(tzinfo=timezone.utc)
            return min(MAX_RETRY_DELAY, max(1, date.timestamp() - now))
        except (TypeError, ValueError, OverflowError):
            pass
    return fallback

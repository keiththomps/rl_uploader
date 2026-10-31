"""Experimental, account-authenticated Match History download protocol.

Adapted from MIT-licensed dank/rlapi; see THIRD_PARTY_NOTICES.md. These are
unofficial services. No game hooks, certificate bypasses, or memory access.
Protocol constants below are public client/build identifiers from that project,
NOT user credentials. Never put a user's access/refresh token in source code.
"""

import base64
import hashlib
import hmac
import json
from pathlib import Path
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

from .api import NoRedirect
from .platform import _dpapi

GAME_VERSION = "261013.63066.530866"
FEATURE_SET = "PrimeUpdate60_1"
BUILD_SECRET = "76656a5b51ec57a13549eef732c8a8367638aa3a7989bade1f7e4e5f93d57625"
SIGNING_KEY = b"c338bd36fb8c42b1a431d30add939fc7"
EOS_CLIENT = "xyza7891p5D7s9R6Gm6moTHWGloerp7B"
EOS_SECRET = "Knh18du4NVlFs+3uQ+ZPpDCVto0WYf4yXP8+OcwVt1o"
DEPLOYMENT = "da32ae9c12ae40e8a112c52e1f17f3ba"
EOS_ROOT = "https://api.epicgames.dev/epic/oauth/v2/"
PSYNET_AUTH = "https://api.rlpp.psynet.gg/rpc/Auth/AuthPlayer/v2"
MAX_JSON_BYTES = 4 * 1024 * 1024


class CloudError(Exception):
    """Contains a safe local message, never a response body or credential."""
    def __init__(self, message, *, code="", permanent=False):
        super().__init__(message)
        self.code = code
        self.permanent = permanent


def build_id(version: str) -> str:
    crc = 0xFFFFFFFF
    for byte in version.encode("utf-16-le"):
        crc ^= byte << 24
        for _ in range(8):
            crc = ((crc << 1) ^ (0x04C11DB7 if crc & 0x80000000 else 0)) & 0xFFFFFFFF
    crc ^= 0xFFFFFFFF
    return str(crc if crc < 0x80000000 else crc - 0x100000000)


def encode_json(value) -> bytes:
    return json.dumps(value, separators=(",", ":"), ensure_ascii=True).encode("utf-8")


def sign(body: bytes) -> str:
    return base64.b64encode(hmac.new(SIGNING_KEY, b"-" + body, hashlib.sha256).digest()).decode("ascii")


def safe_error_code(value) -> str:
    # Allow only known machine error codes into exception messages/state.
    if isinstance(value, str):
        value = value.removeprefix("errors.com.epicgames.account.oauth.")
    known = ("authorization_pending", "slow_down", "access_denied", "expired_token",
             "invalid_grant", "invalid_client", "invalid_token")
    return value if isinstance(value, str) and value in known else ""


def request_json(url, body, headers=None):
    request = urllib.request.Request(url, data=body, headers=headers or {}, method="POST")
    opener = urllib.request.build_opener(NoRedirect())
    try:
        response = opener.open(request, timeout=20)
    except urllib.error.HTTPError as error:
        response = error
    except (OSError, urllib.error.URLError) as error:
        raise CloudError("Cloud service could not be reached. Will retry.") from None
    with response:
        status = response.code
        raw = response.read(MAX_JSON_BYTES + 1)
    if len(raw) > MAX_JSON_BYTES:
        raise CloudError("Cloud response exceeded the size limit.")
    try:
        result = json.loads(raw)
        if not isinstance(result, dict):
            raise ValueError()
    except (ValueError, UnicodeError):
        raise CloudError("Cloud service returned an invalid response.") from None
    if status != 200:
        code = safe_error_code(result.get("error")) or safe_error_code(result.get("errorCode"))
        if code in ("authorization_pending", "slow_down"):
            raise CloudError("Waiting for Epic sign-in.", code=code)
        raise CloudError(f"Cloud authentication returned HTTP {status}. Reconnect Epic if needed.",
                         code=code, permanent=status in (400, 401, 403, 404))
    return result


class EpicAuth:
    def __init__(self, data_dir: Path, request=request_json):
        self.path = data_dir / "epic.dpapi"
        self.request = request

    def connected(self):
        return self.path.exists()

    def _save(self, token):
        account = token.get("account_id", "")
        if not re.fullmatch(r"[a-fA-F0-9]{32}", account) or not token.get("refresh_token") or not token.get("access_token"):
            raise CloudError("Epic returned an incomplete account token.", permanent=True)
        token = {**token, "saved_at": time.time()}
        temporary = self.path.with_suffix(".tmp")
        temporary.write_bytes(_dpapi(encode_json(token), True))
        temporary.replace(self.path)
        return token

    def _token(self, fields):
        fields = {**fields, "deployment_id": DEPLOYMENT, "scope": "basic_profile"}
        return self.request(EOS_ROOT + "token", urllib.parse.urlencode(fields).encode("ascii"), {
            "Content-Type": "application/x-www-form-urlencoded",
            "Authorization": "Basic " + base64.b64encode((EOS_CLIENT + ":" + EOS_SECRET).encode()).decode(),
        })

    def begin(self):
        device = self.request(EOS_ROOT + "deviceAuthorization",
                              urllib.parse.urlencode({"client_id": EOS_CLIENT}).encode(),
                              {"Content-Type": "application/x-www-form-urlencoded"})
        uri = urllib.parse.urlsplit(device.get("verification_uri", ""))
        if uri.scheme != "https" or uri.hostname not in ("epicgames.com", "www.epicgames.com") or uri.username:
            raise CloudError("Epic returned an unexpected sign-in address.", permanent=True)
        if not device.get("device_code") or not re.fullmatch(r"[A-Z0-9-]{4,24}", device.get("user_code", ""), re.I):
            raise CloudError("Epic returned an invalid device code.")
        return device

    def finish(self, device, cancel):
        interval = max(5, min(60, int(device.get("interval", 5))))
        deadline = time.monotonic() + min(1800, max(1, int(device.get("expires_in", 600))))
        while time.monotonic() < deadline:
            if cancel.wait(interval):
                raise CloudError("Epic sign-in cancelled.")
            try:
                token = self._token({"grant_type": "device_code", "device_code": device["device_code"]})
                if cancel.is_set():
                    raise CloudError("Epic sign-in cancelled.")
                return self._save(token)
            except CloudError as error:
                if error.code == "slow_down":
                    interval += 5
                elif error.code != "authorization_pending":
                    raise
        raise CloudError("This code expired. Choose Get a new code to continue.", code="expired_token")

    def token(self):
        try:
            value = json.loads(_dpapi(self.path.read_bytes(), False))
        except (OSError, ValueError, RuntimeError):
            raise CloudError("Epic credentials unavailable. Reconnect Epic.", permanent=True) from None
        if value.get("saved_at", 0) + value.get("expires_in", 0) > time.time() + 120:
            return value
        return self._save(self._token({"grant_type": "refresh_token", "refresh_token": value["refresh_token"]}))


def websocket_factory(url, headers):
    vendor = Path(__file__).resolve().parent.parent / ".vendor"
    if vendor.is_dir() and str(vendor) not in sys.path:
        sys.path.insert(0, str(vendor))
    try:
        import websocket
    except ImportError:
        raise CloudError("Run Setup cloud capture.cmd to install the WebSocket dependency.", permanent=True) from None
    # No redirects and normal certificate verification. No trace/logging enabled.
    socket = websocket.create_connection(url, header=headers, timeout=10,
                                         suppress_origin=True, redirect_limit=0)
    if socket.getstatus() != 101:
        socket.close(timeout=1)
        raise CloudError("Cloud WebSocket handshake was rejected. Redirects are not allowed.")
    return socket


class MatchHistoryClient:
    def __init__(self, auth: EpicAuth, guard, request=request_json, connect=websocket_factory):
        self.auth = auth
        self.guard = guard
        self.request = request
        self.connect = connect

    def _guard(self):
        if not self.guard():
            raise CloudError("Rocket League started or process status is unknown. Cloud sync deferred.")

    def history(self):
        self._guard()
        token = self.auth.token()
        self._guard()
        account = token["account_id"]
        player = "Epic|" + account + "|0"
        body = encode_json({
            "Platform": "Epic", "PlayerName": "", "PlayerID": account, "Language": "INT",
            "AuthTicket": token["access_token"], "BuildRegion": "", "FeatureSet": FEATURE_SET,
            "Device": "PC", "LocalFirstPlayerID": player, "bSkipAuth": False,
            "bSetAsPrimaryAccount": True, "EpicAuthTicket": token["access_token"], "EpicAccountID": account,
        })
        headers = {"Content-Type": "application/x-www-form-urlencoded",
                   "User-Agent": f"RL Win/{GAME_VERSION} gzip (x86_64-pc-win32) curl-7.67.0 Schannel",
                   "PsyBuildID": build_id(GAME_VERSION), "PsyEnvironment": "Prod",
                   "PsyRequestID": "PsyNetMessage_X_0", "PsySig": sign(body), "PsyBuildSecret": BUILD_SECRET}
        wrapper = self.request(PSYNET_AUTH, body, headers)
        if wrapper.get("Error") or not isinstance(wrapper.get("Result"), dict):
            raise CloudError("Rocket League cloud login was rejected. Protocol/build may need updating.", permanent=True)
        result = wrapper["Result"]
        url = result.get("PerConURLv2", "")
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme != "wss" or parsed.hostname != "ws.rlpp.psynet.gg" or parsed.port not in (None, 443) or parsed.username:
            raise CloudError("Unexpected Rocket League WebSocket destination.", permanent=True)
        session_headers = {"PsyBuildID": build_id(GAME_VERSION), "User-Agent": f"RL Win/{GAME_VERSION} gzip",
                           "PsyEnvironment": "Prod", "PsyToken": result.get("PsyToken", ""),
                           "PsySessionID": result.get("SessionID", "")}
        if any(not isinstance(v, str) or not v or "\r" in v or "\n" in v for v in session_headers.values()):
            raise CloudError("Rocket League returned invalid session headers.")
        self._guard()
        socket = None
        try:
            socket = self.connect(url, session_headers)
            self._guard()
            request_id = "PsyNetMessage_X_1"
            payload = encode_json({"PlayerID": player})
            message = (f"PsyService: Matches/GetMatchHistory v1\r\nPsyRequestID: {request_id}\r\n"
                       f"PsySig: {sign(payload)}\r\n\r\n") + payload.decode()
            socket.send(message)
            deadline = time.monotonic() + 20
            for _ in range(100):
                self._guard()
                if time.monotonic() >= deadline:
                    break
                response = socket.recv()
                if not isinstance(response, str) or len(response) > MAX_JSON_BYTES:
                    raise CloudError("Invalid Match History response.")
                if response.startswith("PsyPing:"):
                    socket.send("PsyPong:\r\n\r\n")
                    continue
                raw_headers, separator, content = response.partition("\r\n\r\n")
                pairs = dict(line.split(":", 1) for line in raw_headers.split("\r\n") if ":" in line)
                if not separator or pairs.get("PsyResponseID", "").strip() != request_id:
                    continue
                response_data = json.loads(content)
                if not isinstance(response_data, dict) or response_data.get("Error"):
                    raise CloudError("Match History request was rejected.", permanent=True)
                matches = response_data.get("Result", {}).get("Matches")
                if not isinstance(matches, list) or len(matches) > 100:
                    raise CloudError("Unexpected Match History format.")
                return account, matches
            raise CloudError("Match History request timed out.")
        except CloudError:
            raise
        except Exception:
            raise CloudError("Match History connection failed. Will retry.") from None
        finally:
            if socket is not None:
                try:
                    socket.close(timeout=1)
                except Exception:
                    pass

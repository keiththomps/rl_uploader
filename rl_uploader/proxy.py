"""Standalone TLS relay for exactly ws.rlpp.psynet.gg; no game authentication.

Windows routing is owned by a separate elevated watchdog. No account headers,
raw frames, signed URLs, or replay responses are logged or persisted here.
"""

import base64
import ctypes
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import socket
import ssl
import subprocess
import threading
import time

from .cloud_protocol import CloudError
from .platform import documents_folder, rocket_league_running
from .proxy_certs import HOST, HEALTH_PORT, prepare, server_context, trusted
from .proxy_wire import RelaySession, SessionBroker

MAX_HEADERS = 65536


def read_headers(stream):
    raw = bytearray()
    # Do not consume a frame sent immediately after the HTTP upgrade.
    while not raw.endswith(b"\r\n\r\n"):
        byte = stream.recv(1)
        if not byte:
            raise EOFError()
        raw.extend(byte)
        if len(raw) > MAX_HEADERS:
            raise ValueError("HTTP headers exceed limit")
    lines = bytes(raw).split(b"\r\n")
    first = lines[0].decode("ascii")
    headers = {}
    for line in lines[1:]:
        if not line:
            continue
        key, colon, value = line.partition(b":")
        name = key.decode("ascii").lower()
        if not colon or not re.fullmatch(r"[a-z0-9-]+", name) or name in headers:
            raise ValueError("Invalid HTTP header")
        headers[name] = value.strip().decode("latin-1")
    return bytes(raw), first, headers


def validate_handshake(first, headers):
    parts = first.split(" ")
    if len(parts) != 3 or parts[0] != "GET" or parts[2] != "HTTP/1.1":
        raise ValueError("WebSocket GET required")
    if not re.fullmatch(r"/ws/gc2?|/ws/gc\?PsyConnectionType=Player", parts[1]):
        raise ValueError("Unexpected WebSocket path")
    if headers.get("host", "").lower() not in (HOST, HOST + ":443"):
        raise ValueError("Unexpected WebSocket host")
    if headers.get("upgrade", "").lower() != "websocket" or "upgrade" not in [part.strip().lower() for part in headers.get("connection", "").split(",")]:
        raise ValueError("WebSocket upgrade required")
    if headers.get("sec-websocket-version") != "13":
        raise ValueError("Unsupported WebSocket version")
    try:
        if len(base64.b64decode(headers.get("sec-websocket-key", ""), validate=True)) != 16:
            raise ValueError()
    except ValueError:
        raise ValueError("Invalid WebSocket key") from None
    if not headers.get("psytoken") or not headers.get("psysessionid"):
        raise ValueError("No authenticated game session")


def resolve_upstream():
    if os.name == "nt":
        # Fixed command; DNS only, ignoring our hosts mapping. No public resolver
        # is hard-coded and no certificate verification is disabled.
        command = ("Resolve-DnsName -Name 'ws.rlpp.psynet.gg' -Type A -DnsOnly -NoHostsFile "
                   "-QuickTimeout -ErrorAction Stop | Where-Object Type -eq 'A' | Select-Object -ExpandProperty IPAddress")
        result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                                capture_output=True, text=True, timeout=15,
                                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        candidates = result.stdout.splitlines() if result.returncode == 0 else []
    else:
        candidates = [row[4][0] for row in socket.getaddrinfo(HOST, 443, type=socket.SOCK_STREAM)]
    addresses = []
    for candidate in candidates:
        try:
            address = ipaddress.ip_address(candidate.strip())
            if address.is_global and str(address) not in addresses:
                addresses.append(str(address))
        except ValueError:
            pass
    if not addresses:
        raise CloudError("Could not resolve the real Rocket League WebSocket server. Proxy routing is paused.")
    return addresses


def local_player_id():
    path = documents_folder() / "My Games/Rocket League/TAGame/Logs/Launch.log"
    try:
        with path.open("rb") as stream:
            stream.seek(0, 2)
            length = stream.tell()
            stream.seek(max(0, length - 8 * 1024 * 1024))
            text = stream.read().decode("utf-8", errors="replace")
    except OSError:
        return None
    pattern = (r"HandleLocalPlayerLoginStatusChanged[^\r\n]*?PlayerID="
               r"((?:Epic\|[a-fA-F0-9]{32}|Steam\|[0-9]{17})\|0)"
               r"[^\r\n]*?LoginStatus=LS_LoggedIn[^\r\n]*?IsPrimary=True")
    matches = re.findall(pattern, text)
    return matches[-1] if matches else None


def process_birth():
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    times = [wintypes.FILETIME() for _ in range(4)]
    if not kernel.GetProcessTimes(kernel.GetCurrentProcess(), *(ctypes.byref(value) for value in times)):
        raise ctypes.WinError(ctypes.get_last_error())
    return (times[0].dwHighDateTime << 32) | times[0].dwLowDateTime


class CaptureProxy:
    def __init__(self, data_dir, *, running=rocket_league_running):
        self.data_dir = Path(data_dir)
        self.running = running
        self.stop_event = threading.Event()
        self.trust_valid = False
        self.trust_checked_at = 0
        self.broker = SessionBroker(local_player_id, lambda: self.enabled and self.trust_valid and
                                   not self.stop_event.is_set() and self.running() is True)
        self.nonce = secrets.token_hex(32)
        self.listeners = []
        self.threads = []
        self.sessions = set()
        self.lock = threading.Lock()
        self.dns_lock = threading.Lock()
        self.addresses = []
        self.resolved_at = 0
        self.http = None
        self.started = False
        self.enabled = False
        self.ipv6 = False
        self.route_state = "not_started"
        self.detail = "Set up periodic capture before selecting that mode."

    def history(self):
        if not self.broker.connected():
            raise CloudError(self.detail if not self.started or self.route_state == "error" else
                             "Waiting for the game's captured session. Start the proxy before launching Rocket League.")
        return self.broker.history()

    def active_connections(self):
        with self.lock:
            return len(self.sessions)

    def upstream_addresses(self):
        with self.dns_lock:
            if not self.addresses or time.monotonic() - self.resolved_at > 60:
                self.addresses = resolve_upstream()
                self.resolved_at = time.monotonic()
            return list(self.addresses)

    def start(self):
        self.enabled = True
        if self.started:
            return
        if os.name != "nt":
            raise CloudError("The capture proxy currently requires Windows.")
        if not trusted(self.data_dir):
            self.detail = "Periodic capture needs certificate setup. Choose Set up periodic capture."
            return
        self.trust_valid = True
        self.trust_checked_at = time.monotonic()
        self.stop_event.clear()
        try:
            prepare(self.data_dir)  # Refuse an expired certificate before publishing any route.
            self.upstream_addresses()
            context = server_context(self.data_dir)
            for family, address in ((socket.AF_INET, ("127.0.0.1", 443)),
                                    (socket.AF_INET6, ("::1", 443))):
                listener = socket.socket(family, socket.SOCK_STREAM)
                try:
                    if family == socket.AF_INET6:
                        listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                    listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
                    listener.bind(address)
                    listener.listen(8)
                    listener.settimeout(0.5)
                except OSError:
                    listener.close()
                    if family == socket.AF_INET6:
                        continue
                    raise
                self.listeners.append(listener)
                self.ipv6 |= family == socket.AF_INET6
                thread = threading.Thread(target=self._accept, args=(listener, context), daemon=True, name="replay-proxy-listen")
                thread.start()
                self.threads.append(thread)
            self._start_http()
            self.started = True
            self.route_state = "starting"
            self.detail = "Proxy ready. Approve Windows administrator access for hostname routing."
            self._start_watchdog()
        except Exception as error:
            self.close()
            self.detail = str(error) if isinstance(error, CloudError) else f"Proxy could not start ({type(error).__name__}). Check ports 443 and {HEALTH_PORT}."
            raise CloudError(self.detail) from None

    def _accept(self, listener, context):
        while not self.stop_event.is_set():
            try:
                stream, address = listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            thread = threading.Thread(target=self._connection, args=(stream, context), daemon=True, name="replay-proxy-relay")
            thread.start()

    def _connection(self, stream, context):
        upstream = None
        session = None
        try:
            stream.settimeout(10)
            local = context.wrap_socket(stream, server_side=True)
            stream = local
            raw, first, headers = read_headers(local)
            validate_handshake(first, headers)
            for address in self.upstream_addresses():
                connection = None
                try:
                    connection = socket.create_connection((address, 443), timeout=10)
                    upstream = ssl.create_default_context().wrap_socket(connection, server_hostname=HOST)
                    break
                except OSError:
                    if connection is not None:
                        connection.close()
            if upstream is None:
                raise OSError("Upstream unavailable")
            # Decline optional compression. This preserves raw game payloads
            # and avoids sharing compression dictionaries with injected frames.
            lines = raw.split(b"\r\n")
            request = b"\r\n".join(line for line in lines if not line.lower().startswith(b"sec-websocket-extensions:"))
            upstream.sendall(request)
            response, status, accepted = read_headers(upstream)
            expected = base64.b64encode(hashlib.sha1((headers["sec-websocket-key"] +
                "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            if (not status.startswith("HTTP/1.1 101 ") or accepted.get("sec-websocket-accept") != expected
                    or "sec-websocket-extensions" in accepted):
                raise ValueError("Upstream WebSocket upgrade rejected")
            local.sendall(response)
            local.settimeout(1)
            upstream.settimeout(1)
            session = RelaySession(local, upstream)
            with self.lock:
                self.sessions.add(session)
            self.broker.attach(session)
            self.detail = "Connected to Rocket League's existing session."
            session.run()
        except Exception as error:
            self.detail = f"Game proxy connection ended ({type(error).__name__}). No second login was attempted."
        finally:
            if session is not None:
                self.broker.detach(session)
                with self.lock:
                    self.sessions.discard(session)
                session.close()
            else:
                stream.close()
                if upstream is not None:
                    upstream.close()

    def _start_http(self):
        owner = self
        crl = (self.data_dir / "proxy" / "rl-uploader.crl").read_bytes()
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, data, content_type="application/json"):
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(data)

            def authenticated(self):
                return secrets.compare_digest(self.headers.get("X-RLU-Nonce", ""), owner.nonce)

            def do_GET(self):
                if self.path == "/rl-uploader.crl":
                    self.reply(200, crl, "application/pkix-crl")
                elif self.path == "/health" and self.authenticated():
                    if time.monotonic() - owner.trust_checked_at > 5:
                        owner.trust_valid = trusted(owner.data_dir)
                        owner.trust_checked_at = time.monotonic()
                        if not owner.trust_valid:
                            owner.detail = "Proxy certificate trust was removed. Routing and capture are paused."
                    ready = owner.started and not owner.stop_event.is_set() and all(t.is_alive() for t in owner.threads)
                    self.reply(200, json.dumps({"ready": ready and owner.trust_valid and (owner.enabled or owner.active_connections() > 0),
                                               "ipv6": owner.ipv6}).encode())
                else:
                    self.reply(404, b"{}")

            def do_POST(self):
                if self.path != "/route-status" or not self.authenticated():
                    self.reply(403, b"{}")
                    return
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= 1024:
                        raise ValueError()
                    value = json.loads(self.rfile.read(size))
                    state = value.get("state")
                    if state not in ("ready", "active", "stopped", "error"):
                        raise ValueError()
                    owner.route_state = state
                    if state == "error":
                        owner.detail = "Proxy routing could not start. Close the game and check administrator access or conflicting hostname mappings."
                    self.reply(200, b"{}")
                except (ValueError, TypeError):
                    self.reply(400, b"{}")
        self.http = ThreadingHTTPServer(("127.0.0.1", HEALTH_PORT), Handler)
        self.http.daemon_threads = True
        threading.Thread(target=self.http.serve_forever, daemon=True, name="proxy-health").start()

    def _start_watchdog(self):
        script = Path(__file__).resolve().parent / "proxy_route.ps1"
        args = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", str(script),
                "-OwnerPid", str(os.getpid()), "-OwnerStart", str(process_birth()), "-Nonce", self.nonce]
        argument_line = subprocess.list2cmdline(args).replace("'", "''")
        command = "$ErrorActionPreference='Stop'; Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -ArgumentList '" + argument_line + "'"
        result = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", command],
                                capture_output=True, timeout=60,
                                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
        if result.returncode:
            raise CloudError("Windows routing permission was declined or unavailable. Save & start to retry.")

    def close(self):
        self.enabled = False
        self.started = False
        self.stop_event.set()
        for listener in self.listeners:
            listener.close()
        self.listeners.clear()
        with self.lock:
            sessions = list(self.sessions)
        for session in sessions:
            session.close()
        for thread in self.threads:
            thread.join(timeout=2)
        self.threads.clear()
        if self.http is not None:
            self.http.shutdown()
            self.http.server_close()
            self.http = None

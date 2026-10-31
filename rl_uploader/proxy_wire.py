"""Small byte-preserving WebSocket relay, independent of Windows and TLS setup.

Only our own Match History response is consumed. Game messages, masking bytes,
fragment boundaries, and ping/pong frames are forwarded without rewriting.
"""

from dataclasses import dataclass
import json
import os
import socket
import struct
import threading
import time
import uuid

from .cloud_protocol import CloudError, MAX_JSON_BYTES, encode_json, sign

MAX_FRAME = 16 * 1024 * 1024
MAX_MESSAGE = 32 * 1024 * 1024


@dataclass
class Frame:
    raw: bytes
    payload: bytes
    opcode: int
    final: bool


def read_exact(stream, size, stop):
    data = bytearray()
    while len(data) < size:
        if stop.is_set():
            raise EOFError()
        try:
            part = stream.recv(size - len(data))
        except socket.timeout:
            continue
        if not part:
            raise EOFError()
        data.extend(part)
    return bytes(data)


def read_frame(stream, stop, *, masked):
    head = read_exact(stream, 2, stop)
    first, second = head
    final, opcode = bool(first & 128), first & 15
    if first & 112 or opcode not in (0, 1, 2, 8, 9, 10) or bool(second & 128) != masked:
        raise ValueError("Unsupported WebSocket frame")
    size = second & 127
    if size == 126:
        extra = read_exact(stream, 2, stop)
        size = struct.unpack("!H", extra)[0]
        head += extra
        if size < 126:
            raise ValueError("Non-canonical frame length")
    elif size == 127:
        extra = read_exact(stream, 8, stop)
        size = struct.unpack("!Q", extra)[0]
        head += extra
        if size < 65536:
            raise ValueError("Non-canonical frame length")
    if size > MAX_FRAME or (opcode >= 8 and (not final or size > 125)):
        raise ValueError("WebSocket frame exceeds limit")
    mask = read_exact(stream, 4, stop) if masked else b""
    data = read_exact(stream, size, stop)
    payload = bytes(value ^ mask[index % 4] for index, value in enumerate(data)) if masked else data
    return Frame(head + mask + data, payload, opcode, final)


def encode_frame(payload, *, opcode=1, final=True, masked=True):
    size = len(payload)
    first = (128 if final else 0) | opcode
    flag = 128 if masked else 0
    if size < 126:
        head = bytes((first, flag | size))
    elif size <= 65535:
        head = bytes((first, flag | 126)) + struct.pack("!H", size)
    else:
        head = bytes((first, flag | 127)) + struct.pack("!Q", size)
    if not masked:
        return head + payload
    mask = os.urandom(4)
    return head + mask + bytes(value ^ mask[index % 4] for index, value in enumerate(payload))


class RelaySession:
    def __init__(self, local, upstream, *, timeout=20):
        self.local = local
        self.upstream = upstream
        self.timeout = timeout
        self.stop = threading.Event()
        self.writer = threading.Condition()
        self.fragmented = False
        self.pending = {}
        self.prefix = "RLUploader_" + uuid.uuid4().hex + "_"
        self.sequence = 0
        self.reader = None

    def close(self):
        self.stop.set()
        with self.writer:
            self.writer.notify_all()
        for stream in (self.local, self.upstream):
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            stream.close()

    def run(self):
        self.reader = threading.Thread(target=self._game_to_server, daemon=True, name="game-relay-out")
        self.reader.start()
        try:
            self._server_to_game()
        except (OSError, EOFError, ValueError):
            pass
        finally:
            self.close()
            self.reader.join(timeout=3)

    def _game_to_server(self):
        size = 0
        try:
            while not self.stop.is_set():
                frame = read_frame(self.local, self.stop, masked=True)
                with self.writer:
                    if frame.opcode < 8:
                        if (frame.opcode == 0) != self.fragmented:
                            raise ValueError("Unexpected fragmentation")
                        size += len(frame.payload)
                        if size > MAX_MESSAGE:
                            raise ValueError("Game message exceeds limit")
                    self.upstream.sendall(frame.raw)
                    if frame.opcode < 8:
                        self.fragmented = not frame.final
                        if frame.final:
                            size = 0
                            self.writer.notify_all()
                if frame.opcode == 8:
                    return
        except (OSError, EOFError, ValueError):
            pass
        finally:
            self.close()

    def _server_to_game(self):
        frames, payloads, size = [], [], 0
        kind = None
        while not self.stop.is_set():
            frame = read_frame(self.upstream, self.stop, masked=False)
            if frame.opcode >= 8:
                self.local.sendall(frame.raw)
                if frame.opcode == 8:
                    return
                continue
            if frame.opcode == 0:
                if kind is None:
                    raise ValueError("Unexpected continuation")
            elif kind is not None:
                raise ValueError("Interleaved data messages")
            else:
                kind = frame.opcode
            size += len(frame.payload)
            if size > MAX_MESSAGE:
                raise ValueError("Server message exceeds limit")
            frames.append(frame.raw)
            payloads.append(frame.payload)
            if not frame.final:
                continue
            consumed = kind == 1 and self._consume_response(b"".join(payloads))
            if not consumed:
                for raw in frames:
                    self.local.sendall(raw)
            frames, payloads, size, kind = [], [], 0, None

    def _consume_response(self, payload):
        header, separator, body = payload.partition(b"\r\n\r\n")
        if not separator or len(header) > 65536:
            return False
        response_id = None
        for line in header.split(b"\r\n"):
            name, colon, value = line.partition(b":")
            if colon and name.strip().lower() == b"psyresponseid":
                response_id = value.strip().decode("ascii", errors="replace")
        if not response_id or not response_id.startswith(self.prefix):
            return False
        # Also discard late replies for our expired requests. They do not belong
        # to the game and must never be delivered to its response dispatcher.
        with self.writer:
            if response_id in self.pending:
                self.pending[response_id] = body if len(body) <= MAX_JSON_BYTES else b""
                self.writer.notify_all()
        return True

    def history(self, player, guard):
        deadline = time.monotonic() + self.timeout
        with self.writer:
            while self.fragmented and not self.stop.is_set() and guard():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise CloudError("Game message in progress. Capture will retry.")
                self.writer.wait(min(remaining, 0.25))
            if self.stop.is_set() or not guard():
                raise CloudError("Captured game session disconnected. No second login was attempted.")
            self.sequence += 1
            request_id = self.prefix + str(self.sequence)
            body = encode_json({"PlayerID": player})
            message = (f"PsyService: Matches/GetMatchHistory v1\r\nPsyRequestID: {request_id}\r\n"
                       f"PsySig: {sign(body)}\r\n\r\n").encode() + body
            self.pending[request_id] = None
            try:
                self.upstream.sendall(encode_frame(message))
                while self.pending[request_id] is None:
                    remaining = deadline - time.monotonic()
                    if self.stop.is_set() or not guard():
                        raise CloudError("Captured game session changed. Capture deferred.")
                    if remaining <= 0:
                        raise CloudError("Live Match History timed out. The game connection was kept.")
                    self.writer.wait(min(remaining, 0.25))
                raw = self.pending[request_id]
            except OSError:
                raise CloudError("Could not send through the captured session. Capture deferred.") from None
            finally:
                self.pending.pop(request_id, None)
        try:
            result = json.loads(raw)
            matches = result["Result"]["Matches"]
            if result.get("Error") or not isinstance(matches, list) or len(matches) > 100:
                raise ValueError()
            return player, matches
        except (ValueError, TypeError, KeyError):
            raise CloudError("Live Match History returned an unexpected response.") from None


class SessionBroker:
    """One current relay; a stale disconnect cannot clear a newer connection."""
    def __init__(self, player_id, guard):
        self.player_id = player_id
        self.guard = guard
        self.lock = threading.Lock()
        self.current = None

    def attach(self, session):
        with self.lock:
            self.current = session

    def detach(self, session):
        with self.lock:
            if self.current is session:
                self.current = None

    def connected(self):
        with self.lock:
            return self.current is not None and not self.current.stop.is_set()

    def history(self):
        with self.lock:
            session = self.current
        if session is None or not self.guard():
            raise CloudError("Waiting for Rocket League to connect through the proxy. No second login was attempted.")
        player = self.player_id()
        if not player:
            raise CloudError("Waiting for the game's local player identity in Launch.log.")
        def same_session():
            with self.lock:
                return self.current is session and self.guard()
        return session.history(player, same_session)

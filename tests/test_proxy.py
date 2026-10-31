import base64
import io
import json
import os
from pathlib import Path
import queue
import socket
import ssl
import struct
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock

from rl_uploader.cloud_protocol import CloudError
from rl_uploader.proxy import validate_handshake
from rl_uploader.proxy_wire import MAX_FRAME, RelaySession, SessionBroker, encode_frame, read_frame
from rl_uploader.config import Settings, load_settings, save_settings

PLAYER = "Steam|76561198000000000|0"


class MemorySocket(io.BytesIO):
    def recv(self, size):
        return self.read(size)


class FrameTests(unittest.TestCase):
    def test_frame_payload_and_original_wire_bytes_survive_roundtrip(self):
        for size in (0, 4, 125, 126, 65535, 65536):
            for masked in (False, True):
                with self.subTest(size=size, masked=masked):
                    payload = bytes(index % 256 for index in range(size))
                    raw = encode_frame(payload, opcode=2, masked=masked)
                    frame = read_frame(MemorySocket(raw), threading.Event(), masked=masked)
                    self.assertEqual(frame.raw, raw)
                    self.assertEqual(frame.payload, payload)

    def test_invalid_mask_control_and_length_are_rejected(self):
        for raw, masked in ((b"\x81\x00", True), (b"\x09\x00", False),
                            (b"\x81\x7f" + struct.pack("!Q", MAX_FRAME + 1), False),
                            (b"\xc1\x00", False), (b"\x81\x7e\x00\x01", False)):
            with self.subTest(raw=raw):
                with self.assertRaises(ValueError):
                    read_frame(MemorySocket(raw), threading.Event(), masked=masked)

    def test_only_expected_authenticated_websocket_handshake_is_accepted(self):
        headers = {"host": "ws.rlpp.psynet.gg", "connection": "Upgrade", "upgrade": "websocket",
                   "sec-websocket-version": "13", "sec-websocket-key": base64.b64encode(b"x" * 16).decode(),
                   "psytoken": "synthetic", "psysessionid": "synthetic"}
        validate_handshake("GET /ws/gc2 HTTP/1.1", headers)
        for field, value in (("host", "attacker.invalid"), ("psytoken", ""), ("sec-websocket-key", "invalid")):
            with self.assertRaises(ValueError):
                validate_handshake("GET /ws/gc2 HTTP/1.1", {**headers, field: value})
        with self.assertRaises(ValueError):
            validate_handshake("POST /rpc/Auth/AuthPlayer/v2 HTTP/1.1", headers)


class RelayTests(unittest.TestCase):
    def setUp(self):
        self.game, local = socket.socketpair()
        self.server, upstream = socket.socketpair()
        for stream in (self.game, self.server, local, upstream):
            stream.settimeout(0.2)
        self.relay = RelaySession(local, upstream, timeout=2)
        self.thread = threading.Thread(target=self.relay.run, daemon=True)
        self.thread.start()
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.relay.close()
        self.game.close()
        self.server.close()
        self.thread.join(timeout=3)

    def receive(self, stream, masked):
        stop = threading.Event()
        timer = threading.Timer(3, stop.set)
        timer.start()
        try:
            return read_frame(stream, stop, masked=masked)
        finally:
            timer.cancel()

    def query(self):
        result = queue.Queue()
        def run():
            try:
                result.put(self.relay.history(PLAYER, lambda: True))
            except Exception as error:
                result.put(error)
        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        return result, thread

    def answer(self, request):
        headers = request.payload.split(b"\r\n\r\n", 1)[0]
        request_id = next(line.split(b":", 1)[1].strip() for line in headers.split(b"\r\n")
                          if line.startswith(b"PsyRequestID:"))
        response = b"PsyResponseID: " + request_id + b'\r\n\r\n{"Result":{"Matches":[]}}'
        self.server.sendall(encode_frame(response, masked=False))

    def test_game_bytes_and_control_frames_are_forwarded_unchanged(self):
        raw = encode_frame(b"\xff\x00game data", opcode=1)
        self.game.sendall(raw)
        self.assertEqual(self.receive(self.server, True).raw, raw)
        pong = encode_frame(b"heartbeat", opcode=10, masked=False)
        self.server.sendall(pong)
        self.assertEqual(self.receive(self.game, False).raw, pong)

    def test_history_request_reuses_transport_and_response_is_hidden_from_game(self):
        result, thread = self.query()
        request = self.receive(self.server, True)
        self.assertIn(b"Matches/GetMatchHistory v1", request.payload)
        self.assertNotIn(b"AuthPlayer", request.payload)
        self.assertEqual(json.loads(request.payload.split(b"\r\n\r\n", 1)[1]), {"PlayerID": PLAYER})
        self.answer(request)
        self.assertEqual(result.get(timeout=3), (PLAYER, []))
        thread.join(timeout=3)
        with self.assertRaises(socket.timeout):
            self.game.recv(1)
        normal = encode_frame(b'PsyResponseID: Game_1\r\n\r\n{"Result":{}}', masked=False)
        self.server.sendall(normal)
        self.assertEqual(self.receive(self.game, False).raw, normal)

    def test_request_waits_for_fragmented_game_message(self):
        first = encode_frame(b"first", final=False)
        self.game.sendall(first)
        self.assertEqual(self.receive(self.server, True).raw, first)
        result, thread = self.query()
        with self.assertRaises(socket.timeout):
            self.server.recv(1)
        ping = encode_frame(b"ping", opcode=9)
        self.game.sendall(ping)
        self.assertEqual(self.receive(self.server, True).raw, ping)
        last = encode_frame(b"last", opcode=0)
        self.game.sendall(last)
        self.assertEqual(self.receive(self.server, True).raw, last)
        request = self.receive(self.server, True)
        self.answer(request)
        self.assertEqual(result.get(timeout=3), (PLAYER, []))
        thread.join(timeout=3)

    def test_fragmented_server_message_keeps_payload_and_frame_boundaries(self):
        first = encode_frame(b"PsyResponseID: Game_2\r\n\r\nfirst", final=False, masked=False)
        last = encode_frame(b"last", opcode=0, masked=False)
        self.server.sendall(first + last)
        self.assertEqual(self.receive(self.game, False).raw, first)
        self.assertEqual(self.receive(self.game, False).raw, last)

    def test_connection_loss_fails_pending_request_without_new_login(self):
        result, thread = self.query()
        self.receive(self.server, True)
        self.server.close()
        self.assertIsInstance(result.get(timeout=3), CloudError)
        thread.join(timeout=3)

    def test_request_timeout_keeps_game_connection_usable(self):
        self.relay.timeout = 0.3
        result, thread = self.query()
        request = self.receive(self.server, True)
        self.assertIsInstance(result.get(timeout=3), CloudError)
        self.assertFalse(self.relay.stop.is_set())
        self.answer(request)  # A late own response must still be swallowed.
        normal = encode_frame(b"Game notification", masked=False)
        self.server.sendall(normal)
        self.assertEqual(self.receive(self.game, False).raw, normal)
        thread.join(timeout=3)


class BrokerTests(unittest.TestCase):
    def test_old_disconnect_cannot_clear_replacement_connection(self):
        broker = SessionBroker(lambda: PLAYER, lambda: True)
        old, current = Mock(), Mock()
        current.stop.is_set.return_value = False
        current.history.return_value = (PLAYER, [])
        broker.attach(old)
        broker.attach(current)
        broker.detach(old)
        self.assertTrue(broker.connected())
        self.assertEqual(broker.history(), (PLAYER, []))
        broker.detach(current)
        with self.assertRaises(CloudError):
            broker.history()

    def test_unknown_player_never_requests_another_players_history(self):
        broker = SessionBroker(lambda: None, lambda: True)
        session = Mock()
        broker.attach(session)
        with self.assertRaises(CloudError):
            broker.history()
        session.history.assert_not_called()


class ConfigurationTests(unittest.TestCase):
    def test_old_settings_remain_after_session_and_modes_roundtrip(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            (folder / "settings.json").write_text(json.dumps({"replay_dir": str(folder), "cloud_capture": True}))
            self.assertEqual(load_settings(folder).capture_mode, "after_session")
            settings = Settings(str(folder), cloud_capture=True, capture_mode="periodic", capture_interval_minutes=30)
            save_settings(folder, settings)
            self.assertEqual(load_settings(folder), settings)
        for value in (0, True, 5, 31, "15"):
            with self.assertRaises(ValueError):
                Settings(str(Path.cwd()), capture_interval_minutes=value).validate()


@unittest.skipUnless(os.name == "nt", "Windows DPAPI")
class CertificateTests(unittest.TestCase):
    def test_scoped_certificate_and_tls_without_installing_system_trust(self):
        from rl_uploader.proxy_certs import HOST, crypto, prepare, server_context
        x509, hashes, serialization, rsa = crypto()
        with tempfile.TemporaryDirectory() as directory:
            data = Path(directory)
            manifest = prepare(data)
            root = data / "proxy"
            self.assertEqual(manifest["host"], HOST)
            ca = x509.load_der_x509_certificate((root / "root.cer").read_bytes())
            constraint = ca.extensions.get_extension_for_class(x509.NameConstraints)
            self.assertTrue(constraint.critical)
            self.assertEqual(constraint.value.permitted_subtrees, [x509.DNSName(HOST)])
            self.assertNotIn(b"PRIVATE KEY", (root / "leaf-key.dpapi").read_bytes())
            self.assertFalse((root / "ca-key.pem").exists())
            server = server_context(data)
            client = ssl.create_default_context(cadata=ssl.DER_cert_to_PEM_cert((root / "root.cer").read_bytes()))
            left, right = socket.socketpair()
            left.settimeout(3)
            right.settimeout(3)
            result = queue.Queue()
            def handshake():
                try:
                    with server.wrap_socket(left, server_side=True) as secured:
                        received = secured.recv(4)
                        secured.sendall(b"ok")
                        result.put(received)
                except Exception as error:
                    result.put(error)
            thread = threading.Thread(target=handshake)
            thread.start()
            with client.wrap_socket(right, server_hostname=HOST) as secured:
                secured.sendall(b"test")
                self.assertEqual(secured.recv(2), b"ok")
            self.assertEqual(result.get(timeout=4), b"test")
            thread.join(timeout=4)
            self.assertEqual(list(root.glob("*.encrypted-key")), [])


if __name__ == "__main__":
    unittest.main()

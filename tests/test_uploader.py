from email import policy
from email.parser import BytesParser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import struct
import tempfile
import threading
import unittest
from unittest.mock import patch
import urllib.error

from rl_uploader.api import BallchasingClient, MAX_RETRY_DELAY, Response, retry_after
from rl_uploader.config import Settings, load_settings, save_settings
from rl_uploader.engine import Engine, complete_replay
from rl_uploader.platform import InstanceLock, load_token, save_token, validate_token


def replay_bytes(suffix=b"match data"):
    header = b"test replay header"
    return struct.pack("<II", len(header), 0) + header + struct.pack("<II", len(suffix), 0) + suffix


class FakeClock:
    def __init__(self):
        self.now = 1800000000.0

    def __call__(self):
        return self.now

    def advance(self, seconds=6):
        self.now += seconds


class Client:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.calls = []

    def upload(self, data, visibility):
        self.calls.append((data, visibility))
        response = self.responses.pop(0) if self.responses else Response(201, {}, b'{"id":"test-replay-id"}')
        if isinstance(response, Exception):
            raise response
        return response


class EngineTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.demos = self.root / "Demos"
        self.demos.mkdir()
        self.data = self.root / "data"
        self.data.mkdir()
        self.clock = FakeClock()
        self.client = Client()
        self.engine = None

    def tearDown(self):
        if self.engine:
            self.engine.close()

    def start(self, include_existing=False, visibility="private"):
        if self.engine:
            self.engine.close()
        self.engine = Engine(self.data, Settings(str(self.demos), visibility), self.client,
                             clock=self.clock, include_existing=include_existing)
        return self.engine

    def discover(self, name="one.replay", data=None):
        path = self.demos / name
        path.write_bytes(replay_bytes() if data is None else data)
        self.engine.scan()
        self.clock.advance()
        self.engine.scan()
        return path

    def test_existing_files_excluded_until_imported(self):
        (self.demos / "old.replay").write_bytes(replay_bytes())
        self.start().scan()
        self.clock.advance()
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {})
        self.assertEqual(self.engine.snapshot()["ignored"], 1)
        self.engine.include_existing()
        self.engine.scan()
        self.clock.advance()
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_archive_survives_deleted_source_and_restart(self):
        self.start().scan()
        source = self.discover()
        source.unlink()
        self.start()
        self.assertTrue(self.engine.upload_one())
        self.assertEqual(self.client.calls, [(replay_bytes(), "private")])
        self.assertEqual(self.engine.snapshot()["counts"], {"uploaded": 1})
        self.assertEqual(len(list((self.data / "archive").glob("*.replay"))), 1)

    def test_new_files_while_app_closed_are_discovered(self):
        self.start().scan()
        self.engine.close()
        self.engine = None
        (self.demos / "offline.replay").write_bytes(replay_bytes())
        self.start().scan()
        self.clock.advance()
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_renamed_or_copied_file_deduplicated_by_content(self):
        self.start().scan()
        original = self.discover()
        self.engine.upload_one()
        original.rename(self.demos / "renamed.replay")
        self.engine.scan()
        self.clock.advance()
        self.engine.scan()
        self.assertFalse(self.engine.upload_one())
        self.assertEqual(len(self.client.calls), 1)

    def test_partial_write_waits_even_if_size_is_stable(self):
        self.start().scan()
        data = replay_bytes()
        source = self.discover(data=data[:-2])
        self.assertEqual(self.engine.snapshot()["counts"], {})
        source.write_bytes(data)
        self.engine.scan()
        self.clock.advance(4)
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {})
        self.clock.advance(2)
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_non_replay_and_empty_files_not_uploaded(self):
        self.start().scan()
        self.discover("notes.txt")
        self.discover("empty.replay", b"")
        self.discover("fake.replay", b"x" * 100)
        self.assertEqual(self.engine.snapshot()["counts"], {})

    def test_change_during_read_retried(self):
        self.start().scan()
        path = self.demos / "changing.replay"
        path.write_bytes(replay_bytes())
        self.engine.scan()
        self.clock.advance()
        from rl_uploader.engine import signature
        stable = signature(path)
        with patch("rl_uploader.engine.signature", side_effect=[stable, "changed"]):
            self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {})
        self.engine.scan()
        self.clock.advance()
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_source_is_never_written(self):
        self.start().scan()
        path = self.discover()
        before = (path.read_bytes(), path.stat().st_mtime_ns)
        self.engine.upload_one()
        self.assertEqual((path.read_bytes(), path.stat().st_mtime_ns), before)

    def test_api_duplicate_is_terminal_success(self):
        self.client = Client(Response(409, {}, b'{"id":"existing-replay"}'))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.start()
        self.clock.advance()
        self.assertFalse(self.engine.upload_one())
        row = self.engine.snapshot()["recent"][0]
        self.assertEqual((row["status"], row["replay_id"]), ("duplicate", "existing-replay"))

    def test_quota_cooldown_blocks_whole_queue_across_restarts(self):
        self.client = Client(Response(429, {"Retry-After": "3600"}, b""))
        self.start().scan()
        self.discover()
        self.discover("two.replay", replay_bytes(b"another match"))
        self.engine.upload_one()
        self.start()
        self.clock.advance(3599)
        self.assertFalse(self.engine.upload_one())
        self.clock.advance(1)
        self.assertTrue(self.engine.upload_one())
        self.assertEqual(len(self.client.calls), 2)

    def test_quota_without_retry_after_waits_one_hour(self):
        self.client = Client(Response(429, {}, b""))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.assertEqual(self.engine.snapshot()["cooldown"], self.clock() + 3600)

    def test_bad_token_blocks_until_reconnected(self):
        self.client = Client(Response(401, {}, b'{"error":"secret-token"}'))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.start()
        self.clock.advance(5000)
        self.assertFalse(self.engine.upload_one())
        self.assertNotIn("secret-token", str(self.engine.snapshot()))
        self.engine.state.set("blocked", "")
        self.assertTrue(self.engine.upload_one())

    def test_network_failure_retried_without_losing_job(self):
        self.client = Client(urllib.error.URLError("offline"))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})
        self.assertFalse(self.engine.upload_one())
        self.clock.advance()
        self.assertTrue(self.engine.upload_one())
        self.assertEqual(self.engine.snapshot()["counts"], {"uploaded": 1})

    def test_server_error_honors_retry_after(self):
        self.client = Client(Response(503, {"Retry-After": "120"}, b""))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.assertEqual(self.engine.snapshot()["cooldown"], self.clock() + 120)

    def test_rejected_replay_does_not_block_other_replays(self):
        self.client = Client(Response(400, {}, b""))
        self.start().scan()
        self.discover()
        self.discover("two.replay", replay_bytes(b"two"))
        self.engine.upload_one()
        self.clock.advance()
        self.engine.upload_one()
        self.assertEqual(self.engine.snapshot()["counts"], {"failed": 1, "uploaded": 1})

    def test_unexpected_response_blocks_queue(self):
        self.client = Client(Response(404, {}, b""))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.assertIn("404", self.engine.snapshot()["blocked"])
        self.clock.advance(5000)
        self.assertFalse(self.engine.upload_one())

    def test_malformed_success_retries_instead_of_losing_job(self):
        self.client = Client(Response(201, {}, b"<html>bad gateway</html>"))
        self.start().scan()
        self.discover()
        self.engine.upload_one()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_corrupt_archive_is_never_uploaded(self):
        self.start().scan()
        self.discover()
        next((self.data / "archive").glob("*.replay")).write_bytes(b"damaged")
        self.engine.upload_one()
        self.assertEqual(self.client.calls, [])
        self.assertEqual(self.engine.snapshot()["counts"], {"failed": 1})

    def test_crashed_inflight_upload_is_recovered(self):
        self.start().scan()
        self.discover()
        row = self.engine.snapshot()["recent"][0]
        self.engine.state.update(row["digest"], status="uploading")
        self.start()
        self.assertTrue(self.engine.upload_one())

    def test_visibility_captured_when_queued(self):
        self.start(visibility="private").scan()
        self.discover()
        self.start(visibility="public")
        self.engine.upload_one()
        self.assertEqual(self.client.calls[0][1], "private")

    def test_unavailable_directory_does_not_mark_baseline(self):
        self.demos.rmdir()
        self.start().scan()
        self.assertIn("unavailable", self.engine.scan_message)
        self.demos.mkdir()
        (self.demos / "old.replay").write_bytes(replay_bytes())
        self.engine.scan()
        self.assertEqual(self.engine.snapshot()["ignored"], 1)

    def test_single_instance_lock(self):
        lock = InstanceLock(self.data)
        try:
            with self.assertRaises(RuntimeError):
                InstanceLock(self.data)
        finally:
            lock.close()
        InstanceLock(self.data).close()


class ApiTests(unittest.TestCase):
    def test_multipart_real_http_transport_and_no_redirect_leak(self):
        requests = queue.Queue()

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["Content-Length"]))
                requests.put((self.path, self.headers, body))
                self.send_response(201)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"id":"roundtrip"}')

            def do_GET(self):
                requests.put((self.path, self.headers, b""))
                self.send_response(302)
                self.send_header("Location", "/must-not-follow")
                self.end_headers()

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with patch("rl_uploader.api.API_ROOT", f"http://127.0.0.1:{server.server_port}/api"):
                client = BallchasingClient("test-token-not-a-real-secret")
                data = replay_bytes()
                result = client.upload(data, "private")
                self.assertEqual(result.replay_id(), "roundtrip")
                path, headers, body = requests.get(timeout=1)
                self.assertEqual(path, "/api/v2/upload?visibility=private")
                self.assertEqual(headers["Authorization"], "test-token-not-a-real-secret")
                envelope = ("MIME-Version: 1.0\r\nContent-Type: " + headers["Content-Type"] + "\r\n\r\n").encode() + body
                message = BytesParser(policy=policy.default).parsebytes(envelope)
                parts = list(message.iter_parts())
                self.assertEqual(len(parts), 1)
                self.assertEqual(parts[0].get_param("name", header="content-disposition"), "file")
                self.assertEqual(parts[0].get_payload(decode=True), data)
                self.assertEqual(client.ping().status, 302)
                requests.get(timeout=1)
                self.assertTrue(requests.empty(), "Redirect must not receive credentials")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_retry_after_dates_and_malformed_values(self):
        self.assertEqual(retry_after({"retry-after": "10"}, 0, 60), 10)
        self.assertEqual(retry_after({"Retry-After": "Thu, 01 Jan 1970 00:02:00 GMT"}, 0, 60), 120)
        self.assertEqual(retry_after({"Retry-After": "1e99"}, 0, 60), MAX_RETRY_DELAY)
        for value in ("bad", "-1", "nan", "inf", ""):
            self.assertEqual(retry_after({"Retry-After": value}, 0, 60), 60)

    def test_untrusted_ids_and_json_are_rejected(self):
        for body in (b"[]", b"null", b"{}", b'{"id":"../../bad"}', b'{"id":42}', b"invalid"):
            self.assertEqual(Response(201, {}, body).replay_id(), "")


class SettingsTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows DPAPI")
    def test_token_encrypted_roundtrip_and_settings_have_no_secret(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {"BALLCHASING_TOKEN": ""}):
            folder = Path(temporary)
            save_token(folder, "non-production-test-token")
            self.assertNotIn(b"non-production-test-token", (folder / "token.dpapi").read_bytes())
            self.assertEqual(load_token(folder), "non-production-test-token")
            save_settings(folder, Settings(str(folder)))
            self.assertEqual(load_settings(folder).visibility, "private")
            self.assertNotIn("token", (folder / "settings.json").read_text())

    def test_token_header_injection_blocked(self):
        for token in ("", "abc\r\nX-Evil: yes", "has spaces", "a\x00b"):
            with self.assertRaises(ValueError):
                validate_token(token)

    def test_complete_replay_bounds(self):
        self.assertTrue(complete_replay(replay_bytes()))
        for data in (b"", b"x", b"\xff" * 100, replay_bytes()[:-1], replay_bytes() + b"junk"):
            self.assertFalse(complete_replay(data))


if __name__ == "__main__":
    unittest.main()

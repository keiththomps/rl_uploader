import hashlib
from dataclasses import replace
import io
import json
import os
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.error
from unittest.mock import Mock, patch

from rl_uploader.cloud import CloudCapture, download_replay, save_download
from rl_uploader.cloud_protocol import CloudError, EpicAuth, MatchHistoryClient, build_id, sign, request_json
from rl_uploader.config import Settings
from rl_uploader.engine import Engine
from test_uploader import FakeClock, replay_bytes

ACCOUNT = "a" * 32
MATCH = "B" * 32


class CloudTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.demos = self.root / "Demos"
        self.demos.mkdir()
        self.data = self.root / "data"
        self.data.mkdir()
        self.clock = FakeClock()
        self.engine = Engine(self.data, Settings(str(self.demos), cloud_capture=True), clock=self.clock)
        self.addCleanup(self.engine.close)
        self.engine.scan()
        self.stop = threading.Event()
        self.running = Mock(return_value=False)
        self.auth = Mock()
        self.auth.connected.return_value = True
        self.client = Mock()
        self.client.history.return_value = (ACCOUNT, [{"Match": {"MatchGUID": MATCH},
                                                     "ReplayUrl": "https://api.rlpp.psynet.gg/Match.replay?Signature=test"}])
        self.download = Mock(return_value=replay_bytes())
        self.capture = self.new_capture()

    def new_capture(self):
        return CloudCapture(self.engine, self.stop, clock=self.clock, running=self.running,
                            auth=self.auth, client=self.client, download=self.download)

    def run_capture(self):
        self.capture.tick()
        self.clock.advance(30)
        self.capture.tick()

    def test_game_running_or_unknown_never_logs_in(self):
        for status in (True, None):
            self.running.return_value = status
            self.run_capture()
        self.client.history.assert_not_called()
        self.download.assert_not_called()

    def test_capture_requires_full_thirty_seconds_closed(self):
        self.capture.tick()
        self.clock.advance(29)
        self.capture.tick()
        self.client.history.assert_not_called()
        self.clock.advance(1)
        self.capture.tick()
        self.client.history.assert_called_once()

    def test_capture_writes_actual_replay_and_queues_without_manual_save(self):
        self.run_capture()
        self.assertEqual((self.demos / (MATCH + ".replay")).read_bytes(), replay_bytes())
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})
        self.assertEqual(len(list(self.engine.archive.glob("*.replay"))), 1)
        self.assertNotIn("Signature", str(self.engine.snapshot()))

    def test_restart_keeps_dedup_and_poll_cooldown(self):
        self.run_capture()
        self.capture = self.new_capture()
        self.run_capture()
        self.client.history.assert_called_once()
        self.clock.advance(900)
        self.capture.tick()
        self.assertEqual(self.client.history.call_count, 2)
        self.download.assert_called_once()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_new_game_exit_triggers_fresh_capture_before_idle_poll(self):
        self.run_capture()
        self.running.return_value = True
        self.capture.tick()
        self.running.return_value = False
        self.run_capture()
        self.assertEqual(self.client.history.call_count, 2)

    def test_relaunch_aborts_before_download(self):
        self.client.history.side_effect = lambda: (self.running.configure_mock(return_value=True) or
                                                  (ACCOUNT, [{"Match": {"MatchGUID": MATCH}, "ReplayUrl": "unused"}]))
        self.run_capture()
        self.download.assert_not_called()
        self.assertEqual(self.engine.snapshot()["counts"], {})

    def test_download_failure_retries_without_marking_saved(self):
        self.download.side_effect = CloudError("Download unavailable")
        self.run_capture()
        self.assertEqual(self.engine.state.get("cloud_saved:" + ACCOUNT + ":" + MATCH), "")
        self.assertEqual(self.engine.snapshot()["counts"], {})
        self.capture.tick()
        self.download.assert_called_once()
        self.download.side_effect = None
        self.clock.advance(60)
        self.capture.tick()
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_auth_failure_persists_until_reconnect(self):
        self.client.history.side_effect = CloudError("Reconnect Epic", permanent=True)
        self.run_capture()
        self.capture = self.new_capture()
        self.clock.advance(5000)
        self.run_capture()
        self.client.history.assert_called_once()
        self.capture.reconnect()
        self.client.history.side_effect = None
        self.capture.tick()
        self.assertEqual(self.client.history.call_count, 2)

    def test_one_unavailable_replay_does_not_block_other_matches(self):
        other = "C" * 32
        self.client.history.return_value = (ACCOUNT, [
            {"Match": {"MatchGUID": other}, "ReplayUrl": "ready"},
            {"Match": {"MatchGUID": MATCH}, "ReplayUrl": "unavailable"},
        ])
        self.download.side_effect = [CloudError("Not available yet"), replay_bytes()]
        self.run_capture()
        self.assertFalse((self.demos / (MATCH + ".replay")).exists())
        self.assertTrue((self.demos / (other + ".replay")).exists())
        self.assertIn("1 unavailable", self.capture.message)
        self.download.side_effect = None
        self.download.return_value = replay_bytes(b"late replay")
        self.clock.advance(60)
        self.capture.tick()
        self.assertTrue((self.demos / (MATCH + ".replay")).exists())
        self.assertEqual(self.download.call_count, 3)
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 2})

    def test_partial_replay_never_saved_or_queued(self):
        self.download.return_value = replay_bytes()[:-1]
        self.run_capture()
        self.assertEqual(list(self.demos.iterdir()), [])
        self.assertEqual(self.engine.snapshot()["counts"], {})

    def test_filename_collision_never_overwrites_original(self):
        original = self.demos / (MATCH + ".replay")
        original.write_bytes(replay_bytes(b"different existing replay"))
        self.run_capture()
        self.assertEqual(original.read_bytes(), replay_bytes(b"different existing replay"))
        self.assertEqual(len(list(self.demos.glob("*.replay"))), 2)

    def test_save_is_idempotent_after_interrupted_queue_update(self):
        save_download(self.demos, MATCH, replay_bytes())
        self.run_capture()
        self.assertEqual(len(list(self.demos.glob("*.replay"))), 1)
        self.assertEqual(self.engine.snapshot()["counts"], {"queued": 1})

    def test_untrusted_match_path_rejected(self):
        with self.assertRaises(CloudError):
            save_download(self.demos, "../escape", replay_bytes())

    def test_capture_off_does_not_use_credentials_or_network(self):
        self.engine.settings = Settings(str(self.demos), cloud_capture=False)
        self.run_capture()
        self.auth.connected.assert_not_called()
        self.client.history.assert_not_called()

    def test_cloud_token_not_connected_no_network(self):
        self.auth.connected.return_value = False
        self.run_capture()
        self.client.history.assert_not_called()

    def test_twenty_match_limit_reported(self):
        self.client.history.return_value = (ACCOUNT, [{"Match": {"MatchGUID": f"{index:032x}"},
                                                     "ReplayUrl": "unused"} for index in range(20)])
        self.run_capture()
        self.assertIn("20-match limit", self.capture.message)

    def enable_live_capture(self, interval=15):
        self.engine.settings = replace(self.engine.settings, capture_mode="periodic",
                                       capture_interval_minutes=interval)
        self.running.return_value = True
        self.live_client = Mock()
        self.live_client.history.return_value = self.client.history.return_value
        self.capture.live_client = self.live_client

    def test_periodic_capture_uses_existing_session_without_epic_credentials(self):
        self.enable_live_capture()
        self.auth.connected.return_value = False
        self.capture.tick()
        self.live_client.history.assert_called_once()
        self.client.history.assert_not_called()
        self.auth.connected.assert_not_called()
        self.assertTrue((self.demos / (MATCH + ".replay")).exists())
        self.assertTrue(self.download.call_args.args[1]())

    def test_missing_live_session_never_falls_back_to_second_login(self):
        self.enable_live_capture()
        self.capture.live_client = None
        self.run_capture()
        self.client.history.assert_not_called()
        self.auth.connected.assert_not_called()
        self.download.assert_not_called()
        self.assertIn("No second login", self.capture.message)

    def test_failed_live_request_never_falls_back_to_second_login(self):
        self.enable_live_capture()
        self.live_client.history.side_effect = CloudError("Captured session disconnected")
        self.capture.tick()
        self.clock.advance(60)
        self.capture.tick()
        self.assertEqual(self.live_client.history.call_count, 2)
        self.client.history.assert_not_called()
        self.auth.connected.assert_not_called()
        self.download.assert_not_called()

    def test_periodic_mode_unknown_game_state_uses_neither_source(self):
        self.enable_live_capture()
        self.running.return_value = None
        self.run_capture()
        self.live_client.history.assert_not_called()
        self.client.history.assert_not_called()
        self.auth.connected.assert_not_called()

    def test_game_exit_switches_to_cloud_after_full_idle_delay(self):
        self.enable_live_capture()
        self.capture.tick()
        self.running.return_value = False
        self.capture.tick()
        self.clock.advance(29)
        self.capture.tick()
        self.client.history.assert_not_called()
        self.clock.advance(1)
        self.capture.tick()
        self.client.history.assert_called_once()
        self.live_client.history.assert_called_once()
        self.download.assert_called_once()  # One match stays deduplicated across transports.

    def test_game_exit_during_live_history_defers_downloads(self):
        self.enable_live_capture()
        self.live_client.history.side_effect = lambda: (self.running.configure_mock(return_value=False) or
                                                        self.client.history.return_value)
        self.capture.tick()
        self.download.assert_not_called()
        self.client.history.assert_not_called()

    def test_live_poll_interval_is_fifteen_or_thirty_minutes(self):
        for interval in (15, 30):
            with self.subTest(interval=interval):
                self.enable_live_capture(interval)
                self.capture.reconnect()
                self.capture.tick()
                self.clock.advance(interval * 60 - 1)
                self.capture.tick()
                self.live_client.history.assert_called_once()
                self.clock.advance(1)
                self.capture.tick()
                self.assertEqual(self.live_client.history.call_count, 2)

    def test_epic_auth_block_does_not_block_existing_session(self):
        self.enable_live_capture()
        self.engine.state.set("cloud_blocked", "Reconnect Epic")
        self.capture.tick()
        self.live_client.history.assert_called_once()
        self.client.history.assert_not_called()


class ProtocolTests(unittest.TestCase):
    def test_epic_real_pending_response_is_not_a_signin_failure(self):
        for fields in ({"errorCode": "errors.com.epicgames.account.oauth.authorization_pending"},
                       {"error": "authorization_pending"}):
            response = Mock(code=400)
            response.read.return_value = json.dumps(fields).encode()
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            opener = Mock()
            opener.open.return_value = response
            with patch("rl_uploader.cloud_protocol.urllib.request.build_opener", return_value=opener):
                with self.assertRaises(CloudError) as raised:
                    request_json("https://api.epicgames.dev/epic/oauth/v2/token", b"synthetic")
            self.assertEqual(raised.exception.code, "authorization_pending")
            self.assertFalse(raised.exception.permanent)

    def test_device_signin_waits_through_pending_then_saves_approved_token(self):
        auth = EpicAuth(Path("unused"))
        token = {"account_id": ACCOUNT, "access_token": "synthetic", "refresh_token": "synthetic"}
        auth._token = Mock(side_effect=[CloudError("Waiting", code="authorization_pending"), token])
        auth._save = Mock(return_value=token)
        cancel = Mock()
        cancel.wait.return_value = False
        cancel.is_set.return_value = False
        result = auth.finish({"device_code": "synthetic", "expires_in": 600, "interval": 5}, cancel)
        self.assertEqual(result, token)
        self.assertEqual(cancel.wait.call_count, 2)
        auth._save.assert_called_once_with(token)

    def test_device_expiry_is_explicit_and_does_not_save_credentials(self):
        auth = EpicAuth(Path("unused"))
        auth._token = Mock(side_effect=CloudError("Waiting", code="authorization_pending"))
        auth._save = Mock()
        cancel = Mock()
        cancel.wait.return_value = False
        with patch("rl_uploader.cloud_protocol.time.monotonic", side_effect=[0, 0, 601]):
            with self.assertRaises(CloudError) as raised:
                auth.finish({"device_code": "synthetic", "expires_in": 600}, cancel)
        self.assertEqual(raised.exception.code, "expired_token")
        auth._save.assert_not_called()

    def test_build_id_matches_installed_game_log(self):
        self.assertEqual(build_id("261013.63066.530866"), "-1885667689")

    def test_match_history_uses_only_authenticated_account_and_closes_session(self):
        auth = Mock()
        auth.token.return_value = {"account_id": ACCOUNT, "access_token": "synthetic-access-token"}
        request = Mock(return_value={"Result": {"PerConURLv2": "wss://ws.rlpp.psynet.gg/ws/gc2",
                                                "PsyToken": "test", "SessionID": "test-session"}})
        socket = Mock()
        socket.recv.return_value = 'PsyResponseID: PsyNetMessage_X_1\r\n\r\n{"Result":{"Matches":[]}}'
        connect = Mock(return_value=socket)
        client = MatchHistoryClient(auth, lambda: True, request=request, connect=connect)
        self.assertEqual(client.history(), (ACCOUNT, []))
        body = request.call_args.args[1]
        headers = request.call_args.args[2]
        self.assertEqual(headers["PsySig"], sign(body))
        self.assertEqual(json.loads(body)["PlayerID"], ACCOUNT)
        message = socket.send.call_args.args[0]
        self.assertIn("Matches/GetMatchHistory v1", message)
        self.assertIn("Epic|" + ACCOUNT + "|0", message)
        self.assertNotIn("synthetic-access-token", message)
        socket.close.assert_called_once()

    def test_game_guard_blocks_before_token_access(self):
        auth = Mock()
        client = MatchHistoryClient(auth, lambda: False)
        with self.assertRaises(CloudError):
            client.history()
        auth.token.assert_not_called()

    def test_session_cannot_redirect_credentials(self):
        auth = Mock()
        auth.token.return_value = {"account_id": ACCOUNT, "access_token": "test"}
        for url in ("ws://ws.rlpp.psynet.gg/ws", "wss://attacker.invalid/ws", "wss://ws.rlpp.psynet.gg.attacker.invalid/"):
            request = Mock(return_value={"Result": {"PerConURLv2": url, "PsyToken": "test", "SessionID": "test"}})
            connect = Mock()
            with self.assertRaises(CloudError):
                MatchHistoryClient(auth, lambda: True, request=request, connect=connect).history()
            connect.assert_not_called()

    def test_download_rejects_other_hosts_before_network(self):
        for url in ("file:///C:/test", "https://attacker.invalid/replay", "https://api.rlpp.psynet.gg.attacker.invalid/Match.replay"):
            with patch("rl_uploader.cloud.urllib.request.build_opener") as opener:
                with self.assertRaises(CloudError):
                    download_replay(url, lambda: True)
                opener.assert_not_called()

    def test_replay_download_upgrades_http_and_rejects_partial_body(self):
        for data, valid in ((replay_bytes(), True), (replay_bytes()[:-1], False)):
            response = Mock()
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            response.read.side_effect = [data, b""]
            opener = Mock()
            opener.open.return_value = response
            with patch("rl_uploader.cloud.urllib.request.build_opener", return_value=opener):
                url = "http://api.rlpp.psynet.gg/Match.replay?Signature=synthetic"
                if valid:
                    self.assertEqual(download_replay(url, lambda: True), data)
                else:
                    with self.assertRaises(CloudError):
                        download_replay(url, lambda: True)
                self.assertTrue(opener.open.call_args.args[0].startswith("https://api.rlpp.psynet.gg/"))

    def test_download_follows_only_the_psyonix_https_storage_redirect(self):
        destination = "https://storage.googleapis.com/psyonix-rl-replays/Prod/matches/20261031/014211/" + MATCH + ".replay?Signature=synthetic"
        response = Mock()
        response.__enter__ = Mock(return_value=response)
        response.__exit__ = Mock(return_value=False)
        response.read.side_effect = [replay_bytes(), b""]
        redirect = urllib.error.HTTPError("unused", 302, "Found", {"Location": destination}, io.BytesIO())
        opener = Mock()
        opener.open.side_effect = [redirect, response]
        with patch("rl_uploader.cloud.urllib.request.build_opener", return_value=opener):
            self.assertEqual(download_replay("https://api.rlpp.psynet.gg/Match.replay", lambda: True), replay_bytes())
        self.assertEqual(opener.open.call_args.args[0], destination)
        self.assertEqual(opener.open.call_count, 2)

    def test_download_rejects_redirect_to_other_bucket_or_non_tls(self):
        for destination in ("https://attacker.invalid/replay", "http://storage.googleapis.com/psyonix-rl-replays/Prod/matches/20261031/014211/" + MATCH + ".replay",
                            "https://storage.googleapis.com/unrelated-bucket/file.replay"):
            redirect = urllib.error.HTTPError("unused", 302, "Found", {"Location": destination}, io.BytesIO())
            opener = Mock()
            opener.open.side_effect = redirect
            with patch("rl_uploader.cloud.urllib.request.build_opener", return_value=opener):
                with self.assertRaises(CloudError):
                    download_replay("https://api.rlpp.psynet.gg/Match.replay", lambda: True)
            opener.open.assert_called_once()

    @unittest.skipUnless(os.name == "nt", "Windows DPAPI")
    def test_epic_credentials_encrypted_and_refresh_persisted(self):
        with tempfile.TemporaryDirectory() as temporary:
            token = {"account_id": ACCOUNT, "access_token": "fake-access", "refresh_token": "fake-refresh", "expires_in": 3600}
            auth = EpicAuth(Path(temporary))
            auth._save(token)
            raw = (Path(temporary) / "epic.dpapi").read_bytes()
            self.assertNotIn(b"fake-refresh", raw)
            self.assertEqual(auth.token()["access_token"], "fake-access")
            auth._save({**token, "expires_in": 0})
            auth.request = Mock(return_value={**token, "refresh_token": "rotated-refresh"})
            self.assertEqual(auth.token()["refresh_token"], "rotated-refresh")
            self.assertEqual(EpicAuth(Path(temporary)).token()["refresh_token"], "rotated-refresh")


if __name__ == "__main__":
    unittest.main()

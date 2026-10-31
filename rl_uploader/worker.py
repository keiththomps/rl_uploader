import queue
import threading

from .api import BallchasingClient
from .engine import Engine
from .platform import load_token
from .cloud import CloudCapture


class Worker(threading.Thread):
    def __init__(self, data_dir, settings, events, *, reconnect=False, reconnect_cloud=False, live_client=None):
        super().__init__(name="replay-uploader", daemon=False)
        self.data_dir = data_dir
        self.settings = settings
        self.events = events
        self.reconnect = reconnect
        self.reconnect_cloud = reconnect_cloud
        self.live_client = live_client
        self.stop_requested = threading.Event()
        self.commands = queue.Queue()

    def run(self):
        engine = None
        try:
            token = load_token(self.data_dir)
            engine = Engine(self.data_dir, self.settings, BallchasingClient(token) if token else None)
            if self.reconnect:
                engine.state.set("blocked", "")
            cloud = CloudCapture(engine, self.stop_requested,
                                 lambda message: self.events.put(("cloud_status", message)),
                                 live_client=self.live_client)
            if self.reconnect_cloud:
                cloud.reconnect()
            while not self.stop_requested.is_set():
                while True:
                    try:
                        command = self.commands.get_nowait()
                    except queue.Empty:
                        break
                    if command == "import":
                        engine.include_existing()
                    elif command == "retry":
                        engine.state.retry_failed()
                    elif command == "cloud_reconnect":
                        cloud.reconnect()
                engine.scan()
                cloud.tick()
                self.events.put(("snapshot", engine.snapshot()))
                if not self.stop_requested.is_set():
                    engine.upload_one()
                self.events.put(("snapshot", engine.snapshot()))
                self.stop_requested.wait(2)
        except Exception as error:
            # Do not send exceptions from the network or credential code to the
            # UI/logs: some libraries may embed request headers in exceptions.
            self.events.put(("error", f"Worker stopped ({type(error).__name__}). Check folder permissions, disk space, and saved token."))
        finally:
            if engine:
                engine.close()
            self.events.put(("stopped", None))

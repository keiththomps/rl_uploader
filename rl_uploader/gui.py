from datetime import datetime
import os
from pathlib import Path
import queue
import threading
import time
import tkinter as tk
from tkinter import filedialog, messagebox, ttk
import webbrowser

from .api import BallchasingClient
from .config import Settings, save_settings
from .platform import load_token, save_token, rocket_league_running
from .cloud_protocol import CloudError, EpicAuth
from .signin_dialog import EpicSignInDialog
from .proxy import CaptureProxy
from .proxy_certs import install_trust
from .state import State
from .worker import Worker

BG = "#101820"
CARD = "#1b2733"
FG = "#edf3f8"
MUTED = "#a5b6c5"
ACCENT = "#63d9c4"


class App:
    def __init__(self, root, data_dir: Path, settings: Settings, minimized=False):
        self.root = root
        self.data_dir = data_dir
        self.settings = settings
        self.worker = None
        self.proxy = CaptureProxy(data_dir)
        self.proxy_starting = False
        self.events = queue.Queue()
        self.restarting = False
        self.closing = False
        self.pending_reconnect = False
        self.pending_cloud_reconnect = False
        self.auth_cancel = threading.Event()
        self.auth_generation = 0
        self.auth_dialog = None
        self.snapshot = {"recent": [], "counts": {}}
        self.path = tk.StringVar(value=settings.replay_dir)
        self.token = tk.StringVar()
        self.visibility = tk.StringVar(value=settings.visibility)
        self.cloud_capture = tk.BooleanVar(value=settings.cloud_capture)
        self.capture_mode = tk.StringVar(value=settings.capture_mode)
        self.capture_interval = tk.StringVar(value=str(settings.capture_interval_minutes))
        self.proxy_status = tk.StringVar(value="Periodic capture uses the game's existing session and requires one-time proxy setup.")
        self.cloud_status = tk.StringVar(value="Connect Epic once, then save your settings to enable capture.")
        self.epic_status = tk.StringVar(value="Epic sign-in saved." if (data_dir / "epic.dpapi").exists()
                                       else "Epic is not connected.")
        self.status = tk.StringVar(value="Ready to set up")
        self.detail = tk.StringVar(value="Save a replay in Rocket League. This app takes care of the upload.")
        self.token_status = tk.StringVar(value="Token is encrypted for your Windows account when saved.")
        self.selection_detail = tk.StringVar(value="Select a replay to see its status. Double-click an uploaded replay to open it.")
        self._build()
        root.protocol("WM_DELETE_WINDOW", self.close)
        root.after(150, self.poll)
        state = State(data_dir)
        try:
            self.render(state.summary())
        finally:
            state.close()
        if (data_dir / "token.dpapi").exists():
            self.token_status.set("A token is saved. Leave the field blank to keep it.")
        if os.environ.get("BALLCHASING_TOKEN", "").strip():
            self.token_status.set("BALLCHASING_TOKEN is set in the environment and overrides the saved token.")
        if (data_dir / "settings.json").exists():
            self.start()
        if minimized:
            root.iconify()

    def _build(self):
        root = self.root
        root.title("RL Uploader")
        root.geometry("1080x1000")
        root.minsize(940, 930)
        root.configure(bg=BG)
        style = ttk.Style(root)
        style.theme_use("clam")
        style.configure(".", font=("Segoe UI", 10), background=BG, foreground=FG)
        style.configure("TFrame", background=BG)
        style.configure("Card.TFrame", background=CARD)
        style.configure("TLabel", background=BG, foreground=FG)
        style.configure("Muted.TLabel", foreground=MUTED)
        style.configure("Card.TLabel", background=CARD)
        style.configure("Title.TLabel", font=("Segoe UI", 27, "bold"))
        style.configure("TButton", background="#304354", foreground=FG, padding=(12, 7), borderwidth=0)
        style.map("TButton", background=[("active", "#40596e"), ("disabled", "#23303c")],
                  foreground=[("disabled", "#758695")])
        style.configure("Accent.TButton", background=ACCENT, foreground=BG, font=("Segoe UI", 10, "bold"))
        style.map("Accent.TButton", background=[("active", "#8ce9d9"), ("disabled", "#384b51")])
        style.configure("TEntry", fieldbackground="#0f1c27", foreground=FG, padding=7, insertcolor=FG)
        style.configure("TRadiobutton", background=BG, foreground=FG)
        style.configure("Cloud.TCheckbutton", background=CARD, foreground=FG)
        style.map("Cloud.TCheckbutton", background=[("active", CARD)])
        style.configure("Cloud.TRadiobutton", background=CARD, foreground=FG)
        style.map("Cloud.TRadiobutton", background=[("active", CARD)])
        style.configure("Capture.TCombobox", fieldbackground="#0f1c27", background="#304354", foreground=FG)
        style.map("Capture.TCombobox", fieldbackground=[("readonly", "#0f1c27")], foreground=[("readonly", FG)],
                  selectbackground=[("readonly", "#304354")], selectforeground=[("readonly", FG)])
        style.map("TRadiobutton", background=[("active", BG)])
        style.configure("Treeview", background=CARD, fieldbackground=CARD, foreground=FG,
                        rowheight=31, borderwidth=0, font=("Segoe UI", 10))
        style.configure("Treeview.Heading", background="#273746", foreground=MUTED, padding=7)
        style.map("Treeview", background=[("selected", "#355765")])

        main = ttk.Frame(root, padding=24)
        main.pack(fill="both", expand=True)
        main.columnconfigure(0, weight=1)
        main.rowconfigure(5, weight=1)
        header = ttk.Frame(main)
        header.grid(row=0, column=0, sticky="ew", pady=(0, 14))
        ttk.Label(header, text="RL Uploader", style="Title.TLabel").pack(side="left")
        ttk.Label(header, text="SAVED REPLAYS  →  BALLCHASING", foreground=ACCENT).pack(side="right", pady=10)

        notice = ttk.Frame(main, style="Card.TFrame", padding=(16, 12))
        notice.grid(row=1, column=0, sticky="ew", pady=(0, 14))
        ttk.Label(notice, text="Save replays automatically, then upload when quota allows.",
                  style="Card.TLabel", font=("Segoe UI", 11, "bold")).pack(anchor="w")
        ttk.Label(notice, text="Choose after-session capture, or periodic capture through the game's existing connection.\n"
                  "Limited to the latest 20 matches within 30 days. Uses an unofficial API; Epic approval is unverified.",
                  style="Card.TLabel", foreground=MUTED).pack(anchor="w", pady=(5, 0))
        cloud_controls = ttk.Frame(notice, style="Card.TFrame")
        cloud_controls.pack(fill="x", pady=(10, 0))
        ttk.Checkbutton(cloud_controls, text="Enable automatic replay saving", variable=self.cloud_capture,
                        style="Cloud.TCheckbutton").pack(side="left")
        self.connect_button = ttk.Button(cloud_controls, text="Connect Epic", command=self.connect_epic)
        self.connect_button.pack(side="right")
        modes = ttk.Frame(notice, style="Card.TFrame")
        modes.pack(fill="x", pady=(10, 0))
        ttk.Radiobutton(modes, text="After game closes", variable=self.capture_mode, value="after_session",
                        style="Cloud.TRadiobutton").pack(side="left", padx=(0, 16))
        ttk.Radiobutton(modes, text="Periodically", variable=self.capture_mode, value="periodic",
                        style="Cloud.TRadiobutton").pack(side="left", padx=(0, 6))
        ttk.Combobox(modes, textvariable=self.capture_interval, values=("15", "30"), state="readonly", width=3,
                     style="Capture.TCombobox").pack(side="left")
        ttk.Label(modes, text=" minutes", style="Card.TLabel").pack(side="left")
        self.proxy_setup_button = ttk.Button(modes, text="Set up periodic capture", command=self.setup_periodic)
        self.proxy_setup_button.pack(side="right")
        ttk.Label(notice, textvariable=self.proxy_status, style="Card.TLabel", foreground=MUTED,
                  wraplength=960).pack(anchor="w", pady=(8, 0))
        ttk.Label(notice, textvariable=self.epic_status, style="Card.TLabel", foreground=MUTED,
                  wraplength=960).pack(anchor="w", pady=(8, 0))
        ttk.Label(notice, textvariable=self.cloud_status, style="Card.TLabel", foreground=ACCENT,
                  wraplength=960).pack(anchor="w", pady=(8, 0))

        setup = ttk.Frame(main)
        setup.grid(row=2, column=0, sticky="ew", pady=(0, 14))
        setup.columnconfigure(1, weight=1)
        ttk.Label(setup, text="Replay folder").grid(row=0, column=0, sticky="w", padx=(0, 14))
        ttk.Entry(setup, textvariable=self.path).grid(row=0, column=1, sticky="ew", padx=(0, 8))
        ttk.Button(setup, text="Browse…", command=self.browse).grid(row=0, column=2, sticky="ew")
        ttk.Label(setup, text="Upload token").grid(row=1, column=0, sticky="w", pady=(10, 0))
        ttk.Entry(setup, textvariable=self.token, show="•").grid(row=1, column=1, sticky="ew", padx=(0, 8), pady=(10, 0))
        ttk.Button(setup, text="Get token ↗", command=lambda: webbrowser.open("https://ballchasing.com/upload")).grid(row=1, column=2, sticky="ew", pady=(10, 0))
        ttk.Label(setup, textvariable=self.token_status, style="Muted.TLabel").grid(row=2, column=1, columnspan=2, sticky="w", pady=(5, 9))
        ttk.Label(setup, text="New uploads").grid(row=3, column=0, sticky="w")
        radios = ttk.Frame(setup)
        radios.grid(row=3, column=1, sticky="w")
        for value, label in (("private", "Private"), ("unlisted", "Unlisted"), ("public", "Public")):
            ttk.Radiobutton(radios, text=label, variable=self.visibility, value=value).pack(side="left", padx=(0, 20))
        self.test_button = ttk.Button(setup, text="Test token", command=self.test_token)
        self.test_button.grid(row=3, column=2, sticky="ew")

        controls = ttk.Frame(main)
        controls.grid(row=3, column=0, sticky="ew", pady=(0, 14))
        self.start_button = ttk.Button(controls, text="Save & start", command=self.save_and_start, style="Accent.TButton")
        self.start_button.pack(side="left", padx=(0, 8))
        self.pause_button = ttk.Button(controls, text="Pause", command=self.pause)
        self.pause_button.pack(side="left", padx=(0, 8))
        self.import_button = ttk.Button(controls, text="Import existing replays", command=self.import_existing)
        self.import_button.pack(side="left", padx=(0, 8))
        self.retry_button = ttk.Button(controls, text="Retry failed", command=lambda: self.command("retry"))
        self.retry_button.pack(side="left")
        ttk.Button(controls, text="Local archive", command=self.open_archive).pack(side="right")

        status = ttk.Frame(main, style="Card.TFrame", padding=14)
        status.grid(row=4, column=0, sticky="ew", pady=(0, 14))
        ttk.Label(status, textvariable=self.status, style="Card.TLabel", foreground=ACCENT,
                  font=("Segoe UI", 12, "bold")).pack(anchor="w")
        ttk.Label(status, textvariable=self.detail, style="Card.TLabel", foreground=MUTED,
                  wraplength=960).pack(anchor="w", pady=(5, 0))
        self.counts = tk.StringVar(value="0 queued     0 uploaded     0 already uploaded     0 failed")
        ttk.Label(status, textvariable=self.counts, style="Card.TLabel").pack(anchor="w", pady=(10, 0))

        table_frame = ttk.Frame(main)
        table_frame.grid(row=5, column=0, sticky="nsew")
        table_frame.rowconfigure(0, weight=1)
        table_frame.columnconfigure(0, weight=1)
        self.table = ttk.Treeview(table_frame, columns=("file", "status", "visibility", "added"), show="headings", height=5)
        for column, label, width in (("file", "REPLAY FILE", 420), ("status", "STATUS", 160),
                                     ("visibility", "VISIBILITY", 100), ("added", "ADDED", 130)):
            self.table.heading(column, text=label)
            self.table.column(column, width=width, minwidth=70, stretch=(column == "file"))
        self.table.grid(row=0, column=0, sticky="nsew")
        scroll = ttk.Scrollbar(table_frame, orient="vertical", command=self.table.yview)
        scroll.grid(row=0, column=1, sticky="ns")
        self.table.configure(yscrollcommand=scroll.set)
        self.table.bind("<<TreeviewSelect>>", self.show_selection)
        self.table.bind("<Double-1>", self.open_replay)
        ttk.Label(main, textvariable=self.selection_detail, style="Muted.TLabel", wraplength=970).grid(row=6, column=0, sticky="w", pady=(8, 9))
        ttk.Label(main, text="Minimize to keep saving. With the live proxy, close Rocket League before exiting this app.",
                  style="Muted.TLabel").grid(row=7, column=0, sticky="w")

    def browse(self):
        chosen = filedialog.askdirectory(initialdir=self.path.get(), title="Choose Rocket League's Demos folder")
        if chosen:
            self.path.set(chosen)

    def save_and_start(self):
        try:
            if not self.path.get().strip():
                raise ValueError("Choose your Rocket League replay folder.")
            settings = Settings(str(Path(self.path.get()).expanduser().resolve()), self.visibility.get(), self.cloud_capture.get(),
                                self.capture_mode.get(), int(self.capture_interval.get()))
            settings.validate()
            if not Path(settings.replay_dir).is_dir():
                raise ValueError("The replay folder does not exist. Choose your Rocket League Demos folder.")
            archive = (self.data_dir / "archive").resolve()
            folder = Path(settings.replay_dir)
            if archive == folder or archive.is_relative_to(folder) or folder.is_relative_to(archive):
                raise ValueError("Choose the game's replay folder, separate from this app's data folder.")
            new_token = bool(self.token.get().strip())
            reconnect = new_token or self.pending_reconnect
            if new_token:
                save_token(self.data_dir, self.token.get())
                self.token.set("")
                self.token_status.set("Token saved, encrypted for your Windows account.")
            save_settings(self.data_dir, settings)
            self.settings = settings
        except (ValueError, OSError, RuntimeError) as error:
            messagebox.showerror("Could not save settings", str(error))
            return
        self.pending_reconnect = reconnect
        if self.worker and self.worker.is_alive():
            self.restarting = True
            self.worker.stop_requested.set()
            self.status.set("Applying settings after the current upload…")
            self.start_button.configure(state="disabled")
        else:
            self.start()

    def start(self):
        self.configure_proxy()
        self.worker = Worker(self.data_dir, self.settings, self.events, reconnect=self.pending_reconnect,
                             reconnect_cloud=self.pending_cloud_reconnect, live_client=self.proxy)
        self.pending_reconnect = False
        self.pending_cloud_reconnect = False
        self.worker.start()
        self.start_button.configure(state="normal")
        self.status.set("Starting watcher…")

    def configure_proxy(self):
        self.proxy.enabled = self.settings.cloud_capture and self.settings.capture_mode == "periodic"
        if not self.proxy.enabled:
            if self.proxy.active_connections() == 0 and rocket_league_running() is False:
                self.proxy.close()
            return
        if self.proxy.started or self.proxy_starting:
            return
        self.proxy_starting = True
        self.proxy_status.set("Preparing the live-session proxy…")
        def prepare():
            try:
                self.proxy.start()
                self.events.put(("proxy_ready", self.proxy.detail))
            except CloudError as error:
                self.events.put(("proxy_ready", str(error)))
        threading.Thread(target=prepare, daemon=True, name="proxy-start").start()

    def setup_periodic(self):
        if rocket_league_running() is not False:
            messagebox.showinfo("Close Rocket League first", "Proxy setup must finish before Rocket League starts.")
            return
        if not messagebox.askokcancel("Set up periodic capture",
                "This experimental mode intercepts Rocket League's encrypted WebSocket connection.\n\n"
                "Setup trusts a local certificate limited to ws.rlpp.psynet.gg. Starting periodic mode requests "
                "Windows administrator access for a temporary hostname route and a boot-time cleanup task.\n\n"
                "It does not patch the game or disable EAC. Epic/EAC approval is unverified. Continue?"):
            return
        self.proxy_setup_button.configure(state="disabled")
        self.proxy_status.set("Preparing and trusting the dedicated proxy certificate…")
        def prepare():
            try:
                install_trust(self.data_dir)
                self.events.put(("proxy_setup", (True, "Certificate ready. Choose Periodically and Save & start, then approve Windows routing access.")))
            except CloudError as error:
                self.events.put(("proxy_setup", (False, str(error))))
            except Exception as error:
                self.events.put(("proxy_setup", (False, f"Proxy setup failed ({type(error).__name__}). Run Setup periodic capture.cmd first.")))
        threading.Thread(target=prepare, daemon=True, name="proxy-setup").start()

    def pause(self):
        self.restarting = False
        if self.worker and self.worker.is_alive():
            self.worker.stop_requested.set()
            self.status.set("Pausing after the current upload…")

    def command(self, command):
        if self.worker and self.worker.is_alive() and not self.worker.stop_requested.is_set():
            self.worker.commands.put(command)

    def import_existing(self):
        self.command("import")
        self.detail.set("Existing replays will be archived and queued using the saved visibility setting.")

    def test_token(self):
        try:
            token = os.environ.get("BALLCHASING_TOKEN", "").strip() or self.token.get().strip() or load_token(self.data_dir)
            if not token:
                raise ValueError("Paste an upload token first.")
            client = BallchasingClient(token)
        except (ValueError, OSError, RuntimeError) as error:
            messagebox.showerror("Token unavailable", str(error))
            return
        self.test_button.configure(state="disabled")
        self.token_status.set("Checking token with ballchasing…")

        def check():
            try:
                response = client.ping()
                message = "Token accepted. Save & start to use it." if response.status == 200 else f"Token check returned HTTP {response.status}."
                self.events.put(("token_test", (response.status == 200, message)))
            except Exception:
                self.events.put(("token_test", (False, "Could not reach ballchasing. Check your connection.")))
        threading.Thread(target=check, daemon=True, name="token-check").start()

    def connect_epic(self):
        if rocket_league_running() is not False:
            messagebox.showinfo("Close Rocket League first", "Close Rocket League before connecting Epic for replay capture.")
            return
        self.auth_cancel.set()
        self.auth_generation += 1
        generation = self.auth_generation
        self.auth_cancel = threading.Event()
        cancel = self.auth_cancel
        self.connect_button.configure(state="disabled")
        self.epic_status.set("Preparing Epic device sign-in…")
        if self.auth_dialog is None:
            self.auth_dialog = EpicSignInDialog(self.root, self.connect_epic, self.cancel_epic_auth)
        self.auth_dialog.preparing()

        def authenticate():
            try:
                auth = EpicAuth(self.data_dir)
                device = auth.begin()
                if cancel.is_set():
                    return
                self.events.put(("epic_device", (generation, device)))
                auth.finish(device, cancel)
                self.events.put(("epic_connected", (generation, None)))
            except CloudError as error:
                self.events.put(("epic_error", (generation, str(error))))
            except Exception as error:
                self.events.put(("epic_error", (generation, f"Epic sign-in failed ({type(error).__name__}). Get a new code to retry.")))
        threading.Thread(target=authenticate, daemon=True, name="epic-sign-in").start()

    def cancel_epic_auth(self):
        self.auth_cancel.set()
        self.auth_generation += 1  # Ignore any late result from the cancelled attempt.
        if self.auth_dialog is not None:
            self.auth_dialog.destroy()
            self.auth_dialog = None
        self.connect_button.configure(state="normal")
        self.epic_status.set("Epic sign-in cancelled. Choose Connect Epic when ready.")

    def render(self, snapshot):
        self.snapshot = snapshot
        counts = snapshot["counts"]
        queued = counts.get("queued", 0) + counts.get("uploading", 0)
        self.counts.set(f"{queued} queued     {counts.get('uploaded', 0)} uploaded     "
                        f"{counts.get('duplicate', 0)} already uploaded     {counts.get('failed', 0)} failed")
        existing = set(self.table.get_children())
        for row in snapshot["recent"]:
            values = (row["filename"], row["status"].capitalize(), row["visibility"].capitalize(),
                      datetime.fromtimestamp(row["created"]).strftime("%b %d, %H:%M"))
            if row["digest"] in existing:
                self.table.item(row["digest"], values=values)
                existing.remove(row["digest"])
            else:
                self.table.insert("", "end", iid=row["digest"], values=values)
        for key in existing:
            self.table.delete(key)
        for index, row in enumerate(snapshot["recent"]):
            self.table.move(row["digest"], "", index)
        if not self.worker or self.worker.stop_requested.is_set():
            return
        if snapshot.get("blocked"):
            self.status.set("Uploads paused · action needed")
            self.detail.set(snapshot["blocked"])
        elif not snapshot.get("has_token"):
            self.status.set("Watching locally · add a token to upload")
            self.detail.set(snapshot.get("scan_message", "") + ". New replays are kept in the local archive.")
        elif snapshot.get("cooldown", 0) > time.time() + 5:
            until = datetime.fromtimestamp(snapshot["cooldown"]).strftime("%b %d, %H:%M:%S")
            self.status.set("Waiting to retry · local archiving stays active")
            self.detail.set(f"Next upload attempt after {until}. Account quotas and retry delays survive restarts.")
        else:
            self.status.set(snapshot.get("scan_message", "Watching for saved replays"))
            ignored = snapshot.get("ignored", 0)
            self.detail.set(f"{ignored} existing files excluded. Import existing replays to include them. "
                            "New files upload automatically once fully saved.")
        self.show_selection()

    def show_selection(self, event=None):
        selection = self.table.selection()
        if not selection:
            return
        row = next((r for r in self.snapshot["recent"] if r["digest"] == selection[0]), None)
        if row:
            self.selection_detail.set(row["message"] + f" · {row['attempts']} upload attempt(s)")

    def open_replay(self, event=None):
        selection = self.table.selection()
        row = next((r for r in self.snapshot["recent"] if selection and r["digest"] == selection[0]), None)
        if row and row["replay_id"]:
            webbrowser.open("https://ballchasing.com/replay/" + row["replay_id"])

    def open_archive(self):
        path = self.data_dir / "archive"
        path.mkdir(exist_ok=True)
        if os.name == "nt":
            os.startfile(path)
        else:
            webbrowser.open(path.as_uri())

    def poll(self):
        for _ in range(50):
            try:
                kind, payload = self.events.get_nowait()
            except queue.Empty:
                break
            if kind == "snapshot":
                self.render(payload)
            elif kind == "error":
                self.status.set("Uploader stopped · action needed")
                self.detail.set(payload)
            elif kind == "token_test":
                accepted, message = payload
                self.token_status.set(message)
                self.test_button.configure(state="normal")
                if accepted:
                    self.pending_reconnect = True
            elif kind == "cloud_status":
                self.cloud_status.set(payload)
            elif kind == "proxy_ready":
                self.proxy_starting = False
                self.proxy_status.set(payload)
                if not self.proxy.enabled:
                    self.proxy.close()
                elif self.proxy.started:
                    self.command("cloud_reconnect")
            elif kind == "proxy_setup":
                accepted, message = payload
                self.proxy_setup_button.configure(state="normal")
                self.proxy_status.set(message)
                if accepted:
                    self.configure_proxy()
            elif kind == "epic_device":
                generation, device = payload
                if generation == self.auth_generation and not self.closing and not self.auth_cancel.is_set():
                    self.epic_status.set("Waiting for sign-in on Epic's website…")
                    if self.auth_dialog is not None:
                        self.auth_dialog.set_device(device)
            elif kind in ("epic_connected", "epic_error"):
                generation, message = payload
                if generation != self.auth_generation or self.closing:
                    continue
                self.connect_button.configure(state="normal")
                if kind == "epic_connected":
                    if self.auth_dialog is not None:
                        self.auth_dialog.destroy()
                        self.auth_dialog = None
                    self.epic_status.set("Epic connected. Enable cloud capture and choose Save & start.")
                    self.pending_cloud_reconnect = True
                    self.command("cloud_reconnect")
                else:
                    self.epic_status.set(message)
                    if self.auth_dialog is not None:
                        self.auth_dialog.show_error(message)
            elif kind == "stopped":
                if self.closing:
                    self.root.destroy()
                    return
                if self.restarting:
                    self.restarting = False
                    self.start()
                elif not self.status.get().startswith("Uploader stopped"):
                    self.status.set("Paused")
                    self.detail.set("Save & start resumes watching. Queued replays and the local archive are kept.")
        running = bool(self.worker and self.worker.is_alive() and not self.worker.stop_requested.is_set())
        if self.proxy.started and not self.proxy_starting:
            if self.proxy.broker.connected():
                self.proxy_status.set("Live game session captured. Replay requests reuse this connection.")
            elif self.proxy.route_state == "error":
                self.proxy_status.set(self.proxy.detail)
            elif self.proxy.route_state in ("ready", "active"):
                self.proxy_status.set("Proxy ready. Launch Rocket League to capture its connection.")
        for button in (self.pause_button, self.import_button, self.retry_button):
            button.configure(state="normal" if running else "disabled")
        if not self.closing:
            self.root.after(150, self.poll)
        elif self.worker and self.worker.is_alive():
            self.root.after(150, self.poll)
        else:
            self.root.destroy()

    def close(self):
        if self.proxy.started and (self.proxy.enabled or self.proxy.active_connections() > 0) and rocket_league_running() is not False:
            messagebox.showinfo("Keeping the game connection open",
                                "Rocket League is using or may connect through this proxy. The uploader will minimize "
                                "to preserve that connection. Close Rocket League before exiting the uploader.")
            self.root.iconify()
            return
        self.closing = True
        self.proxy.enabled = False
        self.proxy.close()
        self.auth_cancel.set()
        self.restarting = False
        if self.worker and self.worker.is_alive():
            self.worker.stop_requested.set()
            self.status.set("Finishing the current upload before closing…")
            self.start_button.configure(state="disabled")
        else:
            self.root.destroy()

"""Persistent device sign-in UI; a pending or expired grant never closes it."""

import time
import tkinter as tk
from tkinter import ttk
import webbrowser


class EpicSignInDialog(tk.Toplevel):
    def __init__(self, parent, retry, cancel):
        super().__init__(parent)
        self.title("Connect your linked Epic account")
        self.geometry("640x370")
        self.minsize(640, 370)
        self.configure(bg="#101820")
        self.protocol("WM_DELETE_WINDOW", cancel)
        self.deadline = None
        self.uri = None
        self.timer = None
        self.code = tk.StringVar(value="Requesting code…")
        self.status = tk.StringVar(value="Contacting Epic…")
        frame = ttk.Frame(self, padding=22)
        frame.pack(fill="both", expand=True)
        ttk.Label(frame, text="Sign in on Epic's website", font=("Segoe UI", 17, "bold")).pack(anchor="w")
        ttk.Label(frame, text="Use the Epic account linked to your Steam Rocket League profile.\n"
                  "Enter this code on Epic's site; enter your password only on Epic's site.",
                  style="Muted.TLabel").pack(anchor="w", pady=(10, 12))
        ttk.Label(frame, textvariable=self.code, foreground="#63d9c4",
                  font=("Consolas", 25, "bold")).pack(anchor="w")
        buttons = ttk.Frame(frame)
        buttons.pack(fill="x", pady=(12, 0))
        self.open_button = ttk.Button(buttons, text="Open Epic sign-in ↗", command=self.open_site,
                                      style="Accent.TButton", state="disabled")
        self.open_button.pack(side="left", padx=(0, 10))
        self.retry_button = ttk.Button(buttons, text="Get a new code", command=retry, state="disabled")
        self.retry_button.pack(side="left")
        ttk.Button(buttons, text="Cancel", command=cancel).pack(side="right")
        ttk.Label(frame, textvariable=self.status, style="Muted.TLabel",
                  wraplength=590).pack(anchor="w", pady=(15, 0))
        ttk.Label(frame, text="This window stays open if the code expires. You can request another.",
                  style="Muted.TLabel", wraplength=590).pack(anchor="w", pady=(12, 0))

    def preparing(self):
        self.deadline = None
        self.uri = None
        self.code.set("Requesting code…")
        self.status.set("Contacting Epic…")
        self.open_button.configure(state="disabled")
        self.retry_button.configure(state="disabled")

    def set_device(self, device):
        self.code.set(device["user_code"])
        self.uri = device["verification_uri"]
        self.deadline = time.monotonic() + min(1800, max(1, int(device.get("expires_in", 600))))
        self.open_button.configure(state="normal")
        self.retry_button.configure(state="normal")
        self.update_countdown()

    def update_countdown(self):
        if self.timer is not None:
            self.after_cancel(self.timer)
            self.timer = None
        if self.deadline is None:
            return
        remaining = max(0, int(self.deadline - time.monotonic()))
        if remaining == 0:
            self.show_error("This code expired. Choose Get a new code to continue.")
            return
        self.status.set(f"Waiting for you to finish on Epic's website. Code expires in {remaining // 60}:{remaining % 60:02d}.")
        self.timer = self.after(1000, self.update_countdown)

    def show_error(self, message):
        self.deadline = None
        self.status.set(message + " Your sign-in window will stay open.")
        self.open_button.configure(state="disabled")
        self.retry_button.configure(state="normal")

    def open_site(self):
        if self.uri:
            webbrowser.open(self.uri)

    def destroy(self):
        if self.timer is not None:
            self.after_cancel(self.timer)
            self.timer = None
        super().destroy()

"""Windows integration limited to Documents discovery and secret storage."""

import ctypes
from ctypes import wintypes
import os
from pathlib import Path


def documents_folder() -> Path:
    if os.name == "nt":
        # CSIDL_PERSONAL resolves Windows' redirected Documents (including OneDrive).
        buffer = ctypes.create_unicode_buffer(32768)
        result = ctypes.windll.shell32.SHGetFolderPathW(None, 5, None, 0, buffer)
        if result == 0 and buffer.value:
            return Path(buffer.value)
    return Path.home() / "Documents"


def replay_folder() -> Path:
    return documents_folder() / "My Games" / "Rocket League" / "TAGame" / "Demos"


def default_data_dir() -> Path:
    return Path(os.environ.get("LOCALAPPDATA", Path.home() / ".local" / "share")) / "RLUploader"


def rocket_league_running() -> bool | None:
    """List executable names using Toolhelp; never open/read the game process.

    None means detection failed. Callers must defer cloud login in that case.
    """
    if os.name != "nt":
        return None

    class ProcessEntry(ctypes.Structure):
        _fields_ = [("dwSize", wintypes.DWORD), ("cntUsage", wintypes.DWORD),
                    ("th32ProcessID", wintypes.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
                    ("th32ModuleID", wintypes.DWORD), ("cntThreads", wintypes.DWORD),
                    ("th32ParentProcessID", wintypes.DWORD), ("pcPriClassBase", wintypes.LONG),
                    ("dwFlags", wintypes.DWORD), ("szExeFile", wintypes.WCHAR * 260)]

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
    kernel.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    kernel.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessEntry)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.CreateToolhelp32Snapshot(2, 0)
    if handle == ctypes.c_void_p(-1).value:
        return None
    try:
        entry = ProcessEntry()
        entry.dwSize = ctypes.sizeof(entry)
        if not kernel.Process32FirstW(handle, ctypes.byref(entry)):
            return None
        while True:
            if entry.szExeFile.lower() in ("rocketleague.exe", "rocketleague_eac.exe"):
                return True
            if not kernel.Process32NextW(handle, ctypes.byref(entry)):
                return False if ctypes.get_last_error() == 18 else None
    finally:
        kernel.CloseHandle(handle)


class _Blob(ctypes.Structure):
    _fields_ = [("size", wintypes.DWORD), ("data", ctypes.POINTER(ctypes.c_ubyte))]


def _dpapi(data: bytes, encrypt: bool) -> bytes:
    if os.name != "nt":
        raise RuntimeError("Saved tokens require Windows. Use BALLCHASING_TOKEN elsewhere.")
    crypt = ctypes.WinDLL("crypt32", use_last_error=True)
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    array = (ctypes.c_ubyte * len(data)).from_buffer_copy(data)
    source = _Blob(len(data), array)
    target = _Blob()
    operation = crypt.CryptProtectData if encrypt else crypt.CryptUnprotectData
    operation.argtypes = [ctypes.POINTER(_Blob), ctypes.c_void_p,
                          ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p,
                          wintypes.DWORD, ctypes.POINTER(_Blob)]
    operation.restype = wintypes.BOOL
    kernel.LocalFree.argtypes = [ctypes.c_void_p]
    kernel.LocalFree.restype = ctypes.c_void_p
    # CRYPTPROTECT_UI_FORBIDDEN; default scope is the current Windows user.
    if not operation(ctypes.byref(source), None, None, None, None, 1, ctypes.byref(target)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        return ctypes.string_at(target.data, target.size)
    finally:
        kernel.LocalFree(target.data)


def save_token(data_dir: Path, token: str) -> None:
    token = validate_token(token)
    encrypted = _dpapi(token.encode("utf-8"), True)
    target = data_dir / "token.dpapi"
    temporary = target.with_suffix(".tmp")
    temporary.write_bytes(encrypted)
    temporary.replace(target)


def load_token(data_dir: Path) -> str:
    environment = os.environ.get("BALLCHASING_TOKEN", "").strip()
    if environment:
        return validate_token(environment)
    target = data_dir / "token.dpapi"
    if not target.exists():
        return ""
    return validate_token(_dpapi(target.read_bytes(), False).decode("utf-8"))


def validate_token(token: str) -> str:
    token = token.strip()
    if not token or len(token) > 4096 or not token.isascii() or any(ord(c) < 33 or ord(c) > 126 for c in token):
        raise ValueError("Enter a valid ballchasing API token (without spaces or line breaks).")
    return token


class InstanceLock:
    """OS-owned lock: automatically released even if the app crashes."""

    def __init__(self, data_dir: Path):
        self.stream = (data_dir / "instance.lock").open("a+b")
        self.stream.seek(0, 2)
        if self.stream.tell() == 0:
            self.stream.write(b"0")
            self.stream.flush()
        self.stream.seek(0)
        try:
            if os.name == "nt":
                import msvcrt
                msvcrt.locking(self.stream.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(self.stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            self.stream.close()
            raise RuntimeError("RL Uploader is already running with this data folder.") from error

    def close(self):
        self.stream.close()

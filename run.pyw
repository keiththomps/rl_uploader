"""Double-click with Python installed, or use Start RL Uploader.cmd."""
from pathlib import Path
from rl_uploader.__main__ import main

if __name__ == "__main__":
    raise SystemExit(main(["--data-dir", str(Path(__file__).resolve().parent / ".local")]))

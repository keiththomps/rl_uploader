import argparse
import json
from pathlib import Path
import queue
import sys

from .config import load_settings
from .platform import InstanceLock, default_data_dir, replay_folder


def main(argv=None):
    parser = argparse.ArgumentParser(description="Standalone Rocket League replay file uploader")
    parser.add_argument("--data-dir", type=Path, default=default_data_dir(), help="Settings, encrypted token, queue and archive folder")
    parser.add_argument("--headless", action="store_true", help="Run the configured watcher in the console")
    parser.add_argument("--minimized", action="store_true", help="Start the desktop window minimized")
    parser.add_argument("--doctor", action="store_true", help="Show read-only local discovery; never uploads")
    proxy_action = parser.add_mutually_exclusive_group()
    proxy_action.add_argument("--prepare-proxy", action="store_true", help="Generate the scoped proxy certificate without installing trust")
    proxy_action.add_argument("--install-proxy", action="store_true", help="Trust the scoped proxy certificate for this Windows user")
    proxy_action.add_argument("--remove-proxy-trust", action="store_true", help="Remove only this app's proxy certificate")
    args = parser.parse_args(argv)
    if args.doctor:
        folder = replay_folder()
        print(json.dumps({"replay_folder": str(folder), "folder_exists": folder.is_dir(),
                          "replay_count": sum(1 for p in folder.glob("*") if p.suffix.lower() == ".replay"),
                          "data_folder": str(args.data_dir.resolve()),
                          "automatic_game_saving": "After-session Epic capture, or opt-in periodic capture through a local session proxy.",
                          "game_memory_access": False, "game_file_patching": False}, indent=2))
        return 0
    args.data_dir = args.data_dir.resolve()
    args.data_dir.mkdir(parents=True, exist_ok=True)
    if args.prepare_proxy or args.install_proxy or args.remove_proxy_trust:
        from .proxy_certs import prepare, install_trust, remove_trust
        from .platform import rocket_league_running
        from .cloud_protocol import CloudError
        try:
            if rocket_league_running() is not False:
                raise CloudError("Close Rocket League before changing proxy setup.")
            if args.remove_proxy_trust:
                remove_trust(args.data_dir)
                print("Removed this app's proxy certificate from the current user's trusted roots.")
            else:
                result = install_trust(args.data_dir) if args.install_proxy else prepare(args.data_dir)
                print(json.dumps(result, indent=2))
            return 0
        except Exception as error:
            print(str(error) if isinstance(error, CloudError) else f"Proxy setup failed ({type(error).__name__}).", file=sys.stderr)
            return 1
    try:
        lock = InstanceLock(args.data_dir)
    except RuntimeError as error:
        print(str(error), file=sys.stderr)
        if not args.headless:
            from tkinter import Tk, messagebox
            root = Tk()
            root.withdraw()
            messagebox.showinfo("RL Uploader", str(error))
            root.destroy()
        return 1
    try:
        settings = load_settings(args.data_dir)
        if args.headless:
            if not (args.data_dir / "settings.json").exists():
                print("Open the desktop app and choose Save & start first.", file=sys.stderr)
                return 1
            from .worker import Worker
            from .proxy import CaptureProxy
            from .platform import rocket_league_running
            import time
            events = queue.Queue()
            proxy = CaptureProxy(args.data_dir)
            if settings.cloud_capture and settings.capture_mode == "periodic":
                try:
                    proxy.start()
                except Exception:
                    print(proxy.detail, file=sys.stderr)
            worker = Worker(args.data_dir, settings, events, live_client=proxy)
            worker.start()
            last_status = None
            last_cloud_status = None
            try:
                while worker.is_alive() or not events.empty():
                    try:
                        kind, payload = events.get(timeout=1)
                    except queue.Empty:
                        continue
                    if kind == "error":
                        print(payload, file=sys.stderr)
                        return 1
                    if kind == "cloud_status" and payload != last_cloud_status:
                        print(json.dumps({"cloud_capture": payload}), flush=True)
                        last_cloud_status = payload
                    if kind == "snapshot":
                        status = (payload["counts"], payload["blocked"], payload["scan_message"], payload["has_token"])
                        if status != last_status:
                            print(json.dumps({"counts": status[0], "blocked": status[1],
                                              "watcher": status[2], "token_available": status[3]}), flush=True)
                            last_status = status
            except KeyboardInterrupt:
                pass
            finally:
                worker.stop_requested.set()
                worker.join()
                if proxy.started and rocket_league_running() is not False:
                    print("Capture stopped. Keeping the game relay alive until Rocket League closes. Ctrl+C again disconnects it.", flush=True)
                    try:
                        while rocket_league_running() is not False:
                            time.sleep(1)
                    except KeyboardInterrupt:
                        pass
                proxy.close()
        else:
            from tkinter import Tk
            from .gui import App
            root = Tk()
            App(root, args.data_dir, settings, args.minimized)
            root.mainloop()
        return 0
    except (ValueError, TypeError, OSError) as error:
        message = f"Could not load uploader data ({type(error).__name__}). Check settings.json and folder permissions."
        print(message, file=sys.stderr)
        if not args.headless:
            from tkinter import Tk, messagebox
            root = Tk()
            root.withdraw()
            messagebox.showerror("RL Uploader", message)
            root.destroy()
        return 1
    finally:
        lock.close()


if __name__ == "__main__":
    raise SystemExit(main())

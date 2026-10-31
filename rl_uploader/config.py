from dataclasses import asdict, dataclass
import json
from pathlib import Path

from .platform import replay_folder


@dataclass(frozen=True)
class Settings:
    replay_dir: str
    visibility: str = "private"
    cloud_capture: bool = False
    capture_mode: str = "after_session"
    capture_interval_minutes: int = 15

    def validate(self) -> None:
        if not isinstance(self.cloud_capture, bool):
            raise ValueError("Cloud capture must be true or false.")
        if self.capture_mode not in ("after_session", "periodic"):
            raise ValueError("Capture mode must be after_session or periodic.")
        if type(self.capture_interval_minutes) is not int or self.capture_interval_minutes not in (15, 30):
            raise ValueError("Capture interval must be 15 or 30 minutes.")
        if self.visibility not in ("private", "unlisted", "public"):
            raise ValueError("Visibility must be private, unlisted, or public.")
        if not self.replay_dir or not Path(self.replay_dir).is_absolute():
            raise ValueError("Choose an absolute replay folder path.")


def load_settings(data_dir: Path) -> Settings:
    path = data_dir / "settings.json"
    if not path.exists():
        return Settings(str(replay_folder()))
    value = json.loads(path.read_text(encoding="utf-8"))
    settings = Settings(**value)
    settings.validate()
    return settings


def save_settings(data_dir: Path, settings: Settings) -> None:
    settings.validate()
    target = data_dir / "settings.json"
    temporary = target.with_suffix(".tmp")
    temporary.write_text(json.dumps(asdict(settings), indent=2) + "\n", encoding="utf-8")
    temporary.replace(target)

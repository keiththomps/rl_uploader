"""Durable queue and discovery state, owned by the worker thread."""

from pathlib import Path
import sqlite3


class State:
    def __init__(self, data_dir: Path):
        self.connection = sqlite3.connect(data_dir / "queue.sqlite3", timeout=10)
        self.connection.row_factory = sqlite3.Row
        self.connection.execute("PRAGMA journal_mode=WAL")
        self.connection.executescript("""
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS observed (
                path TEXT PRIMARY KEY, signature TEXT NOT NULL, ignored INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
                digest TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                size INTEGER NOT NULL,
                created REAL NOT NULL,
                visibility TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'queued',
                attempts INTEGER NOT NULL DEFAULT 0,
                next_try REAL NOT NULL DEFAULT 0,
                replay_id TEXT NOT NULL DEFAULT '',
                message TEXT NOT NULL DEFAULT 'Waiting to upload'
            );
        """)
        with self.connection:
            self.connection.execute("UPDATE jobs SET status='queued' WHERE status='uploading'")

    def close(self):
        self.connection.close()

    def get(self, key: str, default="") -> str:
        row = self.connection.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        return row[0] if row else default

    def set(self, key: str, value) -> None:
        with self.connection:
            self.connection.execute("INSERT OR REPLACE INTO meta VALUES (?,?)", (key, str(value)))

    def observed(self, path: str, signature: str) -> bool:
        row = self.connection.execute("SELECT signature FROM observed WHERE path=?", (path,)).fetchone()
        return bool(row and row[0] == signature)

    def remember(self, path: str, signature: str, ignored=False) -> None:
        with self.connection:
            self.connection.execute("INSERT OR REPLACE INTO observed VALUES (?,?,?)",
                                    (path, signature, int(ignored)))

    def baseline(self, key: str, files: list[tuple[str, str]]) -> None:
        with self.connection:
            self.connection.executemany("INSERT OR IGNORE INTO observed VALUES (?,?,1)", files)
            self.connection.execute("INSERT OR REPLACE INTO meta VALUES (?, '1')", (key,))

    def include_existing(self, folder: Path) -> None:
        # Match parent paths in Python, avoiding SQL LIKE wildcard/path issues.
        rows = self.connection.execute("SELECT path FROM observed WHERE ignored=1").fetchall()
        with self.connection:
            self.connection.executemany("DELETE FROM observed WHERE path=?",
                                        [(r[0],) for r in rows if Path(r[0]).parent == folder])

    def enqueue(self, digest: str, filename: str, size: int, now: float, visibility: str,
                source: str, signature: str) -> bool:
        with self.connection:
            result = self.connection.execute(
                "INSERT OR IGNORE INTO jobs (digest,filename,size,created,visibility) VALUES (?,?,?,?,?)",
                (digest, filename, size, now, visibility))
            self.connection.execute("INSERT OR REPLACE INTO observed VALUES (?,?,0)", (source, signature))
            return result.rowcount == 1

    def due(self, now: float):
        return self.connection.execute(
            "SELECT * FROM jobs WHERE status='queued' AND next_try<=? ORDER BY created LIMIT 1", (now,)
        ).fetchone()

    def update(self, digest: str, **fields) -> None:
        allowed = {"status", "attempts", "next_try", "replay_id", "message"}
        if not fields or not fields.keys() <= allowed:
            raise ValueError("Invalid job update")
        with self.connection:
            self.connection.execute("UPDATE jobs SET " + ",".join(f"{k}=?" for k in fields) +
                                    " WHERE digest=?", (*fields.values(), digest))

    def retry_failed(self) -> None:
        with self.connection:
            self.connection.execute("UPDATE jobs SET status='queued',next_try=0,message='Retry requested' "
                                    "WHERE status='failed'")

    def summary(self) -> dict:
        counts = dict(self.connection.execute("SELECT status,count(*) FROM jobs GROUP BY status").fetchall())
        recent = [dict(r) for r in self.connection.execute("SELECT * FROM jobs ORDER BY created DESC LIMIT 100")]
        ignored = self.connection.execute("SELECT count(*) FROM observed WHERE ignored=1").fetchone()[0]
        return {"counts": counts, "recent": recent, "ignored": ignored,
                "blocked": self.get("blocked"), "cooldown": float(self.get("cooldown", "0"))}

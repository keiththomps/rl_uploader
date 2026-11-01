use crate::{Result, db_error};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path, time::Duration};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Job {
    pub digest: String,
    pub filename: String,
    pub size: u64,
    pub created: f64,
    pub visibility: String,
    pub status: String,
    pub attempts: u32,
    pub next_try: f64,
    pub replay_id: String,
    pub message: String,
}
fn job(row: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        digest: row.get(0)?,
        filename: row.get(1)?,
        size: row.get(2)?,
        created: row.get(3)?,
        visibility: row.get(4)?,
        status: row.get(5)?,
        attempts: row.get(6)?,
        next_try: row.get(7)?,
        replay_id: row.get(8)?,
        message: row.get(9)?,
    })
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    pub counts: BTreeMap<String, u64>,
    pub recent: Vec<Job>,
    pub ignored: u64,
    pub blocked: String,
    pub cooldown: f64,
}
pub struct State {
    pub connection: Connection,
}
impl State {
    pub fn open(dir: &Path) -> Result<Self> {
        let connection = Connection::open(dir.join("queue.sqlite3")).map_err(db_error)?;
        connection
            .busy_timeout(Duration::from_secs(10))
            .map_err(db_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS observed (path TEXT PRIMARY KEY, signature TEXT NOT NULL, ignored INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS jobs (digest TEXT PRIMARY KEY, filename TEXT NOT NULL, size INTEGER NOT NULL, created REAL NOT NULL, visibility TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'queued', attempts INTEGER NOT NULL DEFAULT 0, next_try REAL NOT NULL DEFAULT 0, replay_id TEXT NOT NULL DEFAULT '', message TEXT NOT NULL DEFAULT 'Waiting to upload');
            UPDATE jobs SET status='queued' WHERE status='uploading';").map_err(db_error)?;
        Ok(Self { connection })
    }
    pub fn get(&self, key: &str) -> Result<String> {
        self.connection
            .query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
            .optional()
            .map(|v| v.unwrap_or_default())
            .map_err(db_error)
    }
    pub fn number(&self, key: &str) -> Result<f64> {
        Ok(self
            .get(key)?
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .unwrap_or(0.0))
    }
    pub fn set(&self, key: &str, value: impl ToString) -> Result<()> {
        self.connection
            .execute(
                "INSERT OR REPLACE INTO meta VALUES (?,?)",
                params![key, value.to_string()],
            )
            .map_err(db_error)?;
        Ok(())
    }
    pub fn observed(&self, path: &str, sig: &str) -> Result<bool> {
        Ok(self
            .connection
            .query_row("SELECT signature FROM observed WHERE path=?", [path], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(db_error)?
            .as_deref()
            == Some(sig))
    }
    pub fn baseline(&mut self, key: &str, files: &[(String, String)]) -> Result<()> {
        let tx = self.connection.transaction().map_err(db_error)?;
        for (path, sig) in files {
            tx.execute(
                "INSERT OR IGNORE INTO observed VALUES (?,?,1)",
                params![path, sig],
            )
            .map_err(db_error)?;
        }
        tx.execute("INSERT OR REPLACE INTO meta VALUES (?, '1')", [key])
            .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
    pub fn include_existing(&self, folder: &Path) -> Result<()> {
        let mut query = self
            .connection
            .prepare("SELECT path FROM observed WHERE ignored=1")
            .map_err(db_error)?;
        let rows = query
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        for path in rows {
            if Path::new(&path).parent() == Some(folder) {
                self.connection
                    .execute("DELETE FROM observed WHERE path=?", [path])
                    .map_err(db_error)?;
            }
        }
        Ok(())
    }
    pub fn enqueue(&mut self, job: &Job, source: &str, sig: &str) -> Result<bool> {
        let tx = self.connection.transaction().map_err(db_error)?;
        let count = tx.execute("INSERT OR IGNORE INTO jobs (digest,filename,size,created,visibility) VALUES (?,?,?,?,?)", params![job.digest, job.filename, job.size, job.created, job.visibility]).map_err(db_error)?;
        tx.execute(
            "INSERT OR REPLACE INTO observed VALUES (?,?,0)",
            params![source, sig],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(count == 1)
    }
    pub fn due(&self, now: f64) -> Result<Option<Job>> {
        self.connection
            .query_row(
                "SELECT * FROM jobs WHERE status='queued' AND next_try<=? ORDER BY created LIMIT 1",
                [now],
                job,
            )
            .optional()
            .map_err(db_error)
    }
    pub fn update(&self, j: &Job) -> Result<()> {
        self.connection.execute("UPDATE jobs SET status=?,attempts=?,next_try=?,replay_id=?,message=? WHERE digest=?", params![j.status, j.attempts, j.next_try, j.replay_id, j.message, j.digest]).map_err(db_error)?;
        Ok(())
    }
    pub fn retry_failed(&self) -> Result<()> {
        self.connection.execute("UPDATE jobs SET status='queued',next_try=0,message='Retry requested' WHERE status='failed'", []).map_err(db_error)?;
        Ok(())
    }
    pub fn summary(&self) -> Result<Summary> {
        let counts = self
            .connection
            .prepare("SELECT status,count(*) FROM jobs GROUP BY status")
            .map_err(db_error)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()
            .map_err(db_error)?;
        let recent = self
            .connection
            .prepare("SELECT * FROM jobs ORDER BY created DESC LIMIT 100")
            .map_err(db_error)?
            .query_map([], job)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        Ok(Summary {
            counts,
            recent,
            ignored: self
                .connection
                .query_row("SELECT count(*) FROM observed WHERE ignored=1", [], |r| {
                    r.get(0)
                })
                .map_err(db_error)?,
            blocked: self.get("blocked")?,
            cooldown: self.number("cooldown")?,
        })
    }

    pub fn next_upload(&self) -> Result<Option<f64>> {
        if !self.get("blocked")?.is_empty() {
            return Ok(None);
        }
        let next: Option<f64> = self
            .connection
            .query_row(
                "SELECT min(next_try) FROM jobs WHERE status='queued'",
                [],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        Ok(next.map(|n| n.max(self.number("cooldown").unwrap_or(0.0))))
    }
}

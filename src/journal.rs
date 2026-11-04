//! The SQLite move log that makes `undo` possible.
//!
//! Every move is written *before* it happens (status `pending`, with the exact
//! destination path), then marked `done`. If the process dies between the
//! rename and the update, the row is still there with the right destination,
//! and `undo` checks the disk to see whether the move actually happened.
//! A move is never attempted unless its row was written successfully.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = "
CREATE TABLE runs (
    id          INTEGER PRIMARY KEY,
    kind        TEXT    NOT NULL,          -- 'sort' or 'watch'
    started_at  INTEGER NOT NULL           -- Unix time, milliseconds, UTC
);
CREATE TABLE moves (
    id          INTEGER PRIMARY KEY,
    run_id      INTEGER NOT NULL REFERENCES runs(id),
    moved_at    INTEGER NOT NULL,          -- Unix time, milliseconds, UTC
    source      TEXT    NOT NULL,          -- original absolute path
    dest        TEXT    NOT NULL,          -- final absolute path
    size        INTEGER NOT NULL,          -- bytes, to recognise the file at undo time
    mtime       INTEGER NOT NULL,          -- last-write time, ms, likewise
    reason      TEXT    NOT NULL,          -- e.g. 'rule \"Images\"'
    status      TEXT    NOT NULL,          -- pending | done | failed | undone
    error       TEXT,                      -- set when status = failed
    undone_at   INTEGER,                   -- set when status = undone
    undone_to   TEXT                       -- where undo put it (may have a suffix)
);
CREATE INDEX moves_run ON moves(run_id);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pending,
    Done,
    Failed,
    Undone,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Undone => "undone",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "pending" => Status::Pending,
            "done" => Status::Done,
            "failed" => Status::Failed,
            "undone" => Status::Undone,
            other => bail!("unknown move status {other:?} in log"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct MoveRecord {
    pub id: i64,
    pub run_id: i64,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
    pub status: Status,
}

/// What we know about a file when it is about to be moved.
pub struct NewMove<'a> {
    pub run_id: i64,
    pub source: &'a Path,
    pub dest: &'a Path,
    pub size: u64,
    pub mtime: SystemTime,
    pub reason: &'a str,
}

pub struct Journal {
    conn: Connection,
}

pub fn unix_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

pub fn from_unix_ms(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms as u64)
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

/// Paths are stored as text. Windows file names are UTF-16 and may in theory
/// contain unpaired surrogates that have no UTF-8 form; such files are refused
/// rather than logged lossily, because a lossy path could not be undone.
pub fn path_text(p: &Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("path is not valid Unicode: {}", p.display()))
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening move log {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // The watcher and a manual `tao sort`/`undo` may use the log at the
        // same time. WAL lets readers and one writer coexist, and the busy
        // timeout makes a second writer wait instead of failing immediately.
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;

        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                conn.execute_batch(SCHEMA)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            SCHEMA_VERSION => {}
            v => bail!("move log has schema version {v}, this build understands {SCHEMA_VERSION}"),
        }
        Ok(Self { conn })
    }

    pub fn begin_run(&self, kind: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (kind, started_at) VALUES (?1, ?2)",
            params![kind, unix_ms(SystemTime::now())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Records a move that is about to be attempted. Returns its id.
    pub fn record_pending(&self, m: &NewMove<'_>) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO moves (run_id, moved_at, source, dest, size, mtime, reason, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
            params![
                m.run_id,
                unix_ms(SystemTime::now()),
                path_text(m.source)?,
                path_text(m.dest)?,
                m.size as i64,
                unix_ms(m.mtime),
                m.reason,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Changes the planned destination of a pending move (after a name collision).
    pub fn set_dest(&self, id: i64, dest: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE moves SET dest = ?2 WHERE id = ?1",
            params![id, path_text(dest)?],
        )?;
        Ok(())
    }

    pub fn mark_done(&self, id: i64) -> Result<()> {
        self.set_status(id, Status::Done)
    }

    pub fn mark_failed(&self, id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE moves SET status = 'failed', error = ?2 WHERE id = ?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn mark_undone(&self, id: i64, undone_to: &Path) -> Result<()> {
        self.conn.execute(
            "UPDATE moves SET status = 'undone', undone_at = ?2, undone_to = ?3 WHERE id = ?1",
            params![id, unix_ms(SystemTime::now()), path_text(undone_to)?],
        )?;
        Ok(())
    }

    fn set_status(&self, id: i64, status: Status) -> Result<()> {
        self.conn.execute(
            "UPDATE moves SET status = ?2 WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    /// Undoable moves (done or pending) of the most recent run that still has
    /// any, newest first.
    pub fn last_run_moves(&self) -> Result<Vec<MoveRecord>> {
        let run: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(run_id) FROM moves WHERE status IN ('done', 'pending')",
                [],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        match run {
            Some(run) => self.query(
                "WHERE run_id = ?1 AND status IN ('done', 'pending') ORDER BY id DESC",
                params![run],
            ),
            None => Ok(Vec::new()),
        }
    }

    /// The `n` most recent undoable moves across all runs, newest first.
    pub fn last_moves(&self, n: u32) -> Result<Vec<MoveRecord>> {
        self.query(
            "WHERE status IN ('done', 'pending') ORDER BY id DESC LIMIT ?1",
            params![n],
        )
    }

    /// True if `path` is a file that `undo` put back and that hasn't changed
    /// since (same size and last-write time). The watcher leaves such files
    /// alone; otherwise undoing a move would just trigger it again.
    pub fn was_restored_by_undo(&self, path: &Path, size: u64, mtime: SystemTime) -> Result<bool> {
        let Some(path) = path.to_str() else {
            return Ok(false);
        };
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM moves
                 WHERE status = 'undone' AND undone_to = ?1 AND size = ?2 AND mtime = ?3
                 LIMIT 1",
                params![path, size as i64, unix_ms(mtime)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn get(&self, id: i64) -> Result<MoveRecord> {
        self.query("WHERE id = ?1", params![id])?
            .pop()
            .with_context(|| format!("no move with id {id}"))
    }

    fn query(&self, tail: &str, args: impl rusqlite::Params) -> Result<Vec<MoveRecord>> {
        let sql = format!("SELECT id, run_id, source, dest, size, mtime, status FROM moves {tail}");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(args, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, run_id, source, dest, size, mtime_ms, status) = row?;
            Ok(MoveRecord {
                id,
                run_id,
                source: source.into(),
                dest: dest.into(),
                size: size as u64,
                mtime_ms,
                status: Status::parse(&status)?,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(j: &Journal, run: i64, name: &str) -> i64 {
        j.record_pending(&NewMove {
            run_id: run,
            source: Path::new(name),
            dest: &Path::new("dest").join(name),
            size: 1,
            mtime: SystemTime::now(),
            reason: "test",
        })
        .unwrap()
    }

    #[test]
    fn last_run_and_last_n() {
        let j = Journal::open_in_memory().unwrap();
        let r1 = j.begin_run("sort").unwrap();
        let a = record(&j, r1, "a");
        let b = record(&j, r1, "b");
        let r2 = j.begin_run("sort").unwrap();
        let c = record(&j, r2, "c");
        let failed = record(&j, r2, "d");
        for id in [a, b, c] {
            j.mark_done(id).unwrap();
        }
        j.mark_failed(failed, "locked").unwrap();

        let ids = |v: Vec<MoveRecord>| v.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(j.last_run_moves().unwrap()), [c]);
        assert_eq!(ids(j.last_moves(2).unwrap()), [c, b]);

        // Once the newest run is fully undone, "last run" means the one before.
        j.mark_undone(c, Path::new("c")).unwrap();
        assert_eq!(ids(j.last_run_moves().unwrap()), [b, a]);
        assert_eq!(j.get(c).unwrap().status, Status::Undone);
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("moves.db");
        {
            let j = Journal::open(&path).unwrap();
            let run = j.begin_run("sort").unwrap();
            let id = record(&j, run, "a");
            j.mark_done(id).unwrap();
        }
        let j = Journal::open(&path).unwrap();
        assert_eq!(j.last_moves(10).unwrap().len(), 1);
    }

    #[test]
    fn unix_ms_round_trips() {
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(from_unix_ms(unix_ms(t)), t);
        let before = UNIX_EPOCH - Duration::from_millis(5);
        assert_eq!(from_unix_ms(unix_ms(before)), before);
    }
}

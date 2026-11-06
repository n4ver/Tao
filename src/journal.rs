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
use rusqlite::types::Type;
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
    pub fn as_str(self) -> &'static str {
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

/// A full move-log row, for `tao log`.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub id: i64,
    pub run_id: i64,
    pub moved_at: SystemTime,
    pub source: PathBuf,
    pub dest: PathBuf,
    pub reason: String,
    pub status: Status,
    pub error: Option<String>,
    pub undone_to: Option<PathBuf>,
}

/// What [`Journal::prune`] deleted (or, in a dry run, would delete).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub failed: usize,
    pub undone: usize,
    /// Old `undone` rows kept because the restored file is still in place.
    pub kept_restored: usize,
    pub runs: usize,
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

    /// The `n` most recent completed moves that haven't been undone, newest first.
    pub fn recent_moves(&self, n: u32) -> Result<Vec<MoveRecord>> {
        self.query(
            "WHERE status = 'done' ORDER BY id DESC LIMIT ?1",
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

    /// The `limit` most recent log entries of any status, newest first.
    pub fn history(&self, limit: u32) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, moved_at, source, dest, reason, status, error, undone_to
             FROM moves ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            let status = r.get::<_, String>(6)?;
            Ok(LogEntry {
                id: r.get(0)?,
                run_id: r.get(1)?,
                moved_at: from_unix_ms(r.get(2)?),
                source: r.get::<_, String>(3)?.into(),
                dest: r.get::<_, String>(4)?.into(),
                reason: r.get(5)?,
                status: Status::parse(&status).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(6, Type::Text, e.into())
                })?,
                error: r.get(7)?,
                undone_to: r.get::<_, Option<String>>(8)?.map(PathBuf::from),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Deletes log entries that neither `undo` nor the watcher can still
    /// need, for `tao log --prune`. All of it happens in one transaction;
    /// with `dry_run` the transaction is rolled back, so the report says
    /// exactly what a real run would have done.
    ///
    /// What is deleted (only rows that finished before `cutoff`):
    ///
    /// - `failed` moves (`moved_at` before the cutoff). Undo only ever
    ///   selects `done` and `pending` rows, and [`Self::was_restored_by_undo`]
    ///   only reads `undone` ones, so nothing consults these.
    /// - `undone` moves (`undone_at` before the cutoff) whose file is no
    ///   longer at `undone_to` with the logged size and modified time. Those
    ///   are exactly the rows for which `was_restored_by_undo` already returns
    ///   false. A row whose restored file is still sitting there unchanged is
    ///   what keeps the watcher from sorting it again, so it is kept, however
    ///   old.
    /// - `runs` with no moves left that started before the cutoff, except the
    ///   newest run (a watcher may have just created it and be about to log
    ///   into it).
    ///
    /// `done` and `pending` rows are never deleted: they are what `tao undo`,
    /// `undo --last` and the tray's recent-moves list work from, and pending
    /// rows still need undo's check of the disk.
    pub fn prune(&self, cutoff: SystemTime, dry_run: bool) -> Result<PruneReport> {
        // IMMEDIATE takes the write lock up front, so the disk checks below
        // can't race with the watcher marking more rows undone. The busy
        // timeout set in `init` makes this wait for a running watcher.
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let cutoff = unix_ms(cutoff);
        let mut report = PruneReport {
            failed: tx.execute(
                "DELETE FROM moves WHERE status = 'failed' AND moved_at < ?1",
                params![cutoff],
            )?,
            ..PruneReport::default()
        };

        for row in undone_before(&tx, cutoff)? {
            if row.protects_its_file() {
                report.kept_restored += 1;
            } else {
                tx.execute("DELETE FROM moves WHERE id = ?1", params![row.id])?;
                report.undone += 1;
            }
        }

        report.runs = tx.execute(
            "DELETE FROM runs
             WHERE started_at < ?1
               AND id <> (SELECT MAX(id) FROM runs)
               AND NOT EXISTS (SELECT 1 FROM moves WHERE moves.run_id = runs.id)",
            params![cutoff],
        )?;

        if dry_run {
            tx.rollback()?;
        } else {
            tx.commit()?;
        }
        Ok(report)
    }

    /// Rebuilds the database file to give the space freed by pruning back to
    /// the file system. Must run outside a transaction.
    pub fn vacuum(&self) -> Result<()> {
        // In WAL mode VACUUM writes the new pages to the -wal file; the
        // TRUNCATE checkpoint copies them back and shrinks the -wal to zero.
        self.conn
            .execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
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

/// The parts of an `undone` row that decide whether it can be pruned.
struct UndoneRow {
    id: i64,
    undone_to: Option<String>,
    size: u64,
    mtime_ms: i64,
}

impl UndoneRow {
    /// Whether the watcher may still rely on this row (see [`still_restored`]).
    fn protects_its_file(&self) -> bool {
        self.undone_to
            .as_deref()
            .is_some_and(|p| still_restored(Path::new(p), self.size, self.mtime_ms))
    }
}

/// `undone` rows that finished before `cutoff`.
fn undone_before(conn: &Connection, cutoff: i64) -> Result<Vec<UndoneRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, undone_to, size, mtime FROM moves
         WHERE status = 'undone' AND undone_at < ?1",
    )?;
    let rows = stmt.query_map(params![cutoff], |r| {
        Ok(UndoneRow {
            id: r.get(0)?,
            undone_to: r.get(1)?,
            size: r.get::<_, i64>(2)? as u64,
            mtime_ms: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// True if the file undo put back is still at `path` and unchanged: the
/// same test [`Journal::was_restored_by_undo`] applies, so a row passing it
/// is one the watcher still relies on. If the file can't be checked for any
/// reason other than "not found", it counts as still there: when unsure,
/// keep the row.
fn still_restored(path: &Path, size: u64, mtime_ms: i64) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.len() == size && m.modified().is_ok_and(|t| unix_ms(t) == mtime_ms),
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
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

    const HOUR: Duration = Duration::from_secs(3600);

    /// A cutoff after everything in the test log, so age never protects a row.
    fn future() -> SystemTime {
        SystemTime::now() + HOUR
    }

    /// Logs a move of a real file and marks it undone, as `undo` would after
    /// putting it back at `restored`.
    fn undone_real(j: &Journal, run: i64, restored: &Path) -> i64 {
        std::fs::write(restored, "restored").unwrap();
        let meta = std::fs::metadata(restored).unwrap();
        let id = j
            .record_pending(&NewMove {
                run_id: run,
                source: restored,
                dest: Path::new("dest/x"),
                size: meta.len(),
                mtime: meta.modified().unwrap(),
                reason: "test",
            })
            .unwrap();
        j.mark_done(id).unwrap();
        j.mark_undone(id, restored).unwrap();
        id
    }

    fn restored_still_protected(j: &Journal, path: &Path) -> bool {
        let meta = std::fs::metadata(path).unwrap();
        j.was_restored_by_undo(path, meta.len(), meta.modified().unwrap())
            .unwrap()
    }

    fn ids(j: &Journal) -> Vec<i64> {
        j.history(100).unwrap().into_iter().map(|e| e.id).collect()
    }

    #[test]
    fn history_shows_every_status_newest_first() {
        let j = Journal::open_in_memory().unwrap();
        let run = j.begin_run("sort").unwrap();
        let (a, b, c) = (
            record(&j, run, "a"),
            record(&j, run, "b"),
            record(&j, run, "c"),
        );
        j.mark_done(a).unwrap();
        j.mark_failed(b, "locked").unwrap();
        j.mark_undone(c, Path::new("c")).unwrap();
        let h = j.history(10).unwrap();
        assert_eq!(h.iter().map(|e| e.id).collect::<Vec<_>>(), [c, b, a]);
        assert_eq!(h[0].status, Status::Undone);
        assert_eq!(h[0].undone_to.as_deref(), Some(Path::new("c")));
        assert_eq!(h[1].error.as_deref(), Some("locked"));
        assert_eq!(h[2].reason, "test");
        assert_eq!(j.history(1).unwrap().len(), 1);
    }

    #[test]
    fn prune_removes_failed_and_keeps_done_and_pending_at_any_age() {
        let j = Journal::open_in_memory().unwrap();
        let run = j.begin_run("sort").unwrap();
        let done = record(&j, run, "done");
        j.mark_done(done).unwrap();
        let pending = record(&j, run, "pending");
        let failed = record(&j, run, "failed");
        j.mark_failed(failed, "locked").unwrap();

        let report = j.prune(future(), false).unwrap();
        assert_eq!(report.failed, 1);
        assert_eq!(ids(&j), [pending, done]);
        // Undo still sees both.
        let undoable: Vec<_> = j.last_run_moves().unwrap().iter().map(|m| m.id).collect();
        assert_eq!(undoable, [pending, done]);
    }

    #[test]
    fn prune_respects_the_age_cutoff() {
        let j = Journal::open_in_memory().unwrap();
        let run = j.begin_run("sort").unwrap();
        let failed = record(&j, run, "f");
        j.mark_failed(failed, "x").unwrap();
        j.mark_undone(record(&j, run, "u"), Path::new("gone"))
            .unwrap();
        let report = j.prune(SystemTime::now() - HOUR, false).unwrap();
        assert_eq!(report, PruneReport::default());
        assert_eq!(ids(&j).len(), 2);
    }

    /// The watcher's "don't re-sort what undo put back" check must keep
    /// working: an undone row whose file is still in place unchanged stays.
    #[test]
    fn prune_keeps_undone_rows_the_watcher_still_relies_on() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open_in_memory().unwrap();
        let run = j.begin_run("sort").unwrap();
        let unchanged = dir.path().join("unchanged.pdf");
        let edited = dir.path().join("edited.pdf");
        let moved_away = dir.path().join("moved.pdf");
        let keep = undone_real(&j, run, &unchanged);
        undone_real(&j, run, &edited);
        undone_real(&j, run, &moved_away);
        std::fs::write(&edited, "edited since, and longer").unwrap();
        std::fs::rename(&moved_away, dir.path().join("elsewhere.pdf")).unwrap();

        let report = j.prune(future(), false).unwrap();
        assert_eq!(report.undone, 2);
        assert_eq!(report.kept_restored, 1);
        assert_eq!(ids(&j), [keep]);
        assert!(restored_still_protected(&j, &unchanged));

        // Once that file changes too, the next prune removes the row.
        std::fs::write(&unchanged, "now edited as well").unwrap();
        assert_eq!(j.prune(future(), false).unwrap().undone, 1);
        assert!(ids(&j).is_empty());
    }

    #[test]
    fn dry_run_reports_the_same_counts_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open_in_memory().unwrap();
        let old_run = j.begin_run("sort").unwrap();
        let f = record(&j, old_run, "f");
        j.mark_failed(f, "x").unwrap();
        undone_real(&j, old_run, &dir.path().join("kept.txt"));
        j.mark_undone(record(&j, old_run, "u"), Path::new("missing"))
            .unwrap();
        j.begin_run("watch").unwrap();
        let before = ids(&j);

        let dry = j.prune(future(), true).unwrap();
        assert_eq!(ids(&j), before);
        let real = j.prune(future(), false).unwrap();
        assert_eq!(dry, real);
        assert_eq!(
            real,
            PruneReport {
                failed: 1,
                undone: 1,
                kept_restored: 1,
                runs: 0,
            }
        );
    }

    #[test]
    fn prune_removes_empty_old_runs_but_never_the_newest() {
        let j = Journal::open_in_memory().unwrap();
        let emptied = j.begin_run("sort").unwrap();
        let f = record(&j, emptied, "f");
        j.mark_failed(f, "x").unwrap();
        let with_done = j.begin_run("sort").unwrap();
        j.mark_done(record(&j, with_done, "d")).unwrap();
        let newest = j.begin_run("watch").unwrap();

        let report = j.prune(future(), false).unwrap();
        assert_eq!(report.runs, 1, "only the emptied run goes");
        // The newest (empty) run is still usable: the foreign key holds.
        let id = record(&j, newest, "late");
        j.mark_done(id).unwrap();
        assert_eq!(j.last_run_moves().unwrap()[0].id, id);
        let gone = j.conn.execute(
            "INSERT INTO moves (run_id, moved_at, source, dest, size, mtime, reason, status)
             VALUES (?1, 0, 'a', 'b', 0, 0, 'r', 'done')",
            params![emptied],
        );
        assert!(gone.is_err(), "the pruned run must really be gone");
    }

    #[test]
    fn vacuum_after_prune_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("moves.db");
        let j = Journal::open(&path).unwrap();
        let run = j.begin_run("sort").unwrap();
        for i in 0..200 {
            let id = record(&j, run, &format!("file-{i}-{}", "x".repeat(200)));
            j.mark_failed(id, "locked").unwrap();
        }
        let keep = record(&j, run, "keep");
        j.mark_done(keep).unwrap();
        assert_eq!(j.prune(future(), false).unwrap().failed, 200);
        j.vacuum().unwrap();
        drop(j);
        let j = Journal::open(&path).unwrap();
        assert_eq!(ids(&j), [keep]);
    }
}

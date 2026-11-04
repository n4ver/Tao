//! Reverses logged moves.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::journal::{Journal, MoveRecord, Status, unix_ms};
use crate::mover::move_as;

#[derive(Debug, PartialEq, Eq)]
pub enum UndoOutcome {
    /// Moved back. The path differs from the original source if that name
    /// had been taken in the meantime.
    Restored(PathBuf),
    /// Logged as pending but the rename never happened (crash before it).
    NeverMoved,
    Skipped(String),
}

/// Undoes each record, newest first, and returns what happened to each.
/// Problems with one file never stop the others.
pub fn undo(journal: &Journal, records: &[MoveRecord]) -> Vec<(MoveRecord, UndoOutcome)> {
    records
        .iter()
        .map(|rec| {
            let outcome =
                undo_one(journal, rec).unwrap_or_else(|e| UndoOutcome::Skipped(format!("{e:#}")));
            (rec.clone(), outcome)
        })
        .collect()
}

fn undo_one(journal: &Journal, rec: &MoveRecord) -> Result<UndoOutcome> {
    let at_dest = rec.dest.exists();

    if rec.status == Status::Pending && !at_dest && rec.source.exists() {
        // The process stopped between logging and renaming.
        journal.mark_failed(rec.id, "interrupted before the move happened")?;
        return Ok(UndoOutcome::NeverMoved);
    }
    if !at_dest {
        return Ok(UndoOutcome::Skipped(format!(
            "no longer at {} (moved or renamed since?)",
            rec.dest.display()
        )));
    }
    if let Some(why) = changed_since_move(&rec.dest, rec) {
        return Ok(UndoOutcome::Skipped(format!(
            "{why}; it may be a different file, so it was left at {}",
            rec.dest.display()
        )));
    }

    let (Some(original_dir), Some(original_name)) = (rec.source.parent(), rec.source.file_name())
    else {
        return Ok(UndoOutcome::Skipped("log entry has no file name".into()));
    };
    // Restore the *original* name (the file may have been stored as
    // `x (1).pdf`), adding a suffix only if that name is taken by now.
    match move_as(&rec.dest, original_dir, original_name, |_| Ok(())) {
        Ok(path) => {
            journal.mark_undone(rec.id, &path)?;
            Ok(UndoOutcome::Restored(path))
        }
        Err(f) => Ok(UndoOutcome::Skipped(f.to_string())),
    }
}

/// Size and last-write time are kept by a rename, so if either differs the
/// file at `dest` has been edited or replaced since we moved it.
fn changed_since_move(dest: &Path, rec: &MoveRecord) -> Option<String> {
    let meta = match std::fs::metadata(dest) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot read it: {e}")),
    };
    if meta.len() != rec.size {
        return Some(format!(
            "size changed from {} to {} bytes",
            rec.size,
            meta.len()
        ));
    }
    match meta.modified() {
        Ok(t) if unix_ms(t) == rec.mtime_ms => None,
        Ok(_) => Some("modified since it was moved".into()),
        Err(e) => Some(format!("cannot read its modified time: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Reason;
    use crate::mover::move_logged;
    use crate::planner::PlannedMove;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        journal: Journal,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        Fixture {
            _dir: dir,
            root,
            journal: Journal::open_in_memory().unwrap(),
        }
    }

    impl Fixture {
        fn sort(&self, files: &[(&str, &str)]) {
            let run = self.journal.begin_run("sort").unwrap();
            for (name, folder) in files {
                let src = self.root.join(name);
                if !src.exists() {
                    fs::write(&src, name.as_bytes()).unwrap();
                }
                let planned = PlannedMove {
                    source: src,
                    dest_dir: self.root.join(folder),
                    reason: Reason::Fallback,
                };
                move_logged(&self.journal, run, &planned).unwrap();
            }
        }
    }

    #[test]
    fn undo_last_run_restores_files() {
        let f = fixture();
        f.sort(&[("a.pdf", "Documents")]);
        f.sort(&[("b.png", "Images"), ("c.png", "Images")]);

        let recs = f.journal.last_run_moves().unwrap();
        let results = undo(&f.journal, &recs);
        assert!(
            results
                .iter()
                .all(|(_, o)| matches!(o, UndoOutcome::Restored(_)))
        );
        assert!(f.root.join("b.png").exists());
        assert!(f.root.join("c.png").exists());
        assert!(!f.root.join("Images").join("b.png").exists());
        // The earlier run is untouched...
        assert!(f.root.join("Documents").join("a.pdf").exists());
        // ...and is now what "last run" means.
        let recs = f.journal.last_run_moves().unwrap();
        assert_eq!(recs.len(), 1);
        undo(&f.journal, &recs);
        assert!(f.root.join("a.pdf").exists());
        assert!(f.journal.last_run_moves().unwrap().is_empty());
        // The empty category folders are left in place: nothing is deleted.
        assert!(f.root.join("Images").is_dir());
    }

    #[test]
    fn undo_never_overwrites_a_new_file_with_the_same_name() {
        let f = fixture();
        f.sort(&[("x.pdf", "Documents")]);
        // A new download with the same name arrived after sorting.
        fs::write(f.root.join("x.pdf"), "newer download").unwrap();

        let recs = f.journal.last_moves(1).unwrap();
        let results = undo(&f.journal, &recs);
        assert_eq!(
            results[0].1,
            UndoOutcome::Restored(f.root.join("x (1).pdf"))
        );
        assert_eq!(
            fs::read_to_string(f.root.join("x.pdf")).unwrap(),
            "newer download"
        );
        assert_eq!(
            fs::read_to_string(f.root.join("x (1).pdf")).unwrap(),
            "x.pdf"
        );
    }

    #[test]
    fn suffixed_file_goes_back_under_its_original_name() {
        let f = fixture();
        fs::create_dir(f.root.join("Documents")).unwrap();
        fs::write(f.root.join("Documents").join("x.pdf"), "already there").unwrap();
        f.sort(&[("x.pdf", "Documents")]);
        assert!(f.root.join("Documents").join("x (1).pdf").exists());

        let recs = f.journal.last_moves(1).unwrap();
        let results = undo(&f.journal, &recs);
        assert_eq!(results[0].1, UndoOutcome::Restored(f.root.join("x.pdf")));
        assert_eq!(
            fs::read_to_string(f.root.join("Documents").join("x.pdf")).unwrap(),
            "already there"
        );
    }

    #[test]
    fn changed_or_missing_files_are_skipped() {
        let f = fixture();
        f.sort(&[("edited.txt", "Documents"), ("gone.txt", "Documents")]);
        fs::write(
            f.root.join("Documents").join("edited.txt"),
            "much longer now",
        )
        .unwrap();
        fs::rename(
            f.root.join("Documents").join("gone.txt"),
            f.root.join("elsewhere.txt"),
        )
        .unwrap();

        let recs = f.journal.last_run_moves().unwrap();
        for (_, outcome) in undo(&f.journal, &recs) {
            assert!(matches!(outcome, UndoOutcome::Skipped(_)), "{outcome:?}");
        }
        assert!(f.root.join("Documents").join("edited.txt").exists());
        // Skipped moves stay undoable in case the user fixes things up.
        assert_eq!(f.journal.last_run_moves().unwrap().len(), 2);
    }

    #[test]
    fn interrupted_pending_move_is_detected() {
        let f = fixture();
        let src = f.root.join("p.txt");
        fs::write(&src, "p").unwrap();
        let run = f.journal.begin_run("sort").unwrap();
        // Simulate a crash after logging but before renaming.
        f.journal
            .record_pending(&crate::journal::NewMove {
                run_id: run,
                source: &src,
                dest: &f.root.join("Other").join("p.txt"),
                size: 1,
                mtime: fs::metadata(&src).unwrap().modified().unwrap(),
                reason: "test",
            })
            .unwrap();
        let recs = f.journal.last_run_moves().unwrap();
        assert_eq!(undo(&f.journal, &recs)[0].1, UndoOutcome::NeverMoved);
        assert!(src.exists());
        assert!(f.journal.last_run_moves().unwrap().is_empty());
    }
}

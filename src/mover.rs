//! Moving files safely: never overwrite, never delete, log first.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::journal::{Journal, NewMove, path_text};
use crate::planner::PlannedMove;

/// Give up after this many `name (N).ext` attempts.
const MAX_SUFFIX: u32 = 9_999;

/// Why a move did not happen.
#[derive(Debug)]
pub enum MoveFailure {
    /// Another program has the file open (still downloading, being scanned
    /// by antivirus, open in a viewer, ...). Worth retrying later.
    InUse(io::Error),
    /// The file is no longer there.
    Gone,
    /// The move could not be logged, so it was not attempted.
    Journal(anyhow::Error),
    Io(io::Error),
}

impl MoveFailure {
    pub fn is_retryable(&self) -> bool {
        matches!(self, MoveFailure::InUse(_))
    }
}

impl std::fmt::Display for MoveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveFailure::InUse(e) => write!(f, "file is in use ({e})"),
            MoveFailure::Gone => f.write_str("file no longer exists"),
            MoveFailure::Journal(e) => write!(f, "could not write move log, not moved: {e:#}"),
            MoveFailure::Io(e) => write!(f, "{e}"),
        }
    }
}

fn classify(e: io::Error) -> MoveFailure {
    if e.kind() == io::ErrorKind::NotFound {
        MoveFailure::Gone
    } else if is_in_use(&e) {
        MoveFailure::InUse(e)
    } else {
        MoveFailure::Io(e)
    }
}

/// True for the errors Windows returns when another process has the file
/// open in a way that forbids moving it.
pub fn is_in_use(e: &io::Error) -> bool {
    // ERROR_SHARING_VIOLATION (32): the other process opened the file
    //   without FILE_SHARE_DELETE, which a rename requires. Browsers hold
    //   their downloads like this until they finish.
    // ERROR_LOCK_VIOLATION (33): part of the file is locked with LockFile.
    // ERROR_ACCESS_DENIED (5): returned instead of 32 in some cases, notably
    //   when a file is marked for deletion or being written by some AV
    //   scanners. It can also be a real permission problem, which is why
    //   retries are bounded by the caller.
    cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

/// `report.pdf` + 2 -> `report (2).pdf`, the same style Explorer and
/// browsers use. `n == 0` returns the name unchanged.
///
/// `archive.tar.gz` -> `archive (2).tar.gz`, keeping the double extension
/// together. Names without an extension, or dotfiles like `.env`, get the
/// suffix at the end.
pub fn suffixed_name(name: &OsStr, n: u32) -> std::ffi::OsString {
    if n == 0 {
        return name.to_owned();
    }
    let name = name.to_string_lossy();
    let split = match name.rfind('.') {
        Some(0) | None => name.len(),
        Some(i) => {
            // Keep `.tar.xx` together.
            let stem = &name[..i];
            // `.tar` is ASCII, so when the stem ends with it, `len - 4` is a
            // char boundary. (Slicing first and comparing after would panic
            // on names ending in multi-byte characters.)
            if stem.len() > 4 && stem.to_ascii_lowercase().ends_with(".tar") {
                stem.len() - 4
            } else {
                i
            }
        }
    };
    format!("{} ({n}){}", &name[..split], &name[split..]).into()
}

/// Renames `src` to `dst`, failing with `AlreadyExists` if `dst` exists.
///
/// Note that `std::fs::rename` cannot be used for this on Windows: it calls
/// `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` and silently overwrites.
#[cfg(windows)]
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    use crate::win::wide;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_COPY_ALLOWED, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let (src_w, dst_w) = (wide(src), wide(dst));

    // Without MOVEFILE_REPLACE_EXISTING the call fails with
    // ERROR_ALREADY_EXISTS / ERROR_FILE_EXISTS if `dst` exists, and the
    // check-and-rename is atomic inside the file system, so there is no race
    // with another process creating `dst`.
    //
    // MOVEFILE_COPY_ALLOWED: if `dst` is on another drive, Windows copies the
    // file and then removes the source. MOVEFILE_WRITE_THROUGH makes it flush
    // the copy to disk before removing the source. If the copy fails, the
    // source is left untouched.
    //
    // Paths are limited to MAX_PATH (260 chars) unless long paths are enabled
    // system-wide; Downloads paths are normally far shorter.
    // SAFETY: both strings are NUL-terminated and live until the call returns.
    let ok = unsafe {
        MoveFileExW(
            src_w.as_ptr(),
            dst_w.as_ptr(),
            MOVEFILE_COPY_ALLOWED | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Portable fallback so the crate builds and tests on other systems:
/// a hard link fails if `dst` exists, then the old name is removed. This
/// only works within one file system.
#[cfg(not(windows))]
pub fn rename_no_replace(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::hard_link(src, dst)?;
    std::fs::remove_file(src)
}

/// Moves `src` into `dir`, keeping its name or adding the first free
/// ` (N)` suffix. `on_candidate` is called with each name before it is
/// tried; returning an error aborts without moving.
pub fn move_into(
    src: &Path,
    dir: &Path,
    on_candidate: impl FnMut(&Path) -> anyhow::Result<()>,
) -> Result<PathBuf, MoveFailure> {
    let name = src.file_name().ok_or(MoveFailure::Gone)?;
    move_as(src, dir, name, on_candidate)
}

/// Like [`move_into`], but the file is given `name` (plus a suffix if
/// needed) instead of keeping its current name. Used by undo to restore a
/// file that got a collision suffix on the way in.
pub fn move_as(
    src: &Path,
    dir: &Path,
    name: &OsStr,
    mut on_candidate: impl FnMut(&Path) -> anyhow::Result<()>,
) -> Result<PathBuf, MoveFailure> {
    std::fs::create_dir_all(dir).map_err(MoveFailure::Io)?;
    for n in 0..=MAX_SUFFIX {
        let candidate = dir.join(suffixed_name(name, n));
        on_candidate(&candidate).map_err(MoveFailure::Journal)?;
        match rename_no_replace(src, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(classify(e)),
        }
    }
    Err(MoveFailure::Io(io::Error::other(format!(
        "more than {MAX_SUFFIX} files named like {} in {}",
        name.to_string_lossy(),
        dir.display()
    ))))
}

/// Performs one planned move and logs it. Returns the final path.
pub fn move_logged(
    journal: &Journal,
    run_id: i64,
    planned: &PlannedMove,
) -> Result<PathBuf, MoveFailure> {
    let src = &planned.source;
    let meta = std::fs::metadata(src).map_err(classify)?;
    let mtime = meta.modified().map_err(MoveFailure::Io)?;
    path_text(src).map_err(MoveFailure::Journal)?;
    let reason = planned.reason.to_string();

    // The row is written before the first rename attempt and updated with
    // each new candidate name, so the log always names the exact path the
    // file may have gone to, even if we crash mid-way.
    let mut id = None;
    let result = move_into(src, &planned.dest_dir, |candidate| {
        match id {
            None => {
                id = Some(journal.record_pending(&NewMove {
                    run_id,
                    source: src,
                    dest: candidate,
                    size: meta.len(),
                    mtime,
                    reason: &reason,
                })?)
            }
            Some(id) => journal.set_dest(id, candidate)?,
        }
        Ok(())
    });

    match (&result, id) {
        (Ok(_), Some(id)) => {
            if let Err(e) = journal.mark_done(id) {
                // The file was moved and the row has the right destination;
                // `undo` handles rows left in `pending`.
                tracing::warn!("moved but could not mark log entry {id} done: {e:#}");
            }
        }
        (Err(failure), Some(id)) => {
            if let Err(e) = journal.mark_failed(id, &failure.to_string()) {
                tracing::warn!("could not mark log entry {id} failed: {e:#}");
            }
        }
        (_, None) => {}
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Reason;
    use std::fs;

    fn sfx(name: &str, n: u32) -> String {
        suffixed_name(OsStr::new(name), n)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn suffix_naming() {
        assert_eq!(sfx("report.pdf", 0), "report.pdf");
        assert_eq!(sfx("report.pdf", 1), "report (1).pdf");
        assert_eq!(sfx("report.pdf", 12), "report (12).pdf");
        assert_eq!(sfx("my.file.name.txt", 2), "my.file.name (2).txt");
        assert_eq!(sfx("src.tar.gz", 1), "src (1).tar.gz");
        assert_eq!(sfx("SRC.TAR.XZ", 1), "SRC (1).TAR.XZ");
        assert_eq!(sfx(".tar.gz", 1), ".tar (1).gz");
        assert_eq!(sfx("README", 3), "README (3)");
        assert_eq!(sfx(".env", 1), ".env (1)");
        assert_eq!(sfx("ünïcødé 文件.docx", 1), "ünïcødé 文件 (1).docx");
    }

    #[test]
    fn rename_no_replace_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, "A").unwrap();
        fs::write(&b, "B").unwrap();
        let err = rename_no_replace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&a).unwrap(), "A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "B");
    }

    #[test]
    fn move_into_adds_suffixes_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Documents");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("x.pdf"), "old").unwrap();
        fs::write(dest.join("x (1).pdf"), "old1").unwrap();
        let src = dir.path().join("x.pdf");
        fs::write(&src, "new").unwrap();

        let mut tried = Vec::new();
        let got = move_into(&src, &dest, |c| {
            tried.push(c.file_name().unwrap().to_string_lossy().into_owned());
            Ok(())
        })
        .unwrap();

        assert_eq!(got, dest.join("x (2).pdf"));
        assert_eq!(tried, ["x.pdf", "x (1).pdf", "x (2).pdf"]);
        assert_eq!(fs::read_to_string(dest.join("x.pdf")).unwrap(), "old");
        assert_eq!(fs::read_to_string(dest.join("x (1).pdf")).unwrap(), "old1");
        assert_eq!(fs::read_to_string(&got).unwrap(), "new");
        assert!(!src.exists());
    }

    #[test]
    fn move_into_creates_the_destination_and_aborts_if_logging_fails() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        fs::write(&src, "a").unwrap();
        let dest = dir.path().join("deep").join("er");
        let err = move_into(&src, &dest, |_| anyhow::bail!("disk full")).unwrap_err();
        assert!(matches!(err, MoveFailure::Journal(_)));
        assert!(src.exists(), "must not move when the log write fails");
        assert!(dest.is_dir());
    }

    #[test]
    fn move_logged_records_the_final_path() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Images");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("p.png"), "taken").unwrap();
        let src = dir.path().join("p.png");
        fs::write(&src, "png!").unwrap();

        let j = Journal::open_in_memory().unwrap();
        let run = j.begin_run("sort").unwrap();
        let planned = PlannedMove {
            source: src.clone(),
            dest_dir: dest.clone(),
            reason: Reason::rule("Images"),
        };
        let got = move_logged(&j, run, &planned).unwrap();
        assert_eq!(got, dest.join("p (1).png"));

        let rec = &j.last_run_moves().unwrap()[0];
        assert_eq!(rec.source, src);
        assert_eq!(rec.dest, got);
        assert_eq!(rec.size, 4);
        assert_eq!(rec.status, crate::journal::Status::Done);
    }

    #[test]
    fn missing_source_is_reported_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open_in_memory().unwrap();
        let planned = PlannedMove {
            source: dir.path().join("nope.txt"),
            dest_dir: dir.path().join("Other"),
            reason: Reason::Fallback,
        };
        let run = j.begin_run("sort").unwrap();
        assert!(matches!(
            move_logged(&j, run, &planned),
            Err(MoveFailure::Gone)
        ));
    }

    /// A file held open without FILE_SHARE_DELETE (as browsers do while
    /// downloading) cannot be renamed; that must come back as `InUse`.
    #[cfg(windows)]
    #[test]
    fn open_file_is_reported_as_in_use() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("busy.zip");
        fs::write(&src, "zip").unwrap();
        // share_mode 0x1 = FILE_SHARE_READ only: no FILE_SHARE_DELETE.
        let _handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&src)
            .unwrap();
        let err = move_into(&src, &dir.path().join("Archives"), |_| Ok(())).unwrap_err();
        assert!(err.is_retryable(), "{err}");
        assert!(src.exists());
    }
}

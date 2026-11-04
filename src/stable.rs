//! Decides when a file has finished being written.
//!
//! File-system events alone can't tell us that: a browser may pause for many
//! seconds mid-download, and some tools write the final name first and fill
//! it in afterwards. So events only *nominate* a path, and this tracker polls
//! it until its size and last-write time have stayed the same for a whole
//! window of time.
//!
//! The tracker is pure (the clock and the file probe are passed in) so it can
//! be tested without sleeping or touching the disk.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// What we observe about a file on each poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub size: u64,
    pub modified: SystemTime,
}

impl Snapshot {
    pub fn read(path: &Path) -> io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        Ok(Self {
            size: meta.len(),
            modified: meta.modified()?,
        })
    }
}

#[derive(Debug)]
struct Pending {
    /// `None` until the first probe.
    last: Option<Snapshot>,
    /// When `last` was first seen with its current value.
    unchanged_since: Instant,
    /// Don't probe before this (used for retry backoff).
    not_before: Instant,
}

#[derive(Debug)]
pub struct StableTracker {
    window: Duration,
    pending: HashMap<PathBuf, Pending>,
    /// Consecutive failed move attempts, kept while a file waits to be retried.
    failures: HashMap<PathBuf, u32>,
}

impl StableTracker {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            pending: HashMap::new(),
            failures: HashMap::new(),
        }
    }

    pub fn set_window(&mut self, window: Duration) {
        self.window = window;
    }

    /// Starts tracking `path`, or does nothing if it is already tracked.
    /// Repeated events for the same file don't reset anything; a changed
    /// snapshot on the next poll does.
    pub fn track(&mut self, path: PathBuf, now: Instant) {
        self.pending.entry(path).or_insert(Pending {
            last: None,
            unchanged_since: now,
            not_before: now,
        });
    }

    pub fn is_tracked(&self, path: &Path) -> bool {
        self.pending.contains_key(path)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Probes every tracked file that is due and returns the ones that have
    /// been stable for the whole window. Returned paths stop being tracked.
    /// Files that disappeared (or became unreadable) are dropped.
    pub fn poll(
        &mut self,
        now: Instant,
        mut probe: impl FnMut(&Path) -> io::Result<Snapshot>,
    ) -> Vec<PathBuf> {
        let window = self.window;
        let mut ready = Vec::new();
        self.pending.retain(|path, p| {
            if now < p.not_before {
                return true;
            }
            let snap = match probe(path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!("no longer tracking {}: {e}", path.display());
                    return false;
                }
            };
            if p.last != Some(snap) {
                p.last = Some(snap);
                p.unchanged_since = now;
                return true;
            }
            if now.duration_since(p.unchanged_since) >= window {
                ready.push(path.clone());
                return false;
            }
            true
        });
        ready.sort();
        ready
    }

    /// Puts a file back after a failed move (e.g. it was locked). Each
    /// consecutive failure doubles the wait, from 5 s up to 5 min. Returns
    /// the number of consecutive failures so far.
    pub fn retry_later(&mut self, path: PathBuf, now: Instant) -> u32 {
        let failures = self.failures.get(&path).copied().unwrap_or(0) + 1;
        self.failures.insert(path.clone(), failures);
        self.pending.insert(
            path,
            Pending {
                last: None,
                unchanged_since: now,
                not_before: now + backoff(failures),
            },
        );
        failures
    }

    /// Forgets a file entirely, including its failure count. Call this once
    /// a file has been moved or given up on.
    pub fn forget(&mut self, path: &Path) {
        self.pending.remove(path);
        self.failures.remove(path);
    }

    /// Consecutive failed attempts for a path.
    pub fn failures(&self, path: &Path) -> u32 {
        self.failures.get(path).copied().unwrap_or(0)
    }
}

/// 5 s, 10 s, 20 s, ... capped at 5 minutes.
pub fn backoff(failures: u32) -> Duration {
    const BASE: u64 = 5;
    const CAP: u64 = 300;
    let secs = BASE.saturating_mul(1u64 << failures.saturating_sub(1).min(16));
    Duration::from_secs(secs.min(CAP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const WINDOW: Duration = Duration::from_secs(5);

    /// A fake file system: path -> current snapshot (absent = deleted).
    #[derive(Default)]
    struct FakeFs(RefCell<HashMap<PathBuf, Snapshot>>);

    impl FakeFs {
        fn write(&self, name: &str, size: u64, mtime_secs: u64) {
            self.0.borrow_mut().insert(
                name.into(),
                Snapshot {
                    size,
                    modified: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs),
                },
            );
        }
        fn delete(&self, name: &str) {
            self.0.borrow_mut().remove(Path::new(name));
        }
        fn probe(&self) -> impl FnMut(&Path) -> io::Result<Snapshot> + '_ {
            |p| {
                self.0
                    .borrow()
                    .get(p)
                    .copied()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            }
        }
    }

    fn secs(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn file_becomes_ready_after_a_full_unchanged_window() {
        let fs = FakeFs::default();
        let mut t = StableTracker::new(WINDOW);
        let t0 = Instant::now();
        fs.write("a.zip", 100, 1);
        t.track("a.zip".into(), t0);

        assert!(t.poll(t0, fs.probe()).is_empty(), "first sighting");
        assert!(t.poll(secs(t0, 4), fs.probe()).is_empty(), "only 4 s");
        assert_eq!(t.poll(secs(t0, 5), fs.probe()), [PathBuf::from("a.zip")]);
        assert!(t.is_empty(), "ready files stop being tracked");
    }

    #[test]
    fn growing_file_resets_the_window() {
        let fs = FakeFs::default();
        let mut t = StableTracker::new(WINDOW);
        let t0 = Instant::now();
        fs.write("a.zip", 100, 1);
        t.track("a.zip".into(), t0);
        t.poll(t0, fs.probe());

        fs.write("a.zip", 200, 1); // size changed
        assert!(t.poll(secs(t0, 4), fs.probe()).is_empty());
        assert!(
            t.poll(secs(t0, 8), fs.probe()).is_empty(),
            "4 s since change"
        );
        fs.write("a.zip", 200, 2); // only mtime changed
        assert!(t.poll(secs(t0, 9), fs.probe()).is_empty());
        assert!(t.poll(secs(t0, 13), fs.probe()).is_empty());
        assert_eq!(t.poll(secs(t0, 14), fs.probe()).len(), 1);
    }

    #[test]
    fn repeated_events_do_not_reset_the_window() {
        let fs = FakeFs::default();
        let mut t = StableTracker::new(WINDOW);
        let t0 = Instant::now();
        fs.write("a.zip", 1, 1);
        t.track("a.zip".into(), t0);
        t.poll(t0, fs.probe());
        t.track("a.zip".into(), secs(t0, 3)); // e.g. an attribute-change event
        assert_eq!(t.poll(secs(t0, 5), fs.probe()).len(), 1);
    }

    #[test]
    fn deleted_or_renamed_files_are_dropped() {
        let fs = FakeFs::default();
        let mut t = StableTracker::new(WINDOW);
        let t0 = Instant::now();
        fs.write("a.zip", 1, 1);
        t.track("a.zip".into(), t0);
        t.poll(t0, fs.probe());
        fs.delete("a.zip");
        assert!(t.poll(secs(t0, 10), fs.probe()).is_empty());
        assert!(t.is_empty());
    }

    #[test]
    fn retry_waits_for_backoff_then_a_fresh_window() {
        let fs = FakeFs::default();
        let mut t = StableTracker::new(WINDOW);
        let t0 = Instant::now();
        fs.write("locked.exe", 1, 1);
        assert_eq!(t.retry_later("locked.exe".into(), t0), 1);
        assert_eq!(t.failures(Path::new("locked.exe")), 1);

        // Backoff for the first failure is 5 s: no probe before then.
        let mut probed = false;
        t.poll(secs(t0, 4), |_| {
            probed = true;
            Ok(fs.0.borrow()[Path::new("locked.exe")])
        });
        assert!(!probed);
        // After the backoff it needs a full stable window again.
        assert!(t.poll(secs(t0, 5), fs.probe()).is_empty());
        assert_eq!(t.poll(secs(t0, 10), fs.probe()).len(), 1);

        // A second failure doubles the backoff to 10 s.
        assert_eq!(t.retry_later("locked.exe".into(), secs(t0, 10)), 2);
        assert!(t.poll(secs(t0, 19), fs.probe()).is_empty());
        assert!(t.is_tracked(Path::new("locked.exe")));
        t.forget(Path::new("locked.exe"));
        assert_eq!(t.failures(Path::new("locked.exe")), 0);
        assert!(t.is_empty());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let s = |n| backoff(n).as_secs();
        assert_eq!([s(1), s(2), s(3), s(4)], [5, 10, 20, 40]);
        assert_eq!(s(7), 300);
        assert_eq!(s(1000), 300);
    }

    /// The same checks against a real file in a temporary directory.
    #[test]
    fn real_file_snapshot_changes_when_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        std::fs::write(&path, b"1").unwrap();
        let a = Snapshot::read(&path).unwrap();
        std::fs::write(&path, b"12").unwrap();
        let b = Snapshot::read(&path).unwrap();
        assert_ne!(a, b);
        assert_eq!(b, Snapshot::read(&path).unwrap());
        assert!(
            Snapshot::read(dir.path()).is_err(),
            "directories are not tracked"
        );
    }
}

//! `tao watch`: sorts new downloads as they finish.
//!
//! Flow: file-system events (and a periodic rescan) *nominate* paths, the
//! [`StableTracker`] holds them until they stop changing, and then they go
//! through the same planner and mover as `tao sort`.
//!
//! The watcher must never die because of one file, so every per-file
//! problem is logged and the loop carries on. Only failing to start at all
//! (e.g. an invalid config on startup) ends the process.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};

use crate::config::Config;
use crate::journal::Journal;
use crate::mover::{MoveFailure, move_logged};
use crate::paths;
use crate::planner::{Evaluation, Planner};
use crate::stable::{Snapshot, StableTracker, backoff};
use crate::win::SingleInstance;

/// How often tracked files are probed.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Full rescan of the folder, in case events were missed.
const RESCAN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Wait before trying again if the folder can't be watched (e.g. on a
/// drive that isn't connected yet at sign-in).
const REWATCH_DELAY: Duration = Duration::from_secs(30);
/// Debounce window for raw file-system events.
const DEBOUNCE: Duration = Duration::from_secs(1);
/// Give up on a locked file after this many attempts (about an hour with
/// the backoff schedule). It is tried again if it changes.
const MAX_FAILURES: u32 = 16;

/// Everything the watcher loop does except talking to `notify`, so it can
/// be driven directly in tests.
pub struct WatchState {
    planner: Planner,
    journal: Journal,
    tracker: StableTracker,
    /// Files we stopped trying to move, with how they looked at the time.
    /// They are reconsidered only if their size or modified time changes.
    given_up: HashMap<PathBuf, Snapshot>,
}

impl WatchState {
    pub fn new(planner: Planner, journal: Journal, stable_window: Duration) -> Self {
        Self {
            planner,
            journal,
            tracker: StableTracker::new(stable_window),
            given_up: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        self.planner.root()
    }

    pub fn replace_planner(&mut self, planner: Planner, stable_window: Duration) {
        self.planner = planner;
        self.tracker.set_window(stable_window);
    }

    /// Suggests that `path` may need sorting.
    pub fn nominate(&mut self, path: &Path, now: Instant) {
        // Only direct children of the sorted folder; never the category
        // folders' contents.
        if path.parent() != Some(self.planner.root()) || self.tracker.is_tracked(path) {
            return;
        }
        if let Some(old) = self.given_up.get(path) {
            match Snapshot::read(path) {
                Ok(now_snap) if now_snap == *old => return,
                _ => {
                    self.given_up.remove(path);
                }
            }
        }
        self.tracker.track(path.to_path_buf(), now);
    }

    /// Nominates every entry in the folder.
    pub fn rescan(&mut self, now: Instant) {
        let entries = match std::fs::read_dir(self.planner.root()) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("cannot read {}: {e}", self.planner.root().display());
                return;
            }
        };
        for entry in entries.flatten() {
            self.nominate(&entry.path(), now);
        }
    }

    /// Probes tracked files and moves the ones that are ready. Returns the
    /// number of files moved.
    pub fn process(&mut self, now: Instant) -> usize {
        let ready = self.tracker.poll(now, Snapshot::read);
        let mut run_id = None;
        let mut moved = 0;
        for path in ready {
            // A bug in handling one file must not take the watcher down.
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.process_one(&path, now, &mut run_id)
            }));
            match result {
                Ok(true) => moved += 1,
                Ok(false) => {}
                Err(_) => {
                    tracing::error!("internal error while handling {}", path.display());
                    self.give_up(&path);
                }
            }
        }
        moved
    }

    fn process_one(&mut self, path: &Path, now: Instant, run_id: &mut Option<i64>) -> bool {
        let planned = match self.planner.evaluate(path, SystemTime::now()) {
            Ok(Evaluation::Move(m)) => m,
            Ok(Evaluation::Skip(why)) => {
                tracing::debug!("skipping {}: {why}", path.display());
                self.tracker.forget(path);
                return false;
            }
            Err(e) => {
                tracing::debug!("skipping {}: {e}", path.display());
                self.tracker.forget(path);
                return false;
            }
        };

        if self.restored_by_undo(path) {
            tracing::info!(
                "leaving {} alone: it was put back by `tao undo`",
                path.display()
            );
            self.give_up(path);
            return false;
        }

        let run = match run_id {
            Some(id) => *id,
            None => match self.journal.begin_run("watch") {
                Ok(id) => *run_id.insert(id),
                Err(e) => {
                    tracing::warn!("cannot write move log, will retry: {e:#}");
                    self.tracker.retry_later(path.to_path_buf(), now);
                    return false;
                }
            },
        };

        match move_logged(&self.journal, run, &planned) {
            Ok(dest) => {
                tracing::info!(
                    "moved {} -> {} ({})",
                    path.display(),
                    dest.display(),
                    planned.reason
                );
                self.tracker.forget(path);
                true
            }
            Err(MoveFailure::Gone) => {
                self.tracker.forget(path);
                false
            }
            Err(f @ (MoveFailure::InUse(_) | MoveFailure::Journal(_))) => {
                let n = self.tracker.retry_later(path.to_path_buf(), now);
                if n >= MAX_FAILURES {
                    tracing::warn!(
                        "giving up on {} after {n} attempts: {f}. It will be retried if it changes.",
                        path.display()
                    );
                    self.give_up(path);
                } else {
                    tracing::info!(
                        "not moved yet: {} ({f}); retrying in {}s",
                        path.display(),
                        backoff(n).as_secs()
                    );
                }
                false
            }
            Err(f) => {
                tracing::warn!("could not move {}: {f}", path.display());
                self.give_up(path);
                false
            }
        }
    }

    fn restored_by_undo(&self, path: &Path) -> bool {
        let Ok(snap) = Snapshot::read(path) else {
            return false;
        };
        self.journal
            .was_restored_by_undo(path, snap.size, snap.modified)
            .unwrap_or_else(|e| {
                tracing::warn!("cannot query move log: {e:#}");
                false
            })
    }

    fn give_up(&mut self, path: &Path) {
        self.tracker.forget(path);
        if let Ok(snap) = Snapshot::read(path) {
            self.given_up.insert(path.to_path_buf(), snap);
        }
    }
}

fn config_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Runs the watcher until the process is stopped.
pub fn run() -> Result<()> {
    // Sign-in startup plus a manual `tao watch` (or `install`, which also
    // starts one) would otherwise race each other for the same files.
    let Some(_instance) = SingleInstance::acquire("tao-watch")? else {
        tracing::info!("another watcher is already running; exiting");
        return Ok(());
    };
    let config_path = paths::config_file()?;
    crate::config::ensure_exists(&config_path)?;
    let cfg = Config::load(&config_path)?;
    let planner = Planner::from_config(&cfg)?;
    let journal = Journal::open(&paths::journal_file()?)?;
    let mut state = WatchState::new(planner, journal, Duration::from_secs(cfg.stable_seconds));
    let mut config_seen = config_mtime(&config_path);

    tracing::info!(
        "watching {} (config {})",
        state.root().display(),
        config_path.display()
    );

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let mut debouncer = None;
    let mut next_watch_attempt = Instant::now();
    let mut last_rescan = Instant::now();

    loop {
        let now = Instant::now();

        // (Re)start the OS watcher if needed. On Windows, notify uses
        // ReadDirectoryChangesW on a handle to the folder.
        if debouncer.is_none() && now >= next_watch_attempt {
            match new_debouncer(DEBOUNCE, None, tx.clone()).and_then(|mut d| {
                d.watch(state.root(), RecursiveMode::NonRecursive)?;
                Ok(d)
            }) {
                Ok(d) => {
                    debouncer = Some(d);
                    // Sort anything that arrived while we weren't watching.
                    state.rescan(now);
                    last_rescan = now;
                }
                Err(e) => {
                    tracing::warn!(
                        "cannot watch {}: {e}; retrying in {}s",
                        state.root().display(),
                        REWATCH_DELAY.as_secs()
                    );
                    next_watch_attempt = now + REWATCH_DELAY;
                }
            }
        }

        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(Ok(events)) => {
                let now = Instant::now();
                for event in events {
                    if event.need_rescan() {
                        // The OS event buffer overflowed (many files at once).
                        state.rescan(now);
                    }
                    for path in &event.paths {
                        state.nominate(path, now);
                    }
                }
            }
            Ok(Err(errors)) => {
                for e in errors {
                    tracing::warn!("watch error: {e}");
                }
                // Recreate the watcher; the folder may have been removed
                // or the drive disconnected.
                debouncer = None;
                next_watch_attempt = Instant::now() + REWATCH_DELAY;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Can't happen while we hold `tx`, but don't spin if it does.
                debouncer = None;
            }
        }

        // Pick up config edits without a restart.
        let mtime = config_mtime(&config_path);
        if mtime != config_seen {
            config_seen = mtime;
            match Config::load(&config_path).and_then(|c| Ok((Planner::from_config(&c)?, c))) {
                Ok((planner, c)) => {
                    let root_changed = planner.root() != state.root();
                    state.replace_planner(planner, Duration::from_secs(c.stable_seconds));
                    tracing::info!("reloaded config");
                    if root_changed {
                        debouncer = None; // re-watch the new folder
                        next_watch_attempt = Instant::now();
                    } else {
                        state.rescan(Instant::now());
                    }
                }
                Err(e) => {
                    tracing::warn!("config not reloaded, keeping the previous one: {e:#}")
                }
            }
        }

        let now = Instant::now();
        if debouncer.is_some() && now.duration_since(last_rescan) >= RESCAN_INTERVAL {
            state.rescan(now);
            last_rescan = now;
        }
        state.process(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Engine;
    use crate::config::DEFAULT_CONFIG;
    use std::fs;

    fn state(root: &Path) -> WatchState {
        let cfg = Config::parse(DEFAULT_CONFIG).unwrap();
        let planner = Planner::new(root.to_path_buf(), &cfg, Engine::from_config(&cfg).unwrap());
        WatchState::new(planner, Journal::open_in_memory().unwrap(), Duration::ZERO)
    }

    #[test]
    fn stable_file_is_moved_on_the_second_poll() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut s = state(root);
        fs::write(root.join("a.pdf"), "pdf").unwrap();
        let t0 = Instant::now();
        s.nominate(&root.join("a.pdf"), t0);
        assert_eq!(s.process(t0), 0, "first poll only takes a snapshot");
        assert_eq!(s.process(t0), 1);
        assert!(root.join("Documents").join("a.pdf").exists());
    }

    #[test]
    fn files_outside_the_root_and_partial_downloads_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("Images")).unwrap();
        fs::write(root.join("Images").join("x.png"), "x").unwrap();
        fs::write(root.join("big.iso.crdownload"), "x").unwrap();
        let mut s = state(root);
        let t0 = Instant::now();
        s.rescan(t0);
        s.nominate(&root.join("Images").join("x.png"), t0);
        s.process(t0);
        assert_eq!(s.process(t0), 0);
        assert!(root.join("big.iso.crdownload").exists());
        assert!(root.join("Images").join("x.png").exists());
    }

    #[cfg(windows)]
    #[test]
    fn locked_file_is_retried_until_released() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("setup.exe");
        fs::write(&path, "exe").unwrap();
        let mut s = state(root);
        let t0 = Instant::now();

        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1) // FILE_SHARE_READ only: renames are refused
            .open(&path)
            .unwrap();
        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0), 0);
        assert_eq!(s.tracker.failures(&path), 1);
        assert!(path.exists());

        drop(handle);
        // Not before the 5 s backoff...
        assert_eq!(s.process(t0 + Duration::from_secs(1)), 0);
        // ...then a snapshot, then the move.
        let later = t0 + Duration::from_secs(6);
        s.process(later);
        assert_eq!(s.process(later), 1);
        assert!(root.join("Installers").join("setup.exe").exists());
    }

    #[test]
    fn files_put_back_by_undo_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("keep.pdf");
        fs::write(&path, "pdf").unwrap();
        let mut s = state(root);
        let t0 = Instant::now();
        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0), 1);

        let recs = s.journal.last_run_moves().unwrap();
        crate::undo::undo(&s.journal, &recs);
        assert!(path.exists());

        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0), 0, "must not re-sort an undone file");
        assert!(path.exists());

        // Once the file changes, it is fair game again.
        fs::write(&path, "pdf, edited").unwrap();
        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0), 1);
    }
}

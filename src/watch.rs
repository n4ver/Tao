//! `tao watch`: sorts new downloads as they finish.
//!
//! Flow: file-system events (and a periodic rescan) *nominate* paths, the
//! [`StableTracker`] holds them until they stop changing, and then they go
//! through the same planner and mover as `tao sort`.
//!
//! The watcher must never die because of one file, so every per-file
//! problem is logged and the loop carries on. Only failing to start at all
//! (e.g. an invalid config on startup) ends it.
//!
//! [`run_with`] can be driven from another thread: a [`Controller`] sends it
//! [`Control`] commands, and it reports [`WatchEvent`]s through a callback.
//! The tray (`tray.rs`) uses this; [`run_headless`] is the plain version.

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
use crate::undo::UndoOutcome;

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

    /// Forgets which files were given up on, so the next rescan retries them
    /// (used by "Sort now"). Files put back by undo stay protected: that
    /// check reads the move log, not this set.
    pub fn retry_given_up(&mut self) {
        self.given_up.clear();
    }

    /// Probes tracked files and moves the ones that are ready. Returns the
    /// new paths of the files moved.
    pub fn process(&mut self, now: Instant) -> Vec<PathBuf> {
        let ready = self.tracker.poll(now, Snapshot::read);
        let mut run_id = None;
        let mut moved = Vec::new();
        for path in ready {
            // A bug in handling one file must not take the watcher down.
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                self.process_one(&path, now, &mut run_id)
            }));
            match result {
                Ok(Some(dest)) => moved.push(dest),
                Ok(None) => {}
                Err(_) => {
                    tracing::error!("internal error while handling {}", path.display());
                    self.give_up(&path);
                }
            }
        }
        moved
    }

    /// Undoes the most recent move. Returns a one-line description.
    pub fn undo_last(&self) -> String {
        let records = match self.journal.last_moves(1) {
            Ok(r) => r,
            Err(e) => return format!("Undo failed: {e:#}"),
        };
        let Some((rec, outcome)) = crate::undo::undo(&self.journal, &records).pop() else {
            return "Nothing to undo".into();
        };
        let name = file_name(&rec.source);
        match outcome {
            UndoOutcome::Restored(to) => {
                tracing::info!("undo: {} -> {}", rec.dest.display(), to.display());
                format!("Put back {name}")
            }
            UndoOutcome::NeverMoved => format!("{name} was never moved"),
            UndoOutcome::Skipped(why) => {
                tracing::warn!("undo of {} skipped: {why}", rec.dest.display());
                format!("Could not undo {name}: {why}")
            }
        }
    }

    /// The most recent moves still in effect, newest first.
    pub fn recent_moves(&self, n: u32) -> Vec<RecentMove> {
        match self.journal.recent_moves(n) {
            Ok(records) => records
                .into_iter()
                .map(|r| RecentMove {
                    file_name: file_name(&r.dest),
                    folder: r.dest.parent().map(file_name).unwrap_or_default(),
                    dest: r.dest,
                })
                .collect(),
            Err(e) => {
                tracing::warn!("cannot read recent moves: {e:#}");
                Vec::new()
            }
        }
    }

    fn process_one(
        &mut self,
        path: &Path,
        now: Instant,
        run_id: &mut Option<i64>,
    ) -> Option<PathBuf> {
        let planned = match self.planner.evaluate(path, SystemTime::now()) {
            Ok(Evaluation::Move(m)) => m,
            Ok(Evaluation::Skip(why)) => {
                tracing::debug!("skipping {}: {why}", path.display());
                self.tracker.forget(path);
                return None;
            }
            Err(e) => {
                tracing::debug!("skipping {}: {e}", path.display());
                self.tracker.forget(path);
                return None;
            }
        };

        if self.restored_by_undo(path) {
            tracing::info!(
                "leaving {} alone: it was put back by `tao undo`",
                path.display()
            );
            self.give_up(path);
            return None;
        }

        let run = match run_id {
            Some(id) => *id,
            None => match self.journal.begin_run("watch") {
                Ok(id) => *run_id.insert(id),
                Err(e) => {
                    tracing::warn!("cannot write move log, will retry: {e:#}");
                    self.tracker.retry_later(path.to_path_buf(), now);
                    return None;
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
                Some(dest)
            }
            Err(MoveFailure::Gone) => {
                self.tracker.forget(path);
                None
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
                None
            }
            Err(f) => {
                tracing::warn!("could not move {}: {f}", path.display());
                self.give_up(path);
                None
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

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn config_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A move shown in the tray's "Recent moves" menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentMove {
    pub file_name: String,
    /// Name of the destination folder, e.g. `Documents`.
    pub folder: String,
    /// Full current path of the moved file.
    pub dest: PathBuf,
}

/// What the watcher reports to whoever is driving it (the tray).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// The last few moves, newest first. Sent at startup and whenever the
    /// list changes (new moves, undo).
    Recent(Vec<RecentMove>),
    /// One-line status for the tooltip, e.g. "Moved report.pdf to Documents".
    Status(String),
    Paused(bool),
}

/// How many moves the tray lists.
pub const RECENT_COUNT: u32 = 10;

/// Commands from the tray to the watcher thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Rescan now, including files previously given up on. Files still go
    /// through the normal stable-file wait.
    SortNow,
    UndoLast,
    SetPaused(bool),
    Shutdown,
}

/// The watcher's single inbox. File-system events and tray commands share
/// one channel so the loop can block on both at once.
enum Msg {
    Fs(DebounceEventResult),
    Control(Control),
}

/// Sends commands to a running watcher. Cheap to clone.
#[derive(Clone)]
pub struct Controller(mpsc::Sender<Msg>);

impl Controller {
    /// Returns `false` if the watcher has already stopped.
    pub fn send(&self, c: Control) -> bool {
        self.0.send(Msg::Control(c)).is_ok()
    }
}

/// The receiving end, handed to [`run_with`].
pub struct Inbox {
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
}

pub fn control_channel() -> (Controller, Inbox) {
    let (tx, rx) = mpsc::channel();
    (Controller(tx.clone()), Inbox { tx, rx })
}

/// Where the watcher reads its config and writes its move log.
pub struct WatchPaths {
    pub config: PathBuf,
    pub journal: PathBuf,
}

impl WatchPaths {
    pub fn standard() -> Result<Self> {
        Ok(Self {
            config: paths::config_file()?,
            journal: paths::journal_file()?,
        })
    }
}

/// Runs the watcher on this thread with no one to talk to, until the
/// process is stopped. The caller holds the single-instance guard.
pub fn run_headless() -> Result<()> {
    let (_controller, inbox) = control_channel();
    run_with(&WatchPaths::standard()?, inbox, |_| {})
}

/// Runs the watcher until it receives [`Control::Shutdown`] (or all
/// [`Controller`]s are dropped). `report` is called on this thread.
pub fn run_with(locations: &WatchPaths, inbox: Inbox, report: impl Fn(WatchEvent)) -> Result<()> {
    let config_path = &locations.config;
    crate::config::ensure_exists(config_path)?;
    let cfg = Config::load(config_path)?;
    let planner = Planner::from_config(&cfg)?;
    let journal = Journal::open(&locations.journal)?;
    let mut state = WatchState::new(planner, journal, Duration::from_secs(cfg.stable_seconds));
    let mut config_seen = config_mtime(config_path);

    tracing::info!(
        "watching {} (config {})",
        state.root().display(),
        config_path.display()
    );
    report(WatchEvent::Recent(state.recent_moves(RECENT_COUNT)));

    let Inbox { tx, rx } = inbox;
    let mut debouncer = None;
    let mut next_watch_attempt = Instant::now();
    let mut last_rescan = Instant::now();
    let mut paused = false;

    loop {
        let now = Instant::now();

        // (Re)start the OS watcher if needed. On Windows, notify uses
        // ReadDirectoryChangesW on a handle to the folder.
        if debouncer.is_none() && now >= next_watch_attempt {
            let fs_tx = tx.clone();
            let handler = move |r: DebounceEventResult| {
                let _ = fs_tx.send(Msg::Fs(r));
            };
            match new_debouncer(DEBOUNCE, None, handler).and_then(|mut d| {
                d.watch(state.root(), RecursiveMode::NonRecursive)?;
                Ok(d)
            }) {
                Ok(d) => {
                    debouncer = Some(d);
                    // Sort anything that arrived while we weren't watching.
                    if !paused {
                        state.rescan(now);
                    }
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
            Ok(Msg::Fs(Ok(events))) => {
                if !paused {
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
            }
            Ok(Msg::Fs(Err(errors))) => {
                for e in errors {
                    tracing::warn!("watch error: {e}");
                }
                // Recreate the watcher; the folder may have been removed
                // or the drive disconnected.
                debouncer = None;
                next_watch_attempt = Instant::now() + REWATCH_DELAY;
            }
            Ok(Msg::Control(c)) => match c {
                Control::Shutdown => {
                    tracing::info!("watcher stopping");
                    return Ok(());
                }
                Control::SetPaused(p) => {
                    if p != paused {
                        paused = p;
                        tracing::info!("{}", if p { "paused" } else { "resumed" });
                        if !p {
                            // Catch up on whatever arrived while paused.
                            state.rescan(Instant::now());
                        }
                    }
                    report(WatchEvent::Paused(paused));
                }
                Control::SortNow => {
                    if paused {
                        report(WatchEvent::Status("Paused: resume to sort".into()));
                    } else {
                        state.retry_given_up();
                        state.rescan(Instant::now());
                        report(WatchEvent::Status("Sorting Downloads…".into()));
                    }
                }
                Control::UndoLast => {
                    let status = state.undo_last();
                    report(WatchEvent::Recent(state.recent_moves(RECENT_COUNT)));
                    report(WatchEvent::Status(status));
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            // Unreachable while we hold `tx`, but treat it as a shutdown.
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Pick up config edits without a restart.
        let mtime = config_mtime(config_path);
        if mtime != config_seen {
            config_seen = mtime;
            match Config::load(config_path).and_then(|c| Ok((Planner::from_config(&c)?, c))) {
                Ok((planner, c)) => {
                    let root_changed = planner.root() != state.root();
                    state.replace_planner(planner, Duration::from_secs(c.stable_seconds));
                    tracing::info!("reloaded config");
                    if root_changed {
                        debouncer = None; // re-watch the new folder
                        next_watch_attempt = Instant::now();
                    } else if !paused {
                        state.rescan(Instant::now());
                    }
                }
                Err(e) => {
                    tracing::warn!("config not reloaded, keeping the previous one: {e:#}");
                    report(WatchEvent::Status(
                        "Config has an error, see the log".into(),
                    ));
                }
            }
        }

        if paused {
            continue;
        }
        let now = Instant::now();
        if debouncer.is_some() && now.duration_since(last_rescan) >= RESCAN_INTERVAL {
            state.rescan(now);
            last_rescan = now;
        }
        let moved = state.process(now);
        if let Some(last) = moved.last() {
            let folder = last.parent().map(file_name).unwrap_or_default();
            let status = match moved.len() {
                1 => format!("Moved {} to {folder}", file_name(last)),
                n => format!("Moved {n} files, last {} to {folder}", file_name(last)),
            };
            report(WatchEvent::Recent(state.recent_moves(RECENT_COUNT)));
            report(WatchEvent::Status(status));
        }
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
        assert_eq!(s.process(t0).len(), 0, "first poll only takes a snapshot");
        assert_eq!(s.process(t0).len(), 1);
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
        assert_eq!(s.process(t0).len(), 0);
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
        assert_eq!(s.process(t0).len(), 0);
        assert_eq!(s.tracker.failures(&path), 1);
        assert!(path.exists());

        drop(handle);
        // Not before the 5 s backoff...
        assert_eq!(s.process(t0 + Duration::from_secs(1)).len(), 0);
        // ...then a snapshot, then the move.
        let later = t0 + Duration::from_secs(6);
        s.process(later);
        assert_eq!(s.process(later).len(), 1);
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
        assert_eq!(s.process(t0).len(), 1);

        let recs = s.journal.last_run_moves().unwrap();
        crate::undo::undo(&s.journal, &recs);
        assert!(path.exists());

        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0).len(), 0, "must not re-sort an undone file");
        assert!(path.exists());

        // Once the file changes, it is fair game again.
        fs::write(&path, "pdf, edited").unwrap();
        s.nominate(&path, t0);
        s.process(t0);
        assert_eq!(s.process(t0).len(), 1);
    }

    /// Drives the real loop (with notify) on a thread, the way the tray does.
    #[test]
    fn run_with_obeys_controls_and_reports_moves() {
        use std::sync::mpsc::Receiver;

        let dir = tempfile::tempdir().unwrap();
        let dl = dir.path().join("dl");
        fs::create_dir(&dl).unwrap();
        let config = dir.path().join("config.toml");
        fs::write(
            &config,
            format!(
                "downloads_dir = '{}'\nstable_seconds = 0\n{}",
                dl.display(),
                DEFAULT_CONFIG
                    .lines()
                    .filter(|l| !l.starts_with("stable_seconds"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        )
        .unwrap();
        let locations = WatchPaths {
            config,
            journal: dir.path().join("moves.db"),
        };

        let (controller, inbox) = control_channel();
        let (ev_tx, ev_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            run_with(&locations, inbox, move |e| {
                let _ = ev_tx.send(e);
            })
        });

        // Waits for an event matching `pred`, skipping others.
        fn wait_for(rx: &Receiver<WatchEvent>, pred: impl Fn(&WatchEvent) -> bool) -> WatchEvent {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let e = rx.recv_timeout(left).expect("timed out waiting for event");
                if pred(&e) {
                    return e;
                }
            }
        }

        assert_eq!(
            wait_for(&ev_rx, |e| matches!(e, WatchEvent::Recent(_))),
            WatchEvent::Recent(vec![])
        );

        fs::write(dl.join("a.pdf"), "pdf").unwrap();
        let WatchEvent::Recent(list) = wait_for(
            &ev_rx,
            |e| matches!(e, WatchEvent::Recent(l) if !l.is_empty()),
        ) else {
            unreachable!()
        };
        assert_eq!(list[0].file_name, "a.pdf");
        assert_eq!(list[0].folder, "Documents");
        assert_eq!(list[0].dest, dl.join("Documents").join("a.pdf"));

        // Paused: new files stay put.
        assert!(controller.send(Control::SetPaused(true)));
        wait_for(&ev_rx, |e| *e == WatchEvent::Paused(true));
        fs::write(dl.join("b.png"), "png").unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert!(dl.join("b.png").exists(), "must not move while paused");

        // Resumed: it catches up.
        controller.send(Control::SetPaused(false));
        wait_for(
            &ev_rx,
            |e| matches!(e, WatchEvent::Status(s) if s.contains("b.png")),
        );
        assert!(dl.join("Images").join("b.png").exists());

        // Undo last puts b.png back and the list shrinks to a.pdf only.
        controller.send(Control::UndoLast);
        let WatchEvent::Recent(list) = wait_for(&ev_rx, |e| matches!(e, WatchEvent::Recent(_)))
        else {
            unreachable!()
        };
        assert_eq!(list.len(), 1);
        assert!(dl.join("b.png").exists());

        controller.send(Control::Shutdown);
        worker.join().unwrap().unwrap();
    }
}

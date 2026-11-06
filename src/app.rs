//! Runs the subcommands. `main.rs` only parses arguments and sets up logging.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use crate::cli::{Command, LogArgs};
use crate::config::Config;
use crate::journal::{Journal, LogEntry, PruneReport, Status};
use crate::mover::move_logged;
use crate::paths;
use crate::planner::{Plan, Planner};
use crate::rules::parse_duration;
use crate::startup;
use crate::undo::{UndoOutcome, undo};

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Sort { dry_run } => sort(dry_run),
        Command::Watch { headless } => crate::tray::run_watch(headless),
        Command::Undo { last } => undo_cmd(last),
        Command::Log(args) => log_cmd(&args),
        Command::Install { in_place } => install(in_place),
        Command::Uninstall => uninstall(),
        Command::Config => config_cmd(),
    }
}

fn install(in_place: bool) -> Result<()> {
    if !cfg!(windows) {
        bail!("`install` is only supported on Windows");
    }
    // Fail now, not silently at the next sign-in, if the config is broken.
    let cfg = load_config()?;
    Planner::from_config(&cfg)?;

    if std::env::var_os(paths::HOME_ENV).is_some() {
        eprintln!(
            "warning: {} is set in this shell, but the startup entry won't see it; \
             the watcher will use the normal config location.",
            paths::HOME_ENV
        );
    }
    let exe = if in_place {
        install_in_place()?
    } else {
        install_copy()?
    };

    let command = startup::command_line(&exe);
    startup::set(startup::RUN_KEY, startup::VALUE_NAME, &command)?;
    println!(
        "Startup entry added: HKCU\\{}\\{} = {command}",
        startup::RUN_KEY,
        startup::VALUE_NAME
    );
    warn_if_disabled_in_task_manager();
    start_watcher(&exe)
}

/// The old behaviour: register `taow.exe` next to the running `tao.exe`.
fn install_in_place() -> Result<std::path::PathBuf> {
    let exe = startup::background_exe()?;
    eprintln!(
        "note: registering {} where it is. If you move, rename or delete it, \
         run `tao install` again.",
        exe.display()
    );
    Ok(exe)
}

/// Copies both executables to the install folder and returns the copy of
/// `taow.exe` to register.
fn install_copy() -> Result<std::path::PathBuf> {
    let me = std::env::current_exe().context("cannot locate the running executable")?;
    let from = me
        .parent()
        .context("the running executable has no folder")?;
    let to = paths::install_dir()?;
    let names = [startup::CLI_EXE, startup::BACKGROUND_EXE];
    match startup::copy_executables(&names, from, &to)? {
        startup::Copied::Copied => println!("Copied {} to {}", names.join(" and "), to.display()),
        startup::Copied::AlreadyThere => println!("Already running from {}", to.display()),
    }
    if !on_path(&to, std::env::var_os("PATH")) {
        println!(
            "tip: add {} to your PATH (Settings > System > About > Advanced system settings > \
             Environment Variables) to run `tao` from any terminal.",
            to.display()
        );
    }
    Ok(to.join(startup::BACKGROUND_EXE))
}

/// Whether `dir` is one of the folders in `path_var` (the value of `PATH`).
/// Windows paths compare case-insensitively.
fn on_path(dir: &Path, path_var: Option<std::ffi::OsString>) -> bool {
    let norm = |p: &Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    let want = norm(dir);
    path_var.is_some_and(|v| std::env::split_paths(&v).any(|p| norm(&p) == want))
}

/// Task Manager > Startup apps can disable a Run entry without removing it;
/// Windows then skips it at sign-in even though `install` just wrote it.
/// Only reported here: the StartupApproved value is the user's choice.
fn warn_if_disabled_in_task_manager() {
    match startup::approval(startup::APPROVED_KEY, startup::VALUE_NAME) {
        Ok(startup::Approval::Enabled) => {}
        Ok(startup::Approval::Disabled) => eprintln!(
            "warning: the Tao startup entry is disabled in Task Manager, so Windows won't \
             start it at sign-in. Turn it back on in Task Manager > Startup apps > Tao > Enable."
        ),
        Err(e) => tracing::debug!("could not read the StartupApproved state: {e:#}"),
    }
}

fn start_watcher(exe: &Path) -> Result<()> {
    // Start it now too, so there's no need to sign out and in. If one is
    // already running (e.g. from the old location), the new one notices
    // the named mutex and exits; say so rather than claim we started it.
    if crate::win::SingleInstance::acquire(crate::win::WATCHER_MUTEX)?.is_none() {
        println!(
            "A watcher is already running, so no new one was started; the registered one \
             takes over at the next sign-in. To switch now, quit the running one (tray \
             icon > Quit) and run `tao install` again."
        );
        return Ok(());
    }
    std::process::Command::new(exe)
        // Don't hand our console's handles to the child; a shell waiting on
        // a pipe would otherwise wait for the watcher to exit.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    println!(
        "Watcher started in the background. Logs: {}",
        paths::log_dir()?.display()
    );
    Ok(())
}

fn uninstall() -> Result<()> {
    if startup::remove(startup::RUN_KEY, startup::VALUE_NAME)? {
        println!("Startup entry removed.");
    } else {
        println!("No startup entry was installed.");
    }
    if let Ok(dir) = paths::install_dir()
        && dir.join(startup::BACKGROUND_EXE).exists()
    {
        println!(
            "The installed copies in {} were left in place (Tao never deletes files); \
             remove that folder yourself if you no longer want them.",
            dir.display()
        );
    }
    println!(
        "A watcher that is already running keeps going until you sign out, \
         or end `{}` in Task Manager.",
        startup::BACKGROUND_EXE
    );
    Ok(())
}

fn config_cmd() -> Result<()> {
    let path = paths::config_file()?;
    if crate::config::ensure_exists(&path)? {
        println!("Created default config.");
    }
    println!("Opening {}", path.display());
    println!(
        "Check your changes with `tao sort --dry-run`. A running watcher reloads it automatically."
    );
    crate::win::open_in_editor(&path)
}

pub fn load_config() -> Result<Config> {
    let path = paths::config_file()?;
    if crate::config::ensure_exists(&path)? {
        eprintln!("Created default config at {}", path.display());
    }
    Config::load(&path)
}

pub fn open_journal() -> Result<Journal> {
    Journal::open(&paths::journal_file()?)
}

fn sort(dry_run: bool) -> Result<()> {
    let cfg = load_config()?;
    let planner = Planner::from_config(&cfg)?;
    let root = planner.root().to_path_buf();
    let plan = planner.scan(SystemTime::now())?;

    if dry_run {
        print_plan(&plan, &root);
        return Ok(());
    }

    let journal = open_journal()?;
    let run = journal.begin_run("sort")?;
    let (mut moved, mut failed) = (0, 0);
    for m in &plan.moves {
        let from = display_rel(&m.source, &root);
        match move_logged(&journal, run, m) {
            Ok(to) => {
                moved += 1;
                println!("  {from}  ->  {}", display_rel(&to, &root));
            }
            Err(e) => {
                failed += 1;
                println!("  {from}  NOT MOVED: {e}");
            }
        }
    }
    println!(
        "Moved {moved} file(s), {failed} not moved, {} skipped.",
        plan.skipped.len()
    );
    if moved > 0 {
        println!("Run `tao undo` to put them back.");
    }
    if failed > 0 {
        bail!("{failed} file(s) could not be moved");
    }
    Ok(())
}

fn undo_cmd(last: Option<u32>) -> Result<()> {
    let journal = open_journal()?;
    let records = match last {
        Some(n) => journal.last_moves(n)?,
        None => journal.last_run_moves()?,
    };
    if records.is_empty() {
        println!("Nothing to undo.");
        return Ok(());
    }
    let results = undo(&journal, &records);
    let mut skipped = 0;
    for (rec, outcome) in &results {
        // Show paths relative to the folder the file came from (normally Downloads).
        let base = rec.source.parent().unwrap_or(Path::new(""));
        match outcome {
            UndoOutcome::Restored(to) => println!(
                "  {}  ->  {}",
                display_rel(&rec.dest, base),
                display_rel(to, base)
            ),
            UndoOutcome::NeverMoved => println!(
                "  {}  (was never moved; log entry closed)",
                rec.source.display()
            ),
            UndoOutcome::Skipped(why) => {
                skipped += 1;
                println!("  {}  NOT RESTORED: {why}", rec.dest.display())
            }
        }
    }
    println!(
        "Restored {} file(s), {skipped} skipped.",
        results.len() - skipped
    );
    if skipped > 0 {
        bail!("{skipped} file(s) could not be restored");
    }
    Ok(())
}

fn log_cmd(args: &LogArgs) -> Result<()> {
    let journal = open_journal()?;
    if !args.prune {
        return show_log(&journal, args.limit);
    }
    // clap guarantees --older-than accompanies --prune.
    let text = args.older_than.as_deref().unwrap_or_default();
    let age = parse_duration(text).context("--older-than")?;
    let cutoff = SystemTime::now()
        .checked_sub(age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let report = journal.prune(cutoff, args.dry_run)?;
    print_prune_report(&report, text, args.dry_run);

    if args.vacuum {
        let path = paths::journal_file()?;
        let before = db_size(&path);
        journal.vacuum()?;
        println!(
            "Compacted the log: {} -> {}.",
            format_bytes(before),
            format_bytes(db_size(&path))
        );
    }
    Ok(())
}

fn show_log(journal: &Journal, limit: u32) -> Result<()> {
    let entries = journal.history(limit)?;
    if entries.is_empty() {
        println!("The move log is empty.");
        return Ok(());
    }
    let now = SystemTime::now();
    for e in entries.iter().rev() {
        println!("{}", format_entry(e, now));
    }
    println!(
        "{} most recent entries, oldest first. Database: {}",
        entries.len(),
        paths::journal_file()?.display()
    );
    Ok(())
}

/// `#12  3h ago  done    a.pdf  ->  Documents\a.pdf   [rule "Docs"]`
fn format_entry(e: &LogEntry, now: SystemTime) -> String {
    // Paths relative to the folder the file came from (normally Downloads).
    let base = e.source.parent().unwrap_or(Path::new(""));
    let from = display_rel(&e.source, base);
    let what = match (e.status, &e.undone_to, &e.error) {
        (Status::Undone, Some(to), _) => format!(
            "{from}  ->  {}, put back as {}",
            display_rel(&e.dest, base),
            display_rel(to, base)
        ),
        (Status::Failed, _, Some(err)) => format!("{from}  NOT MOVED: {err}"),
        _ => format!("{from}  ->  {}", display_rel(&e.dest, base)),
    };
    let age = now.duration_since(e.moved_at).unwrap_or_default();
    format!(
        "#{:<5} {:>8}  {:<7} {what}   [{}]",
        e.id,
        format_age(age),
        e.status.as_str(),
        e.reason
    )
}

fn print_prune_report(r: &PruneReport, older_than: &str, dry_run: bool) {
    let verb = if dry_run { "Would remove" } else { "Removed" };
    println!(
        "{verb} {} failed and {} undone move(s) older than {older_than}, and {} empty run(s).",
        r.failed, r.undone, r.runs
    );
    if r.kept_restored > 0 {
        println!(
            "Kept {} undone move(s) whose file is still where undo put it, so the \
             watcher keeps leaving those files alone.",
            r.kept_restored
        );
    }
    println!("Moves that can still be undone are never pruned.");
    if dry_run {
        println!("Dry run: nothing was changed.");
    }
}

/// `45s ago`, `12m ago`, `3h ago`, `5d ago`, `2w ago`.
fn format_age(age: Duration) -> String {
    let s = age.as_secs();
    let (n, unit) = match s {
        0..60 => (s, "s"),
        60..3_600 => (s / 60, "m"),
        3_600..86_400 => (s / 3_600, "h"),
        86_400..1_209_600 => (s / 86_400, "d"),
        _ => (s / 604_800, "w"),
    };
    format!("{n}{unit} ago")
}

/// The database plus its WAL file, which holds recent writes.
fn db_size(path: &Path) -> u64 {
    let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    len(path) + len(Path::new(&wal))
}

fn format_bytes(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

/// Shows a path relative to the sorted folder when it is inside it.
fn display_rel(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn print_plan(plan: &Plan, root: &Path) {
    println!("Dry run for {} (nothing will be moved)", root.display());
    if plan.moves.is_empty() {
        println!("Nothing to sort.");
    }
    for m in &plan.moves {
        println!(
            "  {}  ->  {}\\   [{}]",
            display_rel(&m.source, root),
            display_rel(&m.dest_dir, root),
            m.reason
        );
    }
    if !plan.skipped.is_empty() {
        println!("Skipped:");
        for (path, why) in &plan.skipped {
            println!("  {}   ({why})", display_rel(path, root));
        }
    }
    println!(
        "{} file(s) would be moved, {} skipped.",
        plan.moves.len(),
        plan.skipped.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(windows)]
    #[test]
    fn path_lookup_ignores_case_and_trailing_separator() {
        let dir = Path::new(r"C:\Users\a\AppData\Local\Programs\Tao");
        let path = r"C:\Windows;c:\users\A\appdata\local\programs\tao\;D:\bin";
        assert!(on_path(dir, Some(path.into())));
        assert!(!on_path(
            dir,
            Some(r"C:\Windows;C:\Users\a\AppData\Local\Programs".into())
        ));
        assert!(!on_path(dir, None));
    }

    #[test]
    fn ages() {
        let s = Duration::from_secs;
        assert_eq!(format_age(s(0)), "0s ago");
        assert_eq!(format_age(s(59)), "59s ago");
        assert_eq!(format_age(s(60)), "1m ago");
        assert_eq!(format_age(s(3 * 3600 + 5)), "3h ago");
        assert_eq!(format_age(s(13 * 86_400)), "13d ago");
        assert_eq!(format_age(s(21 * 86_400)), "3w ago");
    }

    #[test]
    fn log_entries_read_naturally() {
        let now = SystemTime::now();
        let dl = PathBuf::from("Downloads");
        let entry = |status, undone_to: Option<&str>, error: Option<&str>| LogEntry {
            id: 7,
            run_id: 1,
            moved_at: now - Duration::from_secs(7200),
            source: dl.join("a.pdf"),
            dest: dl.join("Documents").join("a.pdf"),
            reason: "rule \"Docs\"".into(),
            status,
            error: error.map(String::from),
            undone_to: undone_to.map(|p| dl.join(p)),
        };
        let sep = std::path::MAIN_SEPARATOR;
        let done = format_entry(&entry(Status::Done, None, None), now);
        assert!(done.starts_with("#7"), "{done}");
        assert!(done.contains("2h ago  done"), "{done}");
        assert!(
            done.contains(&format!("a.pdf  ->  Documents{sep}a.pdf   [rule \"Docs\"]")),
            "{done}"
        );
        let undone = format_entry(&entry(Status::Undone, Some("a (1).pdf"), None), now);
        assert!(undone.contains("put back as a (1).pdf"), "{undone}");
        let failed = format_entry(&entry(Status::Failed, None, Some("file is in use")), now);
        assert!(
            failed.contains("a.pdf  NOT MOVED: file is in use"),
            "{failed}"
        );
    }
}

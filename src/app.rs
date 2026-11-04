//! Runs the subcommands. `main.rs` only parses arguments and sets up logging.

use std::path::Path;
use std::process::Stdio;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::cli::Command;
use crate::config::Config;
use crate::journal::Journal;
use crate::mover::move_logged;
use crate::paths;
use crate::planner::{Plan, Planner};
use crate::startup;
use crate::undo::{UndoOutcome, undo};

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Sort { dry_run } => sort(dry_run),
        Command::Watch => crate::watch::run(),
        Command::Undo { last } => undo_cmd(last),
        Command::Install => install(),
        Command::Uninstall => uninstall(),
        Command::Config => config_cmd(),
    }
}

fn install() -> Result<()> {
    let exe = startup::background_exe()?;
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
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "note: registering the build output folder. If you move or rebuild into \
             another location, run `tao install` again from there."
        );
    }

    let command = startup::command_line(&exe);
    startup::set(startup::RUN_KEY, startup::VALUE_NAME, &command)?;
    println!(
        "Startup entry added: HKCU\\{}\\{} = {command}",
        startup::RUN_KEY,
        startup::VALUE_NAME
    );

    // Start it now too, so there's no need to sign out and in. If one is
    // already running, the new one notices (named mutex) and exits.
    std::process::Command::new(&exe)
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

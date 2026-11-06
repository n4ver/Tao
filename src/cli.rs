//! Command-line interface definitions.

use clap::{Args, Parser, Subcommand};

/// Sorts your Downloads folder into category folders. Fully local, no network.
#[derive(Debug, Parser)]
#[command(name = "tao", version, about)]
pub struct Cli {
    /// Show debug output.
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sort everything currently in Downloads, then exit.
    Sort {
        /// Print the planned moves without touching any file.
        #[arg(long)]
        dry_run: bool,
    },
    /// Watch Downloads and sort new files as they finish downloading.
    /// Shows a system tray icon on Windows.
    Watch {
        /// Run without the tray icon (until Ctrl+C or the process is ended).
        #[arg(long)]
        headless: bool,
    },
    /// Move files back to where they came from, using the move log.
    Undo {
        /// Undo the last N individual moves instead of the last run.
        #[arg(long, value_name = "N")]
        last: Option<u32>,
    },
    /// Show recent entries of the move log, or prune old ones.
    Log(LogArgs),
    /// Start `watch` automatically when you sign in to Windows.
    ///
    /// Copies tao.exe and taow.exe to %LOCALAPPDATA%\Programs\Tao and
    /// registers that copy, so moving or rebuilding these files doesn't
    /// break the startup entry.
    Install {
        /// Register taow.exe where it is now instead of copying it.
        #[arg(long)]
        in_place: bool,
    },
    /// Remove the sign-in startup entry.
    Uninstall,
    /// Open the rules file in your default editor.
    Config,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// How many recent entries to show.
    #[arg(long, value_name = "N", default_value_t = 20, conflicts_with = "prune")]
    pub limit: u32,

    /// Delete old entries that undo no longer needs: failed moves, and undone
    /// moves whose file is no longer where undo put it. Moves that can still
    /// be undone are always kept.
    #[arg(long, requires = "older_than")]
    pub prune: bool,

    /// With --prune: only entries that finished longer ago than this, e.g. 90d.
    #[arg(long, value_name = "DURATION", requires = "prune")]
    pub older_than: Option<String>,

    /// With --prune: show how many entries would go, change nothing.
    #[arg(long, requires = "prune")]
    pub dry_run: bool,

    /// With --prune: also compact the database file afterwards.
    #[arg(long, requires = "prune", conflicts_with = "dry_run")]
    pub vacuum: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("tao").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn log_arguments() {
        let Command::Log(a) = parse(&["log"]).unwrap().command else {
            panic!()
        };
        assert_eq!((a.limit, a.prune), (20, false));
        let Command::Log(a) = parse(&["log", "--limit", "5"]).unwrap().command else {
            panic!()
        };
        assert_eq!(a.limit, 5);
        let Command::Log(a) = parse(&["log", "--prune", "--older-than", "90d", "--dry-run"])
            .unwrap()
            .command
        else {
            panic!()
        };
        assert!(a.prune && a.dry_run && !a.vacuum);
        assert_eq!(a.older_than.as_deref(), Some("90d"));

        for bad in [
            &["log", "--prune"][..],
            &["log", "--older-than", "1d"],
            &["log", "--dry-run"],
            &["log", "--prune", "--older-than", "1d", "--limit", "3"],
            &[
                "log",
                "--prune",
                "--older-than",
                "1d",
                "--dry-run",
                "--vacuum",
            ],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}

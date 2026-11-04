//! Command-line interface definitions.

use clap::{Parser, Subcommand};

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
    /// Start `watch` automatically when you sign in to Windows.
    Install,
    /// Remove the sign-in startup entry.
    Uninstall,
    /// Open the rules file in your default editor.
    Config,
}

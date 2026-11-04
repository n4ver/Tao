//! `tao.exe`: the command-line entry point (console subsystem).

use std::process::ExitCode;

use clap::Parser;
use tao::cli::{Cli, Command};
use tao::logging::{self, Output};

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The watcher also logs to the file, so tray actions and background
    // errors are recorded even when started from a terminal.
    let output = match cli.command {
        Command::Watch { .. } => Output::ConsoleAndFile,
        _ => Output::Console,
    };
    if let Err(e) = logging::init(output, cli.verbose) {
        eprintln!("warning: cannot open the log file: {e:#}");
        let _ = logging::init(Output::Console, cli.verbose);
    }

    match tao::app::run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` prints the whole anyhow context chain on one line.
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

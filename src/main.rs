//! `tao.exe`: the command-line entry point (console subsystem).

use std::process::ExitCode;

use clap::Parser;
use tao::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();

    let default_level = if cli.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_level)),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(false)
        .init();

    match tao::app::run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` prints the whole anyhow context chain on one line.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

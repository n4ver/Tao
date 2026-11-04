//! `taow.exe`: the background watcher, with no console window.
//!
//! Windows decides whether to give a program a console *before* it starts,
//! based on the "subsystem" field in the .exe header. `tao.exe` is a
//! console program, so launching it at sign-in would open a terminal window.
//! This attribute builds this binary for the GUI subsystem instead: no window
//! appears, and stdout/stderr go nowhere, which is why it logs to a file.
//!
//! It does exactly one thing (`tao watch`) and takes no arguments, the same
//! split as python.exe / pythonw.exe. On other platforms the attribute is ignored.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

fn main() -> ExitCode {
    let _guard = match init_file_logging() {
        Ok(g) => g,
        // Nowhere to report this without a console; just exit with failure.
        Err(_) => return ExitCode::FAILURE,
    };

    // Without a console a panic message would vanish, so route it to the log.
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));

    match tao::watch::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("watcher stopped: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logs to `%LOCALAPPDATA%\Tao\logs\watch.YYYY-MM-DD.log`,
/// one file per day, keeping the last 7.
fn init_file_logging() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_appender::rolling::{Builder, Rotation};

    let dir = tao::paths::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("watch")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?;
    // Writes happen on a helper thread; the guard flushes them on exit.
    let (writer, guard) = tracing_appender::non_blocking(appender);
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .init();
    Ok(guard)
}

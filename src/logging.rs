//! Log output: the terminal, the daily log file, or both.

use anyhow::Result;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Short commands (`sort`, `undo`, ...): stderr only.
    Console,
    /// `tao watch` in a terminal: stderr, plus the log file so tray and
    /// watcher errors are on record either way.
    ConsoleAndFile,
    /// `taow.exe`: it has no console, so the file is the only place.
    File,
}

/// Sets up logging for the whole process. `RUST_LOG` overrides the level.
pub fn init(output: Output, verbose: bool) -> Result<()> {
    let level = if verbose { "debug" } else { "info" };
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let console = matches!(output, Output::Console | Output::ConsoleAndFile).then(|| {
        fmt::layer()
            .with_writer(std::io::stderr)
            .without_time()
            .with_target(false)
            .with_filter(filter())
    });
    let file = match output {
        Output::File | Output::ConsoleAndFile => Some(
            fmt::layer()
                .with_writer(file_appender()?)
                .with_ansi(false)
                .with_target(false)
                .with_filter(filter()),
        ),
        Output::Console => None,
    };
    tracing_subscriber::registry()
        .with(console)
        .with(file)
        .init();

    if output != Output::Console {
        // A panic message normally goes to stderr, which `taow.exe` doesn't
        // have. Record it in the log too (after the default hook prints it).
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!("panic: {info}");
            default_hook(info);
        }));
    }
    Ok(())
}

/// `%LOCALAPPDATA%\Tao\logs\watch.YYYY-MM-DD.log`, a new file each day,
/// keeping the last 7.
///
/// Writes are synchronous (no background writer thread). The watcher logs
/// rarely, and the tray's event loop ends the process with `exit`, which
/// would skip flushing a buffered writer and lose the last lines.
fn file_appender() -> Result<tracing_appender::rolling::RollingFileAppender> {
    use tracing_appender::rolling::{Builder, Rotation};
    let dir = crate::paths::log_dir()?;
    std::fs::create_dir_all(&dir)?;
    Ok(Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix("watch")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)?)
}

//! `taow.exe`: the background watcher with a tray icon and no console window.
//!
//! Windows decides whether to give a program a console *before* it starts,
//! based on the "subsystem" field in the .exe header. `tao.exe` is a
//! console program, so launching it at sign-in would open a terminal window.
//! This attribute builds this binary for the GUI subsystem instead: no
//! console appears, and stdout/stderr go nowhere, which is why everything,
//! errors included, goes to the log file.
//!
//! It does one thing (`tao watch`) and takes one optional flag,
//! `--headless`, to run without the tray icon. Same split as python.exe /
//! pythonw.exe. On other platforms the attribute is ignored.
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use tao::logging::{self, Output};

fn main() -> ExitCode {
    let headless = std::env::args().skip(1).any(|a| a == "--headless");

    if logging::init(Output::File, false).is_err() {
        // Nowhere to report this without a console; just exit with failure.
        return ExitCode::FAILURE;
    }

    match tao::tray::run_watch(headless) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("watcher stopped: {e:#}");
            ExitCode::FAILURE
        }
    }
}

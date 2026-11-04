//! Tao: sorts a Downloads folder into category folders.
//!
//! The library holds all logic so that both executables (`tao.exe` for
//! the CLI and `taow.exe` for the windowless background watcher)
//! share it, and so that it can be unit tested.

pub mod app;
pub mod classify;
pub mod cli;
pub mod config;
pub mod facts;
pub mod journal;
pub mod logging;
pub mod mover;
pub mod paths;
pub mod planner;
pub mod rules;
pub mod stable;
pub mod startup;
pub mod tray;
pub mod undo;
pub mod watch;
pub mod win;

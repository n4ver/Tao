//! Where things live on disk.
//!
//! | What        | Location                        |
//! |-------------|---------------------------------|
//! | Config      | `%APPDATA%\Tao\config.toml`     |
//! | Move log DB | `%LOCALAPPDATA%\Tao\moves.db`   |
//! | Watch logs  | `%LOCALAPPDATA%\Tao\logs\`      |
//!
//! `%APPDATA%` is the *roaming* profile (follows the user between machines in
//! a domain), which suits a hand-edited config. The SQLite log contains
//! absolute, machine-specific paths, so it goes in `%LOCALAPPDATA%`, which
//! never roams.
//!
//! Setting the `TAO_HOME` environment variable puts *all* of the
//! above under that one directory instead. That is meant for testing and for
//! trying the tool out without touching your real profile.

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::{BaseDirs, UserDirs};

const APP_DIR: &str = "Tao";
pub const HOME_ENV: &str = "TAO_HOME";

fn home_override() -> Option<PathBuf> {
    std::env::var_os(HOME_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Directory holding `config.toml`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(home) = home_override() {
        return Ok(home);
    }
    // On Windows `BaseDirs::config_dir()` is FOLDERID_RoamingAppData, i.e. %APPDATA%.
    let base = BaseDirs::new().context("could not determine the user's profile directories")?;
    Ok(base.config_dir().join(APP_DIR))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Directory holding the move log and log files.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(home) = home_override() {
        return Ok(home);
    }
    // On Windows `data_local_dir()` is FOLDERID_LocalAppData, i.e. %LOCALAPPDATA%.
    let base = BaseDirs::new().context("could not determine the user's profile directories")?;
    Ok(base.data_local_dir().join(APP_DIR))
}

pub fn journal_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("moves.db"))
}

pub fn log_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("logs"))
}

/// Where `tao install` copies the executables: `%LOCALAPPDATA%\Programs\Tao`.
///
/// `%LOCALAPPDATA%\Programs` is the per-user counterpart of Program Files
/// (per-user installers such as VS Code use it); it needs no admin rights
/// and doesn't roam. `TAO_HOME` deliberately doesn't apply: the startup
/// entry must point at a stable place that doesn't depend on a shell
/// variable.
pub fn install_dir() -> Result<PathBuf> {
    let base = BaseDirs::new().context("could not determine the user's profile directories")?;
    Ok(base.data_local_dir().join("Programs").join(APP_DIR))
}

/// The user's Downloads folder as Windows reports it.
///
/// This asks the shell for `FOLDERID_Downloads` (via `SHGetKnownFolderPath`)
/// instead of assuming `%USERPROFILE%\Downloads`, so it still works when the
/// user has moved Downloads to another drive with Properties > Location.
pub fn default_downloads_dir() -> Result<PathBuf> {
    let user = UserDirs::new().context("could not determine the user's profile directories")?;
    user.download_dir()
        .map(PathBuf::from)
        .context("Windows did not report a Downloads folder")
}

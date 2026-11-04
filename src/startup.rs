//! The "run at sign-in" entry.
//!
//! Windows starts every program listed as a value under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` when that user signs
//! in. HKCU is the user's own registry hive, so no admin rights are needed.
//! The entry appears in Task Manager > Startup apps, where it can also be
//! disabled (Windows records that separately under `...\Explorer\StartupApproved`,
//! so `install` alone won't re-enable an entry the user turned off there).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "Tao";

/// File name of the windowless background executable.
pub const BACKGROUND_EXE: &str = if cfg!(windows) { "taow.exe" } else { "taow" };

/// `taow.exe` in the same folder as the running executable.
pub fn background_exe() -> Result<PathBuf> {
    let me = std::env::current_exe().context("cannot locate the running executable")?;
    let path = me.with_file_name(BACKGROUND_EXE);
    if !path.is_file() {
        bail!(
            "{} not found next to {}; build it with `cargo build --release` and keep both files together",
            BACKGROUND_EXE,
            me.display()
        );
    }
    Ok(path)
}

/// The command line stored in the Run value. Quoted, because paths with
/// spaces would otherwise be split at the first space.
pub fn command_line(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};

    pub fn set(key: &str, name: &str, command: &str) -> Result<()> {
        let (run, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(key)
            .with_context(|| format!(r"opening HKCU\{key}"))?;
        // REG_SZ string value: the command line to run.
        run.set_value(name, &command.to_string())
            .with_context(|| format!(r"writing HKCU\{key}\{name}"))?;
        Ok(())
    }

    pub fn get(key: &str, name: &str) -> Result<Option<String>> {
        let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ)
        else {
            return Ok(None);
        };
        match run.get_value::<String, _>(name) {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Returns `false` if there was nothing to remove.
    pub fn remove(key: &str, name: &str) -> Result<bool> {
        let Ok(run) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_SET_VALUE)
        else {
            return Ok(false);
        };
        match run.delete_value(name) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!(r"removing HKCU\{key}\{name}")),
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        bail!("`install` is only supported on Windows")
    }
    pub fn get(_: &str, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn remove(_: &str, _: &str) -> Result<bool> {
        bail!("`uninstall` is only supported on Windows")
    }
}

pub use imp::{get, remove, set};

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// Uses a throwaway key, never the real Run key.
    #[test]
    fn set_get_remove_round_trip() {
        let key = format!(r"Software\tao-test-{}", std::process::id());
        let cmd = command_line(Path::new(r"C:\Program Files\Tao\taow.exe"));
        assert_eq!(cmd, r#""C:\Program Files\Tao\taow.exe""#);

        assert_eq!(get(&key, VALUE_NAME).unwrap(), None);
        assert!(!remove(&key, VALUE_NAME).unwrap());
        set(&key, VALUE_NAME, &cmd).unwrap();
        assert_eq!(
            get(&key, VALUE_NAME).unwrap().as_deref(),
            Some(cmd.as_str())
        );
        assert!(remove(&key, VALUE_NAME).unwrap());
        assert_eq!(get(&key, VALUE_NAME).unwrap(), None);

        RegKey::predef(HKEY_CURRENT_USER)
            .delete_subkey(&key)
            .unwrap();
    }
}

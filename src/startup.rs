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

/// Where Task Manager records that a Run entry was disabled. Read only:
/// `install` reports a disabled entry but never changes this.
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// File name of the windowless background executable.
pub const BACKGROUND_EXE: &str = if cfg!(windows) { "taow.exe" } else { "taow" };
/// File name of the command-line executable.
pub const CLI_EXE: &str = if cfg!(windows) { "tao.exe" } else { "tao" };

/// Whether Task Manager > Startup apps has the entry enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Enabled,
    Disabled,
}

/// Interprets a `StartupApproved\Run` value (REG_BINARY, normally 12 bytes).
///
/// The format is undocumented. As observed: byte 0 is a flag where the
/// low bit means *disabled*: `0x02` (and `0x06`) are written when the entry
/// is enabled, `0x03` (and `0x01`, `0x07`) when it is disabled; bytes 4-11
/// then hold the FILETIME of when it was disabled. Bytes 1-3 are zero. No
/// value at all means the user never toggled it, which counts as enabled.
pub fn parse_approval(value: Option<&[u8]>) -> Approval {
    match value.and_then(|v| v.first()) {
        Some(flag) if flag & 1 == 1 => Approval::Disabled,
        _ => Approval::Enabled,
    }
}

/// Reads the Task Manager state of the Run value `name`.
pub fn approval(key: &str, name: &str) -> Result<Approval> {
    Ok(parse_approval(imp::get_binary(key, name)?.as_deref()))
}

/// What [`copy_executables`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Copied {
    /// Both files were copied (replacing older copies of themselves).
    Copied,
    /// `from` already is `to`: running from the install folder.
    AlreadyThere,
}

/// Copies `names` from `from` into `to` (created if needed), replacing
/// previous copies of the same files.
///
/// Nothing is copied unless every target can be replaced: a running .exe
/// cannot be opened for writing (Windows maps the image with sharing that
/// forbids writers, giving ERROR_SHARING_VIOLATION), so each existing target
/// is opened for writing first, without truncating it, and the whole copy is
/// refused with a clear message if one is in use. Nothing is deleted.
pub fn copy_executables(names: &[&str], from: &Path, to: &Path) -> Result<Copied> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    if same_dir(from, to) {
        return Ok(Copied::AlreadyThere);
    }
    for name in names {
        let src = from.join(name);
        if !src.is_file() {
            bail!(
                "{} not found; keep {} together",
                src.display(),
                names.join(" and ")
            );
        }
        ensure_replaceable(&to.join(name))?;
    }
    for name in names {
        let (src, dst) = (from.join(name), to.join(name));
        std::fs::copy(&src, &dst)
            .with_context(|| format!("copying {} to {}", src.display(), dst.display()))?;
    }
    Ok(Copied::Copied)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Fails if `target` exists and can't be overwritten, most likely because
/// it is running.
fn ensure_replaceable(target: &Path) -> Result<()> {
    let opened = std::fs::OpenOptions::new().write(true).open(target);
    match opened {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if crate::mover::is_in_use(&e) => {
            let name = target.file_name().unwrap_or_default().to_string_lossy();
            bail!(
                "{} is running, so it can't be replaced ({e}). Quit it first (tray icon > Quit, \
                 or end `{name}` in Task Manager) and run `tao install` again. Nothing was copied.",
                target.display()
            )
        }
        Err(e) => Err(e).with_context(|| format!("cannot replace {}", target.display())),
    }
}

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

    /// A REG_BINARY (or any) value's raw bytes, `None` if it doesn't exist.
    pub fn get_binary(key: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let Ok(k) = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(key, KEY_READ) else {
            return Ok(None);
        };
        match k.get_raw_value(name) {
            Ok(v) => Ok(Some(v.bytes.to_vec())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!(r"reading HKCU\{key}\{name}")),
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
    pub fn get_binary(_: &str, _: &str) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    pub fn remove(_: &str, _: &str) -> Result<bool> {
        bail!("`uninstall` is only supported on Windows")
    }
}

pub use imp::{get, remove, set};

#[cfg(test)]
mod portable_tests {
    use super::*;
    use std::fs;

    #[test]
    fn approval_bytes() {
        let filetime = [0x10, 0x2a, 0x3b, 0x4c, 0x5d, 0x6e, 0x01, 0xdb];
        let value = |flag: u8| {
            let mut v = vec![flag, 0, 0, 0];
            v.extend(filetime);
            v
        };
        assert_eq!(parse_approval(None), Approval::Enabled);
        assert_eq!(parse_approval(Some(&[])), Approval::Enabled);
        for enabled in [0x02, 0x06] {
            assert_eq!(parse_approval(Some(&value(enabled))), Approval::Enabled);
        }
        for disabled in [0x01, 0x03, 0x07] {
            assert_eq!(parse_approval(Some(&value(disabled))), Approval::Disabled);
        }
    }

    #[test]
    fn copies_both_files_and_replaces_old_copies() {
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let dest = to.path().join("Programs").join("Tao");
        fs::write(from.path().join("a.exe"), "new a").unwrap();
        fs::write(from.path().join("b.exe"), "new b").unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("a.exe"), "old a, longer than the new one").unwrap();

        let got = copy_executables(&["a.exe", "b.exe"], from.path(), &dest).unwrap();
        assert_eq!(got, Copied::Copied);
        assert_eq!(fs::read_to_string(dest.join("a.exe")).unwrap(), "new a");
        assert_eq!(fs::read_to_string(dest.join("b.exe")).unwrap(), "new b");
        // The originals are copied, not moved.
        assert!(from.path().join("a.exe").exists());
        // Installing from the install folder itself is a no-op.
        assert_eq!(
            copy_executables(&["a.exe", "b.exe"], &dest, &dest).unwrap(),
            Copied::AlreadyThere
        );
    }

    #[test]
    fn missing_source_is_an_error_and_copies_nothing() {
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(from.path().join("a.exe"), "a").unwrap();
        let err = copy_executables(&["a.exe", "b.exe"], from.path(), to.path()).unwrap_err();
        assert!(format!("{err:#}").contains("b.exe"), "{err:#}");
        assert!(!to.path().join("a.exe").exists());
    }

    /// A running exe is held open without write sharing; simulate that and
    /// check the copy is refused up front with a helpful message.
    #[cfg(windows)]
    #[test]
    fn running_target_is_refused_before_anything_is_copied() {
        use std::os::windows::fs::OpenOptionsExt;
        let (from, to) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for name in ["a.exe", "b.exe"] {
            fs::write(from.path().join(name), "new").unwrap();
            fs::write(to.path().join(name), "old").unwrap();
        }
        // FILE_SHARE_READ | FILE_SHARE_DELETE, like a mapped executable image.
        let _running = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x4)
            .open(to.path().join("b.exe"))
            .unwrap();
        let err = copy_executables(&["a.exe", "b.exe"], from.path(), to.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("is running") && msg.contains("Nothing was copied"),
            "{msg}"
        );
        assert_eq!(fs::read_to_string(to.path().join("a.exe")).unwrap(), "old");
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    /// Uses a throwaway key, never the real StartupApproved key.
    #[test]
    fn approval_is_read_from_the_registry() {
        use winreg::RegValue;
        use winreg::enums::REG_BINARY;
        let key = format!(r"Software\tao-test-approved-{}", std::process::id());
        assert_eq!(approval(&key, VALUE_NAME).unwrap(), Approval::Enabled);
        let (k, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(&key)
            .unwrap();
        let disabled = RegValue {
            vtype: REG_BINARY,
            bytes: vec![3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8].into(),
        };
        k.set_raw_value(VALUE_NAME, &disabled).unwrap();
        assert_eq!(approval(&key, VALUE_NAME).unwrap(), Approval::Disabled);
        RegKey::predef(HKEY_CURRENT_USER)
            .delete_subkey_all(&key)
            .unwrap();
    }

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

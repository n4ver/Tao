//! Small wrappers around Win32 calls, with portable stand-ins elsewhere.

use std::path::Path;

use anyhow::Result;

/// Converts to a NUL-terminated UTF-16 string, as the Win32 "W" (wide)
/// functions expect.
#[cfg(windows)]
pub fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// Paths at least this long get the `\\?\` prefix in [`wide_path`]. 248 is
/// MAX_PATH (260) minus room for an 8.3 file name, the limit
/// `CreateDirectoryW` applies; std uses the same threshold.
#[cfg(windows)]
const LONG_PATH: usize = 248;

/// Like [`wide`], for passing a *path* to a Win32 "W" function, but also
/// works for paths longer than MAX_PATH (260 characters).
///
/// Without long-path support, Win32 file functions reject paths over
/// MAX_PATH. That support needs both the `longPathAware` manifest (which
/// our executables have) and the system-wide `LongPathsEnabled` setting
/// (which many PCs don't have). The `\\?\` prefix works regardless: it
/// tells Windows to pass the path to the file system as-is, up to ~32,767
/// characters. "As-is" also means no clean-up, so the path is first made
/// absolute and normalised (`/` to `\`, `.` and `..` resolved) with
/// `std::path::absolute` (`GetFullPathNameW`). std::fs does the same
/// internally, which is why only our direct Win32 calls need this.
#[cfg(windows)]
pub fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let len = path.as_os_str().encode_wide().count();
    if len < LONG_PATH {
        return wide(path);
    }
    match std::path::absolute(path) {
        Ok(abs) => verbatim(&abs),
        // Can't normalise it: let Windows report the error for the original.
        Err(_) => wide(path),
    }
}

/// `C:\x` -> `\\?\C:\x`, `\\server\share\x` -> `\\?\UNC\server\share\x`.
/// Already-verbatim paths are left alone. `abs` must be absolute.
#[cfg(windows)]
fn verbatim(abs: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let units: Vec<u16> = abs.as_os_str().encode_wide().collect();
    let text = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let (prefix, rest) = if units.starts_with(&text(r"\\?\")) || units.starts_with(&text(r"\\.\")) {
        (Vec::new(), &units[..])
    } else if units.starts_with(&text(r"\\")) {
        (text(r"\\?\UNC\"), &units[2..])
    } else {
        (text(r"\\?\"), &units[..])
    };
    prefix
        .into_iter()
        .chain(rest.iter().copied())
        .chain(Some(0))
        .collect()
}

/// The named mutex held by the running watcher (see [`SingleInstance`]).
pub const WATCHER_MUTEX: &str = "tao-watch";

/// Held for as long as this process is "the" watcher.
///
/// Uses a named mutex: the kernel object exists while any process holds a
/// handle to it, so the second `CreateMutexW` with the same name opens the
/// existing one and reports `ERROR_ALREADY_EXISTS`. The `Local\` prefix
/// scopes the name to the current sign-in session, so two users on one PC
/// each get their own watcher. Unlike a lock file, it cannot go stale after
/// a crash: Windows closes the handle when the process ends.
pub struct SingleInstance {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl SingleInstance {
    /// Returns `None` if another process already holds `name`.
    #[cfg(windows)]
    pub fn acquire(name: &str) -> Result<Option<Self>> {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let wname = wide(format!("Local\\{name}"));
        // SAFETY: the name is a valid NUL-terminated wide string and a null
        // security-attributes pointer means "default".
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wname.as_ptr()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        // GetLastError must be read right after the call it describes.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe { CloseHandle(handle) };
            return Ok(None);
        }
        Ok(Some(Self { handle }))
    }

    #[cfg(not(windows))]
    pub fn acquire(_name: &str) -> Result<Option<Self>> {
        Ok(Some(Self {}))
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: we own this handle and close it exactly once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Opens a file with the program Windows associates with its extension,
/// like double-clicking it in Explorer. Falls back to Notepad if `.toml` has
/// no association (common on a fresh install).
#[cfg(windows)]
pub fn open_in_editor(path: &Path) -> Result<()> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let (verb, file) = (wide("open"), wide(path));
    // SAFETY: all strings are NUL-terminated wide strings that outlive the call.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // For historical (16-bit Windows) reasons ShellExecute returns a fake
    // HINSTANCE: values above 32 mean success, 32 or less are error codes
    // such as SE_ERR_NOASSOC (31, "no program is associated").
    if result as isize > 32 {
        return Ok(());
    }
    tracing::debug!(
        "ShellExecute failed with code {}; using Notepad",
        result as isize
    );
    std::process::Command::new("notepad.exe")
        .arg(path)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn open_in_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    std::process::Command::new(editor).arg(path).status()?;
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn text(units: &[u16]) -> String {
        String::from_utf16(units.strip_suffix(&[0]).unwrap()).unwrap()
    }

    #[test]
    fn short_paths_are_passed_unchanged() {
        assert_eq!(
            text(&wide_path(Path::new(r"C:\Users\a\Downloads\x.pdf"))),
            r"C:\Users\a\Downloads\x.pdf"
        );
    }

    #[test]
    fn long_paths_become_verbatim_and_normalised() {
        let deep = format!(r"C:\Users\a\Downloads/Images\..\Docs\{}", "x".repeat(250));
        assert_eq!(
            text(&wide_path(Path::new(&deep))),
            format!(r"\\?\C:\Users\a\Downloads\Docs\{}", "x".repeat(250))
        );
        let unc = format!(r"\\nas\share\{}", "y".repeat(250));
        assert_eq!(
            text(&wide_path(Path::new(&unc))),
            format!(r"\\?\UNC\nas\share\{}", "y".repeat(250))
        );
        let already = format!(r"\\?\C:\{}", "z".repeat(250));
        assert_eq!(text(&wide_path(Path::new(&already))), already);
    }

    #[test]
    fn second_acquire_of_the_same_name_fails_until_released() {
        let name = format!("tao-test-{}", std::process::id());
        let first = SingleInstance::acquire(&name).unwrap();
        assert!(first.is_some());
        assert!(SingleInstance::acquire(&name).unwrap().is_none());
        drop(first);
        assert!(SingleInstance::acquire(&name).unwrap().is_some());
    }
}

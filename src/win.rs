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

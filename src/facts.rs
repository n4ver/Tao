//! Facts about a single file that rules can match on.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Everything the rule engine knows about a file.
///
/// Facts that are cheap to read come from one `metadata` call. Expensive
/// facts should be added as lazily-computed fields so that rules which don't
/// need them don't pay for them. The planned example is the download source
/// URL from the `Zone.Identifier` alternate data stream:
///
/// ```ignore
/// zone: std::cell::OnceCell<Option<ZoneInfo>>,
///
/// pub fn zone(&self) -> Option<&ZoneInfo> {
///     // NTFS alternate data streams are opened as "<path>:<stream name>".
///     // Browsers write `[ZoneTransfer] ZoneId=3 HostUrl=... ReferrerUrl=...`
///     // into "file.zip:Zone.Identifier" (this is what triggers the
///     // "this file came from another computer" warning).
///     self.zone.get_or_init(|| read_zone_identifier(&self.path)).as_ref()
/// }
/// ```
///
/// A matching `Condition::SourceUrl(Regex)` in `rules.rs` would then call
/// `facts.zone()`.
#[derive(Debug, Clone)]
pub struct FileFacts {
    pub path: PathBuf,
    /// File name only, e.g. `report.pdf`. Non-UTF-16-valid names (rare on
    /// Windows) are converted lossily for matching purposes.
    pub file_name: String,
    pub size: u64,
    pub modified: SystemTime,
    pub is_dir: bool,
    /// The Windows hidden or system attribute is set (e.g. `desktop.ini`).
    pub hidden_or_system: bool,
}

impl FileFacts {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        // `symlink_metadata` so that a symlink or junction in Downloads is
        // reported as itself and not as whatever it points to.
        let meta = std::fs::symlink_metadata(path)?;
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            path: path.to_path_buf(),
            hidden_or_system: is_hidden_or_system(&meta, &file_name),
            file_name,
            size: meta.len(),
            modified: meta.modified()?,
            is_dir: !meta.is_file(),
        })
    }

    /// How long ago the file was last modified. A modified time in the
    /// future (clock skew, odd archive tools) counts as age zero.
    pub fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.modified).unwrap_or_default()
    }

    /// The file name in lower case, for case-insensitive comparisons.
    /// Windows file names are case-insensitive, so rules are too.
    pub fn lower_name(&self) -> String {
        self.file_name.to_lowercase()
    }

    /// Builds facts without touching the disk, for tests.
    #[cfg(test)]
    pub fn fake(name: &str, modified: SystemTime) -> Self {
        Self {
            path: PathBuf::from(name),
            file_name: name.to_string(),
            size: 0,
            modified,
            is_dir: false,
            hidden_or_system: false,
        }
    }
}

#[cfg(windows)]
fn is_hidden_or_system(meta: &std::fs::Metadata, _name: &str) -> bool {
    use std::os::windows::fs::MetadataExt;
    // Values from winnt.h. Hidden/system are attributes on Windows, not a
    // naming convention as on Unix.
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    meta.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0
}

#[cfg(not(windows))]
fn is_hidden_or_system(_meta: &std::fs::Metadata, name: &str) -> bool {
    name.starts_with('.')
}

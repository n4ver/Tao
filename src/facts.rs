//! Facts about a single file that rules can match on.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::zone::ZoneInfo;

/// Everything the rule engine knows about a file.
///
/// Facts that are cheap to read come from one `metadata` call. Expensive
/// facts are lazily-computed fields so that rules which don't need them
/// don't pay for them. The download source from the `Zone.Identifier`
/// alternate data stream is one: it is read the first time a
/// `source_url`/`source_host` condition calls [`FileFacts::zone`], and
/// never for rules without those conditions.
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
    /// Filled on first use by [`FileFacts::zone`].
    zone: OnceCell<Option<ZoneInfo>>,
}

/// A Zone.Identifier is a few hundred bytes; anything bigger is not one we
/// want to read in full.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_ZONE_BYTES: u64 = 64 * 1024;

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
            zone: OnceCell::new(),
        })
    }

    /// Where the file was downloaded from, if the browser recorded it.
    /// Read from disk on the first call only. `None` if there is no
    /// Zone.Identifier, it can't be read, or it holds no `[ZoneTransfer]`
    /// section; a problem here never stops a file from being sorted.
    pub fn zone(&self) -> Option<&ZoneInfo> {
        self.zone
            .get_or_init(|| read_zone_identifier(&self.path))
            .as_ref()
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
            zone: OnceCell::new(),
        }
    }

    /// Test helper: facts whose Zone.Identifier is already "read".
    #[cfg(test)]
    pub fn with_zone(self, zone: Option<ZoneInfo>) -> Self {
        Self {
            zone: OnceCell::from(zone),
            ..self
        }
    }

    /// Test helper: whether [`FileFacts::zone`] has been called.
    #[cfg(test)]
    pub fn zone_was_read(&self) -> bool {
        self.zone.get().is_some()
    }
}

/// Reads `<path>:Zone.Identifier`.
///
/// NTFS alternate data streams are opened by appending `:<stream name>` to
/// the file's path; the normal file APIs (here `CreateFileW` via std) do the
/// rest. Browsers write `[ZoneTransfer] ZoneId=3 HostUrl=... ReferrerUrl=...`
/// into it (this is what triggers the "this file came from another
/// computer" warning). Files on FAT/exFAT drives, files copied from such
/// drives, and files not downloaded by a browser have no such stream, which
/// is the common `NotFound` case here.
#[cfg(windows)]
fn read_zone_identifier(path: &Path) -> Option<ZoneInfo> {
    use std::io::Read;
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    // std opens with FILE_SHARE_READ | WRITE | DELETE, so holding this
    // briefly never blocks the browser or our own rename.
    let file = std::fs::File::open(&stream).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_ZONE_BYTES).read_to_end(&mut bytes).ok()?;
    crate::zone::parse(&bytes)
}

/// Alternate data streams are an NTFS feature; elsewhere there is no source.
#[cfg(not(windows))]
fn read_zone_identifier(_path: &Path) -> Option<ZoneInfo> {
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_stream_means_no_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, "x").unwrap();
        let facts = FileFacts::read(&path).unwrap();
        assert!(
            !facts.zone_was_read(),
            "reading facts must not read the stream"
        );
        assert_eq!(facts.zone(), None);
        assert!(facts.zone_was_read());
    }

    /// Writes a real alternate data stream in a temp dir (NTFS) and reads it
    /// back the way the rules do.
    #[cfg(windows)]
    #[test]
    fn reads_a_real_zone_identifier_stream() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.zip");
        std::fs::write(&path, "zip bytes").unwrap();
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::write(
            &stream,
            "[ZoneTransfer]\r\nZoneId=3\r\nReferrerUrl=https://github.com/o/r/releases\r\n\
             HostUrl=https://objects.githubusercontent.com/asset\r\n",
        )
        .unwrap();
        // The main stream is untouched by writing the alternate one.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "zip bytes");

        let facts = FileFacts::read(&path).unwrap();
        assert_eq!(facts.size, 9);
        let zone = facts.zone().unwrap();
        assert_eq!(zone.zone_id, Some(3));
        assert_eq!(
            zone.host_url.as_deref(),
            Some("https://objects.githubusercontent.com/asset")
        );
        assert_eq!(
            zone.referrer_url.as_deref(),
            Some("https://github.com/o/r/releases")
        );
    }

    /// An unreadable or oversized stream is tolerated, never an error.
    #[cfg(windows)]
    #[test]
    fn garbage_stream_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("odd.bin");
        std::fs::write(&path, "x").unwrap();
        let mut stream = path.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        std::fs::write(&stream, vec![0u8; (MAX_ZONE_BYTES as usize) * 2]).unwrap();
        assert_eq!(FileFacts::read(&path).unwrap().zone(), None);
    }
}

//! The TOML config file: schema, built-in default, and loading.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// The config file written on first run. It is kept as literal text (rather
/// than serialising a `Config` value) so that it can carry explanatory comments.
pub const DEFAULT_CONFIG: &str = r#"# Tao configuration.
# Run `tao config` to open this file, and `tao sort --dry-run` to preview
# what the rules would do before anything is moved.

# Folder to sort. Leave commented out to use your Windows Downloads folder.
# downloads_dir = 'D:\Downloads'

# Seconds a file's size and modified time must stay unchanged before
# `tao watch` moves it.
stable_seconds = 5

# Files with these extensions are partial downloads and are never touched.
# A file is also skipped while a sibling "<name>.<ext>" with one of these
# extensions exists (Firefox creates an empty placeholder next to its .part file).
skip_extensions = ["crdownload", "part", "partial", "tmp", "opdownload", "download"]

# Skip files with the Windows "hidden" or "system" attribute, such as desktop.ini.
skip_hidden = true

# Where files go when no rule matches.
fallback_dest = "Other"

# Rules are checked from top to bottom and the FIRST matching rule wins.
# Every condition given in a rule must match. Available conditions:
#   extensions = ["pdf", "tar.gz"]   file name ends with one of these (no dot, any case)
#   glob       = "Screenshot*"       wildcard match on the file name (any case)
#   regex      = '^invoice[-_ ]\d+'  regular expression on the file name (any case)
#   older_than = "30d"               last modified more than this long ago (s, m, h, d, w)
#   newer_than = "1h"                last modified less than this long ago
#   min_size   = "500MB"             at least this big (B, KB, MB, GB, TB; 1 KB = 1024 bytes)
#   max_size   = "20KB"              at most this big; fractions like "1.5GB" are fine
#   source_url = 'github\.com/.*/releases'
#                                    regex on the download's source or referring page
#                                    (from the Zone.Identifier browsers attach; any case)
#   source_host = ["github.com"]     downloaded from this site or a subdomain of it
#                                    (github.com also matches objects.github.com)
# `dest` is a folder. Relative paths are relative to the sorted folder;
# absolute paths (e.g. 'D:\Archive') are used as they are.

# Example: keep GitHub release downloads together. Release assets are served
# from githubusercontent.com with github.com as the referring page, so list
# both. To enable it, uncomment the four lines (it is above the general
# rules, so it wins).
# [[rule]]
# name = "GitHub downloads"
# source_host = ["github.com", "githubusercontent.com"]
# dest = "GitHub"

[[rule]]
name = "Installers"
extensions = ["exe", "msi", "msix", "msixbundle", "appx", "appxbundle"]
dest = "Installers"

[[rule]]
name = "Documents"
extensions = ["pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "odp", "epub"]
dest = "Documents"

[[rule]]
name = "Images"
extensions = ["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "svg", "heic", "avif", "ico"]
dest = "Images"

[[rule]]
name = "Archives"
extensions = ["zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "iso"]
dest = "Archives"

[[rule]]
name = "Video"
extensions = ["mp4", "mkv", "mov", "avi", "webm", "wmv", "m4v", "flv"]
dest = "Video"

[[rule]]
name = "Audio"
extensions = ["mp3", "wav", "flac", "aac", "ogg", "opus", "m4a", "wma"]
dest = "Audio"
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub downloads_dir: Option<PathBuf>,
    #[serde(default = "default_stable_seconds")]
    pub stable_seconds: u64,
    #[serde(default = "default_skip_extensions")]
    pub skip_extensions: Vec<String>,
    #[serde(default = "default_true")]
    pub skip_hidden: bool,
    #[serde(default = "default_fallback")]
    pub fallback_dest: PathBuf,
    #[serde(default, rename = "rule")]
    pub rules: Vec<RuleConfig>,
}

/// One `[[rule]]` table, exactly as written in the file. See
/// [`crate::rules::RuleSet::compile`] for validation.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub name: String,
    pub dest: PathBuf,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub glob: Option<String>,
    pub regex: Option<String>,
    pub older_than: Option<String>,
    pub newer_than: Option<String>,
    pub min_size: Option<String>,
    pub max_size: Option<String>,
    pub source_url: Option<String>,
    #[serde(default)]
    pub source_host: Vec<String>,
}

fn default_stable_seconds() -> u64 {
    5
}
fn default_skip_extensions() -> Vec<String> {
    [
        "crdownload",
        "part",
        "partial",
        "tmp",
        "opdownload",
        "download",
    ]
    .map(String::from)
    .to_vec()
}
fn default_true() -> bool {
    true
}
fn default_fallback() -> PathBuf {
    PathBuf::from("Other")
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Loads the config, first writing the default one if the file is missing.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        ensure_exists(path)?;
        Self::load(path)
    }

    /// The folder being sorted: the configured one, or Windows' Downloads folder.
    pub fn downloads_dir(&self) -> Result<PathBuf> {
        match &self.downloads_dir {
            Some(dir) => Ok(dir.clone()),
            None => crate::paths::default_downloads_dir(),
        }
    }
}

/// Writes [`DEFAULT_CONFIG`] to `path` unless a file is already there.
/// Returns `true` if it created the file.
pub fn ensure_exists(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // `create_new` fails instead of truncating if another process created
    // the file between the `exists` check and here.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(DEFAULT_CONFIG.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_and_has_the_default_categories() {
        let cfg = Config::parse(DEFAULT_CONFIG).unwrap();
        let names: Vec<_> = cfg.rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Installers",
                "Documents",
                "Images",
                "Archives",
                "Video",
                "Audio"
            ]
        );
        assert_eq!(cfg.fallback_dest, PathBuf::from("Other"));
        assert_eq!(cfg.stable_seconds, 5);
        assert!(cfg.skip_hidden);
        assert_eq!(cfg.skip_extensions, default_skip_extensions());
    }

    #[test]
    fn minimal_config_uses_defaults() {
        let cfg = Config::parse("").unwrap();
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.stable_seconds, 5);
        assert_eq!(cfg.fallback_dest, PathBuf::from("Other"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        // A typo such as `extension` (singular) should fail loudly rather
        // than silently producing a rule with no conditions.
        let err = Config::parse("[[rule]]\nname='x'\ndest='x'\nextension=['pdf']").unwrap_err();
        assert!(format!("{err:#}").contains("extension"));
    }

    #[test]
    fn ensure_exists_creates_once_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        assert!(ensure_exists(&path).unwrap());
        std::fs::write(&path, "stable_seconds = 9").unwrap();
        assert!(!ensure_exists(&path).unwrap());
        assert_eq!(Config::load(&path).unwrap().stable_seconds, 9);
    }
}

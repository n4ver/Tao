//! Turns the contents of the sorted folder into a list of planned moves.
//!
//! The planner only *decides*; it never touches files. `sort --dry-run`
//! prints its output, `sort` and `watch` hand it to the mover.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::classify::{Engine, Reason};
use crate::config::Config;
use crate::facts::FileFacts;
use crate::rules::has_extension;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMove {
    pub source: PathBuf,
    /// Absolute destination folder. The final file name (which may get a
    /// collision suffix) is chosen at move time.
    pub dest_dir: PathBuf,
    pub reason: Reason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Subfolders, including the category folders themselves, are left alone.
    NotAFile,
    /// The file itself is a partial download such as `x.zip.crdownload`.
    InProgress,
    /// A partial download for this name exists next to it (Firefox writes an
    /// empty `x.zip` placeholder while it downloads into `x.zip.part`).
    PartialSibling(String),
    HiddenOrSystem,
    /// The rule's destination is the sorted folder itself.
    AlreadyInPlace,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Skip::NotAFile => f.write_str("not a regular file"),
            Skip::InProgress => f.write_str("download in progress"),
            Skip::PartialSibling(name) => write!(f, "download in progress ({name})"),
            Skip::HiddenOrSystem => f.write_str("hidden or system file"),
            Skip::AlreadyInPlace => f.write_str("destination is the same folder"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluation {
    Move(PlannedMove),
    Skip(Skip),
}

#[derive(Debug, Default)]
pub struct Plan {
    pub moves: Vec<PlannedMove>,
    pub skipped: Vec<(PathBuf, Skip)>,
}

pub struct Planner {
    root: PathBuf,
    engine: Engine,
    /// Lower case, without leading dot.
    skip_extensions: Vec<String>,
    skip_hidden: bool,
}

impl Planner {
    pub fn new(root: PathBuf, cfg: &Config, engine: Engine) -> Self {
        Self {
            root,
            engine,
            skip_extensions: cfg
                .skip_extensions
                .iter()
                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                .filter(|e| !e.is_empty())
                .collect(),
            skip_hidden: cfg.skip_hidden,
        }
    }

    pub fn from_config(cfg: &Config) -> Result<Self> {
        let root = cfg.downloads_dir()?;
        Ok(Self::new(root, cfg, Engine::from_config(cfg)?))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Absolute folder for a rule's `dest`.
    pub fn resolve_dest(&self, dest: &Path) -> PathBuf {
        if dest.is_absolute() {
            dest.to_path_buf()
        } else {
            self.root.join(dest)
        }
    }

    /// Decides what to do with one file whose facts are already known.
    pub fn evaluate_facts(&self, facts: &FileFacts, now: SystemTime) -> Evaluation {
        if facts.is_dir {
            return Evaluation::Skip(Skip::NotAFile);
        }
        let lower = facts.lower_name();
        if self
            .skip_extensions
            .iter()
            .any(|e| has_extension(&lower, e))
        {
            return Evaluation::Skip(Skip::InProgress);
        }
        if self.skip_hidden && facts.hidden_or_system {
            return Evaluation::Skip(Skip::HiddenOrSystem);
        }
        if let Some(parent) = facts.path.parent() {
            for ext in &self.skip_extensions {
                let sibling = format!("{}.{ext}", facts.file_name);
                // `exists` is case-insensitive on Windows (NTFS lookups are),
                // so this also finds `X.ZIP.PART`.
                if parent.join(&sibling).exists() {
                    return Evaluation::Skip(Skip::PartialSibling(sibling));
                }
            }
        }

        let decision = self.engine.decide(facts, now);
        let dest_dir = self.resolve_dest(&decision.dest);
        if facts.path.parent() == Some(dest_dir.as_path()) {
            return Evaluation::Skip(Skip::AlreadyInPlace);
        }
        Evaluation::Move(PlannedMove {
            source: facts.path.clone(),
            dest_dir,
            reason: decision.reason,
        })
    }

    /// Reads a file's facts and evaluates it.
    pub fn evaluate(&self, path: &Path, now: SystemTime) -> std::io::Result<Evaluation> {
        Ok(self.evaluate_facts(&FileFacts::read(path)?, now))
    }

    /// Plans moves for every entry directly inside the sorted folder.
    /// Subfolders are never descended into.
    pub fn scan(&self, now: SystemTime) -> Result<Plan> {
        let entries = std::fs::read_dir(&self.root)
            .with_context(|| format!("reading {}", self.root.display()))?;
        let mut plan = Plan::default();
        for entry in entries {
            let path = match entry {
                Ok(e) => e.path(),
                Err(e) => {
                    tracing::warn!("skipping unreadable directory entry: {e}");
                    continue;
                }
            };
            match self.evaluate(&path, now) {
                Ok(Evaluation::Move(m)) => plan.moves.push(m),
                Ok(Evaluation::Skip(s)) => plan.skipped.push((path, s)),
                // The file vanished or is inaccessible; nothing to plan.
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        plan.moves.sort_by(|a, b| a.source.cmp(&b.source));
        plan.skipped.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG;
    use std::fs;

    fn planner(root: &Path, toml: &str) -> Planner {
        let cfg = Config::parse(toml).unwrap();
        Planner::new(root.to_path_buf(), &cfg, Engine::from_config(&cfg).unwrap())
    }

    fn names(plan: &Plan) -> Vec<(String, String)> {
        plan.moves
            .iter()
            .map(|m| {
                (
                    m.source.file_name().unwrap().to_string_lossy().into_owned(),
                    m.dest_dir
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn scan_plans_top_level_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["a.pdf", "b.png", "c.xyz", "d.zip.crdownload", "e.tmp"] {
            fs::write(root.join(name), b"x").unwrap();
        }
        fs::create_dir(root.join("Images")).unwrap();
        fs::write(root.join("Images").join("nested.png"), b"x").unwrap();

        let plan = planner(root, DEFAULT_CONFIG)
            .scan(SystemTime::now())
            .unwrap();
        assert_eq!(
            names(&plan),
            [
                ("a.pdf".into(), "Documents".into()),
                ("b.png".into(), "Images".into()),
                ("c.xyz".into(), "Other".into()),
            ]
        );
        let skipped: Vec<_> = plan
            .skipped
            .iter()
            .map(|(p, s)| {
                (
                    p.file_name().unwrap().to_string_lossy().into_owned(),
                    s.clone(),
                )
            })
            .collect();
        assert_eq!(
            skipped,
            [
                ("Images".into(), Skip::NotAFile),
                ("d.zip.crdownload".into(), Skip::InProgress),
                ("e.tmp".into(), Skip::InProgress),
            ]
        );
        // Planning must not have moved anything.
        assert!(root.join("a.pdf").exists());
    }

    #[test]
    fn firefox_placeholder_is_skipped_while_part_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("big.zip"), b"").unwrap();
        fs::write(root.join("big.zip.part"), b"partial").unwrap();
        let p = planner(root, DEFAULT_CONFIG);
        let now = SystemTime::now();
        assert_eq!(
            p.evaluate(&root.join("big.zip"), now).unwrap(),
            Evaluation::Skip(Skip::PartialSibling("big.zip.part".into()))
        );
        fs::remove_file(root.join("big.zip.part")).unwrap();
        assert!(matches!(
            p.evaluate(&root.join("big.zip"), now).unwrap(),
            Evaluation::Move(_)
        ));
    }

    #[test]
    fn absolute_and_same_folder_destinations() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let root = dir.path();
        let toml = format!(
            "[[rule]]\nname='Keep'\nextensions=['keep']\ndest='.'\n\
             [[rule]]\nname='Away'\nextensions=['away']\ndest='{}'",
            elsewhere.path().display()
        );
        fs::write(root.join("x.keep"), b"x").unwrap();
        fs::write(root.join("x.away"), b"x").unwrap();
        let p = planner(root, &toml);
        let now = SystemTime::now();
        assert_eq!(
            p.evaluate(&root.join("x.keep"), now).unwrap(),
            Evaluation::Skip(Skip::AlreadyInPlace)
        );
        match p.evaluate(&root.join("x.away"), now).unwrap() {
            Evaluation::Move(m) => assert_eq!(m.dest_dir, elsewhere.path()),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn hidden_files_are_skipped() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.ini");
        // FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM, as Windows creates desktop.ini.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .attributes(0x2 | 0x4)
            .open(&path)
            .unwrap();
        let p = planner(dir.path(), DEFAULT_CONFIG);
        assert_eq!(
            p.evaluate(&path, SystemTime::now()).unwrap(),
            Evaluation::Skip(Skip::HiddenOrSystem)
        );
    }
}

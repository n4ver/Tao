//! The rule engine: compiles `[[rule]]` tables and finds the first match.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};

use crate::config::RuleConfig;
use crate::facts::FileFacts;

/// One test a file must pass. A rule matches when all its conditions do.
///
/// To match on new facts, add a variant here and a field or method on
/// [`FileFacts`]. The planned next one is
/// `SourceUrl(Regex)`, matching the `HostUrl` in the Zone.Identifier stream.
#[derive(Debug)]
enum Condition {
    /// Lower-case suffixes without the leading dot, e.g. `pdf` or `tar.gz`.
    Extensions(Vec<String>),
    Glob(GlobMatcher),
    Regex(Regex),
    OlderThan(Duration),
    NewerThan(Duration),
}

impl Condition {
    fn matches(&self, facts: &FileFacts, now: SystemTime) -> bool {
        match self {
            Condition::Extensions(exts) => {
                let name = facts.lower_name();
                exts.iter().any(|ext| has_extension(&name, ext))
            }
            Condition::Glob(glob) => glob.is_match(&facts.file_name),
            Condition::Regex(re) => re.is_match(&facts.file_name),
            Condition::OlderThan(d) => facts.age(now) > *d,
            Condition::NewerThan(d) => facts.age(now) < *d,
        }
    }
}

#[derive(Debug)]
pub struct Rule {
    pub name: String,
    /// As written in the config. Relative destinations are resolved against
    /// the sorted folder by the planner.
    pub dest: PathBuf,
    conditions: Vec<Condition>,
}

impl Rule {
    pub fn matches(&self, facts: &FileFacts, now: SystemTime) -> bool {
        self.conditions.iter().all(|c| c.matches(facts, now))
    }

    fn compile(cfg: &RuleConfig) -> Result<Self> {
        let mut conditions = Vec::new();

        if !cfg.extensions.is_empty() {
            let exts = cfg
                .extensions
                .iter()
                .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                .filter(|e| !e.is_empty())
                .collect();
            conditions.push(Condition::Extensions(exts));
        }
        if let Some(pattern) = &cfg.glob {
            let glob = GlobBuilder::new(pattern)
                .case_insensitive(true)
                // We match bare file names, so `*` crossing `\` or `/` is moot,
                // but keep the usual meaning.
                .literal_separator(true)
                .build()
                .with_context(|| format!("invalid glob {pattern:?}"))?;
            conditions.push(Condition::Glob(glob.compile_matcher()));
        }
        if let Some(pattern) = &cfg.regex {
            let re = RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("invalid regex {pattern:?}"))?;
            conditions.push(Condition::Regex(re));
        }
        if let Some(text) = &cfg.older_than {
            conditions.push(Condition::OlderThan(parse_duration(text)?));
        }
        if let Some(text) = &cfg.newer_than {
            conditions.push(Condition::NewerThan(parse_duration(text)?));
        }

        if conditions.is_empty() {
            // A rule with no conditions would match every file and make every
            // rule after it dead. `fallback_dest` is the way to say "everything else".
            bail!("rule has no conditions (use extensions, glob, regex, older_than or newer_than)");
        }
        if cfg.dest.as_os_str().is_empty() {
            bail!("rule has an empty `dest`");
        }

        Ok(Self {
            name: cfg.name.clone(),
            dest: cfg.dest.clone(),
            conditions,
        })
    }
}

/// Rules in config order. The first match wins.
#[derive(Debug)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    pub fn compile(configs: &[RuleConfig]) -> Result<Self> {
        let rules = configs
            .iter()
            .enumerate()
            .map(|(i, cfg)| {
                Rule::compile(cfg).with_context(|| format!("rule #{} ({:?})", i + 1, cfg.name))
            })
            .collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    pub fn first_match(&self, facts: &FileFacts, now: SystemTime) -> Option<&Rule> {
        self.rules.iter().find(|r| r.matches(facts, now))
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

/// True if `lower_name` ends in `.<ext>` and has something before the dot.
/// Both arguments must already be lower case.
///
/// This is a suffix comparison rather than `Path::extension` so that
/// multi-part extensions like `tar.gz` work. Requiring a non-empty stem means
/// a file named `pdf` or `.pdf` does not count as a PDF.
pub fn has_extension(lower_name: &str, ext: &str) -> bool {
    lower_name.len() > ext.len() + 1
        && lower_name.ends_with(ext)
        // The byte before a matched suffix ends a whole character, and UTF-8
        // continuation bytes are never 0x2E, so this byte check is safe.
        && lower_name.as_bytes()[lower_name.len() - ext.len() - 1] == b'.'
}

/// Parses durations like `45s`, `90m`, `12h`, `30d`, `2w`.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .with_context(|| format!("duration {text:?} needs a unit: s, m, h, d or w"))?;
    let (num, unit) = text.split_at(split);
    let n: u64 = num
        .parse()
        .with_context(|| format!("duration {text:?} must start with a whole number"))?;
    let secs_per_unit = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        other => bail!("unknown duration unit {other:?} in {text:?} (use s, m, h, d or w)"),
    };
    let secs = n
        .checked_mul(secs_per_unit)
        .with_context(|| format!("duration {text:?} is too large"))?;
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DEFAULT_CONFIG};

    const DAY: Duration = Duration::from_secs(86_400);

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn file(name: &str) -> FileFacts {
        FileFacts::fake(name, now())
    }

    fn aged(name: &str, age: Duration) -> FileFacts {
        FileFacts::fake(name, now() - age)
    }

    fn rule(name: &str, f: impl FnOnce(&mut RuleConfig)) -> RuleConfig {
        let mut cfg = RuleConfig {
            name: name.into(),
            dest: name.into(),
            ..Default::default()
        };
        f(&mut cfg);
        cfg
    }

    fn matched<'a>(rules: &'a RuleSet, facts: &FileFacts) -> Option<&'a str> {
        rules.first_match(facts, now()).map(|r| r.name.as_str())
    }

    #[test]
    fn default_rules_cover_the_categories() {
        let cfg = Config::parse(DEFAULT_CONFIG).unwrap();
        let rules = RuleSet::compile(&cfg.rules).unwrap();
        let cases = [
            ("setup.exe", Some("Installers")),
            ("Tool.MSI", Some("Installers")),
            ("report.pdf", Some("Documents")),
            ("photo.JPEG", Some("Images")),
            ("src.tar.gz", Some("Archives")),
            ("movie.mkv", Some("Video")),
            ("song.flac", Some("Audio")),
            ("mystery.xyz", None),
            ("no_extension", None),
        ];
        for (name, want) in cases {
            assert_eq!(matched(&rules, &file(name)), want, "{name}");
        }
    }

    #[test]
    fn extension_matching_is_case_insensitive_and_dot_tolerant() {
        let rules = RuleSet::compile(&[rule("Docs", |r| {
            r.extensions = vec![".PDF".into(), "tar.gz".into()]
        })])
        .unwrap();
        assert_eq!(matched(&rules, &file("A.pdf")), Some("Docs"));
        assert_eq!(matched(&rules, &file("a.Tar.Gz")), Some("Docs"));
        // Must be a real suffix after a dot, not just the same letters.
        assert_eq!(matched(&rules, &file("pdf")), None);
        assert_eq!(matched(&rules, &file("notapdf")), None);
        assert_eq!(matched(&rules, &file(".pdf")), None);
        assert_eq!(matched(&rules, &file("x.gz")), None);
    }

    #[test]
    fn first_match_wins() {
        let rules = RuleSet::compile(&[
            rule("Screenshots", |r| r.glob = Some("screenshot*".into())),
            rule("Images", |r| r.extensions = vec!["png".into()]),
        ])
        .unwrap();
        assert_eq!(
            matched(&rules, &file("Screenshot 2026-01-01.png")),
            Some("Screenshots")
        );
        assert_eq!(matched(&rules, &file("cat.png")), Some("Images"));
    }

    #[test]
    fn all_conditions_in_a_rule_must_match() {
        let rules = RuleSet::compile(&[rule("OldInvoices", |r| {
            r.regex = Some(r"^invoice[-_ ]?\d+".into());
            r.extensions = vec!["pdf".into()];
            r.older_than = Some("30d".into());
        })])
        .unwrap();
        assert_eq!(
            matched(&rules, &aged("Invoice-42.pdf", 31 * DAY)),
            Some("OldInvoices")
        );
        assert_eq!(matched(&rules, &aged("Invoice-42.pdf", 29 * DAY)), None);
        assert_eq!(matched(&rules, &aged("Invoice-42.docx", 31 * DAY)), None);
        assert_eq!(matched(&rules, &aged("receipt-42.pdf", 31 * DAY)), None);
    }

    #[test]
    fn glob_and_regex_are_case_insensitive() {
        let rules = RuleSet::compile(&[
            rule("G", |r| r.glob = Some("IMG_*.jpg".into())),
            rule("R", |r| r.regex = Some("^scan".into())),
        ])
        .unwrap();
        assert_eq!(matched(&rules, &file("img_0001.JPG")), Some("G"));
        assert_eq!(matched(&rules, &file("SCAN0001.tif")), Some("R"));
    }

    #[test]
    fn age_conditions() {
        let rules = RuleSet::compile(&[
            rule("Fresh", |r| r.newer_than = Some("1h".into())),
            rule("Stale", |r| r.older_than = Some("1w".into())),
        ])
        .unwrap();
        assert_eq!(
            matched(&rules, &aged("a", Duration::from_secs(60))),
            Some("Fresh")
        );
        assert_eq!(matched(&rules, &aged("a", 8 * DAY)), Some("Stale"));
        assert_eq!(matched(&rules, &aged("a", 2 * DAY)), None);
        // Modified "in the future" counts as brand new, not as an error.
        let future = FileFacts::fake("a", now() + DAY);
        assert_eq!(matched(&rules, &future), Some("Fresh"));
    }

    #[test]
    fn invalid_rules_are_rejected_with_context() {
        let err = RuleSet::compile(&[rule("Empty", |_| {})]).unwrap_err();
        assert!(format!("{err:#}").contains("no conditions"));

        let err = RuleSet::compile(&[rule("BadRe", |r| r.regex = Some("(".into()))]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("rule #1") && msg.contains("BadRe"), "{msg}");

        let err = RuleSet::compile(&[rule("BadGlob", |r| r.glob = Some("[".into()))]);
        assert!(err.is_err());

        let err = RuleSet::compile(&[rule("BadAge", |r| r.older_than = Some("3 years".into()))]);
        assert!(err.is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("90m").unwrap(), Duration::from_secs(5400));
        assert_eq!(
            parse_duration(" 12h ").unwrap(),
            Duration::from_secs(43_200)
        );
        assert_eq!(parse_duration("30d").unwrap(), 30 * DAY);
        assert_eq!(parse_duration("2w").unwrap(), 14 * DAY);
        assert!(parse_duration("30").is_err());
        assert!(parse_duration("d").is_err());
        assert!(parse_duration("1y").is_err());
        assert!(parse_duration("99999999999999999999w").is_err());
    }
}

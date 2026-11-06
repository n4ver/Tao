//! The rule engine: compiles `[[rule]]` tables and finds the first match.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use regex::{Regex, RegexBuilder};

use crate::config::RuleConfig;
use crate::facts::FileFacts;
use crate::zone::{self, ZoneInfo};

/// One test a file must pass. A rule matches when all its conditions do.
///
/// To match on new facts, add a variant here and a field or method on
/// [`FileFacts`]. Expensive facts (like the Zone.Identifier stream) should
/// be lazy on `FileFacts`, and their conditions compiled last so that
/// cheaper ones can rule a file out first.
#[derive(Debug)]
enum Condition {
    /// Lower-case suffixes without the leading dot, e.g. `pdf` or `tar.gz`.
    Extensions(Vec<String>),
    Glob(GlobMatcher),
    Regex(Regex),
    OlderThan(Duration),
    NewerThan(Duration),
    /// Inclusive, in bytes.
    MinSize(u64),
    MaxSize(u64),
    /// Matched against HostUrl and ReferrerUrl.
    SourceUrl(Regex),
    /// Normalised host names; see [`zone::host_matches`].
    SourceHost(Vec<String>),
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
            Condition::MinSize(n) => facts.size >= *n,
            Condition::MaxSize(n) => facts.size <= *n,
            Condition::SourceUrl(_) | Condition::SourceHost(_) => {
                self.matching_url(facts).is_some()
            }
        }
    }

    /// For source conditions, the first recorded URL that satisfies them.
    /// Always `None` (without reading the stream) for other conditions.
    fn matching_url<'f>(&self, facts: &'f FileFacts) -> Option<&'f str> {
        // A closure so that the stream is only read for source conditions.
        let urls = || facts.zone().into_iter().flat_map(ZoneInfo::urls);
        match self {
            Condition::SourceUrl(re) => urls().find(|url| re.is_match(url)),
            Condition::SourceHost(hosts) => urls().find(|url| {
                zone::host_of(url).is_some_and(|h| hosts.iter().any(|p| zone::host_matches(&h, p)))
            }),
            _ => None,
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

    /// The download URL that a `source_url`/`source_host` condition of this
    /// rule matched, for showing in dry-run output and the move log. `None`
    /// if the rule has no source conditions.
    pub fn matched_source(&self, facts: &FileFacts) -> Option<String> {
        self.conditions
            .iter()
            .find_map(|c| c.matching_url(facts))
            .map(str::to_string)
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
        conditions.extend(compile_sizes(cfg)?);
        // Source conditions last: they are the only ones that read from disk
        // (the Zone.Identifier stream), and `all()` stops at the first
        // condition that fails.
        conditions.extend(compile_sources(cfg)?);

        if conditions.is_empty() {
            // A rule with no conditions would match every file and make every
            // rule after it dead. `fallback_dest` is the way to say "everything else".
            bail!(
                "rule has no conditions (use extensions, glob, regex, older_than, newer_than, \
                 min_size, max_size, source_url or source_host)"
            );
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

fn compile_sizes(cfg: &RuleConfig) -> Result<Vec<Condition>> {
    let min = cfg.min_size.as_deref().map(parse_size).transpose()?;
    let max = cfg.max_size.as_deref().map(parse_size).transpose()?;
    if let (Some(min), Some(max)) = (min, max)
        && min > max
    {
        bail!("min_size ({min} bytes) is larger than max_size ({max} bytes), so nothing can match");
    }
    Ok(min
        .map(Condition::MinSize)
        .into_iter()
        .chain(max.map(Condition::MaxSize))
        .collect())
}

fn compile_sources(cfg: &RuleConfig) -> Result<Vec<Condition>> {
    let mut conditions = Vec::new();
    if let Some(pattern) = &cfg.source_url {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .with_context(|| format!("invalid source_url regex {pattern:?}"))?;
        conditions.push(Condition::SourceUrl(re));
    }
    if !cfg.source_host.is_empty() {
        let hosts = cfg
            .source_host
            .iter()
            .map(|h| zone::normalize_host_pattern(h))
            .collect::<Result<_>>()?;
        conditions.push(Condition::SourceHost(hosts));
    }
    Ok(conditions)
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

/// Parses sizes like `20KB`, `500MB`, `1.5GB`, `700 B`.
///
/// Units are binary (1 KB = 1024 bytes, 1 MB = 1024 KB, ...), because that
/// is what Explorer's Size column shows under the same "KB"/"MB" labels, and
/// that is where people will compare against. `KiB`, `MiB`, ... are accepted
/// as the same thing. Units are case-insensitive and required. Fractions are
/// computed in integer arithmetic (no float rounding surprises) and rounded
/// down to whole bytes.
pub fn parse_size(text: &str) -> Result<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .with_context(|| format!("size {text:?} needs a unit: B, KB, MB, GB or TB"))?;
    let (num, unit) = text.split_at(split);
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "b" => 0,
        "kb" | "kib" => 10,
        "mb" | "mib" => 20,
        "gb" | "gib" => 30,
        "tb" | "tib" => 40,
        other => bail!("unknown size unit {other:?} in {text:?} (use B, KB, MB, GB or TB)"),
    };
    let (whole, frac) = num.split_once('.').unwrap_or((num, ""));
    let digits_only = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    // `.5GB` and `5.GB` are rejected as typos, as is a second dot.
    if whole.is_empty() || (num.contains('.') && frac.is_empty()) || !digits_only(frac) {
        bail!("size {text:?} must be a number like 500 or 1.5, followed by a unit");
    }
    let too_large = || format!("size {text:?} is too large");
    let whole: u64 = whole.parse().with_context(too_large)?;
    let mut bytes = whole.checked_mul(1 << shift).with_context(too_large)?;
    if !frac.is_empty() {
        // 18 digits fit in a u64; shifted by up to 40 bits they fit in a u128.
        let frac = &frac[..frac.len().min(18)];
        let numerator = u128::from(frac.parse::<u64>().expect("digits checked above")) << shift;
        let part = numerator / 10u128.pow(frac.len() as u32);
        bytes = bytes.checked_add(part as u64).with_context(too_large)?;
    }
    Ok(bytes)
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

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    fn sized(name: &str, size: u64) -> FileFacts {
        let mut f = file(name);
        f.size = size;
        f
    }

    fn from(name: &str, host_url: Option<&str>, referrer: Option<&str>) -> FileFacts {
        file(name).with_zone(Some(ZoneInfo {
            zone_id: Some(3),
            host_url: host_url.map(String::from),
            referrer_url: referrer.map(String::from),
        }))
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("0B").unwrap(), 0);
        assert_eq!(parse_size("700 B").unwrap(), 700);
        assert_eq!(parse_size("20KB").unwrap(), 20 * KB);
        assert_eq!(parse_size("20kib").unwrap(), 20 * KB);
        assert_eq!(parse_size(" 500MB ").unwrap(), 500 * MB);
        assert_eq!(parse_size("500mb").unwrap(), 500 * MB);
        assert_eq!(parse_size("1.5GB").unwrap(), GB + GB / 2);
        assert_eq!(parse_size("1.5 GiB").unwrap(), GB + GB / 2);
        assert_eq!(parse_size("2TB").unwrap(), 2 * 1024 * GB);
        // Rounded down to whole bytes: 0.001 KB = 1.024 bytes.
        assert_eq!(parse_size("0.001KB").unwrap(), 1);
        assert_eq!(parse_size("0.1B").unwrap(), 0);
        for bad in [
            "",
            "500",
            "MB",
            "1.5",
            "-1MB",
            ".5GB",
            "5.GB",
            "1.2.3MB",
            "1,5GB",
            "5 megabytes",
            "5PB",
            "5 M B",
            "99999999999999999999B",
            "16777216TB",
        ] {
            assert!(parse_size(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn size_bounds_are_inclusive() {
        let rules = RuleSet::compile(&[
            rule("Big", |r| r.min_size = Some("1MB".into())),
            rule("Small", |r| r.max_size = Some("20KB".into())),
        ])
        .unwrap();
        assert_eq!(matched(&rules, &sized("a", MB)), Some("Big"));
        assert_eq!(matched(&rules, &sized("a", MB - 1)), None);
        assert_eq!(matched(&rules, &sized("a", 20 * KB)), Some("Small"));
        assert_eq!(matched(&rules, &sized("a", 20 * KB + 1)), None);
        assert_eq!(matched(&rules, &sized("a", 0)), Some("Small"));
    }

    #[test]
    fn size_range_in_one_rule() {
        let rules = RuleSet::compile(&[rule("Exactly1K", |r| {
            r.min_size = Some("1KB".into());
            r.max_size = Some("1KB".into());
        })])
        .unwrap();
        assert_eq!(matched(&rules, &sized("a", KB)), Some("Exactly1K"));
        assert_eq!(matched(&rules, &sized("a", KB - 1)), None);
        assert_eq!(matched(&rules, &sized("a", KB + 1)), None);
    }

    #[test]
    fn invalid_sizes_name_the_rule() {
        let err = RuleSet::compile(&[rule("Videos", |r| r.min_size = Some("500 megs".into()))])
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("rule #1") && msg.contains("Videos") && msg.contains("500 megs"),
            "{msg}"
        );

        let err = RuleSet::compile(&[rule("Backwards", |r| {
            r.min_size = Some("2MB".into());
            r.max_size = Some("1MB".into());
        })])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Backwards") && msg.contains("larger than max_size"),
            "{msg}"
        );
    }

    #[test]
    fn source_host_matches_either_url_by_suffix() {
        let rules = RuleSet::compile(&[rule("GitHub", |r| {
            r.source_host = vec!["GitHub.com".into()]
        })])
        .unwrap();
        // GitHub release assets: HostUrl on another domain, referrer on github.com.
        let release = from(
            "tool.zip",
            Some("https://objects.githubusercontent.com/asset"),
            Some("https://github.com/o/r/releases"),
        );
        assert_eq!(matched(&rules, &release), Some("GitHub"));
        let rule = rules.first_match(&release, now()).unwrap();
        assert_eq!(
            rule.matched_source(&release).as_deref(),
            Some("https://github.com/o/r/releases")
        );
        let sub = from("a.zip", Some("https://codeload.github.com/o/r/zip"), None);
        assert_eq!(matched(&rules, &sub), Some("GitHub"));
        let lookalike = from("a.zip", Some("https://notgithub.com/x"), None);
        assert_eq!(matched(&rules, &lookalike), None);
        // No stream, or a stream without URLs, never matches a source condition.
        assert_eq!(matched(&rules, &file("a.zip").with_zone(None)), None);
        assert_eq!(matched(&rules, &from("a.zip", None, None)), None);
    }

    #[test]
    fn source_url_regex_is_case_insensitive() {
        let rules = RuleSet::compile(&[rule("Drive", |r| {
            r.source_url = Some(r"drive\.google\.com/.*export".into())
        })])
        .unwrap();
        let url = "https://DRIVE.GOOGLE.COM/u/0/EXPORT?id=1";
        let f = from("doc.pdf", Some(url), None);
        assert_eq!(matched(&rules, &f), Some("Drive"));
        let rule = rules.first_match(&f, now()).unwrap();
        assert_eq!(rule.matched_source(&f).as_deref(), Some(url));
        let other = from("doc.pdf", Some("https://x.com/"), None);
        assert_eq!(matched(&rules, &other), None);
    }

    #[test]
    fn invalid_source_conditions_name_the_rule() {
        let err = RuleSet::compile(&[rule("BadHost", |r| {
            r.source_host = vec!["https://github.com/".into()]
        })])
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("BadHost") && msg.contains("host name"),
            "{msg}"
        );
        let err =
            RuleSet::compile(&[rule("BadUrl", |r| r.source_url = Some("(".into()))]).unwrap_err();
        assert!(format!("{err:#}").contains("BadUrl"));
    }

    /// Rules without source conditions, and rules whose cheaper conditions
    /// already failed, must not read the Zone.Identifier stream.
    #[test]
    fn stream_is_only_read_when_a_source_condition_is_evaluated() {
        let plain = RuleSet::compile(&[
            rule("Docs", |r| r.extensions = vec!["pdf".into()]),
            rule("Big", |r| r.min_size = Some("1MB".into())),
        ])
        .unwrap();
        let f = file("x.zip");
        assert_eq!(matched(&plain, &f), None);
        assert!(!f.zone_was_read());
        // matched_source on a rule without source conditions doesn't read it either.
        let f = file("x.pdf");
        let rule = plain.first_match(&f, now()).unwrap();
        assert_eq!(rule.matched_source(&f), None);
        assert!(!f.zone_was_read());

        let sourced = RuleSet::compile(&[rule_cfg_github_pdfs()]).unwrap();
        let f = file("x.zip");
        assert_eq!(matched(&sourced, &f), None);
        assert!(!f.zone_was_read(), "extension failed first, so no read");
        let f = file("x.pdf");
        assert_eq!(matched(&sourced, &f), None);
        assert!(f.zone_was_read());
    }

    fn rule_cfg_github_pdfs() -> RuleConfig {
        rule("GitHubPdfs", |r| {
            // Written in the config with the source condition first, to show
            // that compile order (not config order) decides evaluation order.
            r.source_host = vec!["github.com".into()];
            r.extensions = vec!["pdf".into()];
        })
    }

    #[test]
    fn commented_example_rule_in_default_config_is_valid() {
        let example: String = DEFAULT_CONFIG
            .lines()
            .skip_while(|l| !l.starts_with("# [[rule]]"))
            .take_while(|l| l.starts_with("# "))
            .map(|l| format!("{}\n", &l[2..]))
            .collect();
        assert!(example.contains("source_host"), "{example}");
        let cfg = Config::parse(&example).unwrap();
        let rules = RuleSet::compile(&cfg.rules).unwrap();
        let f = from(
            "tool.zip",
            Some("https://objects.githubusercontent.com/a"),
            None,
        );
        assert_eq!(matched(&rules, &f), Some("GitHub downloads"));
    }
}

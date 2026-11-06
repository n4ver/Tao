//! The `Zone.Identifier` alternate data stream: where a download came from.
//!
//! When a browser saves a file on NTFS it also writes a small INI-style text
//! into the file's `Zone.Identifier` stream (`file.zip:Zone.Identifier`).
//! That stream is what makes Windows say "this file came from another
//! computer". Typical contents:
//!
//! ```text
//! [ZoneTransfer]
//! ZoneId=3
//! ReferrerUrl=https://github.com/owner/repo/releases
//! HostUrl=https://objects.githubusercontent.com/github-production-release-asset/...
//! ```
//!
//! `HostUrl` is where the bytes were fetched from, `ReferrerUrl` the page that
//! linked to it. Not every browser writes both, and some write placeholders
//! such as `about:internet` (e.g. for private-mode downloads), so every field
//! is optional. This module only parses; reading the stream from disk is in
//! [`crate::facts`].

/// What the stream says about a download's origin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneInfo {
    /// 3 = Internet, 2 = trusted sites, 1 = intranet, 0 = this computer.
    pub zone_id: Option<u32>,
    pub host_url: Option<String>,
    pub referrer_url: Option<String>,
}

impl ZoneInfo {
    /// The recorded URLs, `HostUrl` first.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.host_url
            .iter()
            .chain(self.referrer_url.iter())
            .map(String::as_str)
    }
}

/// Parses the raw bytes of a `Zone.Identifier` stream. Returns `None` if
/// there is no `[ZoneTransfer]` section (empty or unrelated content).
/// Lines it doesn't understand are ignored rather than rejected.
pub fn parse(bytes: &[u8]) -> Option<ZoneInfo> {
    let text = decode(bytes);
    let mut info = ZoneInfo::default();
    let mut in_section = false;
    let mut found_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line.eq_ignore_ascii_case("[ZoneTransfer]");
            found_section |= in_section;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        // Windows INI keys are case-insensitive.
        match key.trim().to_ascii_lowercase().as_str() {
            "zoneid" => info.zone_id = value.trim().parse().ok(),
            "hosturl" => info.host_url = url_value(value),
            "referrerurl" => info.referrer_url = url_value(value),
            _ => {}
        }
    }
    found_section.then_some(info)
}

/// Browsers write the stream as ANSI/UTF-8, but tools such as PowerShell's
/// `Set-Content` may write UTF-16 with a byte-order mark.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], from: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| from([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// An empty value or an `about:` placeholder is not a real source.
fn url_value(value: &str) -> Option<String> {
    let value = value.trim();
    let placeholder = value.is_empty()
        || value
            .get(..6)
            .is_some_and(|p| p.eq_ignore_ascii_case("about:"));
    (!placeholder).then(|| value.to_string())
}

/// The lower-case host name of a URL, without port, credentials or a
/// trailing dot. `blob:https://example.com/<uuid>` gives `example.com`.
/// Returns `None` for URLs without a `scheme://host` part (e.g. `data:`).
pub fn host_of(url: &str) -> Option<String> {
    let url = url.trim();
    let url = match url.get(..5) {
        Some(p) if p.eq_ignore_ascii_case("blob:") => &url[5..],
        _ => url,
    };
    let (_scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#', '\\']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match host_port.strip_prefix('[') {
        // IPv6 literal: `[::1]:8080`.
        Some(v6) => v6.split_once(']')?.0,
        None => host_port.split(':').next()?,
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// True if `host` is `pattern` or a subdomain of it: `github.com` matches
/// `github.com` and `objects.github.com`, but not `notgithub.com`. Both
/// must already be lower case (see [`host_of`], [`normalize_host_pattern`]).
pub fn host_matches(host: &str, pattern: &str) -> bool {
    host == pattern
        || (host.len() > pattern.len()
            && host.ends_with(pattern)
            // `pattern` is ASCII-only (validated), so this byte is on a
            // character boundary.
            && host.as_bytes()[host.len() - pattern.len() - 1] == b'.')
}

/// Validates a `source_host` entry from the config and brings it into the
/// form [`host_matches`] expects. `*.example.com` and `.example.com` are
/// accepted as `example.com`, since subdomains always match anyway.
pub fn normalize_host_pattern(text: &str) -> anyhow::Result<String> {
    let host = text.trim().to_ascii_lowercase();
    let host = host.strip_prefix("*.").unwrap_or(&host);
    let host = host.trim_start_matches('.').trim_end_matches('.');
    let valid = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !valid {
        anyhow::bail!(
            "source_host {text:?} must be a host name like \"github.com\" \
             (no scheme, path or port)"
        );
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Edge and Chrome (Chromium) write ReferrerUrl then HostUrl, CRLF.
    const CHROMIUM: &str = "[ZoneTransfer]\r\nZoneId=3\r\n\
        ReferrerUrl=https://github.com/n4ver/Tao/releases\r\n\
        HostUrl=https://objects.githubusercontent.com/github-production-release-asset-2e65be/1?X-Amz-Algorithm=AWS4\r\n";

    #[test]
    fn chromium_sample() {
        let z = parse(CHROMIUM.as_bytes()).unwrap();
        assert_eq!(z.zone_id, Some(3));
        assert_eq!(
            z.referrer_url.as_deref(),
            Some("https://github.com/n4ver/Tao/releases")
        );
        assert!(
            z.host_url
                .as_deref()
                .unwrap()
                .starts_with("https://objects.githubusercontent.com/")
        );
        // HostUrl is offered first.
        assert!(z.urls().next().unwrap().contains("objects."));
    }

    #[test]
    fn firefox_samples() {
        // Older Firefox: zone only, no URLs.
        let old = parse(b"[ZoneTransfer]\r\nZoneId=3\r\n").unwrap();
        assert_eq!(
            old,
            ZoneInfo {
                zone_id: Some(3),
                ..Default::default()
            }
        );
        assert_eq!(old.urls().count(), 0);
        // Newer Firefox: HostUrl only (and sometimes ReferrerUrl).
        let new = parse(b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://ftp.mozilla.org/x.zip\r\n")
            .unwrap();
        assert_eq!(
            new.host_url.as_deref(),
            Some("https://ftp.mozilla.org/x.zip")
        );
        assert_eq!(new.referrer_url, None);
    }

    #[test]
    fn private_mode_placeholder_is_not_a_url() {
        let z = parse(b"[ZoneTransfer]\nZoneId=3\nHostUrl=about:internet\n").unwrap();
        assert_eq!(z.host_url, None);
    }

    #[test]
    fn empty_missing_and_malformed() {
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"garbage without a section"), None);
        assert_eq!(parse(b"[Other]\nHostUrl=https://a.com/\n"), None);
        // Junk lines, odd spacing and case, keys outside the section.
        let z = parse(
            b"HostUrl=https://outside.com/\n\
              [zonetransfer]\n\
              this line has no equals sign\n\
              =no key\n\
              ZoneId = not-a-number\n\
              hosturl =  https://inside.com/f.zip  \n\
              ReferrerUrl=\n\
              [Next]\nHostUrl=https://after.com/\n",
        )
        .unwrap();
        assert_eq!(z.zone_id, None);
        assert_eq!(z.host_url.as_deref(), Some("https://inside.com/f.zip"));
        assert_eq!(z.referrer_url, None);
        // Invalid UTF-8 is tolerated.
        assert!(parse(b"[ZoneTransfer]\nHostUrl=https://a.com/\xff\xfe\n").is_some());
    }

    #[test]
    fn byte_order_marks() {
        let text = "[ZoneTransfer]\r\nHostUrl=https://example.com/a\r\n";
        let mut utf16le = vec![0xFF, 0xFE];
        utf16le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend(text.as_bytes());
        for bytes in [utf16le, utf8] {
            let z = parse(&bytes).unwrap();
            assert_eq!(z.host_url.as_deref(), Some("https://example.com/a"));
        }
    }

    #[test]
    fn hosts() {
        let h = |u| host_of(u);
        assert_eq!(h("https://GitHub.com/a/b").as_deref(), Some("github.com"));
        assert_eq!(
            h("https://user:pw@dl.example.org:8443/x?y#z").as_deref(),
            Some("dl.example.org")
        );
        assert_eq!(h("http://example.com.").as_deref(), Some("example.com"));
        assert_eq!(h("https://[::1]:8080/x").as_deref(), Some("::1"));
        assert_eq!(
            h("blob:https://web.whatsapp.com/1234-abcd").as_deref(),
            Some("web.whatsapp.com")
        );
        assert_eq!(h("https://example.com?q=1").as_deref(), Some("example.com"));
        assert_eq!(h("data:application/pdf;base64,AAAA"), None);
        assert_eq!(h("not a url"), None);
        assert_eq!(h("https:///path"), None);
    }

    #[test]
    fn host_matching_is_exact_or_subdomain() {
        assert!(host_matches("github.com", "github.com"));
        assert!(host_matches("objects.github.com", "github.com"));
        assert!(host_matches("a.b.github.com", "github.com"));
        assert!(!host_matches("notgithub.com", "github.com"));
        assert!(!host_matches("github.com.evil.net", "github.com"));
        assert!(!host_matches("com", "github.com"));
    }

    #[test]
    fn host_patterns_are_validated() {
        assert_eq!(
            normalize_host_pattern(" GitHub.com ").unwrap(),
            "github.com"
        );
        assert_eq!(
            normalize_host_pattern("*.github.com").unwrap(),
            "github.com"
        );
        assert_eq!(
            normalize_host_pattern(".github.com.").unwrap(),
            "github.com"
        );
        for bad in [
            "",
            "*.",
            "https://github.com",
            "github.com/x",
            "github.com:443",
            "a b.com",
        ] {
            assert!(normalize_host_pattern(bad).is_err(), "{bad:?}");
        }
    }
}

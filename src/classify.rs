//! Decides where a file belongs: rules first, then optional fallback
//! classifiers, then the catch-all folder.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;

use crate::config::Config;
use crate::facts::FileFacts;
use crate::rules::RuleSet;

/// A pluggable classifier consulted only for files no rule matched.
///
/// This is the extension point for a future content-based classifier (a
/// local model, OCR, ...). Implementations return a destination folder
/// (interpreted like a rule's `dest`) or `None` to pass.
pub trait Classifier: Send + Sync {
    /// Short name shown in dry-run output and stored in the move log.
    fn name(&self) -> &str;
    fn classify(&self, facts: &FileFacts) -> Option<PathBuf>;
}

/// Why a file is going where it is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Rule(String),
    Classifier(String),
    Fallback,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::Rule(name) => write!(f, "rule \"{name}\""),
            Reason::Classifier(name) => write!(f, "classifier \"{name}\""),
            Reason::Fallback => f.write_str("no rule matched"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// As written in the config; may be relative to the sorted folder.
    pub dest: PathBuf,
    pub reason: Reason,
}

pub struct Engine {
    rules: RuleSet,
    fallbacks: Vec<Box<dyn Classifier>>,
    fallback_dest: PathBuf,
}

impl Engine {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        Ok(Self {
            rules: RuleSet::compile(&cfg.rules)?,
            fallbacks: Vec::new(),
            fallback_dest: cfg.fallback_dest.clone(),
        })
    }

    /// Adds a classifier to try (in insertion order) when no rule matches.
    pub fn with_fallback(mut self, classifier: Box<dyn Classifier>) -> Self {
        self.fallbacks.push(classifier);
        self
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    pub fn decide(&self, facts: &FileFacts, now: SystemTime) -> Decision {
        if let Some(rule) = self.rules.first_match(facts, now) {
            return Decision {
                dest: rule.dest.clone(),
                reason: Reason::Rule(rule.name.clone()),
            };
        }
        for c in &self.fallbacks {
            if let Some(dest) = c.classify(facts) {
                return Decision {
                    dest,
                    reason: Reason::Classifier(c.name().to_string()),
                };
            }
        }
        Decision {
            dest: self.fallback_dest.clone(),
            reason: Reason::Fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NamesWithTaxGoToTaxes;
    impl Classifier for NamesWithTaxGoToTaxes {
        fn name(&self) -> &str {
            "tax-detector"
        }
        fn classify(&self, facts: &FileFacts) -> Option<PathBuf> {
            facts.lower_name().contains("tax").then(|| "Taxes".into())
        }
    }

    #[test]
    fn rules_then_classifiers_then_fallback() {
        let cfg =
            Config::parse("[[rule]]\nname='Docs'\nextensions=['pdf']\ndest='Documents'").unwrap();
        let engine = Engine::from_config(&cfg)
            .unwrap()
            .with_fallback(Box::new(NamesWithTaxGoToTaxes));
        let now = SystemTime::now();
        let decide = |name| engine.decide(&FileFacts::fake(name, now), now);

        // A rule match wins even if the classifier would also have matched.
        assert_eq!(decide("tax.pdf").reason, Reason::Rule("Docs".into()));
        let d = decide("tax-2025.xyz");
        assert_eq!(d.reason, Reason::Classifier("tax-detector".into()));
        assert_eq!(d.dest, PathBuf::from("Taxes"));
        let d = decide("random.xyz");
        assert_eq!(d.reason, Reason::Fallback);
        assert_eq!(d.dest, PathBuf::from("Other"));
    }
}

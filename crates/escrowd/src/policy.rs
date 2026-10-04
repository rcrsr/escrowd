//! The policy file: software rules that run in the daemon, identical across SDKs.
//!
//! ```yaml
//! version: 1
//! read:
//!   deny: [".env", "secrets/**"]   # a pattern without '/' matches the name at any depth
//! ```
//!
//! Phase 1.3 covers read rules; close-time write rules come with the decision tiers.

use std::path::Path;

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub read: ReadRules,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRules {
    #[serde(default)]
    pub deny: Vec<String>,
}

impl Policy {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading policy {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("policy {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let policy: Policy = serde_saphyr::from_str(text)?;
        if policy.version != 1 {
            bail!("unsupported policy version {} (expected 1)", policy.version);
        }
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_read_rules() {
        let p = Policy::parse("version: 1\nread:\n  deny: ['.env', 'secrets/**']\n").unwrap();
        assert_eq!(p.read.deny, [".env", "secrets/**"]);
    }

    #[test]
    fn read_section_is_optional() {
        assert!(Policy::parse("version: 1\n").unwrap().read.deny.is_empty());
    }

    #[test]
    fn rejects_unknown_keys_and_versions() {
        assert!(Policy::parse("version: 1\nreads: {}\n").is_err());
        assert!(Policy::parse("version: 2\n").is_err());
        assert!(Policy::parse("read: {deny: []}\n").is_err());
    }
}

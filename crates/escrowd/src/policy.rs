//! The policy file: software rules that run in the daemon, identical across SDKs.
//!
//! ```yaml
//! version: 1
//! read:
//!   deny: [".env", "secrets/**"]   # a pattern without '/' matches the name at any depth
//! sandbox:
//!   read: ["~/.local/share/mise"]  # host paths sandboxes may read besides the system dirs
//!   write: ["~/.cache/pnpm"]        # host paths sandboxes may write, outside escrow
//! close:
//!   grace_ms: 2000                  # SIGTERM to a closing scope's children, SIGKILL after this
//! ```
//!
//! Close-time write rules come with the decision tiers.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub read: ReadRules,
    #[serde(default)]
    pub sandbox: SandboxRules,
    #[serde(default)]
    pub close: CloseRules,
}

/// How a scope's close treats its running children.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseRules {
    /// Milliseconds between SIGTERM and SIGKILL (default 2000; 0 kills at once).
    #[serde(default = "CloseRules::default_grace_ms")]
    pub grace_ms: u64,
}

impl CloseRules {
    fn default_grace_ms() -> u64 {
        2000
    }
}

impl Default for CloseRules {
    fn default() -> Self {
        CloseRules {
            grace_ms: Self::default_grace_ms(),
        }
    }
}

/// What a sandbox sees of the host besides the project and the system directories.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxRules {
    /// Absolute paths (or `~/…`) bound read-only into every sandbox: toolchains, package stores.
    #[serde(default)]
    pub read: Vec<String>,
    /// Absolute paths (or `~/…`) bound read-write into every sandbox, outside escrow:
    /// package stores and caches. The ledger records each one when a sandbox starts.
    #[serde(default)]
    pub write: Vec<String>,
}

impl SandboxRules {
    /// The read paths with `~` expanded; relative paths are an error.
    pub fn read_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        expand("sandbox.read", &self.read)
    }

    /// The write paths with `~` expanded; relative paths are an error.
    pub fn write_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        expand("sandbox.write", &self.write)
    }
}

fn expand(key: &str, paths: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    paths
        .iter()
        .map(|p| {
            let path = match p.strip_prefix("~/") {
                Some(rest) => std::env::home_dir()
                    .with_context(|| format!("{key}: no home directory"))?
                    .join(rest),
                None => PathBuf::from(p),
            };
            if !path.is_absolute() {
                bail!("{key}: {p} is not absolute");
            }
            Ok(path)
        })
        .collect()
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
    fn sandbox_read_expands_home_and_needs_absolute_paths() {
        let p = Policy::parse("version: 1\nsandbox:\n  read: ['~/tools', '/opt/x']\n").unwrap();
        let paths = p.sandbox.read_paths().unwrap();
        assert!(paths[0].is_absolute() && paths[0].ends_with("tools"));
        assert_eq!(paths[1], std::path::Path::new("/opt/x"));
        let p = Policy::parse("version: 1\nsandbox:\n  read: ['rel/path']\n").unwrap();
        assert!(p.sandbox.read_paths().is_err());
    }

    #[test]
    fn sandbox_write_expands_home_and_needs_absolute_paths() {
        let p = Policy::parse("version: 1\nsandbox:\n  write: ['~/.cache/pnpm']\n").unwrap();
        assert!(p.sandbox.write_paths().unwrap()[0].ends_with(".cache/pnpm"));
        let p = Policy::parse("version: 1\nsandbox:\n  write: ['cache']\n").unwrap();
        assert!(p.sandbox.write_paths().is_err());
    }

    #[test]
    fn close_grace_defaults_to_two_seconds() {
        assert_eq!(Policy::parse("version: 1\n").unwrap().close.grace_ms, 2000);
        let p = Policy::parse("version: 1\nclose:\n  grace_ms: 0\n").unwrap();
        assert_eq!(p.close.grace_ms, 0);
    }

    #[test]
    fn rejects_unknown_keys_and_versions() {
        assert!(Policy::parse("version: 1\nreads: {}\n").is_err());
        assert!(Policy::parse("version: 2\n").is_err());
        assert!(Policy::parse("read: {deny: []}\n").is_err());
    }
}

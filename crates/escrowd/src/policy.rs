//! The policy file: software rules that run in the daemon, identical across SDKs.
//!
//! ```yaml
//! version: 1
//! read:
//!   deny: [".env", "secrets/**"]   # a pattern without '/' matches the name at any depth
//! roots:
//!   home:                           # $HOME through escrowd's views (absent: an empty tmpfs)
//!     default: capture              # unlisted paths [default: deny]
//!     deny: [~/.ssh, ~/.aws, ~/.gnupg]
//!     passthrough: [~/.cache/pip, ~/.npm]
//!     ephemeral: [~/.bash_history]
//!   tmp: { default: ephemeral }     # /tmp (absent: a private tmpfs)
//!   other:
//!     read: ["~/.local/share/mise"] # host paths sandboxes may read besides the system dirs
//!     passthrough: ["~/.cache/pnpm"] # host paths sandboxes may write, outside escrow
//! close:
//!   grace_ms: 2000                  # SIGTERM to a closing scope's children, SIGKILL after this
//! diff:
//!   file_bytes: 262144              # a file larger on either side is summarized (size, SHA-256)
//!   max_bytes: 1048576              # the change set's diff stops before passing this
//! conflict:
//!   verdict: discard                # on a conflicting commit: discard the scope, or return
//!                                   # (reopen it; the conflicting paths are the reasons)
//!   reads: false                    # also conflict on files the scope only read
//! review:                           # the tiers a change set needs, by path; first match wins
//!   - {paths: ["src/auth/**"], tier: human, wait: required}
//!   - {paths: ["docs/**"], tier: llm, wait: optional}
//! write:                            # software tier: a change set that breaks one is discarded
//!   deny: ["*.pem"]                 # paths it must not touch (created, changed, deleted, renamed)
//!   deny_content: ["BEGIN PRIVATE KEY"] # bytes no file it writes may contain
//! ```
//!
//! Paths in `review:` and `write:` are as the change set shows them: project-relative,
//! `~/…` in `$HOME`, absolute elsewhere; a pattern without `/` matches the name at any depth.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

pub use crate::roots::Rule;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub read: ReadRules,
    #[serde(default)]
    pub roots: Roots,
    #[serde(default)]
    pub close: CloseRules,
    #[serde(default)]
    pub diff: DiffRules,
    #[serde(default)]
    pub conflict: ConflictRules,
    #[serde(default)]
    pub review: Vec<ReviewRule>,
    #[serde(default)]
    pub write: WriteRules,
}

/// The decision tiers, cheapest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The policy's rules, in the daemon.
    Software,
    Llm,
    Human,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Software => "software",
            Tier::Llm => "llm",
            Tier::Human => "human",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        [Tier::Software, Tier::Llm, Tier::Human]
            .into_iter()
            .find(|t| t.as_str() == s)
    }
}

/// Whether the agent waits for a tier's verdict.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Wait {
    /// The session's next scope does not open until the verdict.
    #[default]
    Required,
    /// The client may continue.
    Optional,
}

/// The tier the paths matching `paths` need; the first matching rule applies to a path.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRule {
    pub paths: Vec<String>,
    pub tier: Tier,
    /// [default: required]
    #[serde(default)]
    pub wait: Wait,
}

/// The software tier's close-time rules.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRules {
    /// Paths a change set must not touch.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Byte strings no file a change set writes may contain.
    #[serde(default)]
    pub deny_content: Vec<String>,
}

/// What a commit that conflicts with the project does.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConflictRules {
    /// The verdict a conflict turns into [default: discard].
    #[serde(default)]
    pub verdict: ConflictVerdict,
    /// A file the scope only read that changed in the project since its snapshot also
    /// conflicts [default: false].
    #[serde(default)]
    pub reads: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConflictVerdict {
    /// Drop the scope and its changes.
    #[default]
    Discard,
    /// Reopen the scope with its changes; the conflicting paths go back as reasons.
    Return,
}

/// Size caps of the change set's content diff.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffRules {
    /// A file larger than this on either side is summarized (default 256 KiB).
    #[serde(default = "DiffRules::default_file_bytes")]
    pub file_bytes: u64,
    /// The diff stops before the file that would take it past this (default 1 MiB).
    #[serde(default = "DiffRules::default_max_bytes")]
    pub max_bytes: u64,
}

impl DiffRules {
    fn default_file_bytes() -> u64 {
        256 * 1024
    }

    fn default_max_bytes() -> u64 {
        1024 * 1024
    }

    pub fn caps(&self) -> crate::diff::Caps {
        crate::diff::Caps {
            file_bytes: self.file_bytes,
            max_bytes: self.max_bytes,
        }
    }
}

impl Default for DiffRules {
    fn default() -> Self {
        DiffRules {
            file_bytes: Self::default_file_bytes(),
            max_bytes: Self::default_max_bytes(),
        }
    }
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

/// What a sandbox sees outside the project besides the system directories.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roots {
    /// `$HOME` through the views; None: an empty tmpfs.
    pub home: Option<RootRules>,
    /// `/tmp` through the views; None: a private tmpfs.
    pub tmp: Option<RootRules>,
    #[serde(default)]
    pub other: OtherRules,
}

/// One root's rules: paths are absolute (or `~/…`) and inside the root.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRules {
    /// The rule of unlisted paths [default: deny].
    #[serde(default = "RootRules::default_rule")]
    pub default: Rule,
    #[serde(default)]
    pub capture: Vec<String>,
    #[serde(default)]
    pub ephemeral: Vec<String>,
    #[serde(default)]
    pub passthrough: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl RootRules {
    fn default_rule() -> Rule {
        Rule::Deny
    }

    /// Every listed path with its rule, `~` expanded; each must lie inside `root`
    /// (`key` names the root in errors).
    pub fn paths(&self, key: &str, root: &Path) -> anyhow::Result<Vec<(PathBuf, Rule)>> {
        let mut out = Vec::new();
        for (list, rule) in [
            (&self.capture, Rule::Capture),
            (&self.ephemeral, Rule::Ephemeral),
            (&self.passthrough, Rule::Passthrough),
            (&self.deny, Rule::Deny),
        ] {
            for p in expand(key, list)? {
                if !p.starts_with(root) {
                    bail!("{key}: {} is not inside {}", p.display(), root.display());
                }
                out.push((p, rule));
            }
        }
        Ok(out)
    }
}

/// Host paths outside the served roots' rules.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OtherRules {
    /// Absolute paths (or `~/…`) bound read-only into every sandbox: toolchains, mirrors.
    #[serde(default)]
    pub read: Vec<String>,
    /// Absolute paths (or `~/…`) bound read-write into every sandbox, outside escrow:
    /// package stores and caches. The ledger records each one when a sandbox starts.
    #[serde(default)]
    pub passthrough: Vec<String>,
}

impl OtherRules {
    /// The read paths with `~` expanded; relative paths are an error.
    pub fn read_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        expand("roots.other.read", &self.read)
    }

    /// The passthrough paths with `~` expanded; relative paths are an error.
    pub fn passthrough_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        expand("roots.other.passthrough", &self.passthrough)
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
                None if p == "~" => std::env::home_dir().with_context(|| format!("{key}: no home directory"))?,
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
    fn other_read_expands_home_and_needs_absolute_paths() {
        let p = Policy::parse("version: 1\nroots:\n  other:\n    read: ['~/tools', '/opt/x']\n").unwrap();
        let paths = p.roots.other.read_paths().unwrap();
        assert!(paths[0].is_absolute() && paths[0].ends_with("tools"));
        assert_eq!(paths[1], std::path::Path::new("/opt/x"));
        let p = Policy::parse("version: 1\nroots:\n  other:\n    read: ['rel/path']\n").unwrap();
        assert!(p.roots.other.read_paths().is_err());
    }

    #[test]
    fn other_passthrough_expands_home_and_needs_absolute_paths() {
        let p = Policy::parse("version: 1\nroots:\n  other:\n    passthrough: ['~/.cache/pnpm']\n").unwrap();
        assert!(p.roots.other.passthrough_paths().unwrap()[0].ends_with(".cache/pnpm"));
        let p = Policy::parse("version: 1\nroots:\n  other:\n    passthrough: ['cache']\n").unwrap();
        assert!(p.roots.other.passthrough_paths().is_err());
    }

    #[test]
    fn root_rules_default_to_deny_and_stay_inside_their_root() {
        let p = Policy::parse(
            "version: 1\nroots:\n  home: {deny: ['~/.ssh'], capture: ['~/.gitconfig']}\n  tmp: {default: ephemeral}\n",
        )
        .unwrap();
        let home = p.roots.home.unwrap();
        assert_eq!(home.default, Rule::Deny);
        let home_dir = std::env::home_dir().unwrap();
        let paths = home.paths("roots.home", &home_dir).unwrap();
        assert_eq!(
            paths,
            [
                (home_dir.join(".gitconfig"), Rule::Capture),
                (home_dir.join(".ssh"), Rule::Deny)
            ]
        );
        assert_eq!(p.roots.tmp.unwrap().default, Rule::Ephemeral);
        let p = Policy::parse("version: 1\nroots:\n  tmp: {capture: ['/var/x']}\n").unwrap();
        assert!(p.roots.tmp.unwrap().paths("roots.tmp", Path::new("/tmp")).is_err());
        assert!(Policy::parse("version: 1\nroots:\n  home: {default: keep}\n").is_err());
    }

    #[test]
    fn close_grace_defaults_to_two_seconds() {
        assert_eq!(Policy::parse("version: 1\n").unwrap().close.grace_ms, 2000);
        let p = Policy::parse("version: 1\nclose:\n  grace_ms: 0\n").unwrap();
        assert_eq!(p.close.grace_ms, 0);
    }

    #[test]
    fn diff_caps_default_and_parse() {
        let d = Policy::parse("version: 1\n").unwrap().diff;
        assert_eq!((d.file_bytes, d.max_bytes), (256 * 1024, 1024 * 1024));
        let d = Policy::parse("version: 1\ndiff: {file_bytes: 10}\n").unwrap().diff;
        assert_eq!((d.file_bytes, d.max_bytes), (10, 1024 * 1024));
    }

    #[test]
    fn conflict_rules_default_and_parse() {
        let c = Policy::parse("version: 1\n").unwrap().conflict;
        assert_eq!((c.verdict, c.reads), (ConflictVerdict::Discard, false));
        let c = Policy::parse("version: 1\nconflict: {verdict: return, reads: true}\n")
            .unwrap()
            .conflict;
        assert_eq!((c.verdict, c.reads), (ConflictVerdict::Return, true));
        assert!(Policy::parse("version: 1\nconflict: {verdict: rebase}\n").is_err());
    }

    #[test]
    fn review_and_write_rules_parse() {
        let p = Policy::parse(
            "version: 1\nreview:\n  - {paths: ['src/**'], tier: llm}\n  - {paths: [docs], tier: human, wait: optional}\nwrite: {deny: ['*.pem'], deny_content: ['KEY']}\n",
        )
        .unwrap();
        assert_eq!((p.review[0].tier, p.review[0].wait), (Tier::Llm, Wait::Required));
        assert_eq!((p.review[1].tier, p.review[1].wait), (Tier::Human, Wait::Optional));
        assert_eq!((p.write.deny.len(), p.write.deny_content.len()), (1, 1));
        assert!(Tier::Software < Tier::Llm && Tier::Llm < Tier::Human);
        assert!(Policy::parse("version: 1\nreview: [{paths: [a], tier: robot}]\n").is_err());
        assert!(Policy::parse("version: 1\nwrite: {only: [a]}\n").is_err());
    }

    #[test]
    fn rejects_unknown_keys_and_versions() {
        assert!(Policy::parse("version: 1\nreads: {}\n").is_err());
        assert!(Policy::parse("version: 1\nsandbox: {write: []}\n").is_err());
        assert!(Policy::parse("version: 2\n").is_err());
        assert!(Policy::parse("read: {deny: []}\n").is_err());
    }
}

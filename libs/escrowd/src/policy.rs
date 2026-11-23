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
//! reviewers:                        # named reviewers, in the order they review
//!   security: {}                    # no run: answers on the review socket
//!   lint:
//!     run: [/usr/bin/lint, --strict] # a program that answers on stdout; argv[0] as written
//!     timeout: 30s                  # <int>(ms|s|m|h) [default: 60s]
//!     on_failure: discard           # commit, return or discard [default: discard]
//!     env: [LINT_CONFIG]            # variables copied besides PATH, HOME and LANG
//!     delegate: false               # the run may hand its verdict to another process
//!     can_override: false           # its verdict may loosen the one so far
//! reviewers_parallel: 4             # command reviewers running at once [default: 4]
//! review:                           # the reviewers a change set needs, by path; first match wins
//!   - {paths: ["src/auth/**"], tier: human, wait: required} # tier: llm = [llm]; human = [llm, human]
//!   - {paths: ["docs/**"], reviewers: [lint, security], wait: optional}
//! write:                            # software tier: a change set that breaks one is discarded
//!   deny: ["*.pem"]                 # paths it must not touch (created, changed, deleted, renamed)
//!   deny_content: ["BEGIN PRIVATE KEY"] # bytes no file it writes may contain
//!   only_by:                        # paths only these programs may change, matched on the
//!     - {paths: [".git/**"], programs: ["/usr/bin/git"]} # binary's device and inode
//! ```
//!
//! Paths in `review:` and `write:` are as the change set shows them: project-relative,
//! `~/…` in `$HOME`, absolute elsewhere; a pattern without `/` matches the name at any depth.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

pub use crate::roots::Rule;

#[derive(Debug, Deserialize)]
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
    /// The named reviewers, by name in declaration order.
    #[serde(default, deserialize_with = "ordered_reviewers")]
    pub reviewers: Vec<(String, ReviewerDef)>,
    /// Command reviewers running at once, at least 1 [default: 4].
    #[serde(default = "Policy::default_reviewers_parallel")]
    pub reviewers_parallel: usize,
    #[serde(default)]
    pub review: Vec<ReviewRule>,
    #[serde(default)]
    pub write: WriteRules,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            version: 0,
            read: ReadRules::default(),
            roots: Roots::default(),
            close: CloseRules::default(),
            diff: DiffRules::default(),
            conflict: ConflictRules::default(),
            reviewers: Vec::new(),
            reviewers_parallel: Self::default_reviewers_parallel(),
            review: Vec::new(),
            write: WriteRules::default(),
        }
    }
}

/// The `reviewers:` map as a list: declaration order is review order; a repeated name is an error.
fn ordered_reviewers<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<(String, ReviewerDef)>, D::Error> {
    struct Ordered;

    impl<'de> Visitor<'de> for Ordered {
        type Value = Vec<(String, ReviewerDef)>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map of reviewer names to reviewers")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out: Vec<(String, ReviewerDef)> = Vec::new();
            while let Some((name, def)) = map.next_entry::<String, ReviewerDef>()? {
                if out.iter().any(|(n, _)| *n == name) {
                    return Err(de::Error::custom(format!("duplicate reviewer {name}")));
                }
                out.push((name, def));
            }
            Ok(out)
        }
    }

    d.deserialize_map(Ordered)
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

/// The reviewers the paths matching `paths` need; the first matching rule applies to a path.
/// Give `tier` (llm: the llm reviewer; human: llm then human; software: none) or `reviewers`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRule {
    pub paths: Vec<String>,
    pub tier: Option<Tier>,
    /// Names of declared or built-in reviewers; empty: none.
    pub reviewers: Option<Vec<String>>,
    /// [default: required]
    #[serde(default)]
    pub wait: Wait,
}

/// What a command reviewer's failure (crash, timeout, unreadable answer) turns into.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnFailure {
    Commit,
    Return,
    #[default]
    Discard,
}

/// A named reviewer: a socket reviewer without `run`, a command reviewer with it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerDef {
    /// The program and its arguments; None: the reviewer answers on the review socket.
    pub run: Option<Vec<String>>,
    /// How long a run may take, `<int>(ms|s|m|h)` [default: 60s]. Needs `run`.
    pub timeout: Option<String>,
    /// [default: discard]. Needs `run`.
    pub on_failure: Option<OnFailure>,
    /// Variables copied into the run's environment besides PATH, HOME and LANG. Needs `run`.
    #[serde(default)]
    pub env: Vec<String>,
    /// The run may hand its verdict to another process, which answers with a token.
    /// Needs `run`.
    #[serde(default)]
    pub delegate: bool,
    /// Its verdict may loosen the one so far.
    #[serde(default)]
    pub can_override: bool,
}

impl ReviewerDef {
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

    /// The run's timeout; `Policy::parse` has checked the text.
    pub fn timeout(&self) -> Duration {
        self.timeout
            .as_deref()
            .and_then(|t| parse_duration("timeout", t).ok())
            .unwrap_or(Self::DEFAULT_TIMEOUT)
    }

    pub fn on_failure(&self) -> OnFailure {
        self.on_failure.unwrap_or_default()
    }

    /// The program of `run`: an absolute path, `~/…`, or a bare name found on `PATH` now.
    /// It must be an executable file.
    pub fn program_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        let key = format!("reviewers.{name}");
        let Some(first) = self.run.as_ref().and_then(|r| r.first()) else {
            bail!("{key}: needs run");
        };
        let path = if !first.contains('/') && first != "~" {
            let paths = std::env::var_os("PATH").unwrap_or_default();
            std::env::split_paths(&paths)
                .filter(|d| d.is_absolute())
                .map(|d| d.join(first))
                .find(|p| is_executable(p))
                .with_context(|| format!("{key}: {first}: not found on PATH"))?
        } else {
            expand(&key, std::slice::from_ref(first))?.remove(0)
        };
        let meta = std::fs::metadata(&path).with_context(|| format!("{key}: {first}"))?;
        if !is_executable_file(&meta) {
            bail!("{key}: {first}: not an executable file");
        }
        Ok(path)
    }

    /// The shape checks `Policy::parse` runs.
    fn check(&self, name: &str) -> anyhow::Result<()> {
        let key = format!("reviewers.{name}");
        if !valid_reviewer_name(name) {
            bail!("{key}: invalid name (lowercase letters, digits and '-', starting with a letter)");
        }
        if matches!(name, "llm" | "human") {
            bail!("{key}: {name} is built in");
        }
        match &self.run {
            None => {
                for (k, given) in [
                    ("delegate", self.delegate),
                    ("timeout", self.timeout.is_some()),
                    ("on_failure", self.on_failure.is_some()),
                    ("env", !self.env.is_empty()),
                ] {
                    if given {
                        bail!("{key}: {k} needs run");
                    }
                }
            }
            Some(run) if run.is_empty() => bail!("{key}: run is empty"),
            Some(_) => {}
        }
        if let Some(t) = &self.timeout {
            parse_duration(&format!("{key}.timeout"), t)?;
        }
        Ok(())
    }
}

fn is_executable_file(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.is_file() && m.permissions().mode() & 0o111 != 0
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| is_executable_file(&m))
}

/// A reviewer name: a lowercase letter, then lowercase letters, digits and `-`.
pub fn valid_reviewer_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `<int>(ms|s|m|h)`, above zero; a bare number is refused (`key` names it in errors).
pub fn parse_duration(key: &str, text: &str) -> anyhow::Result<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit()).unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let n: u64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("{key}: {text:?} is not a duration like 500ms, 30s, 5m or 1h"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.saturating_mul(60)),
        "h" => Duration::from_secs(n.saturating_mul(3600)),
        _ => bail!("{key}: {text:?} is not a duration like 500ms, 30s, 5m or 1h"),
    };
    if d.is_zero() {
        bail!("{key}: {text:?} must be above zero");
    }
    Ok(d)
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
    #[serde(default)]
    pub only_by: Vec<OnlyBy>,
}

/// Paths matching `paths` change only through processes running one of `programs`
/// (absolute paths, `~/` expanded; a symlink names its target).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnlyBy {
    pub paths: Vec<String>,
    pub programs: Vec<String>,
}

impl OnlyBy {
    pub fn program_paths(&self) -> anyhow::Result<Vec<PathBuf>> {
        expand("write.only_by.programs", &self.programs)
    }
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
    fn default_reviewers_parallel() -> usize {
        4
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading policy {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("policy {}", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let policy: Policy = serde_saphyr::from_str(text)?;
        if policy.version != 1 {
            bail!("unsupported policy version {} (expected 1)", policy.version);
        }
        if policy.reviewers_parallel == 0 {
            bail!("reviewers_parallel: must be at least 1");
        }
        for (name, def) in &policy.reviewers {
            def.check(name)?;
        }
        Ok(policy)
    }
}

/// IO outside any scope: what the app's sandbox sees at the project path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unscoped {
    /// The real project, bound directly: real IO, not captured.
    Passthrough,
    /// A default scope, decided at exit or by `settle_unscoped`, then replaced by a fresh one.
    Implicit,
    /// A read-only view of the live project: reads pass (through the gate), changes get EROFS.
    Deny,
}

impl std::str::FromStr for Unscoped {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "passthrough" => Ok(Unscoped::Passthrough),
            "implicit" => Ok(Unscoped::Implicit),
            "deny" => Ok(Unscoped::Deny),
            _ => Err(format!("unscoped mode {s}: expected passthrough, implicit or deny")),
        }
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
        assert_eq!((p.review[0].tier, p.review[0].wait), (Some(Tier::Llm), Wait::Required));
        assert_eq!(
            (p.review[1].tier, p.review[1].wait),
            (Some(Tier::Human), Wait::Optional)
        );
        assert_eq!((p.write.deny.len(), p.write.deny_content.len()), (1, 1));
        assert!(Tier::Software < Tier::Llm && Tier::Llm < Tier::Human);
        assert!(Policy::parse("version: 1\nreview: [{paths: [a], tier: robot}]\n").is_err());
        assert!(Policy::parse("version: 1\nwrite: {only: [a]}\n").is_err());
        let p = Policy::parse("version: 1\nwrite: {only_by: [{paths: ['.git/**'], programs: ['/usr/bin/git']}]}\n")
            .unwrap();
        assert_eq!(
            p.write.only_by[0].program_paths().unwrap(),
            [PathBuf::from("/usr/bin/git")]
        );
        let p = Policy::parse("version: 1\nwrite: {only_by: [{paths: [a], programs: [git]}]}\n").unwrap();
        assert!(p.write.only_by[0].program_paths().is_err());
    }

    #[test]
    fn rejects_unknown_keys_and_versions() {
        assert!(Policy::parse("version: 1\nreads: {}\n").is_err());
        assert!(Policy::parse("version: 1\nsandbox: {write: []}\n").is_err());
        assert!(Policy::parse("version: 2\n").is_err());
        assert!(Policy::parse("read: {deny: []}\n").is_err());
    }

    fn parse_err(text: &str) -> String {
        format!("{:#}", Policy::parse(text).unwrap_err())
    }

    #[test]
    fn reviewers_keep_declaration_order_and_defaults() {
        let p = Policy::parse(
            "version: 1\nreviewers:\n  zed: {}\n  alpha: {run: [/bin/sh, -c, x], can_override: true}\n  mid-1: {run: [sh]}\n",
        )
        .unwrap();
        let names: Vec<&str> = p.reviewers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["zed", "alpha", "mid-1"]);
        let (zed, alpha) = (&p.reviewers[0].1, &p.reviewers[1].1);
        assert!(zed.run.is_none() && !zed.can_override);
        assert_eq!(alpha.run.as_deref().unwrap(), ["/bin/sh", "-c", "x"]);
        assert!(alpha.can_override && !alpha.delegate);
        assert_eq!(
            (alpha.timeout(), alpha.on_failure()),
            (Duration::from_secs(60), OnFailure::Discard)
        );
        assert_eq!(p.reviewers_parallel, 4);
        assert_eq!(Policy::default().reviewers_parallel, 4);
        assert_eq!(
            Policy::parse("version: 1\nreviewers_parallel: 2\n")
                .unwrap()
                .reviewers_parallel,
            2
        );
    }

    #[test]
    fn reviewer_options_parse() {
        let p = Policy::parse(
            "version: 1\nreviewers:\n  a: {run: [/bin/sh], timeout: 250ms, on_failure: return, env: [X], delegate: true}\n  b: {run: [/bin/sh], timeout: 2m, on_failure: commit}\n  c: {run: [/bin/sh], timeout: 1h}\n",
        )
        .unwrap();
        let a = &p.reviewers[0].1;
        assert_eq!(
            (a.timeout(), a.on_failure()),
            (Duration::from_millis(250), OnFailure::Return)
        );
        assert!(a.delegate && a.env == ["X"]);
        assert_eq!(
            (p.reviewers[1].1.timeout(), p.reviewers[1].1.on_failure()),
            (Duration::from_secs(120), OnFailure::Commit)
        );
        assert_eq!(p.reviewers[2].1.timeout(), Duration::from_secs(3600));
    }

    #[test]
    fn reviewer_errors_name_the_reviewer_and_key() {
        let cases = [
            ("reviewers: {Bad: {}}", "reviewers.Bad: invalid name"),
            ("reviewers: {'1a': {}}", "reviewers.1a: invalid name"),
            ("reviewers: {llm: {}}", "llm is built in"),
            ("reviewers: {human: {}}", "human is built in"),
            ("reviewers: {a: {}, a: {}}", "duplicate mapping key: a"),
            ("reviewers: {a: {delegate: true}}", "reviewers.a: delegate needs run"),
            ("reviewers: {a: {timeout: 5s}}", "reviewers.a: timeout needs run"),
            (
                "reviewers: {a: {on_failure: commit}}",
                "reviewers.a: on_failure needs run",
            ),
            ("reviewers: {a: {env: [X]}}", "reviewers.a: env needs run"),
            ("reviewers: {a: {run: []}}", "reviewers.a: run is empty"),
            ("reviewers: {a: {run: [x], timeout: 30}}", "reviewers.a.timeout"),
            ("reviewers: {a: {run: [x], timeout: 0s}}", "reviewers.a.timeout"),
            ("reviewers: {a: {run: [x], timeout: 5d}}", "reviewers.a.timeout"),
            ("reviewers: {a: {run: [x], on_failure: retry}}", "retry"),
            ("reviewers_parallel: 0", "reviewers_parallel"),
        ];
        for (body, want) in cases {
            let err = parse_err(&format!("version: 1\n{body}\n"));
            assert!(err.contains(want), "{body}: {err}");
        }
    }

    #[test]
    fn the_reviewers_map_rejects_a_repeated_name_by_name() {
        use serde::de::value::{Error, MapDeserializer};
        let empty = || MapDeserializer::<_, Error>::new(std::iter::empty::<(String, String)>());
        let pair = |n: &str| (n.to_string(), empty());
        let ok = ordered_reviewers(MapDeserializer::<_, Error>::new([pair("b"), pair("a")].into_iter())).unwrap();
        assert_eq!(ok.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        let err = ordered_reviewers(MapDeserializer::<_, Error>::new([pair("a"), pair("a")].into_iter())).unwrap_err();
        assert!(err.to_string().contains("duplicate reviewer a"), "{err}");
    }

    #[test]
    fn program_path_resolves_and_refuses() {
        let def = |run: &str| {
            Policy::parse(&format!("version: 1\nreviewers: {{a: {{run: {run}}}}}\n"))
                .unwrap()
                .reviewers[0]
                .1
                .clone()
        };
        assert_eq!(def("[/bin/sh]").program_path("a").unwrap(), Path::new("/bin/sh"));
        assert!(def("[sh]").program_path("a").unwrap().ends_with("sh"));
        let err = |run: &str| format!("{:#}", def(run).program_path("a").unwrap_err());
        assert!(err("[./x]").contains("is not absolute"));
        assert!(err("[bin/x]").contains("is not absolute"));
        assert!(err("[/no/such/prog]").contains("reviewers.a: /no/such/prog"));
        assert!(err("[no-such-prog-xyz]").contains("reviewers.a: no-such-prog-xyz"));
        assert!(err("[/etc/passwd]").contains("reviewers.a: /etc/passwd"));
        assert!(
            ReviewerDef::default()
                .program_path("a")
                .unwrap_err()
                .to_string()
                .contains("needs run")
        );
    }

    #[test]
    fn reviewer_names_are_a_lowercase_letter_then_letters_digits_dashes() {
        assert!(valid_reviewer_name("a") && valid_reviewer_name("sec-2"));
        assert!(!valid_reviewer_name("") && !valid_reviewer_name("2a") && !valid_reviewer_name("a_b"));
        assert!(!valid_reviewer_name("A") && !valid_reviewer_name("-a"));
    }

    #[test]
    fn review_rules_take_tier_or_reviewers() {
        let p = Policy::parse(
            "version: 1\nreview:\n  - {paths: [a], reviewers: [x, y]}\n  - {paths: [b], tier: software}\n",
        )
        .unwrap();
        assert_eq!(p.review[0].reviewers.as_deref().unwrap(), ["x", "y"]);
        assert_eq!(p.review[0].tier, None);
        assert_eq!(p.review[1].tier, Some(Tier::Software));
    }
}

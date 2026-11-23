//! Close-time review: the software tier's write rules (policy `write:`) and the
//! reviewers a change set needs (policy `reviewers:` and `review:`).
//!
//! A change set that breaks a write rule is discarded: the software tier's verdict
//! is final at close, and a client's commit or return turns into a discard (verdicts
//! only tighten). `write.only_by` names the programs allowed to change a path: a
//! change whose writers include another binary, or that has no recorded writer,
//! breaks it. Otherwise each path gets the reviewers of the first `review:` rule it
//! matches (none if no rule), and the change set needs the union of them, in the order
//! `reviewers:` declares them, then the built-in `llm` and `human`; a rename counts both
//! its paths. `tier: llm` names `[llm]`, `tier: human` names `[llm, human]`, `tier:
//! software` and `reviewers: []` name none. Without `review:` rules nothing needs a
//! reviewer.
//!
//! A reviewer is a socket reviewer (answers on the review socket by name) or a command
//! reviewer (the daemon runs a program). [`Rules::admit`] decides, without side effects,
//! whether a reviewer's verdict is taken now and what it leaves pending.

use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use memchr::memmem::Finder;

use crate::gate::Globs;
use crate::policy::{OnFailure, ReviewRule, ReviewerDef, Tier, Wait, WriteRules};
use crate::proc::Info;

pub struct Rules {
    /// Declared reviewers in declaration order, then the built-in `llm` and `human`.
    reviewers: Vec<Reviewer>,
    /// Paths, the indices into `reviewers` they need, and whether the agent waits.
    review: Vec<(Globs, Vec<usize>, Wait)>,
    deny: Globs,
    /// Each forbidden byte string, with its pattern for the reason.
    deny_content: Vec<(String, Finder<'static>)>,
    /// Paths, and the (device, inode) of the binaries allowed to change them.
    only_by: Vec<(Globs, Vec<(u64, u64)>)>,
}

/// A reviewer of held scopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reviewer {
    pub name: String,
    /// Its verdict may loosen the one so far.
    pub can_override: bool,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Answers on the review socket by name.
    Socket,
    /// The daemon runs a program.
    Command(Command),
}

/// How the daemon runs a command reviewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    /// The executable, resolved when the policy loaded.
    pub program: PathBuf,
    /// The policy's `run`, argv[0] as written.
    pub argv: Vec<String>,
    pub timeout: Duration,
    /// The verdict a failed run turns into.
    pub on_failure: Decision,
    /// Variables copied into the run's environment besides PATH, HOME and LANG.
    pub env: Vec<String>,
    pub delegate: bool,
}

/// A reviewer's verdict taken: where it stood in the pending list and what is left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Admitted {
    /// Index in the pending list; reviewers `..=at` are done.
    pub at: usize,
    /// The verdict so far after this one.
    pub verdict_after: Decision,
    /// The verdict loosened the one so far.
    pub looser: bool,
    /// Nothing more to ask: no one is left pending, or a discard stands that no later
    /// pending reviewer can override.
    pub done: bool,
}

/// Why a verdict is not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    NotPending,
    /// An earlier reviewer (named) is still pending.
    NotFirst(String),
    /// Looser than the verdict so far without the right to override.
    Looser,
}

/// A tier's verdict on a held scope, loosest first: verdicts only tighten.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    Commit,
    Return,
    Discard,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Commit => "commit",
            Decision::Return => "return",
            Decision::Discard => "discard",
        }
    }

    pub fn parse(s: &str) -> Option<Decision> {
        [Decision::Commit, Decision::Return, Decision::Discard]
            .into_iter()
            .find(|d| d.as_str() == s)
    }
}

/// One tier's verdict on a held scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierReview {
    pub reviewer: String,
    pub verdict: Decision,
    pub reasons: Vec<String>,
    /// The reviewer loosened the verdict so far.
    pub over: bool,
}

/// A write rule a change broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// As the change set shows it.
    pub path: PathBuf,
    pub rule: Broken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broken {
    /// `write.deny`.
    Deny,
    /// `write.deny_content`: the forbidden byte string.
    Content(String),
    /// `write.only_by`: the program of a writer not allowed, None if no writer is recorded.
    OnlyBy(Option<PathBuf>),
}

impl Hit {
    pub fn reason(&self) -> String {
        let p = self.path.display();
        match &self.rule {
            Broken::Deny => format!("write.deny: {p}"),
            Broken::Content(c) => format!("write.deny_content: {p} contains {c:?}"),
            Broken::OnlyBy(Some(prog)) => format!("write.only_by: {p} changed by {}", prog.display()),
            Broken::OnlyBy(None) => format!("write.only_by: {p} changed by an unknown process"),
        }
    }

    /// The ledger's op for the rule.
    pub fn op(&self) -> &'static str {
        match self.rule {
            Broken::Deny => "write-deny",
            Broken::Content(_) => "write-content",
            Broken::OnlyBy(_) => "write-only-by",
        }
    }
}

/// The close-time review of a change set. The default is a change set no rule
/// touched: commit, nothing needed above software.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    /// The software tier's verdict: commit, or discard when a write rule broke.
    pub verdict: Decision,
    /// The write rules broken; empty when the verdict is commit.
    pub hits: Vec<Hit>,
    /// The reviewers the change set needs, in review order; empty after a discard.
    pub reviewers: Vec<String>,
    /// A path that needs a reviewer has a rule that requires the agent to wait.
    pub wait: bool,
}

impl Default for Review {
    fn default() -> Self {
        Review {
            verdict: Decision::Commit,
            hits: Vec::new(),
            reviewers: Vec::new(),
            wait: false,
        }
    }
}

impl Review {
    pub fn reasons(&self) -> Vec<String> {
        self.hits.iter().map(Hit::reason).collect()
    }
}

impl Rules {
    pub fn new(review: &[ReviewRule], reviewers: &[(String, ReviewerDef)], write: &WriteRules) -> anyhow::Result<Self> {
        let mut all = Vec::new();
        for (name, def) in reviewers {
            let kind = if def.run.is_some() {
                Kind::Command(Command {
                    program: def.program_path(name)?,
                    argv: def.run.clone().unwrap_or_default(),
                    timeout: def.timeout(),
                    on_failure: match def.on_failure() {
                        OnFailure::Commit => Decision::Commit,
                        OnFailure::Return => Decision::Return,
                        OnFailure::Discard => Decision::Discard,
                    },
                    env: def.env.clone(),
                    delegate: def.delegate,
                })
            } else {
                Kind::Socket
            };
            all.push(Reviewer {
                name: name.clone(),
                can_override: def.can_override,
                kind,
            });
        }
        for (name, can_override) in [("llm", false), ("human", true)] {
            all.push(Reviewer {
                name: name.into(),
                can_override,
                kind: Kind::Socket,
            });
        }
        let index = |name: &str| all.iter().position(|r| r.name == name);
        let mut rules = Vec::new();
        for (i, r) in review.iter().enumerate() {
            if r.paths.is_empty() {
                bail!("review[{i}]: no paths");
            }
            let needs = match (&r.tier, &r.reviewers) {
                (Some(_), Some(_)) => bail!("review[{i}]: give tier or reviewers, not both"),
                (None, None) => bail!("review[{i}]: give tier or reviewers"),
                (Some(Tier::Software), None) => Vec::new(),
                (Some(Tier::Llm), None) => vec!["llm".to_string()],
                (Some(Tier::Human), None) => vec!["llm".to_string(), "human".to_string()],
                (None, Some(names)) => names.clone(),
            };
            let mut at = Vec::new();
            for n in &needs {
                let Some(k) = index(n) else {
                    bail!("review[{i}]: unknown reviewer {n}");
                };
                at.push(k);
            }
            rules.push((Globs::new(&r.paths)?, at, r.wait));
        }
        let mut deny_content = Vec::new();
        for c in &write.deny_content {
            if c.is_empty() {
                bail!("write.deny_content: an empty string matches every file");
            }
            deny_content.push((c.clone(), Finder::new(c.as_bytes()).into_owned()));
        }
        let mut only_by = Vec::new();
        for (i, o) in write.only_by.iter().enumerate() {
            if o.paths.is_empty() {
                bail!("write.only_by[{i}]: no paths");
            }
            let mut bins = Vec::new();
            for p in o.program_paths()? {
                let m = std::fs::metadata(&p).with_context(|| format!("write.only_by[{i}]: {}", p.display()))?;
                bins.push((m.dev(), m.ino()));
            }
            only_by.push((Globs::new(&o.paths)?, bins));
        }
        Ok(Rules {
            reviewers: all,
            review: rules,
            deny: Globs::new(&write.deny)?,
            deny_content,
            only_by,
        })
    }

    /// Whether any rule could change a change set's verdict or tiers.
    pub fn is_empty(&self) -> bool {
        self.review.is_empty() && self.deny.is_empty() && self.deny_content.is_empty() && self.only_by.is_empty()
    }

    /// The `write.only_by` rule `path`, changed by `writers`, breaks, if any.
    pub fn only_by<'a>(&self, path: &Path, writers: impl IntoIterator<Item = &'a Info> + Clone) -> Option<Broken> {
        for (paths, bins) in &self.only_by {
            if !paths.is_match(path) {
                continue;
            }
            let mut any = false;
            for w in writers.clone() {
                any = true;
                if !bins.contains(&(w.dev, w.ino)) {
                    return Some(Broken::OnlyBy(Some(w.program.clone())));
                }
            }
            if !any {
                return Some(Broken::OnlyBy(None));
            }
        }
        None
    }

    /// The reviewer `name`, declared or built in.
    pub fn reviewer(&self, name: &str) -> Option<&Reviewer> {
        self.reviewers.iter().find(|r| r.name == name)
    }

    /// Decide whether `who`'s verdict is taken, given the `pending` reviewers in order and
    /// the verdict `so_far`. A reviewer that may override can answer out of turn and
    /// stands for every reviewer pending before it. A verdict looser than `so_far`
    /// needs `over` and the right to override (`Refused::Looser` otherwise); a run
    /// (`from_run`) is never refused for it: it counts as an override if its reviewer may
    /// override, else it is clamped to `so_far`.
    pub fn admit(
        &self,
        pending: &[String],
        so_far: Decision,
        who: &str,
        verdict: Decision,
        over: bool,
        from_run: bool,
    ) -> Result<Admitted, Refused> {
        let can_override = |name: &str| self.reviewer(name).is_some_and(|r| r.can_override);
        let Some(at) = pending.iter().position(|p| p == who) else {
            return Err(Refused::NotPending);
        };
        if at > 0 && !can_override(who) {
            return Err(Refused::NotFirst(pending[0].clone()));
        }
        let mut looser = verdict < so_far;
        if looser {
            if from_run {
                looser = can_override(who);
            } else if !(over && can_override(who)) {
                return Err(Refused::Looser);
            }
        }
        let verdict_after = if looser { verdict } else { so_far.max(verdict) };
        let later = &pending[at + 1..];
        let done = later.is_empty() || (verdict_after == Decision::Discard && !later.iter().any(|n| can_override(n)));
        Ok(Admitted {
            at,
            verdict_after,
            looser,
            done,
        })
    }

    pub fn denied(&self, path: &Path) -> bool {
        self.deny.is_match(path)
    }

    pub fn checks_content(&self) -> bool {
        !self.deny_content.is_empty()
    }

    /// The first forbidden byte string in `r`, read in 64 KiB chunks.
    pub fn forbidden_content(&self, mut r: impl Read) -> io::Result<Option<&str>> {
        if self.deny_content.is_empty() {
            return Ok(None);
        }
        // Keep the tail a match could start in.
        let keep = self.deny_content.iter().map(|(c, _)| c.len()).max().unwrap_or(1) - 1;
        let mut chunk = vec![0u8; 64 * 1024];
        let mut buf = Vec::with_capacity(chunk.len() + keep);
        loop {
            let n = match r.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            buf.extend_from_slice(&chunk[..n]);
            if let Some((c, _)) = self.deny_content.iter().find(|(_, f)| f.find(&buf).is_some()) {
                return Ok(Some(c));
            }
            buf.drain(..buf.len().saturating_sub(keep));
        }
    }

    /// The review of a change set whose paths (as shown, both paths of a rename) are
    /// `paths` and whose write-rule hits are `hits`.
    pub fn review<'a>(&self, paths: impl IntoIterator<Item = &'a Path>, hits: Vec<Hit>) -> Review {
        if !hits.is_empty() {
            return Review {
                verdict: Decision::Discard,
                hits,
                reviewers: Vec::new(),
                wait: false,
            };
        }
        let mut needed = vec![false; self.reviewers.len()];
        let mut wait = false;
        for p in paths {
            let Some((_, at, w)) = self.review.iter().find(|(g, _, _)| g.is_match(p)) else {
                continue;
            };
            for k in at {
                needed[*k] = true;
            }
            wait |= !at.is_empty() && *w == Wait::Required;
        }
        let reviewers = self
            .reviewers
            .iter()
            .zip(needed)
            .filter(|(_, n)| *n)
            .map(|(r, _)| r.name.clone())
            .collect();
        Review {
            verdict: Decision::Commit,
            hits,
            reviewers,
            wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules_with(reviewers: &str, review: &str, write: &str) -> anyhow::Result<Rules> {
        let p = crate::policy::Policy::parse(&format!(
            "version: 1\nreviewers: {reviewers}\nreview: {review}\nwrite: {write}\n"
        ))
        .unwrap();
        Rules::new(&p.review, &p.reviewers, &p.write)
    }

    fn rules(review: &str, write: &str) -> Rules {
        rules_with("{}", review, write).unwrap()
    }

    fn names(ns: &[&str]) -> Vec<String> {
        ns.iter().map(|n| n.to_string()).collect()
    }

    fn paths(ps: &[&'static str]) -> Vec<&'static Path> {
        ps.iter().map(|p| Path::new(*p)).collect()
    }

    #[test]
    fn tiers_go_up_to_the_highest_path_needs_cheapest_first() {
        let r = rules(
            "[{paths: ['src/auth/**'], tier: human}, {paths: ['src/**'], tier: llm, wait: optional}]",
            "{}",
        );
        let v = r.review(paths(&["README.md"]), vec![]);
        assert_eq!(
            (v.verdict, v.reviewers.clone(), v.wait),
            (Decision::Commit, Vec::<String>::new(), false)
        );
        let v = r.review(paths(&["src/util.py"]), vec![]);
        assert_eq!((v.reviewers.clone(), v.wait), (names(&["llm"]), false));
        // First match wins: src/auth/ is human (wait required) although src/** matches too.
        let v = r.review(paths(&["src/util.py", "src/auth/login.py"]), vec![]);
        assert_eq!((v.reviewers, v.wait), (names(&["llm", "human"]), true));
    }

    #[test]
    fn a_hit_discards_and_needs_no_tier() {
        let r = rules("[{paths: ['**'], tier: human}]", "{deny: ['*.pem']}");
        assert!(r.denied(Path::new("keys/a.pem")) && !r.denied(Path::new("a.pem.txt")));
        let hit = Hit {
            path: "keys/a.pem".into(),
            rule: Broken::Deny,
        };
        let v = r.review(paths(&["keys/a.pem"]), vec![hit]);
        assert_eq!((v.verdict, v.reviewers.len()), (Decision::Discard, 0));
        assert_eq!(v.reasons(), ["write.deny: keys/a.pem"]);
    }

    #[test]
    fn finds_content_across_chunk_boundaries() {
        let r = rules("[]", "{deny_content: ['BEGIN PRIVATE KEY', 'xyz']}");
        let mut data = vec![b'a'; 64 * 1024 - 5];
        data.extend_from_slice(b"BEGIN PRIVATE KEY----");
        assert_eq!(r.forbidden_content(&data[..]).unwrap(), Some("BEGIN PRIVATE KEY"));
        assert_eq!(r.forbidden_content(&b"plain text"[..]).unwrap(), None);
        assert_eq!(r.forbidden_content(&b"..xyz"[..]).unwrap(), Some("xyz"));
        assert!(
            Rules::new(
                &[],
                &[],
                &WriteRules {
                    deny_content: vec![String::new()],
                    ..Default::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn only_by_matches_the_binary_not_its_name() {
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        let r = rules("[]", "{only_by: [{paths: ['.git/**'], programs: ['/bin/sh']}]}");
        let m = std::fs::metadata(&sh).unwrap();
        let info = |program: &str, dev, ino| Info {
            id: 1,
            pid: 1,
            program: program.into(),
            dev,
            ino,
            args: vec![],
            parent: 0,
        };
        let ok = info("/elsewhere/sh", m.dev(), m.ino());
        let fake = info("/bin/sh", m.dev(), m.ino() + 1);
        assert_eq!(r.only_by(Path::new(".git/HEAD"), [&ok]), None);
        assert_eq!(r.only_by(Path::new("README"), [&fake]), None);
        assert_eq!(
            r.only_by(Path::new(".git/HEAD"), [&ok, &fake]),
            Some(Broken::OnlyBy(Some("/bin/sh".into())))
        );
        assert_eq!(r.only_by(Path::new(".git/HEAD"), []), Some(Broken::OnlyBy(None)));
        let p = crate::policy::Policy::parse("version: 1\nwrite: {only_by: [{paths: [a], programs: [/no/such]}]}\n")
            .unwrap();
        assert!(Rules::new(&[], &[], &p.write).is_err());
    }

    const DECLARED: &str = "{a: {}, b: {run: [/bin/sh], can_override: true}}";

    #[test]
    fn declared_reviewers_come_in_declaration_order_then_built_ins() {
        let r = rules_with(
            DECLARED,
            "[{paths: ['x/**'], reviewers: [b, a]}, {paths: ['y/**'], tier: human}]",
            "{}",
        )
        .unwrap();
        let v = r.review(paths(&["x/1"]), vec![]);
        assert_eq!(v.reviewers, names(&["a", "b"]));
        let v = r.review(paths(&["y/1", "x/1"]), vec![]);
        assert_eq!(v.reviewers, names(&["a", "b", "llm", "human"]));
        assert!(
            r.reviewer("b")
                .is_some_and(|b| b.can_override && matches!(b.kind, Kind::Command(_)))
        );
        assert!(
            r.reviewer("a")
                .is_some_and(|a| !a.can_override && a.kind == Kind::Socket)
        );
        assert!(r.reviewer("human").is_some_and(|h| h.can_override));
        assert!(r.reviewer("llm").is_some_and(|l| !l.can_override));
        assert!(r.reviewer("nobody").is_none());
    }

    #[test]
    fn a_command_reviewer_carries_its_run_settings() {
        let r = rules_with(
            "{c: {run: [sh, -c, x], timeout: 5s, on_failure: return, env: [K], delegate: true}}",
            "[]",
            "{}",
        )
        .unwrap();
        let Some(Reviewer {
            kind: Kind::Command(c), ..
        }) = r.reviewer("c")
        else {
            panic!("c is a command reviewer");
        };
        assert_eq!(c.argv, ["sh", "-c", "x"]);
        assert!(c.program.is_absolute() && c.program.ends_with("sh"));
        assert_eq!((c.timeout, c.on_failure), (Duration::from_secs(5), Decision::Return));
        assert!(c.delegate && c.env == ["K"]);
    }

    #[test]
    fn software_tier_and_empty_reviewers_need_nobody() {
        let r = rules(
            "[{paths: ['a/**'], tier: software, wait: required}, {paths: ['b/**'], reviewers: [], wait: required}, {paths: ['c/**'], tier: llm, wait: optional}]",
            "{}",
        );
        let v = r.review(paths(&["a/1", "b/1"]), vec![]);
        assert_eq!((v.reviewers.len(), v.wait), (0, false));
        let v = r.review(paths(&["a/1", "c/1"]), vec![]);
        assert_eq!((v.reviewers, v.wait), (names(&["llm"]), false));
    }

    #[test]
    fn review_rule_errors_name_the_rule() {
        let err = |review: &str| format!("{:#}", rules_with(DECLARED, review, "{}").err().unwrap());
        assert!(err("[{paths: [a]}]").contains("review[0]: give tier or reviewers"));
        assert!(
            err("[{paths: [a], tier: llm}, {paths: [a], tier: llm, reviewers: [a]}]")
                .contains("review[1]: give tier or reviewers")
        );
        assert!(err("[{paths: [a], reviewers: [a, ghost]}]").contains("review[0]: unknown reviewer ghost"));
        let bad = rules_with("{c: {run: [/no/such/prog]}}", "[]", "{}").err().unwrap();
        assert!(format!("{bad:#}").contains("reviewers.c: /no/such/prog"));
    }

    fn admit(
        r: &Rules,
        pending: &[&str],
        so_far: Decision,
        who: &str,
        verdict: Decision,
        (over, from_run): (bool, bool),
    ) -> Result<Admitted, Refused> {
        r.admit(&names(pending), so_far, who, verdict, over, from_run)
    }

    fn admit_rules() -> Rules {
        rules_with(
            "{a: {}, c: {run: [/bin/sh]}, d: {run: [/bin/sh], can_override: true}}",
            "[]",
            "{}",
        )
        .unwrap()
    }

    #[test]
    fn admit_refuses_a_reviewer_that_is_not_pending() {
        let r = admit_rules();
        let no = (false, false);
        assert_eq!(
            admit(&r, &["a"], Decision::Commit, "llm", Decision::Commit, no),
            Err(Refused::NotPending)
        );
        assert_eq!(
            admit(&r, &[], Decision::Commit, "a", Decision::Commit, no),
            Err(Refused::NotPending)
        );
    }

    #[test]
    fn admit_refuses_out_of_turn_without_override() {
        let r = admit_rules();
        let no = (false, false);
        assert_eq!(
            admit(&r, &["a", "llm"], Decision::Commit, "llm", Decision::Commit, no),
            Err(Refused::NotFirst("a".into()))
        );
    }

    #[test]
    fn admit_lets_an_overrider_answer_for_everyone_before_it() {
        let r = admit_rules();
        let got = admit(
            &r,
            &["a", "llm", "human"],
            Decision::Commit,
            "human",
            Decision::Commit,
            (false, false),
        )
        .unwrap();
        assert_eq!(
            got,
            Admitted {
                at: 2,
                verdict_after: Decision::Commit,
                looser: false,
                done: true
            }
        );
    }

    #[test]
    fn admit_continues_while_reviewers_are_pending_and_tightens() {
        let r = admit_rules();
        let got = admit(
            &r,
            &["a", "llm"],
            Decision::Commit,
            "a",
            Decision::Return,
            (false, false),
        )
        .unwrap();
        assert_eq!(
            got,
            Admitted {
                at: 0,
                verdict_after: Decision::Return,
                looser: false,
                done: false
            }
        );
        // A later verdict that is not looser keeps the tighter one.
        let got = admit(&r, &["llm"], Decision::Discard, "llm", Decision::Commit, (false, true)).unwrap();
        assert_eq!((got.verdict_after, got.looser), (Decision::Discard, false));
    }

    #[test]
    fn admit_stops_early_on_a_discard_nobody_later_can_override() {
        let r = admit_rules();
        let got = admit(
            &r,
            &["a", "llm"],
            Decision::Commit,
            "a",
            Decision::Discard,
            (false, false),
        )
        .unwrap();
        assert!(got.done && got.verdict_after == Decision::Discard);
    }

    #[test]
    fn admit_continues_after_a_discard_when_a_later_reviewer_can_override() {
        let r = admit_rules();
        let got = admit(
            &r,
            &["a", "llm", "human"],
            Decision::Commit,
            "a",
            Decision::Discard,
            (false, false),
        )
        .unwrap();
        assert!(!got.done && got.verdict_after == Decision::Discard);
        let got = admit(
            &r,
            &["a", "d"],
            Decision::Commit,
            "a",
            Decision::Discard,
            (false, false),
        )
        .unwrap();
        assert!(!got.done);
    }

    #[test]
    fn admit_refuses_a_looser_verdict_without_the_right_and_the_flag() {
        let r = admit_rules();
        // llm cannot loosen even with the flag; human needs the flag.
        assert_eq!(
            admit(
                &r,
                &["llm", "human"],
                Decision::Discard,
                "llm",
                Decision::Commit,
                (true, false)
            ),
            Err(Refused::Looser)
        );
        assert_eq!(
            admit(
                &r,
                &["human"],
                Decision::Discard,
                "human",
                Decision::Commit,
                (false, false)
            ),
            Err(Refused::Looser)
        );
        let got = admit(
            &r,
            &["human"],
            Decision::Discard,
            "human",
            Decision::Commit,
            (true, false),
        )
        .unwrap();
        assert_eq!(
            got,
            Admitted {
                at: 0,
                verdict_after: Decision::Commit,
                looser: true,
                done: true
            }
        );
    }

    #[test]
    fn admit_clamps_a_runs_looser_verdict_unless_its_reviewer_can_override() {
        let r = admit_rules();
        let run = (false, true);
        let got = admit(&r, &["c"], Decision::Return, "c", Decision::Commit, run).unwrap();
        assert_eq!((got.verdict_after, got.looser), (Decision::Return, false));
        let got = admit(&r, &["d"], Decision::Return, "d", Decision::Commit, run).unwrap();
        assert_eq!((got.verdict_after, got.looser), (Decision::Commit, true));
    }
}

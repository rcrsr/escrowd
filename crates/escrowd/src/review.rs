//! Close-time review: the software tier's write rules (policy `write:`) and the tiers
//! above it a change set needs (policy `review:`).
//!
//! A change set that breaks a write rule is discarded: the software tier's verdict
//! is final at close, and a client's commit or return turns into a discard (verdicts
//! only tighten). Otherwise each path gets the tier of the first `review:` rule it
//! matches (software if none), and the change set needs every tier from the cheapest
//! above software up to the highest any of its paths needs; a rename counts both its
//! paths. Without `review:` rules nothing needs more than software.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::bail;
use memchr::memmem::Finder;

use crate::gate::Globs;
use crate::policy::{ReviewRule, Tier, Wait, WriteRules};

pub struct Rules {
    review: Vec<(Globs, Tier, Wait)>,
    deny: Globs,
    /// Each forbidden byte string, with its pattern for the reason.
    deny_content: Vec<(String, Finder<'static>)>,
}

/// The software tier's verdict on a change set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Commit,
    Discard,
}

/// A write rule a change broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// As the change set shows it.
    pub path: PathBuf,
    /// The forbidden byte string, for `write.deny_content`; None for `write.deny`.
    pub content: Option<String>,
}

impl Hit {
    pub fn reason(&self) -> String {
        match &self.content {
            None => format!("write.deny: {}", self.path.display()),
            Some(c) => format!("write.deny_content: {} contains {c:?}", self.path.display()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    pub verdict: Verdict,
    /// The write rules broken; empty when the verdict is commit.
    pub hits: Vec<Hit>,
    /// The tiers above software the change set needs, cheapest first; empty after a discard.
    pub tiers: Vec<Tier>,
    /// A path that needs a tier above software has a rule that requires the agent to wait.
    pub wait: bool,
}

impl Review {
    pub fn reasons(&self) -> Vec<String> {
        self.hits.iter().map(Hit::reason).collect()
    }
}

impl Rules {
    pub fn new(review: &[ReviewRule], write: &WriteRules) -> anyhow::Result<Self> {
        let mut rules = Vec::new();
        for (i, r) in review.iter().enumerate() {
            if r.paths.is_empty() {
                bail!("review[{i}]: no paths");
            }
            rules.push((Globs::new(&r.paths)?, r.tier, r.wait));
        }
        let mut deny_content = Vec::new();
        for c in &write.deny_content {
            if c.is_empty() {
                bail!("write.deny_content: an empty string matches every file");
            }
            deny_content.push((c.clone(), Finder::new(c.as_bytes()).into_owned()));
        }
        Ok(Rules {
            review: rules,
            deny: Globs::new(&write.deny)?,
            deny_content,
        })
    }

    /// Whether any rule could change a change set's verdict or tiers.
    pub fn is_empty(&self) -> bool {
        self.review.is_empty() && self.deny.is_empty() && self.deny_content.is_empty()
    }

    /// The tier `path` needs and whether its rule requires waiting.
    fn tier(&self, path: &Path) -> (Tier, Wait) {
        self.review
            .iter()
            .find(|(g, _, _)| g.is_match(path))
            .map_or((Tier::Software, Wait::Optional), |(_, t, w)| (*t, *w))
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
                verdict: Verdict::Discard,
                hits,
                tiers: Vec::new(),
                wait: false,
            };
        }
        let (mut top, mut wait) = (Tier::Software, false);
        for p in paths {
            let (tier, w) = self.tier(p);
            top = top.max(tier);
            wait |= tier > Tier::Software && w == Wait::Required;
        }
        let tiers = [Tier::Llm, Tier::Human].into_iter().filter(|t| *t <= top).collect();
        Review {
            verdict: Verdict::Commit,
            hits,
            tiers,
            wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(review: &str, write: &str) -> Rules {
        let p = crate::policy::Policy::parse(&format!("version: 1\nreview: {review}\nwrite: {write}\n")).unwrap();
        Rules::new(&p.review, &p.write).unwrap()
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
        assert_eq!((v.verdict, v.tiers.clone(), v.wait), (Verdict::Commit, vec![], false));
        let v = r.review(paths(&["src/util.py"]), vec![]);
        assert_eq!((v.tiers.clone(), v.wait), (vec![Tier::Llm], false));
        // First match wins: src/auth/ is human (wait required) although src/** matches too.
        let v = r.review(paths(&["src/util.py", "src/auth/login.py"]), vec![]);
        assert_eq!((v.tiers, v.wait), (vec![Tier::Llm, Tier::Human], true));
    }

    #[test]
    fn a_hit_discards_and_needs_no_tier() {
        let r = rules("[{paths: ['**'], tier: human}]", "{deny: ['*.pem']}");
        assert!(r.denied(Path::new("keys/a.pem")) && !r.denied(Path::new("a.pem.txt")));
        let hit = Hit {
            path: "keys/a.pem".into(),
            content: None,
        };
        let v = r.review(paths(&["keys/a.pem"]), vec![hit]);
        assert_eq!((v.verdict, v.tiers.len()), (Verdict::Discard, 0));
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
                &WriteRules {
                    deny: vec![],
                    deny_content: vec![String::new()],
                }
            )
            .is_err()
        );
    }
}

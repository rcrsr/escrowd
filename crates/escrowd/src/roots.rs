//! Path roots: the project, `$HOME` and `/tmp`, each served through the same FUSE
//! layer with a rule per path (policy `roots:`).
//!
//! A scope has one view per served root: `<mount>/<id>/` (the project),
//! `<mount>/<id>.home/` and `<mount>/<id>.tmp/`. Every view of a scope opens,
//! closes and is decided together; a commit applies all of them in one journaled
//! generation. Inside a root, the longest listed path decides:
//!
//! - `capture`: staged in the scope, decided and committed with it;
//! - `ephemeral`: staged in the scope, never in its change set (always discarded);
//! - `passthrough`: bound directly into sandboxes, outside FUSE; the view shows a
//!   mount point (a stub) and serves nothing under it;
//! - `deny`: lookups pass, reads, listings and changes get EACCES (logged).
//!
//! The project, passthrough and read paths inside a root are stubs: sandboxes bind
//! them over the view. escrowd's own state, views and sockets are hidden.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const PROJECT: usize = 0;
pub const HOME: usize = 1;
pub const TMP: usize = 2;
pub const COUNT: usize = 3;

/// The view name suffix of each root.
const SUFFIX: [&str; COUNT] = ["", ".home", ".tmp"];

/// The view (top-level name in the mount) of root `root` of scope `group`.
pub fn view_name(group: &str, root: usize) -> String {
    format!("{group}{}", SUFFIX[root])
}

/// (scope, root) of a view name.
pub fn split_view(view: &str) -> (&str, usize) {
    for root in [HOME, TMP] {
        if let Some(group) = view.strip_suffix(SUFFIX[root]) {
            return (group, root);
        }
    }
    (view, PROJECT)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rule {
    Capture,
    Ephemeral,
    Passthrough,
    Deny,
}

/// What a view does at a path: the rule, or escrowd's own masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Capture,
    Ephemeral,
    Deny,
    /// A mount point for a direct bind (the project, a passthrough or read path):
    /// visible with the base's attributes, nothing served under it, never changed.
    Stub,
    /// Absent: escrowd's state, views and sockets, and anything under a stub.
    Hidden,
}

/// The rules of one root, with paths relative to it.
#[derive(Debug)]
pub struct Rules {
    default: Rule,
    /// Longest first.
    paths: Vec<(PathBuf, Rule)>,
    stubs: Vec<PathBuf>,
    hidden: Vec<PathBuf>,
}

impl Rules {
    /// `paths`: (relative path, rule) for capture, ephemeral and deny; `stubs` and
    /// `hidden` relative to the root too.
    pub fn new(default: Rule, mut paths: Vec<(PathBuf, Rule)>, stubs: Vec<PathBuf>, hidden: Vec<PathBuf>) -> Self {
        paths.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        Rules {
            default,
            paths,
            stubs,
            hidden,
        }
    }

    /// Some path can be captured (else the change set is empty).
    pub fn captures(&self) -> bool {
        self.default == Rule::Capture || self.paths.iter().any(|(_, r)| *r == Rule::Capture)
    }

    pub fn access(&self, rel: &Path) -> Access {
        if self.hidden.iter().any(|h| rel.starts_with(h)) {
            return Access::Hidden;
        }
        for s in &self.stubs {
            if rel == s {
                return Access::Stub;
            }
            if rel.starts_with(s) {
                return Access::Hidden;
            }
        }
        let rule = self
            .paths
            .iter()
            .find(|(p, _)| rel.starts_with(p))
            .map(|(_, r)| *r)
            .unwrap_or(self.default);
        match rule {
            Rule::Capture => Access::Capture,
            Rule::Ephemeral => Access::Ephemeral,
            Rule::Deny => Access::Deny,
            Rule::Passthrough => Access::Stub,
        }
    }
}

/// `p` relative to `root`, if it is at or under it.
pub fn under<'a>(p: &'a Path, root: &Path) -> Option<&'a Path> {
    p.strip_prefix(root).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_names_round_trip() {
        for root in [PROJECT, HOME, TMP] {
            assert_eq!(split_view(&view_name("s12", root)), ("s12", root));
        }
        assert_eq!(split_view("unscoped.home"), ("unscoped", HOME));
    }

    #[test]
    fn longest_path_wins_and_masks_come_first() {
        let p = |s: &str| PathBuf::from(s);
        let r = Rules::new(
            Rule::Capture,
            vec![
                (p(".ssh"), Rule::Deny),
                (p(".ssh/known_hosts"), Rule::Capture),
                (p(".bash_history"), Rule::Ephemeral),
            ],
            vec![p("src/proj"), p(".npm")],
            vec![p(".local/state/escrowd")],
        );
        assert_eq!(r.access(Path::new(".gitconfig")), Access::Capture);
        assert_eq!(r.access(Path::new(".ssh/id_ed25519")), Access::Deny);
        assert_eq!(r.access(Path::new(".ssh/known_hosts")), Access::Capture);
        assert_eq!(r.access(Path::new(".bash_history")), Access::Ephemeral);
        assert_eq!(r.access(Path::new("src/proj")), Access::Stub);
        assert_eq!(r.access(Path::new("src/proj/a.txt")), Access::Hidden);
        assert_eq!(r.access(Path::new("src")), Access::Capture);
        assert_eq!(r.access(Path::new(".local/state/escrowd/x")), Access::Hidden);
        assert_eq!(r.access(Path::new(".local/state")), Access::Capture);
    }
}

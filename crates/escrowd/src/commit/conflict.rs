//! The conflict check of a commit: every path it touches must still be as the
//! scope first saw it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use super::{Commits, Part};
use crate::roots;
use crate::snapshot::Base;
use crate::sys::{self, Version};

/// The version `want` the scope recorded is `have` now, or a rollback replaced
/// it by copies that lead to `have` (at most 16 links).
pub(super) fn same_version(
    aliases: &HashMap<(usize, PathBuf, Version), Version>,
    root: usize,
    p: &Path,
    want: Option<Version>,
    have: Option<Version>,
) -> bool {
    match (want, have) {
        (None, None) => true,
        (Some(mut w), Some(have)) => {
            // A rollback replaced the original by a copy; follow the chain of copies.
            for _ in 0..16 {
                if w == have {
                    return true;
                }
                match aliases.get(&(root, p.to_path_buf(), w)) {
                    Some(next) => w = *next,
                    None => return false,
                }
            }
            false
        }
        _ => false,
    }
}

impl Commits {
    pub(super) fn conflicts(&self, base: &Base, part: &Part, reads: bool) -> io::Result<Vec<PathBuf>> {
        let (lower, h, plan, root) = (part.lower, part.h, &part.plan, part.root);
        let touched = plan.touched();
        let removes: HashSet<&PathBuf> = plan.removes.iter().collect();
        let aliases = self.aliases.lock();
        let same = |p: &Path, want: Option<Version>, have: Option<Version>| same_version(&aliases, root, p, want, have);
        let store = h.store_read();
        let mut out = BTreeSet::new();
        if reads {
            // Files read from the base (never from the upper: those are the scope's own),
            // kept only where they would commit: an ephemeral read changes nothing.
            for (p, _) in store.reads().into_iter().filter(|(_, allowed)| *allowed) {
                if touched.contains(&p) || h.access(&p) != roots::Access::Capture {
                    continue;
                }
                let live = sys::lstat(lower, &p).ok();
                if !same(&p, store.base_version(&p)?, live.as_ref().map(Version::of)) {
                    out.insert(p);
                }
            }
        }
        for p in &touched {
            let want = match store.base_version(p)? {
                Some(v) => Some(v),
                None => base.lstat(p).ok().map(|st| Version::of(&st)),
            };
            let live = sys::lstat(lower, p).ok();
            if !same(p, want, live.as_ref().map(Version::of)) {
                out.insert(p.clone());
                continue;
            }
            // A directory the scope deletes must not have gained entries.
            if removes.contains(p) && live.as_ref().is_some_and(sys::is_dir) {
                for (name, _) in sys::read_dir(lower, p)? {
                    if !touched.contains(&p.join(&name)) {
                        out.insert(p.clone());
                    }
                }
            }
        }
        for (p, _) in &plan.puts {
            for a in p.ancestors().skip(1) {
                if a.as_os_str().is_empty() || touched.contains(a) {
                    break;
                }
                if !sys::lstat(lower, a).is_ok_and(|st| sys::is_dir(&st)) {
                    out.insert(p.clone());
                    break;
                }
            }
        }
        Ok(out.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: i64) -> Version {
        Version {
            ino: n as u64,
            size: n,
            mtime_ns: n,
            ctime_ns: n,
        }
    }

    fn chain(links: i64) -> HashMap<(usize, PathBuf, Version), Version> {
        (0..links).map(|i| ((0, PathBuf::from("f"), v(i)), v(i + 1))).collect()
    }

    #[test]
    fn a_two_link_alias_chain_matches() {
        let aliases = chain(2);
        assert!(same_version(&aliases, 0, Path::new("f"), Some(v(0)), Some(v(2))));
    }

    #[test]
    fn a_chain_longer_than_sixteen_links_does_not_match() {
        let aliases = chain(17);
        assert!(!same_version(&aliases, 0, Path::new("f"), Some(v(0)), Some(v(17))));
    }

    #[test]
    fn an_alias_cycle_does_not_match() {
        let aliases: HashMap<_, _> = [
            ((0, PathBuf::from("f"), v(1)), v(2)),
            ((0, PathBuf::from("f"), v(2)), v(1)),
        ]
        .into();
        assert!(!same_version(&aliases, 0, Path::new("f"), Some(v(1)), Some(v(3))));
    }
}

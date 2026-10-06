//! The read gate: software rules decided synchronously while the caller waits.
//! Deny globs come from the policy file's `read.deny`.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Path globs as the policy writes them: a pattern without a `/` matches the file
/// name at any depth, one with a `/` the whole path (a leading `/` is ignored on
/// both sides, so `/tmp/x` matches `tmp/**`).
pub struct Globs {
    names: GlobSet,
    paths: GlobSet,
}

impl Globs {
    pub fn new(patterns: &[String]) -> Result<Self, globset::Error> {
        let (mut names, mut paths) = (GlobSetBuilder::new(), GlobSetBuilder::new());
        for p in patterns {
            let glob = Glob::new(p.trim_start_matches('/'))?;
            if p.contains('/') {
                paths.add(glob)
            } else {
                names.add(glob)
            };
        }
        Ok(Globs {
            names: names.build()?,
            paths: paths.build()?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.paths.is_empty()
    }

    pub fn is_match(&self, path: &Path) -> bool {
        let rel = path.strip_prefix("/").unwrap_or(path);
        rel.file_name().is_some_and(|n| self.names.is_match(n)) || self.paths.is_match(rel)
    }
}

pub struct Gate {
    deny: Globs,
}

impl Gate {
    pub fn new(deny_read: &[String]) -> Result<Self, globset::Error> {
        Ok(Gate {
            deny: Globs::new(deny_read)?,
        })
    }

    pub fn read_allowed(&self, rel: &Path) -> bool {
        !self.deny.is_match(rel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_at_any_depth_and_paths_whole() {
        let g = Globs::new(&["*.pem".into(), "src/auth/**".into(), "/tmp/x/**".into()]).unwrap();
        assert!(g.is_match(Path::new("a/b/key.pem")));
        assert!(g.is_match(Path::new("src/auth/login.py")));
        assert!(!g.is_match(Path::new("lib/src/auth/login.py")));
        assert!(g.is_match(Path::new("/tmp/x/y")));
        assert!(!g.is_match(Path::new("src/util.py")));
    }
}

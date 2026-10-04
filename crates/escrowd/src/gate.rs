//! The read gate: software rules decided synchronously while the caller waits.
//! Deny globs come from the policy file's `read.deny`.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

pub struct Gate {
    /// Patterns without a `/` match the file name at any depth.
    names: GlobSet,
    /// Patterns with a `/` match the project-relative path.
    paths: GlobSet,
}

impl Gate {
    pub fn new(deny_read: &[String]) -> Result<Self, globset::Error> {
        let (mut names, mut paths) = (GlobSetBuilder::new(), GlobSetBuilder::new());
        for p in deny_read {
            let glob = Glob::new(p.trim_start_matches('/'))?;
            if p.contains('/') {
                paths.add(glob)
            } else {
                names.add(glob)
            };
        }
        Ok(Gate {
            names: names.build()?,
            paths: paths.build()?,
        })
    }

    pub fn read_allowed(&self, rel: &Path) -> bool {
        let name_denied = rel.file_name().is_some_and(|n| self.names.is_match(n));
        !(name_denied || self.paths.is_match(rel))
    }
}

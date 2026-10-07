//! The inode table: path to inode and back per scope, pinned and upper-only
//! numbers, and what the kernel may keep of each inode's cached pages.

use super::*;

/// One view's inode table: path to inode and back, the kernel's lookup count of
/// each inode, and what the kernel may keep of each inode's cached pages. Each view
/// has its own, so dropping a scope drops its tables whole.
#[derive(Default)]
pub(super) struct Inodes {
    pub(super) paths: HashMap<u64, PathBuf>,
    pub(super) inos: HashMap<PathBuf, u64>,
    /// How many times the kernel has looked each inode up; FUSE forget lowers it, and
    /// at 0 the entry goes (its number is derived or pinned, so it comes back the same).
    lookups: HashMap<u64, u64>,
    /// The base version whose pages the kernel may cache for each inode opened from the base.
    pub(super) cached: HashMap<u64, Version>,
    /// Inodes copied up from a base version other than the cached one: the next open drops the pages.
    pub(super) stale: std::collections::HashSet<u64>,
}

impl Inodes {
    /// Entries at or under `from` now live under `to`; returns (new path, ino) of each.
    /// Only a directory has entries under it to look for.
    pub(super) fn rekey(&mut self, from: &Path, to: &Path, dir: bool) -> Vec<(PathBuf, u64)> {
        if let Some(ino) = self.inos.remove(to) {
            self.paths.remove(&ino);
        }
        let old: Vec<(PathBuf, u64)> = if dir {
            self.inos
                .iter()
                .filter(|(p, _)| p.starts_with(from))
                .map(|(p, &i)| (p.clone(), i))
                .collect()
        } else {
            self.inos
                .get(from)
                .map(|&i| (from.to_path_buf(), i))
                .into_iter()
                .collect()
        };
        let mut out = Vec::with_capacity(old.len());
        for (p, ino) in old {
            let new = moved(&p, from, to).expect("filtered on prefix");
            self.inos.remove(&p);
            self.inos.insert(new.clone(), ino);
            self.paths.insert(ino, new.clone());
            out.push((new, ino));
        }
        out
    }

    /// `rel` no longer exists; its inode lives on under another hard link, if any
    /// (`linked`: it had more than one).
    pub(super) fn forget_path(&mut self, rel: &Path, linked: bool) {
        if let Some(ino) = self.inos.remove(rel)
            && self.paths.get(&ino).is_some_and(|p| p == rel)
        {
            let other = || self.inos.iter().find(|(_, i)| **i == ino).map(|(p, _)| p.clone());
            match linked.then(other).flatten() {
                Some(other) => self.paths.insert(ino, other),
                None => self.paths.remove(&ino),
            };
        }
    }

    /// The kernel took one more reference to `ino`, found at `rel`.
    pub(super) fn looked_up(&mut self, rel: &Path, ino: u64) {
        if self.inos.get(rel) != Some(&ino) {
            self.inos.insert(rel.to_path_buf(), ino);
            self.paths.insert(ino, rel.to_path_buf());
        }
        *self.lookups.entry(ino).or_default() += 1;
    }

    /// The kernel dropped `n` references to `ino`; at none, forget the inode under
    /// every name. An inode the kernel holds open has references, so no open file loses
    /// its entry.
    pub(super) fn forget(&mut self, ino: u64, n: u64) {
        let Some(count) = self.lookups.get_mut(&ino) else {
            return;
        };
        *count = count.saturating_sub(n);
        if *count > 0 {
            return;
        }
        self.lookups.remove(&ino);
        self.paths.remove(&ino);
        self.inos.retain(|_, i| *i != ino);
        self.cached.remove(&ino);
        self.stale.remove(&ino);
    }

    /// What the kernel may do with the pages it caches for `ino`, opened at `loc`
    /// (`st`: the base entry when it comes from the base). Files in the upper change
    /// only through the kernel; a base file keeps its pages while its version is
    /// unchanged (an editor outside escrowd changes it).
    pub(super) fn pages(&mut self, ino: u64, loc: Loc, st: &Stat) -> Pages {
        let fresh = !self.stale.remove(&ino);
        match loc {
            Loc::Upper if fresh => Pages::Keep,
            Loc::Upper => Pages::Drop,
            Loc::Lower => {
                let v = Version::of(st);
                match self.cached.insert(ino, v) {
                    _ if !fresh => Pages::Drop,
                    Some(before) if before == v => Pages::Keep,
                    Some(_) => Pages::Drop,
                    None => Pages::Empty(st.st_size as u64),
                }
            }
        }
    }

    /// The top-level names the kernel may have cached.
    pub(super) fn top_names(&self) -> Vec<OsString> {
        self.inos
            .keys()
            .filter(|p| p.components().count() == 1)
            .map(|p| p.as_os_str().to_os_string())
            .collect()
    }
}

/// What the kernel may do with its cached pages of a file being opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pages {
    /// Drop them: the base file changed since they were cached.
    Drop,
    /// Keep them.
    Keep,
    /// None are cached (a first read-only open of a base file of this size): the
    /// daemon may store them ahead of the reads, then let the kernel keep them.
    Empty(u64),
}

pub enum Node {
    Root,
    In(Arc<ScopeHandle>, PathBuf),
}

impl Views {
    // ---- lookups ----

    pub fn scope(&self, id: &str) -> R<Arc<ScopeHandle>> {
        self.scopes.read().get(id).cloned().ok_or(Errno::ENOENT)
    }

    pub fn node(&self, ino: u64) -> R<Node> {
        if ino == ROOT {
            return Ok(Node::Root);
        }
        if (UNSCOPED_ROOT..UNSCOPED_ROOT + roots::COUNT as u64).contains(&ino) {
            let view = roots::view_name(UNSCOPED, (ino - UNSCOPED_ROOT) as usize);
            return Ok(Node::In(self.scope(&view)?, PathBuf::new()));
        }
        let h = self.view_of(ino).ok_or(Errno::ENOENT)?;
        let p = h.inodes.lock().paths.get(&ino).cloned().ok_or(Errno::ENOENT)?;
        Ok(Node::In(h, p))
    }

    /// The view an inode number belongs to (its scope index), if it is still served.
    fn view_of(&self, ino: u64) -> Option<Arc<ScopeHandle>> {
        self.by_idx.read().get(&(ino >> SCOPE_SHIFT)).cloned()
    }

    /// The kernel took a reference to `ino`, `rel` in `h` (an entry or create reply,
    /// or a READDIRPLUS entry other than `.` and `..`): from now on `node` finds it.
    /// The unscoped roots' fixed numbers need no entry.
    pub fn looked_up(&self, h: &ScopeHandle, rel: &Path, ino: u64) {
        if ino >> SCOPE_SHIFT == h.idx {
            h.inodes.lock().looked_up(rel, ino);
        }
    }

    /// FUSE forget: the kernel dropped `n` references to `ino`.
    pub fn forget(&self, ino: u64, n: u64) {
        if let Some(h) = self.view_of(ino) {
            h.inodes.lock().forget(ino, n);
        }
    }

    /// The scope and path of an entry inside a scope; the mount root itself holds no files.
    pub fn key(&self, ino: u64) -> R<(Arc<ScopeHandle>, PathBuf)> {
        match self.node(ino)? {
            Node::Root => Err(Errno::EPERM),
            Node::In(h, p) => Ok((h, p)),
        }
    }

    pub fn child(&self, parent: u64, name: &OsStr) -> R<(Arc<ScopeHandle>, PathBuf)> {
        let (h, p) = self.key(parent)?;
        Ok((h, p.join(name)))
    }
    /// The scope's inode for `rel`: a pinned number, else the base st_ino, else a new
    /// upper number (pinned). The table learns it when the kernel looks it up.
    pub fn ino_for(&self, h: &ScopeHandle, rel: &Path) -> R<u64> {
        if h.group == UNSCOPED && rel.as_os_str().is_empty() {
            return Ok(UNSCOPED_ROOT + h.root as u64);
        }
        if let Some(&ino) = h.inodes.lock().inos.get(rel) {
            return Ok(ino);
        }
        let prefix = h.idx << SCOPE_SHIFT;
        let pinned = h.store_read().pin(rel);
        let ino = match pinned {
            Some(ino) => ino,
            None => match self.lower_stat(h, rel).map(|st| st.st_ino) {
                Some(i) if i < UPPER_BIT => prefix | i,
                _ => {
                    // Under the store's lock: two lookups of a new path pin one number.
                    let mut store = h.store();
                    match store.pin(rel) {
                        Some(ino) => ino,
                        None => {
                            let ino = prefix | UPPER_BIT | store.alloc_upper_ino();
                            store.set_pin(rel, ino).map_err(errno)?;
                            ino
                        }
                    }
                }
            },
        };
        Ok(ino)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inode_stays_while_the_kernel_holds_it_under_any_name() {
        let mut t = Inodes::default();
        let (a, b) = (Path::new("a"), Path::new("d/b"));
        t.looked_up(a, 7);
        t.looked_up(a, 7);
        t.looked_up(b, 7); // a hard link: one inode, two names
        t.forget(7, 2);
        assert_eq!(t.paths.get(&7).map(PathBuf::as_path), Some(b));
        t.forget(7, 1);
        assert!(t.paths.is_empty() && t.inos.is_empty() && t.lookups.is_empty());
        t.forget(7, 1); // a forget for an inode already gone changes nothing
        assert!(t.lookups.is_empty());
    }

    #[test]
    fn a_rename_moves_every_entry_under_a_directory() {
        let mut t = Inodes::default();
        t.looked_up(Path::new("d"), 1);
        t.looked_up(Path::new("d/x"), 2);
        t.looked_up(Path::new("e"), 3);
        let moved = t.rekey(Path::new("d"), Path::new("e"), true);
        assert_eq!(moved.len(), 2);
        assert_eq!(t.inos.get(Path::new("e/x")), Some(&2));
        assert!(!t.paths.values().any(|p| p == Path::new("d")));
        t.forget(3, 1); // the replaced `e` was dropped from the table by the rename
        assert_eq!(t.paths.get(&1).map(PathBuf::as_path), Some(Path::new("e")));
    }
}

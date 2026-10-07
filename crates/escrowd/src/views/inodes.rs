//! The inode table: path to inode and back per scope, pinned and upper-only
//! numbers, and what the kernel may keep of each inode's cached pages.

use super::*;

pub(super) type Key = (String, PathBuf);

#[derive(Default)]
pub(super) struct Tables {
    pub(super) paths: HashMap<u64, Key>,
    pub(super) inos: HashMap<Key, u64>,
    pub(super) files: HashMap<u64, (Arc<ScopeHandle>, Arc<File>)>,
    /// The process that opened each file handle.
    pub(super) openers: HashMap<u64, u32>,
    pub(super) next_fh: u64,
    /// The base version whose pages the kernel may cache for each inode opened from the base.
    pub(super) cached: HashMap<u64, Version>,
    /// Inodes copied up from a base version other than the cached one: the next open drops the pages.
    pub(super) stale: std::collections::HashSet<u64>,
}

impl Tables {
    /// Entries at or under `from` now live under `to`; returns (new path, ino) of each.
    /// Only a directory has entries under it to look for.
    pub(super) fn rekey(&mut self, scope: &str, from: &Path, to: &Path, dir: bool) -> Vec<(PathBuf, u64)> {
        if let Some(ino) = self.inos.remove(&(scope.to_string(), to.to_path_buf())) {
            self.paths.remove(&ino);
        }
        let old: Vec<(PathBuf, u64)> = if dir {
            self.inos
                .iter()
                .filter(|((s, p), _)| s == scope && p.starts_with(from))
                .map(|((_, p), &i)| (p.clone(), i))
                .collect()
        } else {
            let key = (scope.to_string(), from.to_path_buf());
            self.inos.get(&key).map(|&i| (key.1, i)).into_iter().collect()
        };
        let mut out = Vec::with_capacity(old.len());
        for (p, ino) in old {
            let new = moved(&p, from, to).expect("filtered on prefix");
            self.inos.remove(&(scope.to_string(), p));
            self.inos.insert((scope.to_string(), new.clone()), ino);
            self.paths.insert(ino, (scope.to_string(), new.clone()));
            out.push((new, ino));
        }
        out
    }

    /// `rel` no longer exists; its inode lives on under another hard link, if any
    /// (`linked`: it had more than one).
    pub(super) fn forget_path(&mut self, scope: &str, rel: &Path, linked: bool) {
        let key = (scope.to_string(), rel.to_path_buf());
        if let Some(ino) = self.inos.remove(&key)
            && self.paths.get(&ino) == Some(&key)
        {
            let other = || self.inos.iter().find(|(_, i)| **i == ino).map(|(k, _)| k.clone());
            match linked.then(other).flatten() {
                Some(other) => self.paths.insert(ino, other),
                None => self.paths.remove(&ino),
            };
        }
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

    /// Forget a scope's entries; returns the top-level names the kernel may have cached.
    pub(super) fn forget_scope(&mut self, scope: &str, idx: u64) -> Vec<OsString> {
        let top = self
            .inos
            .keys()
            .filter(|(s, p)| s == scope && p.components().count() == 1)
            .map(|(_, p)| p.as_os_str().to_os_string())
            .collect();
        self.inos.retain(|(s, _), _| s != scope);
        self.paths.retain(|_, (s, _)| s != scope);
        self.cached.retain(|ino, _| ino >> SCOPE_SHIFT != idx);
        self.stale.retain(|ino| ino >> SCOPE_SHIFT != idx);
        top
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

    pub(super) fn t(&self) -> MutexGuard<'_, Tables> {
        self.t.lock().unwrap()
    }

    pub fn scope(&self, id: &str) -> R<Arc<ScopeHandle>> {
        self.scopes.read().unwrap().get(id).cloned().ok_or(Errno::ENOENT)
    }

    pub fn node(&self, ino: u64) -> R<Node> {
        if ino == ROOT {
            return Ok(Node::Root);
        }
        if (UNSCOPED_ROOT..UNSCOPED_ROOT + roots::COUNT as u64).contains(&ino) {
            let view = roots::view_name(UNSCOPED, (ino - UNSCOPED_ROOT) as usize);
            return Ok(Node::In(self.scope(&view)?, PathBuf::new()));
        }
        let (s, p) = self.t().paths.get(&ino).cloned().ok_or(Errno::ENOENT)?;
        Ok(Node::In(self.scope(&s)?, p))
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
    /// The scope's inode for `rel`: a pinned number, else the base st_ino, else a new upper number.
    pub fn ino_for(&self, h: &ScopeHandle, rel: &Path) -> R<u64> {
        if h.group == UNSCOPED && rel.as_os_str().is_empty() {
            return Ok(UNSCOPED_ROOT + h.root as u64);
        }
        let key = (h.id.clone(), rel.to_path_buf());
        if let Some(&ino) = self.t().inos.get(&key) {
            return Ok(ino);
        }
        let prefix = h.idx << SCOPE_SHIFT;
        let pinned = h.store_read().pin(rel);
        let ino = match pinned {
            Some(ino) => ino,
            None => match self.lower_stat(h, rel).map(|st| st.st_ino) {
                Some(i) if i < UPPER_BIT => prefix | i,
                _ => {
                    let mut store = h.store();
                    let ino = prefix | UPPER_BIT | store.alloc_upper_ino();
                    store.set_pin(rel, ino).map_err(errno)?;
                    ino
                }
            },
        };
        let mut t = self.t();
        if let Some(&existing) = t.inos.get(&key) {
            return Ok(existing);
        }
        t.paths.insert(ino, key.clone());
        t.inos.insert(key, ino);
        Ok(ino)
    }
}

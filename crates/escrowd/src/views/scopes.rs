//! The scope lifecycle: open, close (the change set and its review), commit,
//! return and discard, as the decision layer drives them.

use super::*;

/// `rel` in `dir` opened for reading if it is a regular file; None for anything else.
pub(super) fn regular_file(dir: std::os::fd::BorrowedFd, rel: &Path) -> io::Result<Option<File>> {
    let f = match sys::open(dir, rel, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK, 0) {
        Ok(f) => f,
        Err(e) if matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOENT | libc::ENXIO)) => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(f.metadata()?.is_file().then_some(f))
}

impl Views {
    // ---- scope lifecycle (RPC) ----

    /// Create a scope with a view per served root; returns its id, its project view in
    /// the mount and its token.
    pub fn open_scope(&self, name: &str, labels: &HashMap<String, String>, session: &str) -> error::Result<Opened> {
        let since = self.commits.current();
        let mut bytes = [0u8; 32];
        sys::random(&mut bytes)?;
        let token = diff::hex(&bytes);
        let hash = token_sha256(&token);
        let mut id = String::new();
        for root in self.served().collect::<Vec<_>>() {
            let idx = self.alloc_idx()?;
            if root == PROJECT {
                id = format!("s{idx}");
            }
            let view = roots::view_name(&id, root);
            let (labels, token, session) = if root == PROJECT {
                (labels, Some(hash.as_str()), session)
            } else {
                (&HashMap::new(), None, "")
            };
            let store = ScopeStore::create(&self.scopes_dir, &view, name, idx, since, false, labels, token, session)?;
            self.attach(store)?;
        }
        self.ledger.append(&id, "open", Path::new(""), None, "allow");
        Ok(Opened {
            root: self.mount.join(&id),
            id,
            token,
        })
    }

    /// `token` is scope `id`'s (`Denied` if not; `NoScope` if no such scope).
    /// The unscoped scope has none and takes any.
    pub fn check_token(&self, id: &str, token: &str) -> error::Result<()> {
        let hs = self.handles(id)?;
        if hs[0].store_read().token_matches(&token_sha256(token)) {
            return Ok(());
        }
        self.ledger.append(id, "token", Path::new(""), None, "deny");
        Err(Error::Denied(format!("scope {id}: missing or wrong token")))
    }

    /// The served roots other than the project, with scope `id`'s view of each in the
    /// mount; none if the scope does not exist (the unscoped scope in passthrough mode).
    pub fn root_views(&self, id: &str) -> Vec<RootView> {
        if !self.scopes.read().unwrap().contains_key(id) {
            return Vec::new();
        }
        self.served()
            .filter(|r| *r != PROJECT)
            .map(|r| RootView {
                host: self.roots[r].host.clone(),
                view: self.mount.join(roots::view_name(id, r)),
                direct: self.roots[r].direct.clone(),
            })
            .collect()
    }

    /// Every view of scope `id`, the project's first.
    pub(super) fn handles(&self, id: &str) -> error::Result<Vec<Arc<ScopeHandle>>> {
        let scopes = self.scopes.read().unwrap();
        let hs: Vec<Arc<ScopeHandle>> = self
            .served()
            .filter_map(|r| scopes.get(&roots::view_name(id, r)).cloned())
            .collect();
        if hs.first().is_none_or(|h| h.root != PROJECT) {
            return Err(Error::no_scope(id));
        }
        Ok(hs)
    }

    /// The change set of every view of a scope.
    pub(super) fn change_set(&self, hs: &[Arc<ScopeHandle>]) -> io::Result<ChangeSet> {
        let mut out = ChangeSet {
            changes: Vec::new(),
            reads: Vec::new(),
            labels: HashMap::new(),
            procs: BTreeMap::new(),
            unscoped: 0,
            review: review::Review::default(),
        };
        for h in hs {
            let cs = changeset::build(&self.base(h), h)?;
            if h.root == PROJECT {
                out.labels = cs.labels;
            }
            out.changes.extend(cs.changes);
            out.procs.extend(cs.procs);
            out.reads.extend(
                cs.reads
                    .into_iter()
                    .map(|(p, allowed)| (self.shown(h.root, &p), allowed)),
            );
        }
        Ok(out)
    }

    /// The change set of a closed scope, with its review; an open scope is an error
    /// (`State`).
    pub fn closed_change_set(&self, id: &str) -> error::Result<ChangeSet> {
        let hs = self.closed_handles(id)?;
        let mut cs = self.change_set(&hs)?;
        cs.review = self.review(id, &hs, &cs, false)?;
        Ok(cs)
    }

    /// Whether the policy has `review:` or `write:` rules.
    pub fn reviews(&self) -> bool {
        !self.review.is_empty()
    }

    /// Run the software tier's write rules over `cs` and find the tiers above it that
    /// `cs` needs; `log`: write each broken rule to the ledger.
    pub(super) fn review(
        &self,
        id: &str,
        hs: &[Arc<ScopeHandle>],
        cs: &ChangeSet,
        log: bool,
    ) -> io::Result<review::Review> {
        let mut hits = Vec::new();
        let mut shown = Vec::new();
        for c in &cs.changes {
            let path = self.shown(c.root, &c.path);
            let from = c.from.as_deref().map(|f| self.shown(c.root, f));
            let writers = c.writers.iter().filter_map(|id| cs.procs.get(id));
            if let Some(p) = std::iter::once(&path).chain(&from).find(|p| self.review.denied(p)) {
                hits.push(review::Hit {
                    path: p.clone(),
                    rule: review::Broken::Deny,
                });
            } else if let Some((p, rule)) = std::iter::once(&path)
                .chain(&from)
                .find_map(|p| Some((p, self.review.only_by(p, writers.clone())?)))
            {
                hits.push(review::Hit { path: p.clone(), rule });
            } else if c.kind != Kind::Delete
                && self.review.checks_content()
                && let Some(h) = hs.iter().find(|h| h.root == c.root)
                && let Some(f) = regular_file(h.upper.as_fd(), &c.path)?
                && let Some(found) = self.review.forbidden_content(f)?
            {
                hits.push(review::Hit {
                    path: path.clone(),
                    rule: review::Broken::Content(found.to_string()),
                });
            }
            shown.push(path);
            shown.extend(from);
        }
        if log {
            for hit in &hits {
                self.ledger.append(id, hit.op(), &hit.path, None, "deny");
            }
        }
        Ok(self.review.review(shown.iter().map(PathBuf::as_path), hits))
    }

    /// The content diff of a closed scope's change set `cs`, against its snapshot.
    pub fn diff(&self, id: &str, cs: &ChangeSet, caps: diff::Caps) -> error::Result<String> {
        let hs = self.closed_handles(id)?;
        let mut out = diff::Diff::new(caps);
        for h in &hs {
            let changes: Vec<_> = cs.changes.iter().filter(|c| c.root == h.root).cloned().collect();
            if changes.is_empty() {
                continue;
            }
            let base = self.base(h);
            let root = diff::Root {
                base: &base,
                upper: h.upper.as_fd(),
                shown: &|p| self.shown(h.root, p).to_string_lossy().into_owned(),
                withheld: &|p| h.root == PROJECT && !self.gate.read_allowed(p),
            };
            out.add(&root, &changes)?;
        }
        Ok(out.finish())
    }

    /// Freeze the scope and return its change set, after its sandboxes (process groups
    /// `stopped`) were stopped. The caller has fsynced its open files (syncfs is not a
    /// barrier on plain FUSE). Closing a closed scope returns the same change set.
    pub fn close_scope_after(&self, id: &str, stopped: &[i32]) -> error::Result<ChangeSet> {
        let hs = self.handles(id)?;
        let first = !hs[0].is_closed();
        if first {
            self.settle_dead_handles(id, stopped, std::time::Duration::from_secs(5));
            if id != UNSCOPED {
                let now = self.unscoped_changes.load(Ordering::Relaxed);
                let at = hs[0].unscoped_at.load(Ordering::Relaxed);
                hs[0].unscoped_seen.store(now.saturating_sub(at), Ordering::Relaxed);
            }
            for h in &hs {
                h.store().set_state(ScopeState::Closed)?;
                h.closed.store(true, Ordering::Release);
            }
            self.ledger.append(id, "close", Path::new(""), None, "allow");
        }
        let mut cs = self.change_set(&hs)?;
        cs.unscoped = hs[0].unscoped_seen.load(Ordering::Relaxed);
        cs.review = self.review(id, &hs, &cs, first)?;
        Ok(cs)
    }
    /// Return to agent: the decision's reasons go back and the scope accepts IO again.
    pub fn reopen_scope(&self, id: &str) -> error::Result<()> {
        let hs = self.closed_handles(id)?;
        let now = self.unscoped_changes.load(Ordering::Relaxed);
        if hs[0].store_read().hold().is_some() {
            hs[0].store().set_hold(None)?;
        }
        for h in &hs {
            h.store().set_state(ScopeState::Open)?;
            h.unscoped_at.store(now, Ordering::Relaxed);
            h.closed.store(false, Ordering::Release);
        }
        self.ledger.append(id, "decide", Path::new(""), None, "return");
        Ok(())
    }

    /// What a scope with a pending decision shows at start (`history::Fate`); `finished`:
    /// the scopes whose commit the journal finished.
    pub(super) fn fate(&self, id: &str, finished: &[String]) -> history::Fate {
        if finished.iter().any(|f| f == id) {
            return history::Fate::Committed;
        }
        let Ok(hs) = self.handles(id) else {
            return history::Fate::Gone;
        };
        if hs[0].is_closed() {
            return history::Fate::Closed;
        }
        // A decided unscoped scope is replaced by a fresh one, with no changes.
        if id == UNSCOPED && self.change_set(&hs).is_ok_and(|cs| cs.changes.is_empty()) {
            return history::Fate::Gone;
        }
        history::Fate::Open
    }

    /// Commit a closed scope's change set to the project, all or nothing, and drop the
    /// scope. On a conflict nothing is written and the policy's `conflict.verdict`
    /// drops the scope (discard) or reopens it (return); on a failure the commit is
    /// rolled back and the scope stays closed (`Aborted`).
    pub fn commit_scope(&self, id: &str) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        let cs = self.change_set(&hs)?;
        self.commit_change_set(id, &hs, &cs, false)
    }

    /// `commit_scope` with the change set `close_scope` returned (the scope is frozen since).
    pub fn commit_closed(&self, id: &str, cs: &ChangeSet) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        self.commit_change_set(id, &hs, cs, false)
    }

    /// `commit_scope` (with `cs`, if given) that keeps a committed scope until
    /// `finish_commit`: the caller settles the decision in the history in between, so
    /// a scope gone at the next start was never committed (`history::Fate`).
    pub fn commit_kept(&self, id: &str, cs: Option<&ChangeSet>) -> error::Result<Outcome> {
        let hs = self.closed_handles(id)?;
        match cs {
            Some(cs) => self.commit_change_set(id, &hs, cs, true),
            None => self.commit_change_set(id, &hs, &self.change_set(&hs)?, true),
        }
    }

    /// Drop a scope `commit_kept` committed.
    pub fn finish_commit(&self, id: &str, generation: Option<u64>) -> error::Result<()> {
        self.remove_scope(id)?;
        // One journal write records the scope gone (before a new scope of the same id
        // opens) and forgets what no scope reads any more.
        self.gc(generation)?;
        Ok(self.ensure_unscoped()?)
    }

    pub(super) fn closed_handles(&self, id: &str) -> error::Result<Vec<Arc<ScopeHandle>>> {
        let hs = self.handles(id)?;
        if !hs[0].is_closed() {
            return Err(Error::State(format!("scope {id} is not closed")));
        }
        Ok(hs)
    }

    pub(super) fn commit_change_set(
        &self,
        id: &str,
        hs: &[Arc<ScopeHandle>],
        cs: &ChangeSet,
        keep: bool,
    ) -> error::Result<Outcome> {
        let hs: Vec<&ScopeHandle> = hs.iter().map(|h| h.as_ref()).collect();
        let show = |root: usize, p: &Path| self.shown(root, p);
        // A failed commit rolled back in place: the scope is still closed, and the
        // caller may decide again. A failed rollback is the host's problem.
        let outcome = match self.commits.commit(&self.lowers(), &hs, cs, self.conflict.reads, &show) {
            Ok(o) => o,
            Err(e) if crate::commit::rollback_failed(&e) => return Err(e.into()),
            Err(e) => return Err(Error::Aborted(format!("commit rolled back: {e}"))),
        };
        match &outcome {
            Outcome::Committed(generation, _) => {
                crate::fault::hit("committed", 0)?;
                self.ledger.append(id, "decide", Path::new(""), None, "commit");
                if !keep {
                    self.finish_commit(id, *generation)?;
                }
                return Ok(outcome);
            }
            Outcome::Conflict(paths) => {
                for p in paths {
                    self.ledger.append(id, "conflict", p, None, "deny");
                }
                self.ledger.append(id, "decide", Path::new(""), None, "conflict");
                if self.conflict.verdict == ConflictVerdict::Return {
                    // Back to the agent with its changes; reopen logs `decide=return`.
                    self.reopen_scope(id)?;
                    return Ok(outcome);
                }
                self.remove_scope(id)?;
                self.gc(None)?;
            }
        }
        self.ensure_unscoped()?;
        Ok(outcome)
    }

    /// Drop a scope and its staged changes.
    pub fn drop_scope(&self, id: &str) -> error::Result<()> {
        self.handles(id)?;
        self.ledger.append(id, "decide", Path::new(""), None, "discard");
        self.remove_scope(id)?;
        self.gc(None)?;
        Ok(self.ensure_unscoped()?)
    }

    /// Drop generations no open scope reads through any more; `dropped`: the
    /// generation whose scope was just dropped.
    pub(super) fn gc(&self, dropped: Option<u64>) -> io::Result<()> {
        let oldest = self.scopes.read().unwrap().values().map(|h| h.since).min();
        self.commits.gc(oldest, dropped)
    }

    /// Remove every view of scope `id`.
    pub(super) fn remove_scope(&self, id: &str) -> error::Result<()> {
        let hs: Vec<Arc<ScopeHandle>> = {
            let mut scopes = self.scopes.write().unwrap();
            (0..roots::COUNT)
                .filter_map(|r| scopes.remove(&roots::view_name(id, r)))
                .collect()
        };
        if hs.is_empty() {
            return Err(Error::no_scope(id));
        }
        for h in hs {
            let names = self.t().forget_scope(&h.id, h.idx);
            // After the last settle the mount goes away: invalidating is wasted kernel work.
            if id == UNSCOPED
                && !self.last_settle.load(Ordering::Acquire)
                && let Some(n) = self.notifier.get()
            {
                let root_ino = fuser::INodeNo(UNSCOPED_ROOT + h.root as u64);
                for name in names {
                    let _ = n.inval_entry(root_ino, &name);
                }
                let _ = n.inval_inode(root_ino, 0, 0);
            }
            // FUSE calls in flight may still hold the handle; they fail once the directory is gone.
            let gone = h.store().discard_to(&self.trash)?;
            let mut cleaners = self.cleaners.lock().unwrap();
            cleaners.retain(|c| !c.is_finished());
            cleaners.push(std::thread::spawn(move || {
                let _ = fs::remove_dir_all(gone);
            }));
        }
        Ok(())
    }

    /// At shutdown: write every scope's deferred metadata and finish deleting dropped scopes.
    pub fn flush(&self) -> io::Result<()> {
        for h in self.scopes.read().unwrap().values() {
            h.store().flush()?;
        }
        for c in self.cleaners.lock().unwrap().drain(..) {
            let _ = c.join();
        }
        Ok(())
    }

    pub fn scope_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.scopes.read().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }
}

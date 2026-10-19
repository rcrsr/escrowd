//! Held scopes: a closed scope reviewers have, each tier's verdict, and the
//! session's next scope waiting behind a hold with a wait.

use super::*;

/// A scope as reviewers see it.
pub struct Identity {
    pub id: String,
    pub name: String,
    pub session: String,
    pub token_sha256: Option<String>,
    pub labels: HashMap<String, String>,
    pub hold: Option<Hold>,
}

/// A tier's review of a held scope: still held for the tiers left, or the last
/// pending tier gave its verdict (`Hold::verdict`, with every review; not written).
pub enum Reviewed {
    Held(Hold),
    Final(Hold),
}

impl Views {
    /// The hold of scope `id`, if reviewers have it (`NoScope` if no such scope).
    pub fn held(&self, id: &str) -> error::Result<Option<Hold>> {
        Ok(self.handles(id)?[0].store_read().hold().cloned())
    }

    /// Scope `id`'s name, session, token hash and hold (`NoScope` if no such scope).
    pub fn identity(&self, id: &str) -> error::Result<Identity> {
        let hs = self.handles(id)?;
        let store = hs[0].store_read();
        Ok(Identity {
            id: id.to_string(),
            name: store.name.clone(),
            session: store.session.clone(),
            token_sha256: store.token_sha256().map(str::to_string),
            labels: store.labels()?,
            hold: store.hold().cloned(),
        })
    }

    /// Every held scope, oldest hold first.
    pub fn held_scopes(&self) -> error::Result<Vec<Identity>> {
        let ids: Vec<String> = {
            let scopes = self.scopes.read();
            scopes
                .values()
                .filter(|h| h.root == PROJECT && h.store_read().hold().is_some())
                .map(|h| h.group.clone())
                .collect()
        };
        let mut out = Vec::new();
        for id in ids {
            match self.identity(&id) {
                Ok(i) if i.hold.is_some() => out.push(i),
                Ok(_) => {}
                Err(e) if e.is_no_scope() => {}
                Err(e) => return Err(e),
            }
        }
        out.sort_by(|a, b| (a.hold.as_ref().map(|h| h.at_ms), &a.id).cmp(&(b.hold.as_ref().map(|h| h.at_ms), &b.id)));
        Ok(out)
    }

    /// Hold closed scope `id` for the reviewers of `tiers` (cheapest first); `wait`: its
    /// session's next scope does not open until the verdict.
    pub fn hold_scope(&self, id: &str, tiers: Vec<Tier>, wait: bool) -> error::Result<Hold> {
        let hs = self.closed_handles(id)?;
        let names: Vec<&str> = tiers.iter().map(|t| t.as_str()).collect();
        let decision = format!("{}{}", names.join(","), if wait { ",wait" } else { "" });
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let hold = Hold {
            tiers,
            wait,
            at_ms,
            verdict: Decision::Commit,
            reviews: Vec::new(),
        };
        hs[0].store().set_hold(Some(hold.clone()))?;
        self.ledger.append(id, "hold", Path::new(""), None, &decision);
        Ok(hold)
    }

    /// `tier`'s verdict on held scope `id`. The tier must be pending; a human's verdict
    /// also stands for the cheaper tiers pending before it. A verdict looser than the
    /// one so far needs a human and `over` (`Denied` otherwise); the ledger
    /// records each verdict and every override. Before the last pending tier the hold
    /// keeps the verdict (written at once); after it nothing is written: the caller
    /// applies the final verdict, which drops or reopens the scope.
    pub fn review_scope(
        &self,
        id: &str,
        tier: Tier,
        verdict: Decision,
        reasons: Vec<String>,
        over: bool,
    ) -> error::Result<Reviewed> {
        let hs = self.closed_handles(id)?;
        let mut store = hs[0].store();
        let Some(mut hold) = store.hold().cloned() else {
            return Err(Error::State(format!("scope {id} is not held")));
        };
        let Some(at) = hold.tiers.iter().position(|t| *t == tier) else {
            let waiting: Vec<&str> = hold.tiers.iter().map(|t| t.as_str()).collect();
            return Err(Error::State(format!(
                "scope {id} is not waiting for {}: it waits for {}",
                tier.as_str(),
                waiting.join(", ")
            )));
        };
        if tier != Tier::Human && at > 0 {
            return Err(Error::State(format!(
                "scope {id} waits for {} first",
                hold.tiers[0].as_str()
            )));
        }
        let looser = verdict < hold.verdict;
        if looser && !(over && tier == Tier::Human) {
            return Err(Error::Denied(format!(
                "{} cannot loosen {} to {}: only a human can, with an override",
                tier.as_str(),
                hold.verdict.as_str(),
                verdict.as_str()
            )));
        }
        if looser {
            let change = format!("{}-to-{}", hold.verdict.as_str(), verdict.as_str());
            self.ledger.append(id, "override", Path::new(""), None, &change);
            hold.verdict = verdict;
        } else {
            hold.verdict = hold.verdict.max(verdict);
        }
        self.ledger.append(
            id,
            &format!("review-{}", tier.as_str()),
            Path::new(""),
            None,
            verdict.as_str(),
        );
        hold.reviews.push(TierReview {
            tier,
            verdict,
            reasons,
            over: looser,
        });
        hold.tiers.drain(..=at);
        if hold.tiers.is_empty() {
            return Ok(Reviewed::Final(hold));
        }
        store.set_hold(Some(hold.clone()))?;
        Ok(Reviewed::Held(hold))
    }

    /// The held scope of `session` its next scope waits for, if any.
    pub fn blocking_hold(&self, session: &str) -> Option<String> {
        if session.is_empty() {
            return None;
        }
        let scopes = self.scopes.read();
        let mut ids: Vec<&String> = scopes
            .values()
            .filter(|h| h.root == PROJECT)
            .filter(|h| {
                let store = h.store_read();
                store.session == session && store.hold().is_some_and(|hold| hold.wait)
            })
            .map(|h| &h.group)
            .collect();
        ids.sort();
        ids.first().map(|id| id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: diff::Caps = diff::Caps {
        file_bytes: 1 << 20,
        max_bytes: 1 << 20,
    };

    /// A closed scope held for `tiers`.
    fn held(name: &str, tiers: Vec<Tier>) -> (Views, PathBuf, String) {
        let (views, dir) = Views::fixture(name);
        let id = views.open_scope("t", &HashMap::new(), "").unwrap().id;
        views.close_scope_after(&id, &[], CAPS).unwrap();
        views.hold_scope(&id, tiers, false).unwrap();
        (views, dir, id)
    }

    fn ledger(dir: &Path) -> String {
        fs::read_to_string(dir.join("state/ledger.log")).unwrap()
    }

    fn review(views: &Views, id: &str, tier: Tier, verdict: Decision, over: bool) -> error::Result<Reviewed> {
        views.review_scope(id, tier, verdict, Vec::new(), over)
    }

    #[test]
    fn a_tier_that_is_not_pending_is_a_state_error() {
        let (views, dir, id) = held("hold-not-pending", vec![Tier::Software]);
        assert!(matches!(
            review(&views, &id, Tier::Llm, Decision::Commit, false),
            Err(Error::State(_))
        ));
        assert_eq!(views.held(&id).unwrap().unwrap().tiers, [Tier::Software]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_llm_tier_before_the_software_tier_is_a_state_error() {
        let (views, dir, id) = held("hold-order", vec![Tier::Software, Tier::Llm]);
        assert!(matches!(
            review(&views, &id, Tier::Llm, Decision::Commit, false),
            Err(Error::State(_))
        ));
        assert_eq!(views.held(&id).unwrap().unwrap().tiers, [Tier::Software, Tier::Llm]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_looser_verdict_without_a_human_override_is_denied() {
        let (views, dir, id) = held("hold-denied", vec![Tier::Software, Tier::Llm, Tier::Human]);
        assert!(matches!(
            review(&views, &id, Tier::Software, Decision::Discard, false),
            Ok(Reviewed::Held(_))
        ));
        // A machine tier cannot loosen, and a human without `over` cannot either.
        assert!(matches!(
            review(&views, &id, Tier::Llm, Decision::Commit, true),
            Err(Error::Denied(_))
        ));
        assert!(matches!(
            review(&views, &id, Tier::Human, Decision::Commit, false),
            Err(Error::Denied(_))
        ));
        let hold = views.held(&id).unwrap().unwrap();
        assert_eq!(hold.verdict, Decision::Discard);
        assert_eq!(hold.tiers, [Tier::Llm, Tier::Human]);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_human_override_loosens_the_verdict_and_logs_it() {
        let (views, dir, id) = held("hold-override", vec![Tier::Software, Tier::Human]);
        review(&views, &id, Tier::Software, Decision::Discard, false).unwrap();
        let Reviewed::Final(hold) = review(&views, &id, Tier::Human, Decision::Commit, true).unwrap() else {
            panic!("the human is the last tier");
        };
        assert_eq!(hold.verdict, Decision::Commit);
        assert!(hold.reviews.last().is_some_and(|r| r.over && r.tier == Tier::Human));
        let log = ledger(&dir);
        assert!(log.contains("op=override") && log.contains("decision=discard-to-commit"));
        assert!(log.contains("op=review-human"));
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_last_tier_is_final_and_writes_nothing() {
        let (views, dir, id) = held("hold-final", vec![Tier::Software]);
        let Reviewed::Final(hold) = review(&views, &id, Tier::Software, Decision::Return, false).unwrap() else {
            panic!("software is the last tier");
        };
        assert_eq!(hold.verdict, Decision::Return);
        assert!(hold.tiers.is_empty());
        // The stored hold is as hold_scope left it.
        let stored = views.held(&id).unwrap().unwrap();
        assert_eq!(stored.tiers, [Tier::Software]);
        assert_eq!(stored.verdict, Decision::Commit);
        assert!(stored.reviews.is_empty());
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_tier_before_the_last_leaves_the_scope_held_with_the_tiers_drained() {
        let (views, dir, id) = held("hold-drain", vec![Tier::Software, Tier::Llm, Tier::Human]);
        let Reviewed::Held(hold) = review(&views, &id, Tier::Software, Decision::Return, false).unwrap() else {
            panic!("tiers remain");
        };
        assert_eq!(hold.tiers, [Tier::Llm, Tier::Human]);
        assert_eq!(hold.verdict, Decision::Return);
        assert_eq!(views.held(&id).unwrap().unwrap(), hold);
        // A human's verdict also stands for the cheaper tiers pending before it.
        let Reviewed::Final(done) = review(&views, &id, Tier::Human, Decision::Return, false).unwrap() else {
            panic!("the human is last");
        };
        assert!(done.tiers.is_empty());
        assert_eq!(done.reviews.len(), 2);
        drop(views);
        fs::remove_dir_all(&dir).unwrap();
    }
}

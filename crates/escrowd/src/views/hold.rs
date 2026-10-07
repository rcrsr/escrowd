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

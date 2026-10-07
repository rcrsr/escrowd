//! Decisions: what a scope's opener, its reviewers and its followers ask of it, and
//! the order each one's steps run in. The RPC layer only translates messages.
//!
//! A scope's decision runs under that scope's lock, start to finish: the held check,
//! the review, the hold or the verdict, the history. A reviewer's verdict and a
//! follower's first look take the same lock, so none of them sees a scope between
//! two steps of another.
//!
//! A decision of a scope that has a session or was held is kept in the history:
//! written ahead as pending with the outcome it intends, applied, settled with the
//! outcome it got, and only then is a committed scope dropped (`Kept`).

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use tokio::sync::{OwnedMutexGuard, mpsc, watch};

use crate::changeset::ChangeSet;
use crate::commit;
use crate::convert;
use crate::diff;
use crate::error::{self, Error};
use crate::exec::Children;
use crate::policy::{ConflictVerdict, Tier};
use crate::proto;
use crate::record;
use crate::review::{Decision, TierReview};
use crate::store::Hold;
use crate::views::{Identity, Opened, Reviewed, RootView, UNSCOPED, Unscoped, Views};

/// The session's earlier decisions a reviewer gets.
const HISTORY: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Committed,
    Discarded,
    Returned,
    Conflict,
    /// Not decided yet: reviewers have the scope.
    Held,
}

/// A decision's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub scope_id: String,
    pub status: Status,
    /// The changed paths, or the conflicting ones.
    pub paths: Vec<String>,
    pub reasons: Vec<String>,
    /// The scope is open again with its changes.
    pub reopened: bool,
    /// Held: the tiers still to review, cheapest first.
    pub tiers: Vec<Tier>,
    /// Held: the session's next scope waits for the verdict.
    pub wait: bool,
}

impl Outcome {
    fn new(scope_id: &str, status: Status) -> Self {
        Outcome {
            scope_id: scope_id.to_string(),
            status,
            paths: Vec::new(),
            reasons: Vec::new(),
            reopened: false,
            tiers: Vec::new(),
            wait: false,
        }
    }

    fn held(scope_id: &str, hold: &Hold) -> Self {
        Outcome {
            tiers: hold.tiers.clone(),
            wait: hold.wait,
            ..Outcome::new(scope_id, Status::Held)
        }
    }
}

/// A closed scope's change set and its diff against the snapshot.
pub struct Closed {
    pub change_set: ChangeSet,
    pub diff: String,
}

/// A held scope as a reviewer reads it.
pub struct HeldScope {
    pub who: Identity,
    pub closed: Option<Closed>,
    /// The session's last decisions, oldest first, as `record` encodes them.
    pub history: Vec<Vec<u8>>,
}

/// A decision the history keeps: of a scope that has a session or was held.
struct Kept {
    who: Identity,
    /// As decided, diff included, in the record's form; None for an open scope.
    change_set: Option<proto::ChangeSet>,
    reviews: Vec<TierReview>,
}

impl Kept {
    fn entry(&self, outcome: Outcome) -> Vec<u8> {
        record::encode(&self.who, self.change_set.as_ref(), outcome, &self.reviews)
    }

    /// The outcome `verdict` gets if it applies with no conflict.
    fn intended(&self, id: &str, verdict: Decision, reasons: &[String]) -> Outcome {
        let (status, paths) = match verdict {
            Decision::Commit => (
                Status::Committed,
                self.change_set
                    .iter()
                    .flat_map(|cs| cs.changes.iter().map(|c| c.path.clone()))
                    .collect(),
            ),
            Decision::Return => (Status::Returned, vec![]),
            Decision::Discard => (Status::Discarded, vec![]),
        };
        Outcome {
            paths,
            reasons: reasons.to_vec(),
            reopened: verdict == Decision::Return,
            ..Outcome::new(id, status)
        }
    }
}

/// One async lock per scope id, kept in the table only while someone holds or
/// waits for it.
#[derive(Default)]
struct Locks(Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl Locks {
    async fn lock(&self, id: &str) -> ScopeLock<'_> {
        let lock = self.0.lock().entry(id.to_string()).or_default().clone();
        let guard = lock.clone().lock_owned().await;
        ScopeLock {
            locks: self,
            id: id.to_string(),
            lock,
            guard: Some(guard),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().len()
    }
}

/// Holds one scope's lock; the last holder removes it from the table.
struct ScopeLock<'a> {
    locks: &'a Locks,
    id: String,
    lock: Arc<tokio::sync::Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Drop for ScopeLock<'_> {
    fn drop(&mut self) {
        self.guard.take();
        let mut locks = self.locks.0.lock();
        // The table's reference and ours: nobody else waits for it.
        if Arc::strong_count(&self.lock) == 2 {
            locks.remove(&self.id);
        }
    }
}

pub struct Decisions {
    views: Arc<Views>,
    children: Arc<Children>,
    diff: diff::Caps,
    /// Bumped after every decision and review: an open waiting behind a held scope,
    /// and the clients following one (AwaitDecision), check again.
    changed: watch::Sender<u64>,
    /// Set at shutdown: calls that wait (an open behind a held scope, AwaitDecision)
    /// end with `Stopping`, or the server would wait on them forever.
    stopping: watch::Sender<bool>,
    /// Each scope's lock, while someone holds or waits for it.
    locks: Locks,
}

impl Decisions {
    pub fn new(views: Arc<Views>, children: Arc<Children>, diff: diff::Caps) -> Self {
        Decisions {
            views,
            children,
            diff,
            changed: watch::Sender::new(0),
            stopping: watch::Sender::new(false),
            locks: Locks::default(),
        }
    }

    pub fn views(&self) -> &Views {
        &self.views
    }

    /// Start shutting down: waiting calls return `Stopping`.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    /// Resolves once shutdown starts.
    pub async fn stopped(&self) {
        let _ = self.stopping.subscribe().wait_for(|s| *s).await;
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Filesystem work runs off the async executor.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Views) -> error::Result<T> + Send + 'static,
    ) -> error::Result<T> {
        let views = self.views.clone();
        tokio::task::spawn_blocking(move || f(&views))
            .await
            .map_err(|e| Error::Io(std::io::Error::other(e)))?
    }

    // ---- the opener ----

    /// Open a scope. In a session, wait while another of its scopes is held with a
    /// wait, until `until` (then `State`) or shutdown.
    pub async fn open(
        &self,
        name: String,
        labels: HashMap<String, String>,
        session: String,
        until: Option<tokio::time::Instant>,
    ) -> error::Result<(Opened, Vec<RootView>)> {
        let mut changed = self.changed.subscribe();
        let expired = async move {
            match until {
                Some(until) => tokio::time::sleep_until(until).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(expired);
        loop {
            changed.borrow_and_update();
            let Some(held) = self.views.blocking_hold(&session) else {
                break;
            };
            tokio::select! {
                _ = changed.changed() => {}
                _ = &mut expired => {
                    return Err(Error::State(format!(
                        "session {session}: scope {held} is held for review; the next scope opens after its verdict"
                    )));
                }
                _ = self.stopped() => return Err(Error::Stopping),
            }
        }
        self.blocking(move |v| {
            let opened = v.open_scope(&name, &labels, &session)?;
            let roots = v.root_views(&opened.id);
            Ok((opened, roots))
        })
        .await
    }

    /// Stop the scope's children (their last writes flush on exit), then freeze it.
    pub async fn close(&self, id: String, token: String) -> error::Result<Closed> {
        let (children, caps) = (self.children.clone(), self.diff);
        self.blocking(move |v| {
            v.check_token(&id, &token)?;
            let stopped = children.stop(&id);
            let change_set = v.close_scope_after(&id, &stopped)?;
            let diff = v.diff(&id, &change_set, caps)?;
            Ok(Closed { change_set, diff })
        })
        .await
    }

    /// Close the implicit default scope; it is decided as scope `unscoped`.
    pub async fn settle_unscoped(&self) -> error::Result<Closed> {
        if self.views.unscoped() != Unscoped::Implicit {
            return Err(Error::State("settle_unscoped needs unscoped = implicit".into()));
        }
        self.close(UNSCOPED.to_string(), String::new()).await
    }

    /// A closed scope's change set and diff (`State` if it is open).
    pub async fn change_set(&self, id: String) -> error::Result<Closed> {
        let caps = self.diff;
        self.blocking(move |v| {
            let change_set = v.closed_change_set(&id)?;
            let diff = v.diff(&id, &change_set, caps)?;
            Ok(Closed { change_set, diff })
        })
        .await
    }

    /// A closed scope's change set and diff; None if it is open.
    async fn closed(&self, id: &str) -> error::Result<Option<Closed>> {
        match self.change_set(id.to_string()).await {
            Ok(c) => Ok(Some(c)),
            Err(Error::State(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The record's form of a closed scope's change set.
    async fn closed_record(&self, id: &str) -> error::Result<Option<proto::ChangeSet>> {
        Ok(self
            .closed(id)
            .await?
            .map(|c| convert::change_set(&self.views, id.to_string(), c.change_set, c.diff)))
    }

    /// The opener's decision (`verdict` None: unspecified). A commit runs the review
    /// and may hold the scope; a held scope can only be withdrawn (discarded). The
    /// decision of a scope in a session, or held, is kept.
    pub async fn decide(
        &self,
        id: String,
        token: String,
        verdict: Option<Decision>,
        reasons: Vec<String>,
        wait: bool,
    ) -> error::Result<Outcome> {
        let _lock = self.locks.lock(&id).await;
        let scope = id.clone();
        let who = self
            .blocking(move |v| {
                v.check_token(&scope, &token)?;
                v.identity(&scope)
            })
            .await?;
        let keep = match !who.session.is_empty() || who.hold.is_some() {
            true => Some(Kept {
                change_set: self.closed_record(&who.id).await?,
                reviews: who.hold.as_ref().map(|h| h.reviews.clone()).unwrap_or_default(),
                who,
            }),
            false => None,
        };
        let out = self.propose(id, verdict, reasons, wait, keep).await?;
        if out.status != Status::Held {
            self.notify();
        }
        Ok(out)
    }

    /// `decide` after the token check: the opener proposes, the review tightens.
    async fn propose(
        &self,
        id: String,
        verdict: Option<Decision>,
        mut reasons: Vec<String>,
        wait: bool,
        keep: Option<Kept>,
    ) -> error::Result<Outcome> {
        // A held scope is its reviewers': the opener can only withdraw it.
        if verdict != Some(Decision::Discard) {
            let scope = id.clone();
            if let Some(hold) = self.blocking(move |v| v.held(&scope)).await? {
                return Err(Error::Denied(format!(
                    "scope {id} is held for {}: only its reviewers commit or return it; its opener can discard it",
                    hold.tiers[0].as_str()
                )));
            }
        }
        let Some(mut verdict) = verdict else {
            return Err(Error::Invalid("verdict must be set".into()));
        };
        // The review's verdict only tightens: a discard by the software tier wins, and
        // a commit of a change set that needs a tier above software holds it.
        let mut closed = None;
        if verdict != Decision::Discard && self.views.reviews() {
            let scope = id.clone();
            let cs = self.blocking(move |v| v.closed_change_set(&scope)).await?;
            let r = &cs.review;
            if r.verdict == Decision::Discard {
                verdict = Decision::Discard;
                reasons = r.reasons();
            } else if verdict == Decision::Commit && !r.tiers.is_empty() {
                let (scope, tiers, wait) = (id.clone(), r.tiers.clone(), r.wait || wait);
                let hold = self.blocking(move |v| v.hold_scope(&scope, tiers, wait)).await?;
                return Ok(Outcome::held(&id, &hold));
            }
            closed = Some(cs);
        }
        self.apply(id, verdict, reasons, closed, keep).await
    }

    /// Apply a verdict: discard drops the scope (open or closed); return reopens a
    /// closed scope; commit applies a closed scope's change set (`closed`, if the
    /// caller built it), or reports the conflicting paths and drops or reopens the
    /// scope, per the policy's conflict.verdict. With `keep`, the decision goes to the
    /// history: written ahead, then settled before a committed scope is dropped.
    async fn apply(
        &self,
        id: String,
        verdict: Decision,
        reasons: Vec<String>,
        closed: Option<ChangeSet>,
        keep: Option<Kept>,
    ) -> error::Result<Outcome> {
        let children = self.children.clone();
        self.blocking(move |v| {
            let Some(keep) = keep else {
                return apply_now(v, &children, &id, verdict, reasons, closed, false).map(|(out, _)| out);
            };
            let seq = v.history.intend(
                &keep.who.id,
                &keep.who.session,
                keep.who.token_sha256.as_deref(),
                &keep.entry(keep.intended(&id, verdict, &reasons)),
            )?;
            crate::fault::hit("intended", 0)?;
            let (out, finish) = match apply_now(v, &children, &id, verdict, reasons, closed, true) {
                Ok(r) => r,
                Err(e) => {
                    if let Err(c) = v.history.cancel(seq) {
                        eprintln!("escrowd: history: {c}");
                    }
                    return Err(e);
                }
            };
            crate::fault::hit("applied", 0)?;
            if let Err(e) = v.history.settle(seq, &keep.entry(out.clone())) {
                eprintln!("escrowd: history: {e}");
            }
            crate::fault::hit("settled", 0)?;
            if let Some(generation) = finish {
                v.finish_commit(&id, generation)?;
            }
            Ok(out)
        })
        .await
    }

    // ---- followers ----

    /// A held scope's updates until its verdict; a decided scope's last outcome.
    /// The stream ends after the verdict, an error, or shutdown.
    pub async fn follow(
        self: &Arc<Self>,
        id: String,
        token: String,
    ) -> error::Result<mpsc::Receiver<error::Result<Outcome>>> {
        let scope = id.clone();
        // A review or a withdrawal drops the scope before it records the decision: read
        // between their steps, the scope would be neither live nor kept.
        let settled = self.locks.lock(&id).await;
        let (live, latest) = self
            .blocking(move |v| {
                // The last kept decision first: a hold seen after it is decided later,
                // and kept with a newer sequence.
                let latest = v.history.latest(&scope)?;
                let live = match v.identity(&scope) {
                    Ok(who) => {
                        v.check_token(&scope, &token)?;
                        Some(who)
                    }
                    Err(e) if e.is_no_scope() => None,
                    Err(e) => return Err(e),
                };
                // Gone: decided, maybe since the first read.
                let latest = match live {
                    Some(_) => latest,
                    None => v.history.latest(&scope)?,
                };
                if live.is_none()
                    && let Some(l) = &latest
                    && l.token_sha256
                        .as_ref()
                        .is_some_and(|t| *t != crate::views::token_sha256(&token))
                {
                    return Err(Error::Denied(format!("scope {scope}: missing or wrong token")));
                }
                Ok((live, latest.map(|l| l.seq)))
            })
            .await?;
        drop(settled);
        let after = match (&live, latest) {
            // Held: follow it to a decision newer than the last kept.
            (Some(who), seq) if who.hold.is_some() => seq.unwrap_or(0),
            // Decided (gone, or reopened by a return): its last decision.
            (_, Some(seq)) => seq - 1,
            (Some(_), None) => return Err(Error::State(format!("scope {id} is not held"))),
            (None, None) => return Err(Error::no_scope(&id)),
        };
        let (tx, rx) = mpsc::channel(4);
        let me = self.clone();
        tokio::spawn(async move { me.send_updates(id, after, tx).await });
        Ok(rx)
    }

    /// Send a client following scope `id` its hold's updates, then its first kept
    /// decision after `after` (history sequence), and stop.
    async fn send_updates(&self, id: String, after: u64, tx: mpsc::Sender<error::Result<Outcome>>) {
        let mut changed = self.changed.subscribe();
        let mut sent: Option<Vec<Tier>> = None;
        loop {
            changed.borrow_and_update();
            let scope = id.clone();
            let state = self
                .blocking(move |v| {
                    let held = match v.held(&scope) {
                        Ok(h) => h,
                        Err(e) if e.is_no_scope() => None,
                        Err(e) => return Err(e),
                    };
                    let decided = match held {
                        Some(_) => None,
                        None => v.history.latest(&scope)?.filter(|l| l.seq > after),
                    };
                    Ok((held, decided))
                })
                .await;
            match state {
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
                Ok((Some(hold), _)) if sent.as_ref() != Some(&hold.tiers) => {
                    sent = Some(hold.tiers.clone());
                    if tx.send(Ok(Outcome::held(&id, &hold))).await.is_err() {
                        return;
                    }
                }
                Ok((_, Some(l))) => {
                    let out = record::outcome(&l.entry).ok_or_else(|| {
                        Error::Io(std::io::Error::other(format!("scope {id}: unreadable history entry")))
                    });
                    let _ = tx.send(out).await;
                    return;
                }
                // Not held and no new decision yet: the decider keeps it next.
                Ok(_) => {}
            }
            tokio::select! {
                r = changed.changed() => if r.is_err() { return },
                _ = tx.closed() => return,
                _ = self.stopped() => {
                    let _ = tx.send(Err(Error::Stopping)).await;
                    return;
                }
            }
        }
    }

    // ---- reviewers ----

    /// Every held scope, oldest hold first.
    pub async fn list_held(&self) -> error::Result<Vec<Identity>> {
        self.blocking(|v| v.held_scopes()).await
    }

    /// A held scope with its change set and its session's last decisions.
    pub async fn get_held(&self, id: String) -> error::Result<HeldScope> {
        let scope = id.clone();
        let (who, history) = self
            .blocking(move |v| {
                let who = v.identity(&scope)?;
                let history = v.history.session(&who.session, HISTORY)?;
                Ok((who, history))
            })
            .await?;
        if who.hold.is_none() {
            return Err(Error::State(format!("scope {id} is not held")));
        }
        Ok(HeldScope {
            who,
            closed: self.closed(&id).await?,
            history,
        })
    }

    /// `tier`'s verdict on held scope `id`; after the last pending tier, the scope is
    /// decided with the verdict of every tier and their reasons.
    pub async fn review(
        &self,
        id: String,
        tier: Tier,
        verdict: Decision,
        reasons: Vec<String>,
        over: bool,
    ) -> error::Result<Outcome> {
        let _lock = self.locks.lock(&id).await;
        let scope = id.clone();
        let who = self.blocking(move |v| v.identity(&scope)).await?;
        let scope = id.clone();
        let reviewed = self
            .blocking(move |v| v.review_scope(&scope, tier, verdict, reasons, over))
            .await?;
        let out = match reviewed {
            Reviewed::Held(hold) => Outcome::held(&id, &hold),
            Reviewed::Final(hold) => {
                let change_set = self.closed_record(&id).await?;
                let reasons = hold
                    .reviews
                    .iter()
                    .flat_map(|r| r.reasons.iter().map(move |s| format!("{}: {s}", r.tier.as_str())))
                    .collect();
                let keep = Kept {
                    who,
                    change_set,
                    reviews: hold.reviews,
                };
                self.apply(id, hold.verdict, reasons, None, Some(keep)).await?
            }
        };
        self.notify();
        Ok(out)
    }
}

/// `Decisions::apply` on the blocking pool. With `keep`, a committed scope stays until
/// `finish_commit`, and the second value is the generation to finish it with.
fn apply_now(
    v: &Views,
    children: &Children,
    id: &str,
    verdict: Decision,
    reasons: Vec<String>,
    closed: Option<ChangeSet>,
    keep: bool,
) -> error::Result<(Outcome, Option<Option<u64>>)> {
    let mut out = Outcome {
        reasons,
        ..Outcome::new(id, Status::Discarded)
    };
    let mut finish = None;
    match verdict {
        Decision::Commit => {
            let committed = match keep {
                true => v.commit_kept(id, closed.as_ref()),
                false => closed.map_or_else(|| v.commit_scope(id), |cs| v.commit_closed(id, &cs)),
            };
            match committed.inspect(|_| children.release(id))? {
                commit::Outcome::Committed(generation, changed) => {
                    out.status = Status::Committed;
                    out.paths = changed.iter().map(|p| convert::path_str(p)).collect();
                    finish = keep.then_some(generation);
                }
                commit::Outcome::Conflict(conflicts) => {
                    out.status = Status::Conflict;
                    out.reopened = v.conflict().verdict == ConflictVerdict::Return;
                    out.paths = conflicts.iter().map(|p| convert::path_str(p)).collect();
                    out.reasons = out
                        .paths
                        .iter()
                        .map(|p| format!("conflict: {p} changed in the project since the scope read it"))
                        .collect();
                }
            }
        }
        Decision::Return => {
            v.reopen_scope(id)?;
            children.release(id);
            out.status = Status::Returned;
            out.reopened = true;
        }
        Decision::Discard => {
            if v.scope(id).is_err() {
                return Err(Error::no_scope(id));
            }
            children.stop(id);
            let r = v.drop_scope(id);
            children.release(id);
            r?;
        }
    }
    Ok((out, finish))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_scope_lock_serializes_one_scope_and_leaves_no_entry() {
        let locks = Arc::new(Locks::default());
        let first = locks.lock("s1").await;
        let other = locks.lock("s2").await; // another scope does not wait
        assert_eq!(locks.len(), 2);
        let waiter = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _second = locks.lock("s1").await;
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished(), "the second holder waits for the first");
        drop(first);
        waiter.await.unwrap();
        drop(other);
        assert_eq!(locks.len(), 0);
    }
}

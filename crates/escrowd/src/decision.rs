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
//!
//! A command reviewer's program runs outside the scope's lock, as a task of its own
//! (`next_run`, in the locked step that makes the reviewer next). Its report is applied
//! in a locked step of its own (`finish_run`), and only if its run is still the
//! scope's current one: a verdict that ended or skipped the reviewer, a withdrawal, or
//! a newer run makes the report stale, and it is dropped.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::Mutex;

use tokio::sync::{OwnedMutexGuard, mpsc, watch};

use crate::commit;
use crate::convert;
use crate::diff;
use crate::error::{self, Error};
use crate::exec::Children;
use crate::policy::ConflictVerdict;
use crate::proto;
use crate::record;
use crate::review::{self, Decision, TierReview};
use crate::reviewer::{Answer, Failure, RunReport, RunRequest, Runner, format_duration};
use crate::store::Hold;
use crate::views::{Frozen, Giver, Identity, Opened, Reviewed, RootView, UNSCOPED, Unscoped, Views};

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
    /// Held: the reviewers still to review, in order.
    pub reviewers: Vec<String>,
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
            reviewers: Vec::new(),
            wait: false,
        }
    }

    fn held(scope_id: &str, hold: &Hold) -> Self {
        Outcome {
            reviewers: hold.reviewers.clone(),
            wait: hold.wait,
            ..Outcome::new(scope_id, Status::Held)
        }
    }
}

/// A held scope as a reviewer reads it.
pub struct HeldScope {
    pub who: Identity,
    pub closed: Option<Arc<Frozen>>,
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
        // Built before the wait: a cancelled waiter's drop still prunes the entry.
        let mut held = ScopeLock {
            locks: self,
            id: id.to_string(),
            lock: lock.clone(),
            guard: None,
        };
        held.guard = Some(lock.lock_owned().await);
        held
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
    /// Runs the command reviewers' programs.
    runner: Runner,
}

impl Decisions {
    pub fn new(views: Arc<Views>, children: Arc<Children>, diff: diff::Caps, runner: Runner) -> Self {
        Decisions {
            views,
            children,
            diff,
            changed: watch::Sender::new(0),
            stopping: watch::Sender::new(false),
            locks: Locks::default(),
            runner,
        }
    }

    pub fn views(&self) -> &Views {
        &self.views
    }

    /// Start shutting down: waiting calls return `Stopping`, and the reviewers' programs
    /// are stopped; what they report is applied to nothing.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
        self.runner.stop_all();
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
            let hold_of = session.clone();
            let Some(held) = self.blocking(move |v| Ok(v.blocking_hold(&hold_of))).await? else {
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
    pub async fn close(&self, id: String, token: String) -> error::Result<Arc<Frozen>> {
        let (children, caps) = (self.children.clone(), self.diff);
        self.blocking(move |v| {
            v.check_token(&id, &token)?;
            let stopped = children.stop(&id);
            v.close_scope_after(&id, &stopped, caps)
        })
        .await
    }

    /// Close the implicit default scope; it is decided as scope `unscoped`.
    pub async fn settle_unscoped(&self) -> error::Result<Arc<Frozen>> {
        if self.views.unscoped() != Unscoped::Implicit {
            return Err(Error::State("settle_unscoped needs unscoped = implicit".into()));
        }
        self.close(UNSCOPED.to_string(), String::new()).await
    }

    /// A closed scope's change set and diff (`State` if it is open).
    pub async fn change_set(&self, id: String) -> error::Result<Arc<Frozen>> {
        let caps = self.diff;
        self.blocking(move |v| v.frozen(&id, caps)).await
    }

    /// A closed scope's change set and diff; None if it is open.
    async fn closed(&self, id: &str) -> error::Result<Option<Arc<Frozen>>> {
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
            .map(|c| convert::change_set(&self.views, id.to_string(), &c.change_set, &c.diff)))
    }

    /// The opener's decision (`verdict` None: unspecified). A commit runs the review
    /// and may hold the scope; a held scope can only be withdrawn (discarded). The
    /// decision of a scope in a session, or held, is kept.
    pub async fn decide(
        self: &Arc<Self>,
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
        self: &Arc<Self>,
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
                    hold.reviewers[0]
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
            let caps = self.diff;
            let frozen = self.blocking(move |v| v.frozen(&scope, caps)).await?;
            let r = &frozen.change_set.review;
            if r.verdict == Decision::Discard {
                verdict = Decision::Discard;
                reasons = r.reasons();
            } else if verdict == Decision::Commit && !r.reviewers.is_empty() {
                let (scope, reviewers, wait) = (id.clone(), r.reviewers.clone(), r.wait || wait);
                let hold = self.blocking(move |v| v.hold_scope(&scope, reviewers, wait)).await?;
                self.next_run(&id).await;
                return Ok(Outcome::held(&id, &hold));
            }
            closed = Some(frozen);
        }
        let withdrawn = verdict == Decision::Discard;
        let out = self.apply(id.clone(), verdict, reasons, closed, keep).await?;
        if withdrawn {
            // A held scope's reviewer program has no one to answer to.
            self.runner.cancel(&id);
        }
        Ok(out)
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
        closed: Option<Arc<Frozen>>,
        keep: Option<Kept>,
    ) -> error::Result<Outcome> {
        let (children, caps) = (self.children.clone(), self.diff);
        self.blocking(move |v| {
            // A commit applies the frozen record: the one the review read, else the scope's.
            let frozen = match (verdict, closed) {
                (Decision::Commit, Some(f)) => Some(f),
                (Decision::Commit, None) => Some(v.frozen(&id, caps)?),
                _ => None,
            };
            let Some(keep) = keep else {
                return apply_now(v, &children, &id, verdict, reasons, frozen, false).map(|(out, _)| out);
            };
            let seq = v.history.intend(
                &keep.who.id,
                &keep.who.session,
                keep.who.token_sha256.as_deref(),
                &keep.entry(keep.intended(&id, verdict, &reasons)),
            )?;
            crate::fault::hit("intended", 0)?;
            let (out, finish) = match apply_now(v, &children, &id, verdict, reasons, frozen, true) {
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
        let mut sent: Option<Vec<String>> = None;
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
                Ok((Some(hold), _)) if sent.as_ref() != Some(&hold.reviewers) => {
                    sent = Some(hold.reviewers.clone());
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

    /// `reviewer`'s verdict on held scope `id`: from the review socket, or (`token`) from
    /// a delegate; after the last pending reviewer, the scope is decided with the
    /// verdict of every reviewer and their reasons.
    pub async fn review(
        self: &Arc<Self>,
        id: String,
        reviewer: String,
        token: Option<String>,
        verdict: Decision,
        reasons: Vec<String>,
        over: bool,
    ) -> error::Result<Outcome> {
        let _lock = self.locks.lock(&id).await;
        let scope = id.clone();
        let who = self.blocking(move |v| v.identity(&scope)).await?;
        let scope = id.clone();
        let reviewed = self
            .blocking(move |v| {
                let tokened = token.is_some();
                let giver = match token {
                    Some(t) => Giver::Token(crate::views::token_sha256(&t)),
                    None => Giver::Socket,
                };
                let command = matches!(
                    v.reviewer(&reviewer),
                    Some(review::Reviewer {
                        kind: review::Kind::Command(_),
                        ..
                    })
                );
                match v.review_scope(&scope, &reviewer, giver, verdict, reasons, over) {
                    // With a token every refusal reads the same, and is ledgered; so is a
                    // command reviewer named without one.
                    Err(e) if !e.is_no_scope() && (tokened || (command && matches!(e, Error::Denied(_)))) => {
                        if !matches!(e, Error::Denied(_)) {
                            eprintln!("escrowd: reviewer: {scope}: {reviewer}: {e}");
                        }
                        v.deny_review(&scope, &reviewer);
                        Err(Error::Denied(format!(
                            "scope {scope}: {reviewer}: a delegate needs the run's current token"
                        )))
                    }
                    r => r,
                }
            })
            .await?;
        if !matches!(reviewed, Reviewed::Stale) {
            // The verdict ends or skips the reviewer whose program is running, if any.
            self.runner.cancel(&id);
        }
        let out = self.conclude(id, who, reviewed).await?;
        self.notify();
        Ok(out)
    }

    /// What a taken verdict leads to: the next reviewer's run, or the scope's decision.
    async fn conclude(self: &Arc<Self>, id: String, who: Identity, reviewed: Reviewed) -> error::Result<Outcome> {
        match reviewed {
            Reviewed::Stale => Err(Error::State(format!("scope {id}: the review is no longer current"))),
            Reviewed::Held(hold) => {
                self.next_run(&id).await;
                Ok(Outcome::held(&id, &hold))
            }
            Reviewed::Final(hold) => {
                let change_set = self.closed_record(&id).await?;
                let reasons = hold
                    .reviews
                    .iter()
                    .flat_map(|r| r.reasons.iter().map(move |s| format!("{}: {s}", r.reviewer)))
                    .collect();
                let keep = Kept {
                    who,
                    change_set,
                    reviews: hold.reviews,
                };
                self.apply(id, hold.verdict, reasons, None, Some(keep)).await
            }
        }
    }

    // ---- command reviewers ----

    /// Pick up the reviews of the scopes held at start, once, before serving. Each held
    /// scope's first pending reviewer, in a locked step of the scope:
    ///
    /// - not in the policy any more: fails with `not in the policy`;
    /// - a socket reviewer: nothing, it answers when it comes;
    /// - a command reviewer with no run, or whose run was cut off: runs (again), one
    ///   attempt up (a delegate gets a new token);
    /// - a delegate waiting for its token: the token still works; its expiry timer is
    ///   armed again, or fires at once if the deadline passed while the daemon was down.
    ///
    /// A scope that cannot be resumed is reported on stderr and keeps its hold.
    pub async fn resume(self: &Arc<Self>) -> error::Result<()> {
        for held in self.list_held().await? {
            if let Err(e) = self.resume_scope(&held.id).await {
                eprintln!("escrowd: reviewer: {}: cannot resume its review: {e}", held.id);
            }
        }
        Ok(())
    }

    async fn resume_scope(self: &Arc<Self>, id: &str) -> error::Result<()> {
        let lock = self.locks.lock(id).await;
        let scope = id.to_string();
        let who = self.blocking(move |v| v.identity(&scope)).await?;
        let Some(hold) = who.hold.clone() else {
            return Ok(());
        };
        let Some(name) = hold.reviewers.first().cloned() else {
            return Ok(());
        };
        let Some(reviewer) = self.views.reviewer(&name) else {
            let (scope, missing) = (id.to_string(), name.clone());
            let cause = "not in the policy".to_string();
            let reasons = vec![format!("failed: {cause}")];
            let reviewed = self
                .blocking(move |v| {
                    let giver = Giver::Missing { cause };
                    v.review_scope(
                        &scope,
                        &missing,
                        giver,
                        Decision::Discard.max(hold.verdict),
                        reasons,
                        false,
                    )
                })
                .await?;
            self.conclude(id.to_string(), who, reviewed).await?;
            self.notify();
            return Ok(());
        };
        let review::Kind::Command(command) = reviewer.kind else {
            return Ok(());
        };
        let waiting = hold
            .run
            .filter(|r| r.reviewer == name && r.waiting && command.delegate)
            .and_then(|r| Some((r.attempt, r.deadline_ms?)));
        let Some((attempt, deadline_ms)) = waiting else {
            self.next_run(id).await;
            return Ok(());
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        match deadline_ms.checked_sub(now_ms).filter(|left| *left > 0) {
            Some(left) => self.arm_expiry(id, &name, attempt, command, std::time::Duration::from_millis(left)),
            None => {
                // `expire` takes the scope's lock itself.
                drop(lock);
                self.clone().expire(id.to_string(), name, attempt, command).await;
            }
        }
        Ok(())
    }

    /// If the first pending reviewer of held scope `id` is a command reviewer, record its
    /// run, write its files and start its program as a task. Runs in the locked step
    /// that makes the reviewer next. A failure to set the run up is reported on stderr:
    /// the scope stays held and its reviewer runs at the next start.
    async fn next_run(self: &Arc<Self>, id: &str) {
        if *self.stopping.borrow() {
            return;
        }
        let (scope, caps) = (id.to_string(), self.diff);
        let prepared = self
            .blocking(move |v| {
                let who = v.identity(&scope)?;
                let Some(hold) = who.hold.clone() else {
                    return Ok(None);
                };
                let Some(name) = hold.reviewers.first() else {
                    return Ok(None);
                };
                let Some(review::Reviewer {
                    kind: review::Kind::Command(command),
                    ..
                }) = v.reviewer(name)
                else {
                    return Ok(None);
                };
                let frozen = v.frozen(&scope, caps)?;
                let change_set = convert::change_set(v, scope.clone(), &frozen.change_set, &frozen.diff);
                let history = v.history.session(&who.session, HISTORY)?;
                let attempt = hold.next_attempt(name);
                let stdin = convert::review_request(&who, name, attempt, &hold, &change_set, &history)
                    .map_err(|e| Error::Io(e.into()))?;
                let change_set_json = serde_json::to_vec(&change_set).map_err(|e| Error::Io(e.into()))?;
                let ticket = v.start_run(
                    &scope,
                    name,
                    command.delegate,
                    &[
                        ("change_set.json", change_set_json.as_slice()),
                        ("diff.patch", frozen.diff.as_bytes()),
                    ],
                )?;
                Ok(Some((name.clone(), command, ticket, stdin)))
            })
            .await;
        let (reviewer, command, ticket, stdin) = match prepared {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                eprintln!("escrowd: reviewer: {id}: cannot start the next reviewer: {e}");
                return;
            }
        };
        let request = RunRequest {
            scope: id.to_string(),
            reviewer: reviewer.clone(),
            attempt: ticket.attempt,
            cwd: ticket.cwd,
            stdin,
            token: ticket.token,
        };
        let me = self.clone();
        let (id, attempt) = (id.to_string(), ticket.attempt);
        tokio::spawn(async move {
            if *me.stopping.borrow() {
                return;
            }
            let report = me.runner.run(&command, request).await;
            me.finish_run(id, reviewer, attempt, command, report).await;
        });
    }

    /// A run's report, in a locked step of its own. A report of a run that is no longer
    /// the scope's current one (or of a scope that is gone) is dropped; a cancelled run
    /// is never applied. A failed run gives its reviewer's `on_failure` verdict, never
    /// looser than the verdict so far, with the reason `failed: <cause>`.
    ///
    /// Boxed: a run's report can start the next run, which spawns this task.
    fn finish_run(
        self: Arc<Self>,
        id: String,
        reviewer: String,
        attempt: u32,
        command: review::Command,
        report: RunReport,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            let RunReport { pid, result } = report;
            let answer = match result {
                Err(Failure::Cancelled) => return,
                Ok(Answer::Verdict(verdict, reasons)) => Some(RunAnswer::Verdict(verdict, reasons)),
                // A delegate hands the verdict over: it comes by token.
                Ok(Answer::Pending(_)) => None,
                Err(f) => Some(RunAnswer::Failed(f.cause(command.timeout), command.on_failure)),
            };
            let _lock = self.locks.lock(&id).await;
            if *self.stopping.borrow() {
                return;
            }
            let applied = match answer {
                None => self.await_run(&id, &reviewer, attempt, pid, command).await,
                Some(answer) => self.apply_run(&id, &reviewer, attempt, pid, answer).await,
            };
            match applied {
                Ok(true) => self.notify(),
                Ok(false) => {}
                Err(e) if e.is_no_scope() => {}
                Err(e) => eprintln!("escrowd: reviewer: {id}: {reviewer}: {e}"),
            }
        })
    }

    /// Give the run's answer to the scope as `reviewer`'s verdict; false if the run is stale.
    async fn apply_run(
        self: &Arc<Self>,
        id: &str,
        reviewer: &str,
        attempt: u32,
        pid: Option<u32>,
        answer: RunAnswer,
    ) -> error::Result<bool> {
        let scope = id.to_string();
        let who = self.blocking(move |v| v.identity(&scope)).await?;
        let Some(so_far) = who.hold.as_ref().map(|h| h.verdict) else {
            return Ok(false);
        };
        let (verdict, reasons, failure) = match answer {
            RunAnswer::Verdict(verdict, reasons) => (verdict, reasons, None),
            RunAnswer::Failed(cause, on_failure) => {
                (on_failure.max(so_far), vec![format!("failed: {cause}")], Some(cause))
            }
        };
        let (scope, name) = (id.to_string(), reviewer.to_string());
        let reviewed = self
            .blocking(move |v| {
                let giver = Giver::Run { attempt, pid, failure };
                v.review_scope(&scope, &name, giver, verdict, reasons, false)
            })
            .await?;
        if matches!(reviewed, Reviewed::Stale) {
            return Ok(false);
        }
        self.conclude(id.to_string(), who, reviewed).await?;
        Ok(true)
    }
    /// A delegate's `pending` answer: the run waits for its token until the reviewer's
    /// timeout from now. Starts the timer that expires the token; false if the run is
    /// stale (a token verdict or a withdrawal came first).
    async fn await_run(
        self: &Arc<Self>,
        id: &str,
        reviewer: &str,
        attempt: u32,
        pid: Option<u32>,
        command: review::Command,
    ) -> error::Result<bool> {
        let deadline = std::time::SystemTime::now() + command.timeout;
        let deadline_ms = deadline
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let (scope, name) = (id.to_string(), reviewer.to_string());
        let waiting = self
            .blocking(move |v| v.await_delegate(&scope, &name, attempt, pid, deadline_ms))
            .await?;
        if waiting {
            let delay = command.timeout;
            self.arm_expiry(id, reviewer, attempt, command, delay);
        }
        Ok(waiting)
    }

    /// Expire a waiting delegate's token after `delay`, unless shutdown comes first.
    fn arm_expiry(
        self: &Arc<Self>,
        id: &str,
        reviewer: &str,
        attempt: u32,
        command: review::Command,
        delay: std::time::Duration,
    ) {
        let me = self.clone();
        let (id, reviewer) = (id.to_string(), reviewer.to_string());
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = me.stopped() => return,
            }
            me.expire(id, reviewer, attempt, command).await;
        });
    }

    /// A waiting delegate's token ran out: in a locked step of its own, if the same run
    /// still waits (no token verdict, withdrawal or newer run came first), the reviewer's
    /// `on_failure` verdict applies, never looser than the verdict so far.
    async fn expire(self: Arc<Self>, id: String, reviewer: String, attempt: u32, command: review::Command) {
        let _lock = self.locks.lock(&id).await;
        if *self.stopping.borrow() {
            return;
        }
        let cause = format!("no verdict within {}", format_duration(command.timeout));
        match self
            .apply_expiry(&id, &reviewer, attempt, cause, command.on_failure)
            .await
        {
            Ok(true) => self.notify(),
            Ok(false) => {}
            Err(e) if e.is_no_scope() => {}
            Err(e) => eprintln!("escrowd: reviewer: {id}: {reviewer}: {e}"),
        }
    }

    async fn apply_expiry(
        self: &Arc<Self>,
        id: &str,
        reviewer: &str,
        attempt: u32,
        cause: String,
        on_failure: Decision,
    ) -> error::Result<bool> {
        let scope = id.to_string();
        let who = self.blocking(move |v| v.identity(&scope)).await?;
        let Some(so_far) = who.hold.as_ref().map(|h| h.verdict) else {
            return Ok(false);
        };
        let (scope, name) = (id.to_string(), reviewer.to_string());
        let reasons = vec![format!("failed: {cause}")];
        let reviewed = self
            .blocking(move |v| {
                let giver = Giver::Expired { attempt, cause };
                v.review_scope(&scope, &name, giver, on_failure.max(so_far), reasons, false)
            })
            .await?;
        if matches!(reviewed, Reviewed::Stale) {
            return Ok(false);
        }
        self.conclude(id.to_string(), who, reviewed).await?;
        Ok(true)
    }
}

/// What a finished run gives its reviewer's verdict from.
enum RunAnswer {
    Verdict(Decision, Vec<String>),
    /// The cause, and the verdict a failure of this reviewer turns into.
    Failed(String, Decision),
}

/// `Decisions::apply` on the blocking pool. With `keep`, a committed scope stays until
/// `finish_commit`, and the second value is the generation to finish it with.
fn apply_now(
    v: &Views,
    children: &Children,
    id: &str,
    verdict: Decision,
    reasons: Vec<String>,
    frozen: Option<Arc<Frozen>>,
    keep: bool,
) -> error::Result<(Outcome, Option<Option<u64>>)> {
    let mut out = Outcome {
        reasons,
        ..Outcome::new(id, Status::Discarded)
    };
    let mut finish = None;
    match verdict {
        Decision::Commit => {
            let frozen = frozen.expect("apply resolves the frozen record for a commit");
            let committed = match keep {
                true => v.commit_kept(id, &frozen.change_set),
                false => v.commit_closed(id, &frozen.change_set),
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

    #[tokio::test]
    async fn a_cancelled_waiter_leaves_no_entry() {
        let locks = Arc::new(Locks::default());
        let first = locks.lock("s1").await;
        let waiter = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _second = locks.lock("s1").await;
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(first); // the holder leaves while the waiter is queued
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(locks.len(), 0);
    }
}

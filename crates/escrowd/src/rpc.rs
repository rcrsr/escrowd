//! gRPC services over Unix sockets: `EscrowService` for clients, `ReviewerService` on
//! the review socket (`<socket>.review`, mode 0600) for reviewers of held scopes.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};
use prost::Message;
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tonic::{Request, Response, Status};

use crate::error::{self, Error};
use crate::exec::Children;
use crate::policy::ConflictVerdict;
use crate::proto::escrow_service_server::{EscrowService, EscrowServiceServer};
use crate::proto::reviewer_service_server::{ReviewerService, ReviewerServiceServer};
use crate::proto::*;
use crate::review::{self as rules, Decision};
use crate::store::Hold;
use crate::views::{self, Identity, Reviewed, UNSCOPED, Unscoped, Views};
use crate::{changeset, commit, diff, policy};

/// The session's earlier decisions a reviewer gets.
const HISTORY: usize = 20;

/// How long shutdown waits for clients to finish their calls before it unmounts anyway.
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Served on the protocol socket (`EscrowService`) and the review socket (`ReviewerService`).
#[derive(Clone)]
pub struct Service(Arc<Shared>);

pub struct Shared {
    views: Arc<Views>,
    children: Arc<Children>,
    diff: diff::Caps,
    /// Bumped after every decision and review: an open waiting behind a held scope,
    /// and the clients following one (AwaitDecision), check again.
    changed: tokio::sync::watch::Sender<u64>,
    /// Reviews, and an opener's withdrawal of a held scope, one at a time.
    reviewing: tokio::sync::Mutex<()>,
    /// Set at shutdown: calls that wait (an open behind a held scope, AwaitDecision)
    /// end with UNAVAILABLE, or the server would wait on them forever.
    stopping: tokio::sync::watch::Sender<bool>,
}

impl std::ops::Deref for Service {
    type Target = Shared;
    fn deref(&self) -> &Shared {
        &self.0
    }
}

/// The review socket beside the protocol socket.
pub fn review_socket(socket: &Path) -> PathBuf {
    let mut s = socket.as_os_str().to_owned();
    s.push(".review");
    PathBuf::from(s)
}

/// When the caller stops waiting (`grpc-timeout`), less a margin for the reply to
/// reach it; None: no deadline.
fn deadline(md: &tonic::metadata::MetadataMap) -> Option<tokio::time::Instant> {
    use std::time::Duration;
    let v = md.get("grpc-timeout")?.to_str().ok()?;
    let (n, unit) = v.split_at(v.len().checked_sub(1)?);
    let n: u64 = n.parse().ok()?;
    let d = match unit {
        "H" => Duration::from_secs(n.saturating_mul(3600)),
        "M" => Duration::from_secs(n.saturating_mul(60)),
        "S" => Duration::from_secs(n),
        "m" => Duration::from_millis(n),
        "u" => Duration::from_micros(n),
        "n" => Duration::from_nanos(n),
        _ => return None,
    };
    Some(tokio::time::Instant::now() + d.saturating_sub(Duration::from_millis(100)))
}

/// The protocol's status code of each daemon error (`docs/protocol.md`).
fn status(e: Error) -> Status {
    match e {
        Error::NoScope(m) => Status::not_found(m),
        Error::State(m) => Status::failed_precondition(m),
        Error::Denied(m) => Status::permission_denied(m),
        Error::Invalid(m) => Status::invalid_argument(m),
        Error::Aborted(m) => Status::aborted(m),
        Error::Io(e) => Status::internal(e.to_string()),
    }
}

fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The change set with each path as the caller sees it (`~/x` in `$HOME`).
fn to_proto(views: &Views, scope_id: String, cs: changeset::ChangeSet, diff: String) -> ChangeSet {
    let kind = |k| match k {
        changeset::Kind::Create => ChangeKind::Create,
        changeset::Kind::Modify => ChangeKind::Modify,
        changeset::Kind::Delete => ChangeKind::Delete,
        changeset::Kind::Rename => ChangeKind::Rename,
    };
    ChangeSet {
        scope_id,
        changes: cs
            .changes
            .into_iter()
            .map(|c| Change {
                kind: kind(c.kind).into(),
                path: path_str(&views.shown(c.root, &c.path)),
                from_path: c
                    .from
                    .as_deref()
                    .map(|f| path_str(&views.shown(c.root, f)))
                    .unwrap_or_default(),
                writers: c.writers,
            })
            .collect(),
        reads: cs
            .reads
            .into_iter()
            .map(|(p, allowed)| Read {
                path: path_str(&p),
                decision: if allowed {
                    ReadDecision::Allow
                } else {
                    ReadDecision::Deny
                }
                .into(),
            })
            .collect(),
        labels: cs.labels,
        diff,
        unscoped_ops: cs.unscoped,
        review: Some(review_proto(cs.review)),
        processes: cs
            .procs
            .into_values()
            .map(|p| Process {
                id: p.id,
                pid: p.pid,
                program: path_str(&p.program),
                dev: p.dev,
                ino: p.ino,
                args: p.args,
                parent: p.parent,
            })
            .collect(),
    }
}

fn tier_proto(t: policy::Tier) -> Tier {
    match t {
        policy::Tier::Software => Tier::Software,
        policy::Tier::Llm => Tier::Llm,
        policy::Tier::Human => Tier::Human,
    }
}

/// The tiers a reviewer can give a verdict for.
fn tier_from(t: i32) -> Option<policy::Tier> {
    match Tier::try_from(t).ok()? {
        Tier::Llm => Some(policy::Tier::Llm),
        Tier::Human => Some(policy::Tier::Human),
        Tier::Software | Tier::Unspecified => None,
    }
}

fn verdict_proto(d: Decision) -> Verdict {
    match d {
        Decision::Commit => Verdict::Commit,
        Decision::Return => Verdict::Return,
        Decision::Discard => Verdict::Discard,
    }
}

fn tier_reviews(rs: &[rules::TierReview]) -> Vec<TierReview> {
    rs.iter()
        .map(|r| TierReview {
            tier: tier_proto(r.tier).into(),
            verdict: verdict_proto(r.verdict).into(),
            reasons: r.reasons.clone(),
            r#override: r.over,
        })
        .collect()
}

fn held_proto(i: Identity) -> HeldScope {
    let (tiers, wait, verdict, reviews, at) = match &i.hold {
        Some(h) => (h.tiers.as_slice(), h.wait, h.verdict, h.reviews.as_slice(), h.at_ms),
        None => (&[][..], false, Decision::Commit, &[][..], 0),
    };
    HeldScope {
        tiers: tiers.iter().map(|t| tier_proto(*t).into()).collect(),
        wait,
        verdict: verdict_proto(verdict).into(),
        reviews: tier_reviews(reviews),
        held_at_ms: at,
        scope_id: i.id,
        name: i.name,
        labels: i.labels,
        session: i.session,
    }
}

fn held_outcome(id: String, hold: &Hold) -> Outcome {
    Outcome {
        scope_id: id,
        status: OutcomeStatus::Held.into(),
        tiers: hold.tiers.iter().map(|t| tier_proto(*t).into()).collect(),
        wait: hold.wait,
        ..Default::default()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn review_proto(r: rules::Review) -> Review {
    Review {
        verdict: verdict_proto(r.verdict).into(),
        reasons: r.reasons(),
        tiers: r.tiers.into_iter().map(|t| tier_proto(t).into()).collect(),
        wait_required: r.wait,
    }
}

/// A decision the history keeps: of a scope that has a session or was held.
pub struct Kept {
    who: Identity,
    /// As decided, diff included; None for an open scope.
    change_set: Option<ChangeSet>,
    reviews: Vec<rules::TierReview>,
}

impl Kept {
    fn entry(&self, outcome: Outcome) -> Vec<u8> {
        Decided {
            scope_id: self.who.id.clone(),
            name: self.who.name.clone(),
            change_set: self.change_set.clone(),
            outcome: Some(outcome),
            reviews: tier_reviews(&self.reviews),
            decided_at_ms: now_ms(),
        }
        .encode_to_vec()
    }

    /// The outcome `verdict` gets if it applies with no conflict.
    fn intended(&self, id: &str, verdict: Verdict, reasons: &[String]) -> Outcome {
        let (status, paths) = match verdict {
            Verdict::Commit => (
                OutcomeStatus::Committed,
                self.change_set
                    .iter()
                    .flat_map(|cs| cs.changes.iter().map(|c| c.path.clone()))
                    .collect(),
            ),
            Verdict::Return => (OutcomeStatus::Returned, vec![]),
            _ => (OutcomeStatus::Discarded, vec![]),
        };
        Outcome {
            scope_id: id.to_string(),
            status: status.into(),
            paths,
            reasons: reasons.to_vec(),
            reopened: verdict == Verdict::Return,
            ..Default::default()
        }
    }
}

/// `Service::apply` on the blocking pool. With `keep`, a committed scope stays until
/// `finish_commit`, and the second value is the generation to finish it with.
fn apply_now(
    v: &Views,
    children: &Children,
    id: &str,
    verdict: Verdict,
    mut reasons: Vec<String>,
    closed: Option<changeset::ChangeSet>,
    keep: bool,
) -> error::Result<(Outcome, Option<Option<u64>>)> {
    let (mut paths, mut reopened, mut finish) = (vec![], false, None);
    let status = match verdict {
        Verdict::Commit => {
            let committed = match keep {
                true => v.commit_kept(id, closed.as_ref()),
                false => closed.map_or_else(|| v.commit_scope(id), |cs| v.commit_closed(id, &cs)),
            };
            let outcome = committed.inspect(|_| children.release(id))?;
            match outcome {
                commit::Outcome::Committed(generation, changed) => {
                    paths = changed.iter().map(|p| path_str(p)).collect();
                    finish = keep.then_some(generation);
                    OutcomeStatus::Committed
                }
                commit::Outcome::Conflict(conflicts) => {
                    reopened = v.conflict().verdict == ConflictVerdict::Return;
                    paths = conflicts.iter().map(|p| path_str(p)).collect();
                    reasons = paths
                        .iter()
                        .map(|p| format!("conflict: {p} changed in the project since the scope read it"))
                        .collect();
                    OutcomeStatus::Conflict
                }
            }
        }
        Verdict::Return => {
            v.reopen_scope(id)?;
            children.release(id);
            reopened = true;
            OutcomeStatus::Returned
        }
        _ => {
            if v.scope(id).is_err() {
                return Err(Error::no_scope(id));
            }
            children.stop(id);
            let r = v.drop_scope(id);
            children.release(id);
            r?;
            OutcomeStatus::Discarded
        }
    };
    let out = Outcome {
        scope_id: id.to_string(),
        status: status.into(),
        paths,
        reasons,
        reopened,
        ..Default::default()
    };
    Ok((out, finish))
}

impl Service {
    pub fn new(views: Arc<Views>, children: Arc<Children>, diff: diff::Caps) -> Self {
        Service(Arc::new(Shared {
            views,
            children,
            diff,
            changed: tokio::sync::watch::Sender::new(0),
            reviewing: tokio::sync::Mutex::new(()),
            stopping: tokio::sync::watch::Sender::new(false),
        }))
    }

    fn stop(&self) {
        self.stopping.send_replace(true);
    }

    /// Resolves once shutdown starts.
    async fn stopped(&self) {
        let _ = self.stopping.subscribe().wait_for(|s| *s).await;
    }

    fn notify(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Stop the scope's children (their last writes flush on exit), then freeze it.
    async fn close(&self, id: String, token: String) -> Result<ChangeSet, Status> {
        let (children, caps) = (self.children.clone(), self.diff);
        let id2 = id.clone();
        let (cs, diff) = self
            .blocking(move |v| {
                v.check_token(&id2, &token)?;
                let stopped = children.stop(&id2);
                let cs = v.close_scope_after(&id2, &stopped)?;
                let diff = v.diff(&id2, &cs, caps)?;
                Ok((cs, diff))
            })
            .await?;
        Ok(to_proto(&self.views, id, cs, diff))
    }

    /// A closed scope's change set and diff; None if it is open.
    async fn closed_proto(&self, id: &str) -> Result<Option<ChangeSet>, Status> {
        let (id2, caps) = (id.to_string(), self.diff);
        let got = self
            .blocking(move |v| match v.closed_change_set(&id2) {
                Ok(cs) => {
                    let diff = v.diff(&id2, &cs, caps)?;
                    Ok(Some((cs, diff)))
                }
                Err(Error::State(_)) => Ok(None),
                Err(e) => Err(e),
            })
            .await?;
        Ok(got.map(|(cs, diff)| to_proto(&self.views, id.to_string(), cs, diff)))
    }

    /// The opener's decision: a commit runs the review and may hold the scope; a held
    /// scope can only be withdrawn (discarded).
    async fn decide_scope(&self, req: DecideRequest, keep: Option<Kept>) -> Result<Outcome, Status> {
        let (id, mut reasons) = (req.scope_id.clone(), req.reasons);
        let mut verdict = Verdict::try_from(req.verdict).unwrap_or(Verdict::Unspecified);
        let wait = req.wait;
        // A held scope is its reviewers': the opener can only withdraw it.
        if verdict != Verdict::Discard {
            let scope = id.clone();
            if let Some(hold) = self.blocking(move |v| v.held(&scope)).await? {
                return Err(Status::permission_denied(format!(
                    "scope {id} is held for {}: only its reviewers commit or return it; its opener can discard it",
                    hold.tiers[0].as_str()
                )));
            }
        }
        // The review's verdict only tightens: a discard by the software tier wins, and
        // a commit of a change set that needs a tier above software holds it.
        let mut closed = None;
        if matches!(verdict, Verdict::Commit | Verdict::Return) && self.views.reviews() {
            let scope = id.clone();
            let cs = self.blocking(move |v| v.closed_change_set(&scope)).await?;
            let r = &cs.review;
            if r.verdict == Decision::Discard {
                verdict = Verdict::Discard;
                reasons = r.reasons();
            } else if verdict == Verdict::Commit && !r.tiers.is_empty() {
                let (scope, tiers, wait) = (id.clone(), r.tiers.clone(), r.wait || wait);
                let hold = self.blocking(move |v| v.hold_scope(&scope, tiers, wait)).await?;
                return Ok(held_outcome(id, &hold));
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
        verdict: Verdict,
        reasons: Vec<String>,
        closed: Option<changeset::ChangeSet>,
        keep: Option<Kept>,
    ) -> Result<Outcome, Status> {
        if verdict == Verdict::Unspecified {
            return Err(Status::invalid_argument("verdict must be set"));
        }
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

    /// Send a client following scope `id` its hold's updates, then its first kept
    /// decision after `after` (history sequence), and stop.
    async fn follow(&self, id: String, after: u64, tx: mpsc::Sender<Result<AwaitDecisionResponse, Status>>) {
        let send = |out: Result<Outcome, Status>| tx.send(out.map(|o| AwaitDecisionResponse { outcome: Some(o) }));
        let mut changed = self.changed.subscribe();
        let mut sent: Option<Vec<policy::Tier>> = None;
        loop {
            changed.borrow_and_update();
            let id2 = id.clone();
            let state = self
                .blocking(move |v| {
                    let held = match v.held(&id2) {
                        Ok(h) => h,
                        Err(e) if e.is_no_scope() => None,
                        Err(e) => return Err(e),
                    };
                    let decided = match held {
                        Some(_) => None,
                        None => v.history.latest(&id2)?.filter(|l| l.seq > after),
                    };
                    Ok((held, decided))
                })
                .await;
            match state {
                Err(e) => {
                    let _ = send(Err(e)).await;
                    return;
                }
                Ok((Some(hold), _)) if sent.as_ref() != Some(&hold.tiers) => {
                    sent = Some(hold.tiers.clone());
                    if send(Ok(held_outcome(id.clone(), &hold))).await.is_err() {
                        return;
                    }
                }
                Ok((_, Some(l))) => {
                    let out = Decided::decode(&l.entry[..]).ok().and_then(|d| d.outcome);
                    let _ = send(out.ok_or_else(|| Status::internal(format!("scope {id}: unreadable history entry"))))
                        .await;
                    return;
                }
                // Not held and no new decision yet: the decider keeps it next.
                Ok(_) => {}
            }
            tokio::select! {
                r = changed.changed() => if r.is_err() { return },
                _ = tx.closed() => return,
                _ = self.stopped() => {
                    let _ = send(Err(Status::unavailable("escrowd is shutting down"))).await;
                    return;
                }
            }
        }
    }

    /// Filesystem work runs off the async executor.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Views) -> error::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        let views = self.views.clone();
        tokio::task::spawn_blocking(move || f(&views))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(status)
    }
}

#[tonic::async_trait]
impl EscrowService for Service {
    async fn ping(&self, _req: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: crate::PROTOCOL_VERSION,
        }))
    }

    /// In a session, wait while another of its scopes is held with a wait.
    async fn open_scope(&self, req: Request<OpenScopeRequest>) -> Result<Response<OpenScopeResponse>, Status> {
        let until = deadline(req.metadata());
        let req = req.into_inner();
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
            let Some(held) = self.views.blocking_hold(&req.session) else {
                break;
            };
            tokio::select! {
                _ = changed.changed() => {}
                _ = &mut expired => {
                    return Err(Status::failed_precondition(format!(
                        "session {}: scope {held} is held for review; the next scope opens after its verdict",
                        req.session
                    )));
                }
                _ = self.stopped() => return Err(Status::unavailable("escrowd is shutting down")),
            }
        }
        let (opened, roots) = self
            .blocking(move |v| {
                let opened = v.open_scope(&req.name, &req.labels, &req.session)?;
                let roots = v.root_views(&opened.id);
                Ok((opened, roots))
            })
            .await?;
        Ok(Response::new(OpenScopeResponse {
            scope_id: opened.id,
            root: path_str(&opened.root),
            token: opened.token,
            roots: roots
                .into_iter()
                .map(|r| ScopeRoot {
                    path: path_str(&r.host),
                    view: path_str(&r.view),
                    direct: r.direct.iter().map(|p| path_str(p)).collect(),
                })
                .collect(),
        }))
    }

    async fn close_scope(&self, req: Request<CloseScopeRequest>) -> Result<Response<CloseScopeResponse>, Status> {
        let req = req.into_inner();
        Ok(Response::new(CloseScopeResponse {
            change_set: Some(self.close(req.scope_id, req.token).await?),
        }))
    }

    /// Decide; keep the decision of a scope in a session, or held; then wake the opens
    /// waiting behind a held scope and the clients following it.
    async fn decide(&self, req: Request<DecideRequest>) -> Result<Response<DecideResponse>, Status> {
        let req = req.into_inner();
        let (id, token) = (req.scope_id.clone(), req.token.clone());
        let who = self
            .blocking(move |v| {
                v.check_token(&id, &token)?;
                v.identity(&id)
            })
            .await?;
        let _withdrawing = match who.hold {
            Some(_) => Some(self.reviewing.lock().await),
            None => None,
        };
        let keep = match !who.session.is_empty() || who.hold.is_some() {
            true => Some(Kept {
                change_set: self.closed_proto(&who.id).await?,
                reviews: who.hold.as_ref().map(|h| h.reviews.clone()).unwrap_or_default(),
                who,
            }),
            false => None,
        };
        let out = self.decide_scope(req, keep).await?;
        if out.status() != OutcomeStatus::Held {
            self.notify();
        }
        Ok(Response::new(DecideResponse { outcome: Some(out) }))
    }

    /// Close the implicit default scope and return its change set; decide it as scope `unscoped`.
    async fn settle_unscoped(
        &self,
        _req: Request<SettleUnscopedRequest>,
    ) -> Result<Response<SettleUnscopedResponse>, Status> {
        if self.views.unscoped() != Unscoped::Implicit {
            return Err(Status::failed_precondition("settle_unscoped needs unscoped = implicit"));
        }
        Ok(Response::new(SettleUnscopedResponse {
            change_set: Some(self.close(UNSCOPED.to_string(), String::new()).await?),
        }))
    }

    async fn get_change_set(
        &self,
        req: Request<GetChangeSetRequest>,
    ) -> Result<Response<GetChangeSetResponse>, Status> {
        let (id, caps) = (req.into_inner().scope_id, self.diff);
        let id2 = id.clone();
        let (cs, diff) = self
            .blocking(move |v| {
                let cs = v.closed_change_set(&id2)?;
                let diff = v.diff(&id2, &cs, caps)?;
                Ok((cs, diff))
            })
            .await?;
        Ok(Response::new(GetChangeSetResponse {
            change_set: Some(to_proto(&self.views, id, cs, diff)),
        }))
    }

    type AwaitDecisionStream = ReceiverStream<Result<AwaitDecisionResponse, Status>>;

    /// A held scope's updates until its verdict; a decided scope's last outcome.
    async fn await_decision(
        &self,
        req: Request<AwaitDecisionRequest>,
    ) -> Result<Response<Self::AwaitDecisionStream>, Status> {
        let AwaitDecisionRequest { scope_id: id, token } = req.into_inner();
        let id2 = id.clone();
        // A review or a withdrawal drops the scope before it records the decision: read
        // between their steps, the scope would be neither live nor kept.
        let settled = self.reviewing.lock().await;
        let (live, latest) = self
            .blocking(move |v| {
                // The last kept decision first: a hold seen after it is decided later,
                // and kept with a newer sequence.
                let latest = v.history.latest(&id2)?;
                let live = match v.identity(&id2) {
                    Ok(who) => {
                        v.check_token(&id2, &token)?;
                        Some(who)
                    }
                    Err(e) if e.is_no_scope() => None,
                    Err(e) => return Err(e),
                };
                // Gone: decided, maybe since the first read.
                let latest = match live {
                    Some(_) => latest,
                    None => v.history.latest(&id2)?,
                };
                if live.is_none()
                    && let Some(l) = &latest
                    && l.token_sha256
                        .as_ref()
                        .is_some_and(|t| *t != views::token_sha256(&token))
                {
                    return Err(Error::Denied(format!("scope {id2}: missing or wrong token")));
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
            (Some(_), None) => return Err(Status::failed_precondition(format!("scope {id} is not held"))),
            (None, None) => return Err(Status::not_found(format!("no scope {id}"))),
        };
        let (tx, rx) = mpsc::channel(4);
        let service = self.clone();
        tokio::spawn(async move { service.follow(id, after, tx).await });
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[tonic::async_trait]
impl ReviewerService for Service {
    async fn list_held(&self, _req: Request<ListHeldRequest>) -> Result<Response<ListHeldResponse>, Status> {
        let held = self.blocking(|v| v.held_scopes()).await?;
        Ok(Response::new(ListHeldResponse {
            scopes: held.into_iter().map(held_proto).collect(),
        }))
    }

    async fn get_held(&self, req: Request<GetHeldRequest>) -> Result<Response<GetHeldResponse>, Status> {
        let id = req.into_inner().scope_id;
        let id2 = id.clone();
        let (who, history) = self
            .blocking(move |v| {
                let who = v.identity(&id2)?;
                let history = v.history.session(&who.session, HISTORY)?;
                Ok((who, history))
            })
            .await?;
        if who.hold.is_none() {
            return Err(Status::failed_precondition(format!("scope {id} is not held")));
        }
        let change_set = self.closed_proto(&id).await?;
        Ok(Response::new(GetHeldResponse {
            held: Some(held_proto(who)),
            change_set,
            history: history.iter().filter_map(|e| Decided::decode(&e[..]).ok()).collect(),
        }))
    }

    /// A tier's verdict; after the last pending tier, the scope is decided.
    async fn review(&self, req: Request<ReviewRequest>) -> Result<Response<ReviewResponse>, Status> {
        let req = req.into_inner();
        let tier = tier_from(req.tier).ok_or_else(|| Status::invalid_argument("tier must be llm or human"))?;
        let verdict = match Verdict::try_from(req.verdict).unwrap_or(Verdict::Unspecified) {
            Verdict::Commit => Decision::Commit,
            Verdict::Return => Decision::Return,
            Verdict::Discard => Decision::Discard,
            Verdict::Unspecified => return Err(Status::invalid_argument("verdict must be set")),
        };
        let _one = self.reviewing.lock().await;
        let id = req.scope_id.clone();
        let who = self.blocking(move |v| v.identity(&id)).await?;
        let id = req.scope_id.clone();
        let (reasons, over) = (req.reasons, req.r#override);
        let reviewed = self
            .blocking(move |v| v.review_scope(&id, tier, verdict, reasons, over))
            .await?;
        let id = req.scope_id;
        let out = match reviewed {
            Reviewed::Held(hold) => held_outcome(id, &hold),
            Reviewed::Final(hold) => {
                let change_set = self.closed_proto(&id).await?;
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
                self.apply(id, verdict_proto(hold.verdict), reasons, None, Some(keep))
                    .await?
            }
        };
        self.notify();
        Ok(Response::new(ReviewResponse { outcome: Some(out) }))
    }
}

/// Remove a stale socket file left by a dead daemon; fail if a live one still accepts
/// connections on it.
pub fn clear_stale(socket: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(socket).is_err() {
        return Ok(());
    }
    if std::os::unix::net::UnixStream::connect(socket).is_ok() {
        bail!("{} is in use by another daemon", socket.display());
    }
    std::fs::remove_file(socket).with_context(|| format!("removing stale socket {}", socket.display()))
}

/// Bind a Unix socket, replacing a stale socket file left by a previous daemon.
pub fn bind(socket: &Path) -> anyhow::Result<UnixListener> {
    clear_stale(socket)?;
    UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))
}

/// Bind `socket` with mode 0600: bound under a temporary name, then renamed, so no
/// other user can connect in between.
fn bind_private(socket: &Path) -> anyhow::Result<UnixListener> {
    let mut tmp = socket.as_os_str().to_owned();
    tmp.push(format!(".{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let listener = bind(&tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {}", tmp.display()))?;
    std::fs::rename(&tmp, socket).with_context(|| format!("renaming {} to {}", tmp.display(), socket.display()))?;
    Ok(listener)
}

/// Serve clients on `socket` and reviewers on its review socket until `shutdown`
/// resolves; the sockets are removed on exit. Waiting calls end at once; calls still
/// running after `GRACE` are abandoned, so the caller can unmount and flush.
pub async fn serve(socket: &Path, service: Service, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    let review_path = review_socket(socket);
    let review_listener = bind_private(&review_path)?;
    let listener = bind(socket)?;
    let started = service.clone();
    let reviewers = tonic::transport::Server::builder()
        .add_service(ReviewerServiceServer::new(service.clone()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(review_listener), async move {
            started.stopped().await
        });
    let stopper = service.clone();
    let clients = tonic::transport::Server::builder()
        .add_service(EscrowServiceServer::new(service.clone()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async move {
            shutdown.await;
            stopper.stop();
        });
    // The reviewers stop with the clients, also when the clients' server fails.
    let clients = async {
        let r = clients.await;
        service.stop();
        r
    };
    let both = async { tokio::join!(clients, reviewers) };
    let (result, reviewed) = tokio::select! {
        r = both => r,
        _ = async {
            service.stopped().await;
            tokio::time::sleep(GRACE).await;
        } => {
            eprintln!("escrowd: clients still busy {}s after shutdown; closing anyway", GRACE.as_secs());
            (Ok(()), Ok(()))
        }
    };
    let _ = std::fs::remove_file(socket);
    let _ = std::fs::remove_file(&review_path);
    result?;
    Ok(reviewed?)
}

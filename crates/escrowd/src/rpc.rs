//! gRPC service over a Unix socket.

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

use crate::exec::Children;
use crate::policy::ConflictVerdict;
use crate::proto::escrow_server::{Escrow, EscrowServer};
use crate::proto::*;
use crate::views::{UNSCOPED, Unscoped, Views};
use crate::{changeset, commit, diff, policy, review};

pub struct Service {
    views: Arc<Views>,
    children: Arc<Children>,
    diff: diff::Caps,
}

fn io_status(e: std::io::Error) -> Status {
    match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(e.to_string()),
        std::io::ErrorKind::InvalidInput => Status::failed_precondition(e.to_string()),
        std::io::ErrorKind::PermissionDenied => Status::permission_denied(e.to_string()),
        std::io::ErrorKind::Interrupted => Status::aborted(e.to_string()),
        _ => Status::internal(e.to_string()),
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
        review: cs.review.map(review_proto),
    }
}

fn tier_proto(t: policy::Tier) -> Tier {
    match t {
        policy::Tier::Software => Tier::Software,
        policy::Tier::Llm => Tier::Llm,
        policy::Tier::Human => Tier::Human,
    }
}

fn review_proto(r: review::Review) -> Review {
    Review {
        verdict: match r.verdict {
            review::Verdict::Commit => Verdict::Commit,
            review::Verdict::Discard => Verdict::Discard,
        }
        .into(),
        reasons: r.reasons(),
        tiers: r.tiers.into_iter().map(|t| tier_proto(t).into()).collect(),
        wait_required: r.wait,
    }
}

impl Service {
    pub fn new(views: Arc<Views>, children: Arc<Children>, diff: diff::Caps) -> Self {
        Service { views, children, diff }
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

    /// Filesystem work runs off the async executor.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Views) -> std::io::Result<T> + Send + 'static,
    ) -> Result<T, Status> {
        let views = self.views.clone();
        tokio::task::spawn_blocking(move || f(&views))
            .await
            .map_err(|e| Status::internal(e.to_string()))?
            .map_err(io_status)
    }
}

#[tonic::async_trait]
impl Escrow for Service {
    async fn ping(&self, _req: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: crate::PROTOCOL_VERSION,
        }))
    }

    async fn open_scope(&self, req: Request<OpenScopeRequest>) -> Result<Response<OpenScopeResponse>, Status> {
        let req = req.into_inner();
        let (opened, roots) = self
            .blocking(move |v| {
                let opened = v.open_scope(&req.name, &req.labels)?;
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

    async fn close_scope(&self, req: Request<CloseScopeRequest>) -> Result<Response<ChangeSet>, Status> {
        let req = req.into_inner();
        Ok(Response::new(self.close(req.scope_id, req.token).await?))
    }

    /// Commit applies a closed scope's change set (or reports the conflicting paths and
    /// drops or reopens the scope, per the policy's conflict.verdict); discard drops the
    /// scope (open or closed); return reopens a closed scope.
    async fn decide(&self, req: Request<DecideRequest>) -> Result<Response<Outcome>, Status> {
        let req = req.into_inner();
        let (id, token) = (req.scope_id.clone(), req.token.clone());
        self.blocking(move |v| v.check_token(&id, &token)).await?;
        let (id, mut reasons) = (req.scope_id.clone(), req.reasons);
        let (mut paths, mut reopened) = (vec![], false);
        let mut verdict = Verdict::try_from(req.verdict).unwrap_or(Verdict::Unspecified);
        // The review's verdict only tightens: a discard by the software tier wins, and
        // a change set that needs a tier above software is not the opener's to commit.
        let mut closed = None;
        if matches!(verdict, Verdict::Commit | Verdict::Return) && self.views.reviews() {
            let scope = id.clone();
            let cs = self.blocking(move |v| v.closed_change_set(&scope)).await?;
            let r = cs.review.as_ref().expect("a closed change set has a review");
            if r.verdict == review::Verdict::Discard {
                verdict = Verdict::Discard;
                reasons = r.reasons();
            } else if verdict == Verdict::Commit && !r.tiers.is_empty() {
                let tiers: Vec<String> = r.tiers.iter().map(|t| format!("{t:?}").to_lowercase()).collect();
                return Err(Status::failed_precondition(format!(
                    "scope {id} needs review by {}; only a discard or a return can decide it",
                    tiers.join(", ")
                )));
            }
            closed = Some(cs);
        }
        let status = match verdict {
            Verdict::Discard => {
                let children = self.children.clone();
                self.blocking(move |v| {
                    v.scope(&req.scope_id)
                        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, format!("no scope {}", req.scope_id)))?;
                    children.stop(&req.scope_id);
                    let r = v.drop_scope(&req.scope_id);
                    children.release(&req.scope_id);
                    r
                })
                .await?;
                OutcomeStatus::Discarded
            }
            Verdict::Return => {
                let children = self.children.clone();
                self.blocking(move |v| {
                    v.reopen_scope(&req.scope_id)?;
                    children.release(&req.scope_id);
                    Ok(())
                })
                .await?;
                reopened = true;
                OutcomeStatus::Returned
            }
            Verdict::Commit => {
                let scope = req.scope_id;
                let children = self.children.clone();
                let outcome = self
                    .blocking(move |v| {
                        match closed
                            .map_or_else(|| v.commit_scope(&scope), |cs| v.commit_closed(&scope, &cs))
                            .inspect(|_| children.release(&scope))
                        {
                            Err(e) if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput) => {
                                Err(io::Error::new(
                                    io::ErrorKind::Interrupted,
                                    format!("commit rolled back: {e}"),
                                ))
                            }
                            r => r,
                        }
                    })
                    .await?;
                match outcome {
                    commit::Outcome::Committed(_, changed) => {
                        paths = changed.iter().map(|p| path_str(p)).collect();
                        OutcomeStatus::Committed
                    }
                    commit::Outcome::Conflict(conflicts) => {
                        reopened = self.views.conflict().verdict == ConflictVerdict::Return;
                        paths = conflicts.iter().map(|p| path_str(p)).collect();
                        reasons = paths
                            .iter()
                            .map(|p| format!("conflict: {p} changed in the project since the scope read it"))
                            .collect();
                        OutcomeStatus::Conflict
                    }
                }
            }
            Verdict::Unspecified => return Err(Status::invalid_argument("verdict must be set")),
        };
        Ok(Response::new(Outcome {
            scope_id: id,
            status: status.into(),
            paths,
            reasons,
            reopened,
        }))
    }

    /// Close the implicit default scope and return its change set; decide it as scope `unscoped`.
    async fn settle_unscoped(&self, _req: Request<SettleUnscopedRequest>) -> Result<Response<ChangeSet>, Status> {
        if self.views.unscoped() != Unscoped::Implicit {
            return Err(Status::failed_precondition("settle_unscoped needs unscoped = implicit"));
        }
        Ok(Response::new(self.close(UNSCOPED.to_string(), String::new()).await?))
    }

    async fn get_change_set(&self, req: Request<GetChangeSetRequest>) -> Result<Response<ChangeSet>, Status> {
        let (id, caps) = (req.into_inner().scope_id, self.diff);
        let id2 = id.clone();
        let (cs, diff) = self
            .blocking(move |v| {
                let cs = v.closed_change_set(&id2)?;
                let diff = v.diff(&id2, &cs, caps)?;
                Ok((cs, diff))
            })
            .await?;
        Ok(Response::new(to_proto(&self.views, id, cs, diff)))
    }
}

/// Bind a Unix socket, replacing a stale socket file left by a previous daemon.
pub fn bind(socket: &Path) -> anyhow::Result<UnixListener> {
    if socket.exists() {
        std::fs::remove_file(socket).with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))
}

/// Serve on `socket` until `shutdown` resolves; the socket is removed on exit.
pub async fn serve(socket: &Path, service: Service, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    let listener = bind(socket)?;
    let result = tonic::transport::Server::builder()
        .add_service(EscrowServer::new(service))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await;
    let _ = std::fs::remove_file(socket);
    Ok(result?)
}

//! gRPC service over a Unix socket.

use std::io;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

use crate::exec::Children;
use crate::proto::escrow_server::{Escrow, EscrowServer};
use crate::proto::*;
use crate::views::{UNSCOPED, Unscoped, Views};
use crate::{changeset, commit};

pub struct Service {
    views: Arc<Views>,
    children: Arc<Children>,
}

fn io_status(e: std::io::Error) -> Status {
    match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(e.to_string()),
        std::io::ErrorKind::InvalidInput => Status::failed_precondition(e.to_string()),
        std::io::ErrorKind::Interrupted => Status::aborted(e.to_string()),
        _ => Status::internal(e.to_string()),
    }
}

fn path_str(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

fn to_proto(scope_id: String, cs: changeset::ChangeSet) -> ChangeSet {
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
                path: path_str(&c.path),
                from_path: c.from.as_deref().map(path_str).unwrap_or_default(),
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
    }
}

impl Service {
    pub fn new(views: Arc<Views>, children: Arc<Children>) -> Self {
        Service { views, children }
    }

    /// Stop the scope's children (their last writes flush on exit), then freeze it.
    async fn close(&self, id: String) -> Result<ChangeSet, Status> {
        let children = self.children.clone();
        let id2 = id.clone();
        let cs = self
            .blocking(move |v| {
                v.scope(&id2)
                    .map_err(|_| io::Error::new(io::ErrorKind::NotFound, format!("no scope {id2}")))?;
                children.stop(&id2);
                v.close_scope(&id2)
            })
            .await?;
        Ok(to_proto(id, cs))
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
        let (scope_id, root) = self.blocking(move |v| v.open_scope(&req.name, &req.labels)).await?;
        Ok(Response::new(OpenScopeResponse {
            scope_id,
            root: path_str(&root),
        }))
    }

    async fn close_scope(&self, req: Request<CloseScopeRequest>) -> Result<Response<ChangeSet>, Status> {
        let id = req.into_inner().scope_id;
        Ok(Response::new(self.close(id).await?))
    }

    /// Commit applies a closed scope's change set (or reports the conflicting paths and
    /// drops the scope); discard drops the scope (open or closed); return reopens a closed scope.
    async fn decide(&self, req: Request<DecideRequest>) -> Result<Response<Outcome>, Status> {
        let req = req.into_inner();
        let (id, mut reasons) = (req.scope_id.clone(), req.reasons);
        let mut paths = vec![];
        let status = match Verdict::try_from(req.verdict) {
            Ok(Verdict::Discard) => {
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
            Ok(Verdict::Return) => {
                let children = self.children.clone();
                self.blocking(move |v| {
                    v.reopen_scope(&req.scope_id)?;
                    children.release(&req.scope_id);
                    Ok(())
                })
                .await?;
                OutcomeStatus::Returned
            }
            Ok(Verdict::Commit) => {
                let scope = req.scope_id;
                let children = self.children.clone();
                let outcome = self
                    .blocking(
                        move |v| match v.commit_scope(&scope).inspect(|_| children.release(&scope)) {
                            Err(e) if !matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput) => {
                                Err(io::Error::new(
                                    io::ErrorKind::Interrupted,
                                    format!("commit rolled back: {e}"),
                                ))
                            }
                            r => r,
                        },
                    )
                    .await?;
                match outcome {
                    commit::Outcome::Committed(_, changed) => {
                        paths = changed.iter().map(|p| path_str(p)).collect();
                        OutcomeStatus::Committed
                    }
                    commit::Outcome::Conflict(conflicts) => {
                        paths = conflicts.iter().map(|p| path_str(p)).collect();
                        reasons = paths
                            .iter()
                            .map(|p| format!("conflict: {p} changed in the project since the scope read it"))
                            .collect();
                        OutcomeStatus::Conflict
                    }
                }
            }
            _ => return Err(Status::invalid_argument("verdict must be set")),
        };
        Ok(Response::new(Outcome {
            scope_id: id,
            status: status.into(),
            paths,
            reasons,
        }))
    }

    /// Close the implicit default scope and return its change set; decide it as scope `unscoped`.
    async fn settle_unscoped(&self, _req: Request<SettleUnscopedRequest>) -> Result<Response<ChangeSet>, Status> {
        if self.views.unscoped() != Unscoped::Implicit {
            return Err(Status::failed_precondition("settle_unscoped needs unscoped = implicit"));
        }
        Ok(Response::new(self.close(UNSCOPED.to_string()).await?))
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

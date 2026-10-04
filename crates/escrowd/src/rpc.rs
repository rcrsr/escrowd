//! gRPC service over a Unix socket.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

use crate::proto::escrow_server::{Escrow, EscrowServer};
use crate::proto::*;
use crate::views::Views;

pub struct Service {
    views: Arc<Views>,
}

fn io_status(e: std::io::Error) -> Status {
    match e.kind() {
        std::io::ErrorKind::NotFound => Status::not_found(e.to_string()),
        _ => Status::internal(e.to_string()),
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
        let (scope_id, root) = self.views.open_scope(&req.into_inner().name).map_err(io_status)?;
        Ok(Response::new(OpenScopeResponse {
            scope_id,
            root: root.to_string_lossy().into_owned(),
        }))
    }

    async fn close_scope(&self, _req: Request<CloseScopeRequest>) -> Result<Response<ChangeSet>, Status> {
        Err(Status::unimplemented("close_scope: phase 1.3"))
    }

    /// Phase 1.2 handles discard only: drop the scope and its staged changes.
    async fn decide(&self, req: Request<DecideRequest>) -> Result<Response<Outcome>, Status> {
        let req = req.into_inner();
        match Verdict::try_from(req.verdict) {
            Ok(Verdict::Discard) => {
                self.views.drop_scope(&req.scope_id).map_err(io_status)?;
                Ok(Response::new(Outcome {
                    scope_id: req.scope_id,
                    status: OutcomeStatus::Discarded.into(),
                    paths: vec![],
                    reasons: req.reasons,
                }))
            }
            Ok(Verdict::Commit | Verdict::Return) => Err(Status::unimplemented("commit and return: phase 1.3 and 1.4")),
            _ => Err(Status::invalid_argument("verdict must be set")),
        }
    }

    async fn spawn(&self, _req: Request<SpawnRequest>) -> Result<Response<SpawnResponse>, Status> {
        Err(Status::unimplemented("spawn: phase 1.5"))
    }

    async fn settle_unscoped(&self, _req: Request<SettleUnscopedRequest>) -> Result<Response<ChangeSet>, Status> {
        Err(Status::unimplemented("settle_unscoped: phase 1.5"))
    }
}

/// Serve on `socket` until `shutdown` resolves. A stale socket file left by a
/// previous daemon is replaced; the socket is removed on exit.
pub async fn serve(socket: &Path, views: Arc<Views>, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    if socket.exists() {
        std::fs::remove_file(socket).with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    let result = tonic::transport::Server::builder()
        .add_service(EscrowServer::new(Service { views }))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await;
    let _ = std::fs::remove_file(socket);
    Ok(result?)
}

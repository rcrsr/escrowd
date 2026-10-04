//! gRPC service over a Unix socket.

use std::path::Path;

use anyhow::Context;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

use crate::proto::escrow_server::{Escrow, EscrowServer};
use crate::proto::*;

#[derive(Default)]
pub struct Service;

#[tonic::async_trait]
impl Escrow for Service {
    async fn ping(&self, _req: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        Ok(Response::new(PingResponse {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: crate::PROTOCOL_VERSION,
        }))
    }

    async fn open_scope(&self, _req: Request<OpenScopeRequest>) -> Result<Response<OpenScopeResponse>, Status> {
        Err(Status::unimplemented("open_scope: phase 1.3"))
    }

    async fn close_scope(&self, _req: Request<CloseScopeRequest>) -> Result<Response<ChangeSet>, Status> {
        Err(Status::unimplemented("close_scope: phase 1.3"))
    }

    async fn decide(&self, _req: Request<DecideRequest>) -> Result<Response<Outcome>, Status> {
        Err(Status::unimplemented("decide: phase 1.3"))
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
pub async fn serve(socket: &Path, shutdown: impl Future<Output = ()>) -> anyhow::Result<()> {
    if socket.exists() {
        std::fs::remove_file(socket).with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    let result = tonic::transport::Server::builder()
        .add_service(EscrowServer::new(Service))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await;
    let _ = std::fs::remove_file(socket);
    Ok(result?)
}

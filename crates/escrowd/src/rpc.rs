//! gRPC services over Unix sockets: `EscrowService` for clients, `ReviewerService` on
//! the review socket (`<socket>.review`, mode 0600) for reviewers of held scopes.
//! Each call translates its messages and hands the work to `decision.rs`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, bail};
use prost::Message;
use tokio::net::UnixListener;
use tokio_stream::wrappers::{ReceiverStream, UnixListenerStream};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

use crate::convert;
use crate::decision::{Closed, Decisions};
use crate::diff;
use crate::error::Error;
use crate::exec::Children;
use crate::proto::escrow_service_server::{EscrowService, EscrowServiceServer};
use crate::proto::reviewer_service_server::{ReviewerService, ReviewerServiceServer};
use crate::proto::*;
use crate::views::{UNSCOPED, Views};

/// How long shutdown waits for clients to finish their calls before it unmounts anyway.
const GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// Served on the protocol socket (`EscrowService`) and the review socket (`ReviewerService`).
#[derive(Clone)]
pub struct Service(Arc<Decisions>);

impl Service {
    pub fn new(views: Arc<Views>, children: Arc<Children>, diff: diff::Caps) -> Self {
        Service(Arc::new(Decisions::new(views, children, diff)))
    }

    fn change_set(&self, id: String, c: Closed) -> ChangeSet {
        convert::change_set(self.0.views(), id, c.change_set, c.diff)
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
        Error::Stopping => Status::unavailable("escrowd is shutting down"),
        Error::Io(e) => Status::internal(e.to_string()),
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
        let (opened, roots) = self
            .0
            .open(req.name, req.labels, req.session, until)
            .await
            .map_err(status)?;
        Ok(Response::new(OpenScopeResponse {
            scope_id: opened.id,
            root: convert::path_str(&opened.root),
            token: opened.token,
            roots: roots
                .into_iter()
                .map(|r| ScopeRoot {
                    path: convert::path_str(&r.host),
                    view: convert::path_str(&r.view),
                    direct: r.direct.iter().map(|p| convert::path_str(p)).collect(),
                })
                .collect(),
        }))
    }

    async fn close_scope(&self, req: Request<CloseScopeRequest>) -> Result<Response<CloseScopeResponse>, Status> {
        let req = req.into_inner();
        let closed = self.0.close(req.scope_id.clone(), req.token).await.map_err(status)?;
        Ok(Response::new(CloseScopeResponse {
            change_set: Some(self.change_set(req.scope_id, closed)),
        }))
    }

    async fn decide(&self, req: Request<DecideRequest>) -> Result<Response<DecideResponse>, Status> {
        let req = req.into_inner();
        let verdict = convert::decision(req.verdict);
        let out = self
            .0
            .decide(req.scope_id, req.token, verdict, req.reasons, req.wait)
            .await
            .map_err(status)?;
        Ok(Response::new(DecideResponse {
            outcome: Some(convert::outcome(out)),
        }))
    }

    /// Close the implicit default scope and return its change set; decide it as scope `unscoped`.
    async fn settle_unscoped(
        &self,
        _req: Request<SettleUnscopedRequest>,
    ) -> Result<Response<SettleUnscopedResponse>, Status> {
        let closed = self.0.settle_unscoped().await.map_err(status)?;
        Ok(Response::new(SettleUnscopedResponse {
            change_set: Some(self.change_set(UNSCOPED.to_string(), closed)),
        }))
    }

    async fn get_change_set(
        &self,
        req: Request<GetChangeSetRequest>,
    ) -> Result<Response<GetChangeSetResponse>, Status> {
        let id = req.into_inner().scope_id;
        let closed = self.0.change_set(id.clone()).await.map_err(status)?;
        Ok(Response::new(GetChangeSetResponse {
            change_set: Some(self.change_set(id, closed)),
        }))
    }

    type AwaitDecisionStream = Pin<Box<dyn Stream<Item = Result<AwaitDecisionResponse, Status>> + Send>>;

    /// A held scope's updates until its verdict; a decided scope's last outcome.
    async fn await_decision(
        &self,
        req: Request<AwaitDecisionRequest>,
    ) -> Result<Response<Self::AwaitDecisionStream>, Status> {
        let AwaitDecisionRequest { scope_id, token } = req.into_inner();
        let rx = self.0.follow(scope_id, token).await.map_err(status)?;
        let stream = ReceiverStream::new(rx).map(|r| {
            r.map(|o| AwaitDecisionResponse {
                outcome: Some(convert::outcome(o)),
            })
            .map_err(status)
        });
        Ok(Response::new(Box::pin(stream)))
    }
}

#[tonic::async_trait]
impl ReviewerService for Service {
    async fn list_held(&self, _req: Request<ListHeldRequest>) -> Result<Response<ListHeldResponse>, Status> {
        let held = self.0.list_held().await.map_err(status)?;
        Ok(Response::new(ListHeldResponse {
            scopes: held.into_iter().map(convert::held).collect(),
        }))
    }

    async fn get_held(&self, req: Request<GetHeldRequest>) -> Result<Response<GetHeldResponse>, Status> {
        let id = req.into_inner().scope_id;
        let held = self.0.get_held(id.clone()).await.map_err(status)?;
        Ok(Response::new(GetHeldResponse {
            held: Some(convert::held(held.who)),
            change_set: held.closed.map(|c| self.change_set(id, c)),
            history: held
                .history
                .iter()
                .filter_map(|e| Decided::decode(&e[..]).ok())
                .collect(),
        }))
    }

    /// A tier's verdict; after the last pending tier, the scope is decided.
    async fn review(&self, req: Request<ReviewRequest>) -> Result<Response<ReviewResponse>, Status> {
        let req = req.into_inner();
        let tier =
            convert::reviewer_tier(req.tier).ok_or_else(|| Status::invalid_argument("tier must be llm or human"))?;
        let verdict = convert::decision(req.verdict).ok_or_else(|| Status::invalid_argument("verdict must be set"))?;
        let out = self
            .0
            .review(req.scope_id, tier, verdict, req.reasons, req.r#override)
            .await
            .map_err(status)?;
        Ok(Response::new(ReviewResponse {
            outcome: Some(convert::outcome(out)),
        }))
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
    let started = service.0.clone();
    let reviewers = tonic::transport::Server::builder()
        .add_service(ReviewerServiceServer::new(service.clone()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(review_listener), async move {
            started.stopped().await
        });
    let stopper = service.0.clone();
    let clients = tonic::transport::Server::builder()
        .add_service(EscrowServiceServer::new(service.clone()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), async move {
            shutdown.await;
            stopper.stop();
        });
    // The reviewers stop with the clients, also when the clients' server fails.
    let clients = async {
        let r = clients.await;
        service.0.stop();
        r
    };
    let both = async { tokio::join!(clients, reviewers) };
    let (result, reviewed) = tokio::select! {
        r = both => r,
        _ = async {
            service.0.stopped().await;
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

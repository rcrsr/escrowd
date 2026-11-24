//! `escrow diff`: a closed scope's diff against its snapshot.

use std::io::Write;
use std::path::PathBuf;

use escrowd::proto::GetChangeSetRequest;
use escrowd::proto::escrow_service_client::EscrowServiceClient;

use crate::client::{channel, socket_of};

pub(crate) async fn run(socket: Option<PathBuf>, project: Option<PathBuf>, scope: String) -> anyhow::Result<()> {
    let socket = socket_of(socket, project)?;
    let cs = EscrowServiceClient::new(channel(&socket).await?)
        .max_decoding_message_size(usize::MAX)
        .get_change_set(GetChangeSetRequest { scope_id: scope })
        .await
        .map_err(anyhow::Error::from)?
        .into_inner()
        .change_set
        .unwrap_or_default();
    std::io::stdout().lock().write_all(cs.diff.as_bytes())?;
    Ok(())
}

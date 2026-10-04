//! escrowd: the daemon behind escrow scopes.
//!
//! Phase 1.2: one FUSE mount serves a copy-on-write view per scope; scopes are
//! opened over RPC and dropped by a discard decision. Close, commit and the
//! remaining calls land in 1.3 to 1.5.

pub mod daemon;
pub mod fuse;
pub mod gate;
pub mod ledger;
pub mod rpc;
pub mod store;
pub mod sys;
pub mod views;

/// Protocol version reported by `Ping`. Bumped on any incompatible change
/// until phase 3 freezes the schema.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod proto {
    tonic::include_proto!("escrow.v1");
}

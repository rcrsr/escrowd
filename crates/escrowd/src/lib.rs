//! escrowd: the daemon behind escrow scopes.
//!
//! One FUSE mount serves a copy-on-write view per scope. Scopes are opened over
//! RPC, closed (frozen, returning their change set) and decided: commit applies
//! the change set through the journal, discard drops it, return reopens the
//! scope. Scope children start in their own bwrap sandbox through the exec
//! socket; IO outside any scope follows the unscoped mode.

pub mod changeset;
pub mod commit;
pub mod daemon;
pub mod exec;
pub mod fault;
pub mod fuse;
pub mod gate;
pub mod journal;
pub mod ledger;
pub mod policy;
pub mod roots;
pub mod rpc;
pub mod sandbox;
pub mod snapshot;
pub mod store;
pub mod sys;
pub mod views;

/// Protocol version reported by `Ping`. Bumped on any incompatible change
/// until phase 3 freezes the schema.
pub const PROTOCOL_VERSION: u32 = 3;

pub mod proto {
    tonic::include_proto!("escrow.v1");
}

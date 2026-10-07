//! escrowd: the daemon behind escrow scopes.
//!
//! One FUSE mount serves a copy-on-write view per scope. Scopes are opened over
//! RPC, closed (frozen, returning their change set) and decided: commit applies
//! the change set through the journal, discard drops it, return reopens the
//! scope. Scope children start in their own bwrap sandbox through the exec
//! socket; IO outside any scope follows the unscoped mode.

pub mod changeset;
pub mod commit;
pub mod convert;
pub mod daemon;
pub mod decision;
pub mod diff;
pub mod error;
pub mod exec;
pub mod fault;
pub mod fuse;
pub mod gate;
pub mod history;
pub mod journal;
pub mod ledger;
pub mod policy;
pub mod proc;
pub mod record;
pub mod review;
pub mod roots;
pub mod rpc;
pub mod sandbox;
pub mod snapshot;
pub mod store;
pub mod sys;
pub mod views;

/// Protocol version reported by `Ping`: 7 since the freeze (tag `protocol-v1`),
/// for the whole of `escrow.v1`; see `docs/protocol.md`.
pub const PROTOCOL_VERSION: u32 = 7;

pub mod proto {
    tonic::include_proto!("escrow.v1");
}

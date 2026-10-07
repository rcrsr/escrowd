//! escrowd: the daemon behind escrow scopes.
//!
//! One FUSE mount serves a copy-on-write view per scope. Scopes are opened over
//! RPC, closed (frozen, returning their change set) and decided: commit applies
//! the change set through the journal, discard drops it, return reopens the
//! scope. Scope children start in their own bwrap sandbox through the exec
//! socket; IO outside any scope follows the unscoped mode.

pub(crate) mod changeset;
pub(crate) mod commit;
pub(crate) mod convert;
pub mod daemon;
pub mod decision;
pub(crate) mod diff;
pub mod error;
pub mod exec;
pub(crate) mod fault;
pub(crate) mod fuse;
pub(crate) mod gate;
pub(crate) mod history;
pub(crate) mod journal;
pub(crate) mod ledger;
pub mod policy;
pub(crate) mod proc;
pub(crate) mod record;
pub(crate) mod review;
pub(crate) mod roots;
pub mod rpc;
pub mod sandbox;
pub(crate) mod snapshot;
pub(crate) mod store;
pub(crate) mod sys;
pub mod views;

/// Protocol version reported by `Ping`: 7 since the freeze (tag `protocol-v1`),
/// for the whole of `escrow.v1`; see `docs/protocol.md`.
pub const PROTOCOL_VERSION: u32 = 7;

pub mod proto {
    tonic::include_proto!("escrow.v1");
}

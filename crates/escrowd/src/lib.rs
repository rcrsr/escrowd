//! escrowd: the daemon behind escrow scopes. Phase 1.1 serves only `Ping`;
//! every other call returns UNIMPLEMENTED until its sub-phase lands.

pub mod rpc;

/// Protocol version reported by `Ping`. Bumped on any incompatible change
/// until phase 3 freezes the schema.
pub const PROTOCOL_VERSION: u32 = 1;

pub mod proto {
    tonic::include_proto!("escrow.v1");
}

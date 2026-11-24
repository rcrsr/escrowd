//! The `escrow.v1` protocol, generated at build time from `proto/escrow/v1/escrow.proto`
//! by tonic-prost-build (messages and gRPC stubs) and pbjson-build (serde with protobuf
//! JSON names). escrowd re-exports it as `escrowd::proto`. No hand-written code here.

tonic::include_proto!("escrow.v1");
include!(concat!(env!("OUT_DIR"), "/escrow.v1.serde.rs"));

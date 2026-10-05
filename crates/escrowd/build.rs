fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc, so building needs no system protobuf compiler.
    // SAFETY: build scripts are single-threaded.
    unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    // The client serves `escrow diff`.
    tonic_prost_build::configure().compile_protos(&["../../proto/escrow/v1/escrow.proto"], &["../../proto"])?;
    Ok(())
}

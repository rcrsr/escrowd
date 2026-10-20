use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Vendored protoc, so building needs no system protobuf compiler.
    // SAFETY: build scripts are single-threaded.
    unsafe { std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?) };
    let descriptors = PathBuf::from(std::env::var("OUT_DIR")?).join("escrow_descriptor.bin");
    // The client serves `escrow diff`.
    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptors)
        .compile_protos(&["../../proto/escrow/v1/escrow.proto"], &["../../proto"])?;
    // Serde impls with protobuf JSON names (snake_case as in the .proto, defaults
    // emitted): the request JSON a command reviewer reads on stdin.
    pbjson_build::Builder::new()
        .register_descriptors(&std::fs::read(&descriptors)?)?
        .preserve_proto_field_names()
        .emit_fields()
        .build(&[".escrow.v1"])?;
    Ok(())
}

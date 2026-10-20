//! Build wiring: compile `proto/gardenbed.proto` and generate the tonic
//! client/server + prost message code.
//!
//! `protoc` is NOT required: the pure-Rust [`protox`] compiler produces the
//! `FileDescriptorSet`, which is fed straight into
//! `tonic_prost_build::configure().compile_fds(...)`.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto");
    let proto_file = proto_root.join("gardenbed.proto");

    // Pure-Rust protobuf compilation — no system protoc involved.
    let file_descriptors = protox::compile([proto_file], [&proto_root])?;

    // Persist the descriptor set so tests can assert the published protocol
    // shape (service methods, `repo` field, enum values) via reflection.
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let mut fds_bytes = Vec::new();
    prost::Message::encode(&file_descriptors, &mut fds_bytes)?;
    std::fs::write(out_dir.join("gardenbed.fds.bin"), fds_bytes)?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds_with_config(file_descriptors, {
            let mut config = tonic_prost_build::Config::new();
            config.type_attribute(".", "#[allow(clippy::all)]");
            config
        })?;

    println!("cargo:rerun-if-changed=proto/gardenbed.proto");
    Ok(())
}

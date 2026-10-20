//! Build wiring: compile `proto/daemon.proto` and generate the tonic
//! client/server + prost message code — the same pure-Rust path the
//! protocol crate uses (protox → `tonic_prost_build::compile_fds`), so no
//! system `protoc` is required.

use std::env;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto");
    let proto_file = proto_root.join("daemon.proto");

    let file_descriptors = protox::compile([proto_file], [&proto_root])?;

    // Persist the descriptor set so tests can assert the sidecar surface
    // (Ping/Register/Generation) via reflection.
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let mut fds_bytes = Vec::new();
    prost::Message::encode(&file_descriptors, &mut fds_bytes)?;
    std::fs::write(out_dir.join("daemon.fds.bin"), fds_bytes)?;

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds_with_config(file_descriptors, {
            let mut config = tonic_prost_build::Config::new();
            config.type_attribute(".", "#[allow(clippy::all)]");
            config
        })?;

    println!("cargo:rerun-if-changed=proto/daemon.proto");
    Ok(())
}

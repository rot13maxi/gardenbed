//! Build wiring: compile `proto/daemon.proto` and generate the tonic
//! client/server + prost message code — the same pure-Rust path the
//! protocol crate uses (protox → `tonic_prost_build::compile_fds`), so no
//! system `protoc` is required.
//!
//! It also embeds `GARDENBED_DAEMON_SRC_DIGEST`: a deterministic SHA-256
//! over `src/**` file contents, the same oracle `crates/server/build.rs`
//! provides for the server. `crates/bed/tests/common/mod.rs::daemon_bin()`
//! compares it against the working copy, so a daemon binary built from
//! different tree state is refused even when its mtime looks fresh
//! (dogfood #12's staleness class; a stale daemon produced one spurious
//! 4 MiB test failure).

use std::collections::BTreeMap;
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

    embed_src_digest()?;

    println!("cargo:rerun-if-changed=proto/daemon.proto");
    Ok(())
}

/// Emits `GARDENBED_DAEMON_SRC_DIGEST`: SHA-256 over (sorted rel-path,
/// length, content) of every file under `src/` — deterministic,
/// independent of readdir order and mtimes. Identical in shape to
/// `crates/server/build.rs` so one test helper can serve both.
fn embed_src_digest() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let src = manifest.join("src");
    let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(&manifest)
                    .map_err(|_| format!("{} escapes the manifest dir", path.display()))?;
                files.insert(
                    rel.to_path_buf(),
                    std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?,
                );
            }
        }
    }
    assert!(!files.is_empty(), "crates/daemon/src has no files");
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (rel, content) in &files {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(content.len().to_le_bytes());
        hasher.update(content);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=GARDENBED_DAEMON_SRC_DIGEST={hex}");
    println!("cargo:rerun-if-changed=src/");
    Ok(())
}

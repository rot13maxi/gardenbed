//! Build fingerprint: a deterministic hash over `src/**` file contents,
//! embedded as `GARDENBED_SERVER_SRC_DIGEST`. The test harness compares
//! it against the running sources to catch a binary built from DIFFERENT
//! tree state than the working copy — mtime-based staleness checks are
//! blind to that (`bed switch` materializes sources with commit-time
//! mtimes, so a stale binary can look fresh; dogfood finding #12's core).
//!
//! Cargo re-runs this script whenever any file under `src/` changes
//! (`cargo:rerun-if-changed`), so the digest always matches the sources
//! the binary was built from.
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let src = manifest.join("src");
    let mut files: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).expect("server src readable");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(&manifest).expect("under manifest");
                files.insert(
                    rel.to_path_buf(),
                    std::fs::read(&path).expect("server src file readable"),
                );
            }
        }
    }
    assert!(!files.is_empty(), "crates/server/src has no files");
    // Deterministic digest: SHA-256 over (sorted rel-path, len, content)
    // pairs — sorted so the hash is independent of readdir order.
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for (rel, content) in &files {
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(content.len().to_le_bytes());
        hasher.update(content);
    }
    let digest = hasher.finalize();
    println!(
        "cargo:rustc-env=GARDENBED_SERVER_SRC_DIGEST={}",
        hex(&digest)
    );
    println!("cargo:rerun-if-changed=src/");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

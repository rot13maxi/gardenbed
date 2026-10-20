//! Shared helpers for the gardenbed-store verification suite.
#![allow(dead_code)]

use gardenbed_model::Id;
use sha2::Digest;
use sha2::Sha256;
use std::path::PathBuf;

/// §6 envelope magic.
pub const MAGIC: &[u8; 5] = b"GBLO1";
/// Magic + u64 raw length.
pub const HEADER_LEN: usize = MAGIC.len() + 8;

/// Blobs per changeset in the torture child (a changeset is written every N
/// blobs); parent and child must agree on this.
pub const CHANGESET_EVERY: usize = 8;

/// Deterministic xorshift64* stream — stable across runs and processes so
/// the torture parent and child compute identical content.
pub fn xorshift64(seed: u64) -> u64 {
    let mut x = seed | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x.wrapping_mul(0x2545F4914F6CDD1D)
}

/// `len` deterministic pseudo-random bytes (incompressible).
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed;
    while out.len() < len {
        state = xorshift64(state);
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// `len` bytes of a compressible repeating pattern.
pub fn compressible_bytes(seed: u64, len: usize) -> Vec<u8> {
    let unit = random_bytes(seed, 16);
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&unit);
    }
    out.truncate(len);
    out
}

/// Hash helper for building arbitrary distinct ids.
pub fn id_of(seed: u64) -> Id {
    Id::from_bytes(Sha256::digest(seed.to_le_bytes()).into())
}

/// Builds the §6 envelope for `content` (test-side re-implementation, so the
/// store's own encoder is never trusted by the byte-exactness checks).
pub fn envelope_for(content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + content.len() / 2 + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(content.len() as u64).to_le_bytes());
    let frame = zstd::bulk::compress(content, 3).expect("test zstd compression failed");
    out.extend_from_slice(&frame);
    out
}

/// The shard path for `id` under a store rooted at `root` — the layout is
/// contractual: `objects/<first 2 hex>/<remaining 62>`.
pub fn shard_path(root: &std::path::Path, id: Id) -> PathBuf {
    let hex = id.to_hex();
    root.join("objects").join(&hex[..2]).join(&hex[2..])
}

/// A valid two-entry tree (file + dir) as canonical bytes, plus its id.
pub fn sample_tree() -> (gardenbed_model::Tree, Id, Vec<u8>) {
    use gardenbed_model::{Mode, Tree, TreeEntry, TreeKind};
    let tree = Tree {
        entries: vec![
            TreeEntry {
                name: "dir".into(),
                kind: TreeKind::Dir,
                mode: Mode::Regular,
                blob: None,
                tree: Some(id_of(0xDD01)),
            },
            TreeEntry {
                name: "file.txt".into(),
                kind: TreeKind::File,
                mode: gardenbed_model::Mode::Exec,
                blob: Some(gardenbed_model::blob_id(b"file content")),
                tree: None,
            },
        ],
    };
    let bytes = tree.canonical_bytes().expect("tree encodes");
    let id = Id::from_bytes(Sha256::digest(&bytes).into());
    (tree, id, bytes)
}

/// A valid changeset, deterministic in `seed`, with `n_changes` add-ops.
pub fn sample_changeset(seed: u64, n_changes: usize) -> gardenbed_model::Changeset {
    use gardenbed_model::{Change, Changeset, Identity, Mode, Op};
    let mut changes = Vec::new();
    for i in 0..n_changes {
        changes.push(Change {
            path: format!("src/file_{i:04}.txt"),
            op: Op::Add,
            blob: Some(id_of(seed ^ (0x5EED ^ i as u64))),
            src_path: None,
            mode: Mode::Regular,
        });
    }
    Changeset {
        parents: vec![],
        changes,
        message: format!("torture changeset seed={seed}"),
        author: Identity {
            name: "torture".into(),
            email: "torture@gardenbed.test".into(),
            time_secs: seed,
            tz_min: 60,
        },
        committer: Identity {
            name: "torture".into(),
            email: "torture@gardenbed.test".into(),
            time_secs: seed,
            tz_min: 60,
        },
        signature: None,
    }
}

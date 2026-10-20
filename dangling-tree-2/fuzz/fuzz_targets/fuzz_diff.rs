//! `fuzz_diff` — random tree pairs through the engine's recursive diff
//! (v0.1.md M8.3 "diff" target).
//!
//! The input seeds a deterministic RNG; the target builds two random flat
//! file sets (paths from a small pool, so file↔directory swaps and shared
//! subtrees occur), sows each as a changeset chain (root A, child B with
//! B's full content), diffs `A → B` with the engine, and then asserts the
//! re-apply property: applying the diff as a child changeset of A must
//! converge on exactly B's tree. Errors (a diff that cannot re-apply) are
//! failures worth investigating; panics are the fuzz contract.

#![no_main]

use std::collections::BTreeMap;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use gardenbed_derive::Engine;
use gardenbed_model::{blob_id, Change, Changeset, Id, Identity, Mode, Op};
use gardenbed_store::{MemoryStore, MetadataStore as _};

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// Paths small enough to collide across the two sides (same file with
/// different content, file↔directory swaps, nested overlaps).
const PATHS: &[&str] = &[
    "a",
    "a/b",
    "a/b/c",
    "a/b/c/d.txt",
    "a/b/e",
    "a/f.txt",
    "g",
    "g/h",
    "g/h/i.txt",
    "j.bin",
];

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let mut seed = [0u8; 8];
    seed.copy_from_slice(&data[..8]);
    let mut rng = Rng(u64::from_le_bytes(seed) | 1);

    let ident = Identity {
        name: "Fuzzer".into(),
        email: "fuzz@bed.example".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    };

    // Two random file sets. Blobs are content-addressed by their bytes so
    // the changesets are canonical (the store validates ids).
    let rand_files = |rng: &mut Rng| -> BTreeMap<String, (Mode, Id, Vec<u8>)> {
        let mut files = BTreeMap::new();
        for _ in 0..rng.below(9) {
            let path = PATHS[rng.below(PATHS.len() as u64) as usize];
            let mode = match rng.below(4) {
                0 => Mode::Regular,
                1 => Mode::Exec,
                2 => Mode::Symlink,
                _ => Mode::Gitlink,
            };
            let content = if mode == Mode::Gitlink {
                // A submodule pointer: 12 zero bytes + a pseudo git sha.
                let mut b = vec![0u8; 12];
                for _ in 0..20 {
                    b.push(rng.below(256) as u8);
                }
                b
            } else {
                let n = rng.below(48) as usize;
                (0..n).map(|_| rng.below(256) as u8).collect::<Vec<u8>>()
            };
            files.insert(path.to_owned(), (mode, blob_id(&content), content));
        }
        files
    };
    let fa = rand_files(&mut rng);
    let fb = rand_files(&mut rng);

    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());

    let changes_of = |files: &BTreeMap<String, (Mode, Id, Vec<u8>)>| {
        files
            .iter()
            .map(|(path, (mode, blob, content))| {
                (
                    Change {
                        path: path.clone(),
                        op: Op::Add,
                        blob: Some(*blob),
                        src_path: None,
                        mode: *mode,
                    },
                    (blob.to_hex(), content.clone()),
                )
            })
            .collect::<Vec<_>>()
    };

    // Root changeset A = tree of `fa`; child B = tree of `fb`.
    let mut inline = BTreeMap::new();
    let mut adds = Vec::new();
    for (change, (hex, content)) in changes_of(&fa) {
        inline.insert(hex, content);
        adds.push(change);
    }
    let cs_a = Changeset {
        parents: vec![],
        changes: adds,
        message: "a".into(),
        author: ident.clone(),
        committer: ident.clone(),
        signature: None,
    };
    let id_a = match store.put_changeset(&cs_a) {
        Ok(id) => id,
        Err(_) => return, // rejected input: skip
    };

    let mut inline_b = BTreeMap::new();
    let mut adds_b = Vec::new();
    for (change, (hex, content)) in changes_of(&fb) {
        inline_b.insert(hex, content);
        adds_b.push(change);
    }
    let cs_b = Changeset {
        parents: vec![id_a],
        changes: adds_b,
        message: "b".into(),
        author: ident.clone(),
        committer: ident.clone(),
        signature: None,
    };
    let id_b = match store.put_changeset(&cs_b) {
        Ok(id) => id,
        Err(_) => return,
    };

    // Diff A → B via the engine (derives both trees recursively).
    let changes = match engine.diff(id_a, id_b) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Re-apply: the diff, replayed onto A, must reproduce B exactly.
    let cs_c = Changeset {
        parents: vec![id_a],
        changes,
        message: "re-apply of diff(a, b)".into(),
        author: ident.clone(),
        committer: ident,
        signature: None,
    };
    let id_c = match store.put_changeset(&cs_c) {
        Ok(id) => id,
        Err(e) => panic!("diff output is not a canonical changeset: {e}"),
    };
    let tree_b = engine.derive_tree(id_b).expect("tree b derives");
    let tree_c = engine
        .derive_tree(id_c)
        .expect("the re-applied diff must derive a tree");
    assert_eq!(
        tree_b, tree_c,
        "applying diff(a, b) onto a did not converge on b"
    );
});

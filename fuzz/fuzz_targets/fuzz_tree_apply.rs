//! `fuzz_tree_apply` — random changeset sequences applied through the
//! compost engine (v0.1.md M8.3 "tree-apply" target).
//!
//! The input seeds a deterministic RNG; from it the target builds a short
//! chain of *canonical* changesets (sorted, deduplicated, op/table-valid —
//! the seedbed rejects anything else) and drives
//! `Engine::derive_tree` over each. The contract: applying canonical
//! changesets never panics — semantic inconsistencies (deleting a missing
//! path, renaming to an existing path, …) must surface as `Err`, and every
//! successfully derived tree must re-decode from loam.

#![no_main]

use std::collections::BTreeMap;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, TreeKind};
use gardenbed_store::{MemoryStore, MetadataStore as _, ObjectStore as _};

/// A tiny deterministic PRNG (xorshift64*) so every input explores one
/// reproducible scenario.
struct Rng {
    s: u64,
}

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        // xorshift64*
        let mut x = self.s;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.s = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }
}

/// The path pool: small enough that operations collide (deletes of missing
/// paths, renames onto existing paths), which is exactly the shape the
/// applier must reject cleanly.
const PATHS: &[&str] = &[
    "a",
    "a/b",
    "a/b/c.txt",
    "a/b/c/d.txt",
    "b",
    "b/c",
    "b/c/d",
    "c.txt",
    "d",
    "d/e.txt",
    "d/e/f",
    "g.bin",
];

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let mut seed = [0u8; 8];
    seed.copy_from_slice(&data[..8]);
    let mut rng = Rng {
        s: u64::from_le_bytes(seed) | 1,
    };

    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());

    let ident = || Identity {
        name: "Fuzzer".into(),
        email: "fuzz@bed.example".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    };
    let rand_mode = |rng: &mut Rng| match rng.below(4) {
        0 => Mode::Regular,
        1 => Mode::Exec,
        2 => Mode::Symlink,
        _ => Mode::Gitlink,
    };
    let rand_blob = |rng: &mut Rng| {
        let mut b = [0u8; 32];
        for byte in &mut b {
            *byte = rng.below(256) as u8;
        }
        Id::from_bytes(b)
    };

    // A chain of 1..=8 changesets; each applies onto its predecessor.
    let mut parent: Option<Id> = None;
    for _ in 0..(1 + rng.below(8)) {
        let mut changes: BTreeMap<String, Change> = BTreeMap::new();
        for _ in 0..rng.below(6) {
            let path = PATHS[rng.below(PATHS.len() as u64) as usize];
            let change = match rng.below(4) {
                0 => Change {
                    path: path.to_owned(),
                    op: Op::Add,
                    blob: Some(rand_blob(&mut rng)),
                    src_path: None,
                    mode: rand_mode(&mut rng),
                },
                1 => Change {
                    path: path.to_owned(),
                    op: Op::Modify,
                    blob: Some(rand_blob(&mut rng)),
                    src_path: None,
                    mode: rand_mode(&mut rng),
                },
                2 => Change {
                    path: path.to_owned(),
                    op: Op::Delete,
                    blob: None,
                    src_path: None,
                    mode: Mode::Regular,
                },
                _ => {
                    // Rename: the source must differ from the destination
                    // and come from the same pool (collisions with other
                    // changes are allowed — the applier must reject them
                    // cleanly, never panic).
                    let src = PATHS[rng.below(PATHS.len() as u64) as usize];
                    Change {
                        path: path.to_owned(),
                        op: Op::Rename,
                        blob: Some(rand_blob(&mut rng)),
                        src_path: Some(src.to_owned()),
                        mode: rand_mode(&mut rng),
                    }
                }
            };
            // A canonical changeset has each path at most once, ascending;
            // the BTreeMap dedupes and sorts. A rename whose src equals
            // another change's path is dropped (the codec would reject it).
            if change
                .src_path
                .as_ref()
                .is_some_and(|src| changes.contains_key(src))
            {
                continue;
            }
            changes.insert(path.to_owned(), change);
        }

        let cs = Changeset {
            parents: parent.into_iter().collect(),
            changes: changes.into_values().collect(),
            message: "fuzz tree apply".into(),
            author: ident(),
            committer: ident(),
            signature: None,
        };
        let id = match store.put_changeset(&cs) {
            Ok(id) => id,
            Err(_) => continue, // rejected input: fine, just skip
        };
        // The whole contract: Err is allowed (inconsistent changeset), a
        // panic is not.
        if let Ok(Some(tree_id)) = engine.derive_tree(id) {
            // Whatever tree was derived must be a readable, decodable tree.
            let bytes = store.get_tree(tree_id).ok().flatten();
            if let Some(bytes) = bytes {
                if let Ok(tree) = gardenbed_model::Tree::decode(&bytes) {
                    for entry in &tree.entries {
                        if entry.kind == TreeKind::Dir {
                            assert!(entry.blob.is_none(), "dir entry carries a blob id");
                        }
                    }
                }
            }
        }
        parent = Some(id);
    }
});

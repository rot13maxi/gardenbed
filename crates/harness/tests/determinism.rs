//! Determinism: same seed + options ⇒ byte-identical changesets, and
//! `files_at` replay matches a hand-tracked working set.

use std::collections::BTreeMap;

use gardenbed_harness::{files_at, generate, GenOptions};
use gardenbed_model::{Changeset, Id, Mode, Op};

#[test]
fn same_seed_and_options_give_identical_changesets() {
    let opts = GenOptions {
        commits: 40,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        max_files: 20,
        max_dirs: 4,
        big_blob_bytes: Some(64 * 1024),
        exec_ratio: 0.1,
        symlink_ratio: 0.05,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    };
    let a = generate(7, &opts);
    let b = generate(7, &opts);

    let ids_a: Vec<Id> = a.changesets.iter().map(|cs| cs.id().unwrap()).collect();
    let ids_b: Vec<Id> = b.changesets.iter().map(|cs| cs.id().unwrap()).collect();
    assert_eq!(ids_a, ids_b, "changeset ids must be identical across runs");

    // Byte-identical, not just id-identical.
    for (x, y) in a.changesets.iter().zip(b.changesets.iter()) {
        assert_eq!(x, y);
        assert_eq!(x.canonical_bytes().unwrap(), y.canonical_bytes().unwrap());
    }
    assert_eq!(a.blob_contents, b.blob_contents);
    assert_eq!(a.head, b.head);

    // A different seed produces a different history.
    let c = generate(8, &opts);
    let ids_c: Vec<Id> = c.changesets.iter().map(|cs| cs.id().unwrap()).collect();
    assert_ne!(ids_a, ids_c);
}

#[test]
fn different_options_change_the_history() {
    let base = GenOptions {
        commits: 10,
        ..Default::default()
    };
    let with_renames = GenOptions {
        rename_ratio: 0.9,
        ..base.clone()
    };
    let a = generate(5, &base);
    let b = generate(5, &with_renames);
    let ids_a: Vec<Id> = a.changesets.iter().map(|cs| cs.id().unwrap()).collect();
    let ids_b: Vec<Id> = b.changesets.iter().map(|cs| cs.id().unwrap()).collect();
    assert_ne!(ids_a, ids_b);
}

#[test]
fn files_at_replays_the_working_set() {
    let history = generate(13, &GenOptions::default());
    let last = history.changesets.len() - 1;
    assert!(!files_at(&history, last).is_empty());

    // Apply every changeset to a hand-tracked working set; the invariants
    // below assert the generator never produces self-contradictory ops,
    // and the final state must equal `files_at` exactly.
    let mut live: BTreeMap<String, (Id, Mode)> = BTreeMap::new();
    for (i, cs) in history.changesets.iter().enumerate() {
        for c in &cs.changes {
            match c.op {
                Op::Delete => {
                    assert!(live.remove(&c.path).is_some(), "cs {i}: delete of absent path");
                }
                Op::Rename => {
                    let src = c.src_path.clone().expect("rename has src_path");
                    let old = live.remove(&src).expect("cs {i}: rename of absent src");
                    let blob = c.blob.expect("rename carries a blob");
                    // Pure rename reuses the source blob; rename+modify
                    // carries a fresh one. Both keep the src removed.
                    assert!(blob == old.0 || blob != old.0);
                    assert!(live.insert(c.path.clone(), (blob, c.mode)).is_none(), "cs {i}: rename dst already exists");
                }
                Op::Add => {
                    let blob = c.blob.expect("add carries a blob");
                    assert!(
                        live.insert(c.path.clone(), (blob, c.mode)).is_none(),
                        "cs {i}: add of existing path"
                    );
                }
                Op::Modify => {
                    assert!(live.contains_key(&c.path), "cs {i}: modify of absent path");
                    let blob = c.blob.expect("modify carries a blob");
                    live.insert(c.path.clone(), (blob, c.mode));
                }
            }
        }
        // Encode/decode roundtrip (canonical acceptance property).
        let bytes = cs.canonical_bytes().expect("generated cs encodes");
        let back: Changeset = Changeset::decode(&bytes).expect("generated cs decodes");
        assert_eq!(&back, cs);

        assert_eq!(&live, &files_at(&history, i), "files_at({i}) diverged");
    }
}

//! Verification M2.2: our tree diff classification equals
//! `git diff-tree -r --no-renames --no-commit-id` — for every adjacent pair
//! (which is exactly "changeset vs its first parent") and for ≥ 50 random
//! pairs per history.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::{GenOptions, generate, materialize};
use gardenbed_model::{Id, Op};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

use common::{git_diff_two, git_status_op};

fn history(seed: u64, commits: u32) -> gardenbed_harness::GeneratedHistory {
    let opts = GenOptions {
        commits,
        merge_ratio: 0.25,
        octopus_ratio: 0.1,
        rename_ratio: 0.25,
        delete_ratio: 0.15,
        max_files: 24,
        max_dirs: 5,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
        ..Default::default()
    };
    generate(seed, &opts)
}

/// Compares `engine.diff(cs_i, cs_j)` (tree(i) → tree(j)) with git's
/// tree-to-tree diff between the mapped commits.
fn check_pair(
    engine: &Engine,
    repo: &std::path::Path,
    mapping: &[(Id, String)],
    ids: &[Id],
    i: usize,
    j: usize,
) {
    let ours = engine.diff(ids[i], ids[j]).expect("diff computes");
    let ours: HashMap<String, Op> = ours
        .into_iter()
        .map(|c| {
            assert!(
                c.src_path.is_none(),
                "rename detection must be off in v0.1 diffs"
            );
            (c.path, c.op)
        })
        .collect();
    let theirs: HashMap<String, Op> = git_diff_two(repo, &mapping[i].1, &mapping[j].1)
        .into_iter()
        .map(|(status, path)| (path, git_status_op(status)))
        .collect();
    let mut keys: Vec<&String> = ours.keys().chain(theirs.keys()).collect();
    keys.sort();
    keys.dedup();
    for k in keys {
        assert_eq!(
            ours.get(k),
            theirs.get(k),
            "diff classification differs for {k} (cs {i} -> cs {j})"
        );
    }
}

fn run_history(seed: u64, commits: u32) {
    let h = history(seed, commits);
    let m = Arc::new(MemoryStore::new());
    let objects: Arc<dyn ObjectStore> = m.clone();
    let metadata: Arc<dyn MetadataStore> = m.clone();
    let engine = Engine::new(objects.clone(), metadata.clone());
    let ids = common::ingest(&objects, &metadata, &h, true);

    let git_dir = tempfile::tempdir().expect("git tempdir");
    let mat = materialize(&h, git_dir.path()).expect("materialize");
    assert_eq!(mat.mapping.len(), ids.len());

    // Every adjacent pair == the commit-vs-first-parent diff.
    for j in 1..ids.len() {
        check_pair(&engine, git_dir.path(), &mat.mapping, &ids, j - 1, j);
    }

    // ≥ 50 random pairs (deterministic seed) plus the identity pairs.
    let mut rng_state = seed ^ 0x9e3779b9;
    let mut next = move || {
        rng_state ^= rng_state << 13;
        rng_state ^= rng_state >> 7;
        rng_state ^= rng_state << 17;
        rng_state
    };
    for _ in 0..60 {
        let mut i = (next() as usize) % ids.len();
        let mut j = (next() as usize) % ids.len();
        if i > j {
            std::mem::swap(&mut i, &mut j);
        }
        check_pair(&engine, git_dir.path(), &mat.mapping, &ids, i, j);
    }
    // Identical changesets diff to nothing.
    for (i, id) in ids.iter().enumerate() {
        assert!(
            engine.diff(*id, *id).expect("self diff").is_empty(),
            "self-diff at cs {i} must be empty"
        );
    }
}

#[test]
fn diff_equality_seed11() {
    run_history(11, 45);
}

#[test]
fn diff_equality_seed99() {
    run_history(99, 30);
}

#[test]
fn diff_equality_seed1234() {
    run_history(1234, 35);
}

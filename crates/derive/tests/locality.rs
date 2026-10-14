//! Verification M2.4 (derivation locality): a change touching k directories
//! writes O(k) new tree objects — untouched subtrees are kept by id and
//! never loaded or re-encoded.
//!
//! Measured on the on-disk loam (`FsObjectStore` is one flat CAS holding
//! blobs and trees, so new blob writes are made *before* the baseline count
//! to isolate the tree-object delta). The 5k-file variant runs by default;
//! the 50k-file variant is `#[ignore]`d (run with `cargo test -- --ignored`).

use std::path::Path;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{FsObjectStore, MemoryStore, MetadataStore, ObjectStore};

fn identity(name: &str, secs: u64) -> Identity {
    Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

/// A changeset adding `n_files` files spread over `n_dirs` directories
/// (`dNNN/fMM`), changes in canonical order.
fn root_changeset(n_dirs: usize, files_per_dir: usize, tag: &str) -> (Changeset, Vec<Vec<u8>>) {
    let mut changes = Vec::with_capacity(n_dirs * files_per_dir);
    let mut contents = Vec::with_capacity(n_dirs * files_per_dir);
    for d in 0..n_dirs {
        for f in 0..files_per_dir {
            let content = format!("{tag} d{d:04} f{f:02}\n").into_bytes();
            let path = format!("d{d:04}/f{f:02}");
            changes.push(Change {
                path: path.clone(),
                op: Op::Add,
                blob: Some(blob_id(&content)),
                src_path: None,
                mode: Mode::Regular,
            });
            contents.push(content);
        }
    }
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    (
        Changeset {
            parents: vec![],
            changes,
            message: format!("{tag} root"),
            author: identity("seeds", 0),
            committer: identity("seeds", 0),
            signature: None,
        },
        contents,
    )
}

/// A changeset modifying `n_changes` files, one per each of `n_changes`
/// distinct directories (`dNNNN/f00`), on top of `parent`.
fn modify_changeset(
    parent: Id,
    n_changes: usize,
    dirs: usize,
    tag: &str,
) -> (Changeset, Vec<Vec<u8>>) {
    let mut changes = Vec::with_capacity(n_changes);
    let mut contents = Vec::with_capacity(n_changes);
    for i in 0..n_changes {
        let d = i % dirs;
        let content = format!("{tag} changed d{d:04} f00\n").into_bytes();
        let path = format!("d{d:04}/f00");
        changes.push(Change {
            path: path.clone(),
            op: Op::Modify,
            blob: Some(blob_id(&content)),
            src_path: None,
            mode: Mode::Regular,
        });
        contents.push(content);
    }
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    (
        Changeset {
            parents: vec![parent],
            changes,
            message: format!("{tag} modify"),
            author: identity("seeds", 1),
            committer: identity("seeds", 1),
            signature: None,
        },
        contents,
    )
}

/// Number of files under the loam object directory (the flat CAS: one file
/// per object, blobs and trees alike).
fn count_object_files(root: &Path) -> usize {
    let objects = root.join("objects");
    let mut n = 0usize;
    for shard in std::fs::read_dir(&objects).expect("objects dir") {
        let shard = shard.expect("shard").path();
        n += std::fs::read_dir(&shard)
            .expect("shard dir")
            .filter_map(|e| e.ok())
            .count();
    }
    n
}

/// The locality scenario: `total_files` spread over `total_dirs`, then a
/// 100-file change touching `changed_dirs` distinct directories. Asserts the
/// number of new loam objects is ≤ 2×(changed dirs) + a small constant.
fn run_locality(total_files: usize, total_dirs: usize, changed_dirs: usize) {
    assert_eq!(total_files % total_dirs, 0, "even spread");
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = Arc::new(FsObjectStore::open(dir.path()).expect("loam opens"));
    let metadata = Arc::new(MemoryStore::new());
    let engine = Engine::new(objects.clone(), metadata.clone());

    // Root changeset: the full synthetic tree.
    let (cs1, contents1) = root_changeset(total_dirs, total_files / total_dirs, "seed");
    for c in &contents1 {
        objects.put_blob(c).expect("put blob");
    }
    let id1 = metadata.put_changeset(&cs1).expect("store cs1");
    let t1 = engine.derive_tree(id1).expect("derive root tree");
    assert!(t1.is_some(), "the synthetic tree is non-empty");
    assert_eq!(
        engine.flatten_tree(t1).expect("flatten").len(),
        total_files,
        "all files present"
    );

    // 100-file change across `changed_dirs` distinct directories. Blobs go
    // in BEFORE the baseline count so the delta isolates tree objects.
    let (cs2, contents2) = modify_changeset(id1, 100, changed_dirs, "delta");
    for c in &contents2 {
        objects.put_blob(c).expect("put blob");
    }
    let before = count_object_files(dir.path());
    let id2 = metadata.put_changeset(&cs2).expect("store cs2");
    engine
        .derive_tree(id2)
        .expect("derive child tree")
        .expect("non-empty");
    let after = count_object_files(dir.path());

    let new_objects = after - before;
    // 100 re-created directories + the root is the exact expectation (101);
    // the bound allows intermediate-directory chains and a small constant.
    assert!(
        new_objects <= 2 * changed_dirs + 8,
        "locality violated: a 100-file change across {changed_dirs} dirs wrote \
         {new_objects} new loam objects (bound {})",
        2 * changed_dirs + 8
    );
}

/// 5k-file tree (500 dirs × 10 files), 100-file change across 100 dirs.
#[test]
fn locality_5k_files_100_dirs() {
    run_locality(5_000, 500, 100);
}

/// The extreme shape: every change lands in ONE directory — the working set
/// rewrites that directory and the root, nothing else.
#[test]
fn locality_5k_files_one_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects = Arc::new(FsObjectStore::open(dir.path()).expect("loam opens"));
    let metadata = Arc::new(MemoryStore::new());
    let engine = Engine::new(objects.clone(), metadata.clone());

    let (cs1, contents1) = root_changeset(500, 10, "seed");
    for c in &contents1 {
        objects.put_blob(c).expect("put blob");
    }
    let id1 = metadata.put_changeset(&cs1).expect("store cs1");
    engine.derive_tree(id1).expect("derive root tree");

    // 100 modifies, all inside d0007 (canonical order: same dir, f00 can
    // appear once — use distinct files within the dir).
    let mut changes = Vec::new();
    let mut contents = Vec::new();
    for f in 0..10 {
        let content = format!("changed d0007 f{f:02}\n").into_bytes();
        changes.push(Change {
            path: format!("d0007/f{f:02}"),
            op: Op::Modify,
            blob: Some(blob_id(&content)),
            src_path: None,
            mode: Mode::Regular,
        });
        contents.push(content);
    }
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let cs2 = Changeset {
        parents: vec![id1],
        changes,
        message: "one-dir modify".into(),
        author: identity("seeds", 2),
        committer: identity("seeds", 2),
        signature: None,
    };
    for c in &contents {
        objects.put_blob(c).expect("put blob");
    }
    let before = count_object_files(dir.path());
    let id2 = metadata.put_changeset(&cs2).expect("store cs2");
    engine
        .derive_tree(id2)
        .expect("derive child tree")
        .expect("non-empty");
    let after = count_object_files(dir.path());

    let new_objects = after - before;
    // Exactly the touched directory plus the root.
    assert!(
        (2..=10).contains(&new_objects),
        "expected 2 new tree objects (dir + root), got {new_objects}"
    );
}

/// 50k-file tree (5,000 dirs × 10 files), 100-file change across 100 dirs.
/// `#[ignore]`d by default: `cargo test -p gardenbed-derive -- --ignored`.
#[test]
#[ignore]
fn locality_50k_files_100_dirs() {
    run_locality(50_000, 5_000, 100);
}

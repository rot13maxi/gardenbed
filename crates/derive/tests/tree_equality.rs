//! Verification M2.1 + M2.3: derived trees match `git ls-tree -r` on every
//! changeset of seeded histories (paths, modes, contents), and identical
//! tree content reached via different histories hashes to identical ids.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::{GenOptions, GeneratedHistory, files_at, generate, materialize};
use gardenbed_model::{Id, Mode, Tree};
use gardenbed_store::{
    FsObjectStore, MemoryStore, MetadataStore, ObjectStore, SqliteMetadataStore,
};

use common::{git_out, mode_to_git};

fn history(seed: u64) -> GeneratedHistory {
    let opts = GenOptions {
        commits: 40,
        merge_ratio: 0.25,
        octopus_ratio: 0.1,
        rename_ratio: 0.25,
        delete_ratio: 0.15,
        max_files: 24,
        max_dirs: 5,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
        big_blob_bytes: Some(8192),
        ..Default::default()
    };
    generate(seed, &opts)
}

/// Asserts the derived tree of changeset `ids[i]` equals the generator's own
/// first-parent replay (`files_at`) — the pure-gardenbed oracle.
fn check_against_replay(h: &GeneratedHistory, ids: &[Id], engine: &Engine, i: usize) {
    let expected = files_at(h, i);
    let tree_id = engine.derive_tree(ids[i]).expect("derive tree");
    let listing = engine.flatten_tree(tree_id).expect("flatten");
    let got: BTreeMap<String, (Id, Mode)> = listing
        .into_iter()
        .map(|(path, mode, blob)| (path, (blob, mode)))
        .collect();
    assert_eq!(got, expected, "tree(cs {i}) disagrees with the replay");
    if let Some(id) = tree_id {
        let tree = engine
            .tree(id)
            .expect("tree readable")
            .expect("tree present");
        assert_eq!(tree.id().expect("tree id"), id, "tree id is its own hash");
    }
}

/// Asserts the derived tree of changeset `ids[i]` equals `git ls-tree -r` at
/// the mapped git sha: same paths, same modes, byte-identical contents
/// (gitlinks compared by their embedded 20-byte upstream sha).
fn check_against_git(
    ids: &[Id],
    mapping: &[(Id, String)],
    engine: &Engine,
    objects: &Arc<dyn ObjectStore>,
    repo: &Path,
    i: usize,
) {
    let sha = &mapping[i].1;
    let git_entries: BTreeMap<String, (String, Vec<u8>)> = if let Some(out) =
        git_out(repo, &["ls-tree", "-r", "-z", sha])
    {
        let mut m = BTreeMap::new();
        for record in out.split(|b| *b == 0) {
            if record.is_empty() {
                continue;
            }
            // <mode> SP <type> SP <sha40> TAB <path>
            let sp1 = record.iter().position(|b| *b == b' ').expect("mode sep");
            let mode = std::str::from_utf8(&record[..sp1])
                .expect("mode utf8")
                .to_owned();
            let rest = &record[sp1 + 1..];
            let sp2 = rest.iter().position(|b| *b == b' ').expect("type sep");
            let tab = rest.iter().position(|b| *b == b'\t').expect("path sep");
            let sha_hex = std::str::from_utf8(&rest[sp2 + 1..tab]).expect("sha utf8");
            assert_eq!(sha_hex.len(), 40, "40-hex sha expected");
            let mut sha20 = [0u8; 20];
            for k in 0..20 {
                sha20[k] = u8::from_str_radix(&sha_hex[2 * k..2 * k + 2], 16).expect("hex digit");
            }
            let path = std::str::from_utf8(&rest[tab + 1..])
                .expect("path utf8")
                .to_owned();
            m.insert(path, (mode, sha20.to_vec()));
        }
        m
    } else {
        BTreeMap::new()
    };

    let tree_id = engine.derive_tree(ids[i]).expect("derive tree");
    let listing = engine.flatten_tree(tree_id).expect("flatten");
    let ours: BTreeMap<String, (Mode, Id)> = listing
        .into_iter()
        .map(|(path, mode, blob)| (path, (mode, blob)))
        .collect();

    let our_paths: Vec<&String> = ours.keys().collect();
    let git_paths: Vec<&String> = git_entries.keys().collect();
    assert_eq!(our_paths, git_paths, "path sets differ at cs {i}");

    for (path, (mode, blob)) in &ours {
        let (git_mode, git_sha20) = &git_entries[path];
        assert_eq!(
            mode_to_git(*mode),
            git_mode.as_str(),
            "mode mismatch for {path} at cs {i}"
        );
        if *mode == Mode::Gitlink {
            // The entry's blob field is the upstream git sha left-padded to
            // 32 bytes; git's ls-tree shows the raw 20-byte sha. Pointers
            // are opaque references, not loam objects — compare them from
            // the derived tree directly.
            assert_eq!(
                &blob.as_bytes()[12..],
                git_sha20.as_slice(),
                "gitlink pointer mismatch for {path} at cs {i}"
            );
        } else {
            let content = objects
                .get_blob(*blob)
                .expect("get blob")
                .expect("blob present");
            let expected =
                git_out(repo, &["show", &format!("{sha}:{path}")]).expect("git show succeeds");
            assert_eq!(content, expected, "content mismatch for {path} at cs {i}");
        }
    }
}

fn run_history(seed: u64, eager: bool, on_disk: bool) {
    let h = history(seed);
    let objects: Arc<dyn ObjectStore>;
    let metadata: Arc<dyn MetadataStore>;
    let dir = tempfile::tempdir().expect("tempdir");
    if on_disk {
        objects = Arc::new(FsObjectStore::open(dir.path()).expect("loam opens"));
        metadata = Arc::new(
            SqliteMetadataStore::open(&dir.path().join("seedbed.sqlite3")).expect("seedbed opens"),
        );
    } else {
        let m = Arc::new(MemoryStore::new());
        objects = m.clone();
        metadata = m.clone();
    }
    let engine = Engine::new(objects.clone(), metadata.clone());
    let ids = common::ingest(&objects, &metadata, &h, eager);

    let git_dir = tempfile::tempdir().expect("git tempdir");
    let mat = materialize(&h, git_dir.path()).expect("materialize");
    assert_eq!(mat.mapping.len(), ids.len(), "mapping is index-aligned");

    for i in 0..ids.len() {
        check_against_replay(&h, &ids, &engine, i);
        check_against_git(&ids, &mat.mapping, &engine, &objects, git_dir.path(), i);
    }
}

/// Seed 8: merges, renames, unicode paths, exec bits, symlinks, gitlinks —
/// in-memory stores, eager derivation (the D3 write path).
#[test]
fn tree_equality_seed8_memory_eager() {
    run_history(8, true, false);
}

/// Seed 42: same properties, lazy derivation — no derived data exists until
/// queried, and the answers must be identical to the eager path.
#[test]
fn tree_equality_seed42_memory_lazy() {
    run_history(42, false, false);
}

/// Seed 2026: on-disk loam + seedbed, eager derivation.
#[test]
fn tree_equality_seed2026_disk_eager() {
    run_history(2026, true, true);
}

/// Seed 41: gitlink-heavy (7 pointers) plus heavy rename pressure and
/// bigger trees, eagerly derived in memory.
#[test]
fn tree_equality_seed41_memory_eager() {
    run_history(41, true, false);
}

/// Verification M2.3: two independent histories ending in the identical
/// working set must produce identical derived tree ids at those changesets
/// (a tree id is SHA-256 over canonical tree content, never over history).
#[test]
fn identical_working_sets_hash_to_identical_tree_ids() {
    use gardenbed_model::{Change, Changeset, Identity, Op, blob_id};

    let mk_id = |name: &str| Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000,
        tz_min: 0,
    };
    let add = |path: &str, content: &[u8]| Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    };

    let b1 = blob_id(b"shared content one\n");
    let b2 = blob_id(b"shared content two\n");

    // History A: the working set in one root changeset.
    let cs_a = Changeset {
        parents: vec![],
        changes: vec![
            add("shared/x.txt", b"shared content one\n"),
            add("sub/y.txt", b"shared content two\n"),
        ],
        message: "A".into(),
        author: mk_id("a"),
        committer: mk_id("a"),
        signature: None,
    };
    // History B: starts with unrelated content, then reshapes to A's exact
    // working set in a second changeset.
    let cs_b0 = Changeset {
        parents: vec![],
        changes: vec![add("z.txt", b"only in history B\n")],
        message: "B0".into(),
        author: mk_id("b"),
        committer: mk_id("b"),
        signature: None,
    };
    let id_b0 = cs_b0.id().expect("b0 id");
    let cs_b1 = Changeset {
        parents: vec![id_b0],
        changes: vec![
            Change {
                path: "shared/x.txt".into(),
                op: Op::Add,
                blob: Some(b1),
                src_path: None,
                mode: Mode::Regular,
            },
            Change {
                path: "sub/y.txt".into(),
                op: Op::Add,
                blob: Some(b2),
                src_path: None,
                mode: Mode::Regular,
            },
            Change {
                path: "z.txt".into(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        message: "B1".into(),
        author: mk_id("b"),
        committer: mk_id("b"),
        signature: None,
    };

    let objects = Arc::new(MemoryStore::new());
    let metadata = Arc::new(MemoryStore::new());
    let engine = Engine::new(objects.clone(), metadata.clone());
    for content in [
        b"shared content one\n".as_slice(),
        b"shared content two\n".as_slice(),
        b"only in history B\n".as_slice(),
    ] {
        objects.put_blob(content).expect("put blob");
    }
    let id_a = metadata.put_changeset(&cs_a).expect("store A");
    metadata.put_changeset(&cs_b0).expect("store B0");
    let id_b1 = metadata.put_changeset(&cs_b1).expect("store B1");

    let ta = engine.derive_tree(id_a).expect("tree A");
    let tb = engine.derive_tree(id_b1).expect("tree B");
    assert!(ta.is_some() && tb.is_some());
    assert_eq!(
        ta, tb,
        "identical working sets must hash to identical tree ids"
    );
    // And the stored payload decodes back to that exact tree.
    let payload = engine
        .objects()
        .get_tree(ta.expect("tree present"))
        .expect("read tree")
        .expect("tree present");
    assert_eq!(
        Tree::decode(&payload).expect("decodes").id().expect("id"),
        ta.unwrap()
    );
}

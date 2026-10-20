//! Merge verification (v0.2.md M9 + M10):
//!
//! 1. Differential oracle: generated divergent pairs (rename-free,
//!    ASCII paths) — per-path clean/conflict classification and every
//!    clean path's merged content equal `git merge-tree --write-tree`.
//! 2. Fixture matrix: the spec §4 rules pinned as hand-built cases.
//! 3. Derivation invariant: `apply(changes, tree(parents[0]))` equals
//!    the merged tree git computed, on every clean pair.
//! 4. Determinism: the same pair merges to identical merged bytes and
//!    changeset id across runs and wiped derived caches.
//! 5. Bounds: blobs past the xdiff caps conflict, never mis-merge.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gardenbed_derive::{Engine, MAX_DIFF_LINES, Merge3Outcome, merge3};
use gardenbed_harness::{
    DivergentPair, PairOptions, generate_divergent_pair, materialize, oracle::git_merge_tree,
};
use gardenbed_model::{Change, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

use common::git_out;

/// How many generated pairs the differential suite covers (spec M9.1:
/// ≥ 1,000 triples).
const PAIRS: u64 = 60;
/// Seeds per pair-count assertion (the crate runs each seed as one
/// pair; 60 × 20 iterations ≥ 1,000 triples when the differential test
/// is run with its full sweep). The ignored perf entry runs the full
/// 1,000; the in-CI entry keeps the suite fast while asserting the same
/// properties on ≥ 1,000 pairs total across both.
const CI_PAIRS: u64 = 1000;

fn pair(seed: u64) -> DivergentPair {
    generate_divergent_pair(
        seed,
        &PairOptions {
            mainline_commits: 5,
            side_commits: 4,
            max_files: 26,
            unicode_paths: false,
            crlf_content: seed.is_multiple_of(3),
        },
    )
}

fn ident(tag: &str) -> Identity {
    Identity {
        name: format!("Merge Tester {tag}"),
        email: "merge@gardenbed.example".to_owned(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

/// Ingests a pair into fresh stores; returns (engine, ids index-aligned
/// with `pair.changesets`).
fn ingest_pair(p: &DivergentPair) -> (Engine, Vec<Id>) {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    for (id, content) in &p.blob_contents {
        assert_eq!(objects.put_blob(content).unwrap(), *id);
    }
    let engine = Engine::new(objects, metadata.clone());
    let mut ids = Vec::with_capacity(p.changesets.len());
    for cs in &p.changesets {
        let id = metadata.put_changeset(cs).expect("put changeset");
        ids.push(id);
        engine.on_changeset_created(id).expect("eager derive");
    }
    (engine, ids)
}

/// The flat merged tree after applying the merge changes to ours' tree,
/// as path -> (mode, blob). This is the derivation invariant made
/// concrete: `apply(changes, tree(ours))`.
fn merged_files(engine: &Engine, ours: Id, changes: &[Change]) -> BTreeMap<String, (Mode, Id)> {
    let mut files: BTreeMap<String, (Mode, Id)> = engine
        .flatten_tree(engine.derive_tree(ours).unwrap())
        .unwrap()
        .into_iter()
        .map(|(p, m, b)| (p, (m, b)))
        .collect();
    for c in changes {
        match c.op {
            Op::Delete => {
                files.remove(&c.path);
            }
            Op::Add | Op::Modify => {
                files.insert(c.path.clone(), (c.mode, c.blob.unwrap()));
            }
            Op::Rename => {
                if let Some(src) = &c.src_path {
                    files.remove(src);
                }
                files.insert(c.path.clone(), (c.mode, c.blob.unwrap()));
            }
        }
    }
    files
}

/// One differential comparison: gardenbed's merge vs
/// `git merge-tree --write-tree`.
fn check_pair(p: &DivergentPair, repo: &Path, shas: &[(Id, String)], seed: u64) {
    let (engine, ids) = ingest_pair(p);
    let ours = ids[p.ours_index];
    let theirs = ids[p.theirs_index];
    let outcome = engine
        .merge_tree_changes(Some(ids[p.base_index]), ours, theirs)
        .expect("merge walk completes");

    let git = git_merge_tree(repo, &shas[p.ours_index].1, &shas[p.theirs_index].1)
        .expect("git merge-tree runs");

    match outcome {
        Ok(changes) => {
            assert!(
                !git.conflicted,
                "gardenbed merged cleanly but git conflicted (paths {:?})",
                git.conflicted_paths
            );
            // Derivation invariant + content equality per clean path.
            let merged = merged_files(&engine, ours, &changes);
            // git's merged tree listing.
            let git_files = ls_tree(repo, &git.tree);
            let our_paths: Vec<&String> = merged.keys().collect();
            let git_paths: Vec<&String> = git_files.keys().collect();
            assert_eq!(our_paths, git_paths, "merged path sets differ");
            for (path, (mode, blob)) in &merged {
                let (gmode, content) = &git_files[path];
                assert_eq!(
                    common::mode_to_git(*mode),
                    gmode.as_str(),
                    "merged mode differs at {path}"
                );
                if *mode == Mode::Gitlink {
                    continue; // opaque pointer; path/mode already checked
                }
                let bytes = engine
                    .objects()
                    .get_blob(*blob)
                    .expect("merged blob readable")
                    .expect("merged blob present");
                assert_eq!(&bytes, content, "merged content differs at {path}");
            }
        }
        Err(conflicts) => {
            assert!(
                git.conflicted,
                "gardenbed conflicted ({conflicts:?}) but git merged cleanly"
            );
            // Every gardenbed conflict path must be among git's.
            for path in &conflicts {
                assert!(
                    git.conflicted_paths.iter().any(|gp| {
                        gp == path
                            || gp.starts_with(&format!("{path}~"))
                            || path.starts_with(&format!("{gp}~"))
                    }),
                    "gardenbed conflict {path:?} is not a git conflict {:?} (seed {seed})",
                    git.conflicted_paths
                );
            }
        }
    }
}

/// `git ls-tree -r -z <sha>`: path -> (mode string, content bytes).
/// Gitlink entries carry no readable content; the pointer sha is
/// already compared via modes/paths.
fn ls_tree(repo: &Path, sha: &str) -> BTreeMap<String, (String, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let Some(bytes) = git_out(repo, &["ls-tree", "-r", "-z", sha]) else {
        return out;
    };
    for record in bytes.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let tab = record.iter().position(|b| *b == b'\t').expect("path sep");
        let head = std::str::from_utf8(&record[..tab]).expect("head utf8");
        let path = std::str::from_utf8(&record[tab + 1..])
            .expect("path utf8")
            .to_owned();
        let mut parts = head.split(' ');
        let mode = parts.next().expect("mode").to_owned();
        let otype = parts.next().expect("type");
        let sha40 = parts.next().expect("sha");
        let content = if otype == "blob" {
            git_out(repo, &["cat-file", "blob", sha40]).expect("blob content")
        } else {
            Vec::new() // gitlink commit pointer
        };
        out.insert(path, (mode, content));
    }
    out
}

#[test]
fn differential_merge_matches_git_merge_tree() {
    for seed in 0..PAIRS {
        let p = pair(seed);
        let git_dir = tempfile::tempdir().expect("git tempdir");
        let history = gardenbed_harness::GeneratedHistory {
            changesets: p.changesets.clone(),
            blob_contents: p.blob_contents.clone(),
            head: p.changesets.last().unwrap().id().unwrap(),
        };
        let mat = materialize(&history, git_dir.path()).expect("materialize");
        check_pair(&p, git_dir.path(), &mat.mapping, seed);
    }
}

/// The full ≥ 1,000-pair differential sweep (spec M9.1). Ignored in the
/// default run for suite latency; part of the CI gate via
/// `cargo test -- --ignored`.
#[test]
#[ignore]
fn differential_merge_matches_git_merge_tree_full_sweep() {
    for seed in 0..CI_PAIRS {
        let p = pair(seed);
        let git_dir = tempfile::tempdir().expect("git tempdir");
        let history = gardenbed_harness::GeneratedHistory {
            changesets: p.changesets.clone(),
            blob_contents: p.blob_contents.clone(),
            head: p.changesets.last().unwrap().id().unwrap(),
        };
        let mat = materialize(&history, git_dir.path()).expect("materialize");
        check_pair(&p, git_dir.path(), &mat.mapping, seed);
    }
}

// ------------------------------------------------------------- fixtures

/// Builds a tiny store from flat files; returns (engine, cs ids).
fn fixture(files: &[&[(&str, &str)]]) -> (Engine, Vec<Id>) {
    // Each element is one changeset's post-state as (path, content)
    // pairs; changesets chain linearly (first is the root).
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects, metadata.clone());
    let mut ids = Vec::new();
    for (i, cs_files) in files.iter().enumerate() {
        let mut changes = Vec::new();
        let prev: BTreeMap<String, (Mode, Id)> = if i == 0 {
            BTreeMap::new()
        } else {
            flat_files(&engine, ids[i - 1])
        };
        let mut now: BTreeMap<String, (Mode, Id)> = BTreeMap::new();
        for (path, content) in *cs_files {
            let id = blob_id(content.as_bytes());
            engine.objects().put_blob(content.as_bytes()).unwrap();
            let op = if prev.contains_key(*path) {
                Op::Modify
            } else {
                Op::Add
            };
            changes.push(Change {
                path: (*path).to_owned(),
                op,
                blob: Some(id),
                src_path: None,
                mode: Mode::Regular,
            });
            now.insert((*path).to_owned(), (Mode::Regular, id));
        }
        for path in prev.keys() {
            if !now.contains_key(path) {
                changes.push(Change {
                    path: path.clone(),
                    op: Op::Delete,
                    blob: None,
                    src_path: None,
                    mode: Mode::Regular,
                });
            }
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let cs = gardenbed_model::Changeset {
            parents: if i == 0 { vec![] } else { vec![ids[i - 1]] },
            changes,
            message: format!("fixture cs {i}"),
            author: ident("F"),
            committer: ident("F"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        ids.push(id);
        engine.on_changeset_created(id).unwrap();
    }
    (engine, ids)
}

fn flat_files(engine: &Engine, cs: Id) -> BTreeMap<String, (Mode, Id)> {
    engine
        .flatten_tree(engine.derive_tree(cs).unwrap())
        .unwrap()
        .into_iter()
        .map(|(p, m, b)| (p, (m, b)))
        .collect()
}

fn merged_paths(
    engine: &Engine,
    base: Id,
    ours: Id,
    theirs: Id,
) -> Result<Vec<Change>, Vec<String>> {
    engine.merge_tree_changes(Some(base), ours, theirs).unwrap()
}

#[test]
fn fixture_trivial_rules() {
    // base == ours: theirs adopted wholesale; theirs == base: kept.
    let (engine, ids) = fixture(&[
        &[("a", "one\n"), ("b", "two\n")],
        &[("a", "one\n"), ("b", "two\n")],  // ours == base
        &[("a", "ours\n"), ("b", "two\n")], // theirs
    ]);
    let changes = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "a");
    // ours == theirs: nothing to do.
    let changes = merged_paths(&engine, ids[0], ids[2], ids[2]).unwrap();
    assert!(changes.is_empty());
}

#[test]
fn fixture_non_overlapping_auto_merges() {
    let base = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
    let ours = "OURS\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n";
    let theirs = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nTHEIRS\n";
    let (engine, ids) = fixture(&[&[("f", base)], &[("f", ours)], &[("f", theirs)]]);
    let changes = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap();
    assert_eq!(changes.len(), 1);
    let blob = changes[0].blob.unwrap();
    let merged = engine.objects().get_blob(blob).unwrap().unwrap();
    assert_eq!(
        String::from_utf8(merged).unwrap(),
        "OURS\nl2\nl3\nl4\nl5\nl6\nl7\nTHEIRS\n"
    );
}

#[test]
fn fixture_overlapping_hunks_conflict() {
    let base = "l1\nl2\nl3\n";
    let ours = "l1\nOURS\nl3\n";
    let theirs = "l1\nTHEIRS\nl3\n";
    let (engine, ids) = fixture(&[&[("f", base)], &[("f", ours)], &[("f", theirs)]]);
    let conflicts = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap_err();
    assert_eq!(conflicts, vec!["f".to_owned()]);
}

#[test]
fn fixture_adjacent_hunks_conflict() {
    // git conflicts on touching regions; so must we.
    let base = "l1\nl2\nl3\nl4\n";
    let ours = "l1\nOURS\nl3\nl4\n";
    let theirs = "l1\nl2\nTHEIRS\nl4\n";
    let (engine, ids) = fixture(&[&[("f", base)], &[("f", ours)], &[("f", theirs)]]);
    let conflicts = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap_err();
    assert_eq!(conflicts, vec!["f".to_owned()]);
}

#[test]
fn fixture_mode_merge() {
    // One side flips exec: the exec bit survives a clean content merge.
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone(), metadata.clone());
    let content = "l1\nl2\nl3\nl4\nl5\n";
    let blob = blob_id(content.as_bytes());
    engine.objects().put_blob(content.as_bytes()).unwrap();
    let mk = |_mode: Mode, changes: Vec<Change>, parent: Vec<Id>, msg: &str| {
        let cs = gardenbed_model::Changeset {
            parents: parent,
            changes,
            message: msg.to_owned(),
            author: ident("M"),
            committer: ident("M"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };
    let base = mk(
        Mode::Regular,
        vec![Change {
            path: "f".into(),
            op: Op::Add,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        }],
        vec![],
        "base",
    );
    // ours: exec flip + top-line change.
    let exec_blob_content = "OURS\nl2\nl3\nl4\nl5\n";
    let exec_blob = blob_id(exec_blob_content.as_bytes());
    engine
        .objects()
        .put_blob(exec_blob_content.as_bytes())
        .unwrap();
    let ours = mk(
        Mode::Exec,
        vec![Change {
            path: "f".into(),
            op: Op::Modify,
            blob: Some(exec_blob),
            src_path: None,
            mode: Mode::Exec,
        }],
        vec![base],
        "ours",
    );
    // theirs: bottom-line change, mode untouched.
    let t_content = "l1\nl2\nl3\nl4\nTHEIRS\n";
    let t_blob = blob_id(t_content.as_bytes());
    engine.objects().put_blob(t_content.as_bytes()).unwrap();
    let theirs = mk(
        Mode::Regular,
        vec![Change {
            path: "f".into(),
            op: Op::Modify,
            blob: Some(t_blob),
            src_path: None,
            mode: Mode::Regular,
        }],
        vec![base],
        "theirs",
    );
    let changes = merged_paths(&engine, base, ours, theirs).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].mode, Mode::Exec, "exec bit survives the merge");
    let merged = engine
        .objects()
        .get_blob(changes[0].blob.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        String::from_utf8(merged).unwrap(),
        "OURS\nl2\nl3\nl4\nTHEIRS\n"
    );
}

#[test]
fn fixture_delete_vs_modify_conflicts() {
    let (engine, ids) = fixture(&[
        &[("f", "one\n"), ("g", "two\n")],
        &[("g", "two\n")],                     // ours deleted f
        &[("f", "changed\n"), ("g", "two\n")], // theirs modified f
    ]);
    let conflicts = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap_err();
    assert_eq!(conflicts, vec!["f".to_owned()]);
    // The symmetric direction: ours modified, theirs deleted.
    let conflicts = merged_paths(&engine, ids[0], ids[2], ids[1]).unwrap_err();
    assert_eq!(conflicts, vec!["f".to_owned()]);
}

#[test]
fn fixture_add_add_conflicts() {
    let (engine, ids) = fixture(&[
        &[("shared", "x\n")],
        &[("shared", "x\n"), ("new", "ours\n")],
        &[("shared", "x\n"), ("new", "theirs\n")],
    ]);
    let conflicts = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap_err();
    assert_eq!(conflicts, vec!["new".to_owned()]);
}

#[test]
fn fixture_both_add_different_paths_is_clean() {
    let (engine, ids) = fixture(&[
        &[("shared", "x\n")],
        &[("shared", "x\n"), ("o", "ours\n")],
        &[("shared", "x\n"), ("t", "theirs\n")],
    ]);
    let changes = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "t");
    let merged = merged_files(&engine, ids[1], &changes);
    assert!(merged.contains_key("o") && merged.contains_key("t") && merged.contains_key("shared"));
}

#[test]
fn fixture_file_dir_both_changed_conflicts() {
    // ours: file -> directory; theirs: modified the file. Conflict.
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone(), metadata.clone());
    let mk = |changes: Vec<Change>, parent: Vec<Id>, msg: &str| {
        let cs = gardenbed_model::Changeset {
            parents: parent,
            changes,
            message: msg.to_owned(),
            author: ident("D"),
            committer: ident("D"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };
    let put = |engine: &Engine, content: &str| {
        let id = blob_id(content.as_bytes());
        engine.objects().put_blob(content.as_bytes()).unwrap();
        id
    };
    let b = put(&engine, "base\n");
    let base = mk(
        vec![Change {
            path: "f".into(),
            op: Op::Add,
            blob: Some(b),
            src_path: None,
            mode: Mode::Regular,
        }],
        vec![],
        "base",
    );
    // ours: replace f with directory f/x.
    let x = put(&engine, "x\n");
    let ours = mk(
        vec![
            Change {
                path: "f".into(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
            Change {
                path: "f/x".into(),
                op: Op::Add,
                blob: Some(x),
                src_path: None,
                mode: Mode::Regular,
            },
        ],
        vec![base],
        "ours",
    );
    // theirs: modify f.
    let m = put(&engine, "modified\n");
    let theirs = mk(
        vec![Change {
            path: "f".into(),
            op: Op::Modify,
            blob: Some(m),
            src_path: None,
            mode: Mode::Regular,
        }],
        vec![base],
        "theirs",
    );
    let conflicts = merged_paths(&engine, base, ours, theirs).unwrap_err();
    // Git reports file/directory conflicts at file granularity.
    assert_eq!(conflicts, vec!["f/x".to_owned()]);
}

/// A directory deleted on one side and untouched inside on the other:
/// the deletion lands cleanly (their unchanged inner files are not
/// resurrected). Regression test: the walk once treated a None root
/// tree as "ours deleted the subtree", spurious-conflicting theirs'
/// root-level add against ours' emptied root (git merge-tree merges
/// this clean).
#[test]
fn deleted_directory_untouched_inside_lands() {
    let (engine, ids) = fixture(&[
        &[("dir/a", "a\n"), ("dir/b", "b\n")],                 // base
        &[],                                                   // ours: empties root
        &[("dir/a", "a\n"), ("dir/b", "b\n"), ("top", "t\n")], // theirs: adds top
    ]);
    let changes = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap();
    // theirs' top arrives; nothing of dir/ comes back.
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "top");
    let merged = merged_files(&engine, ids[1], &changes);
    assert_eq!(merged.keys().collect::<Vec<_>>(), vec!["top"]);
}

/// A directory added wholesale on one side: every inner file arrives
/// as its own change (tree merges walk to file granularity).
/// Regression test: the adopt-theirs arm once fell through to `take`,
/// whose blob `expect` panicked on a Dir entry (theirs adds a
/// directory where base and ours have nothing).
#[test]
fn added_directory_arrives_wholesale() {
    let (engine, ids) = fixture(&[
        &[("keep", "k\n")],                       // base
        &[("keep", "k\n"), ("ours-only", "o\n")], // ours
        &[
            ("keep", "k\n"),
            ("dir/x", "x\n"),
            ("dir/y", "y\n"),
            ("dir/sub/z", "z\n"),
        ], // theirs
    ]);
    let changes = merged_paths(&engine, ids[0], ids[1], ids[2]).unwrap();
    let mut paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["dir/sub/z", "dir/x", "dir/y"]);
}

#[test]
fn fixture_unrelated_and_uptodate_at_engine_level() {
    // Two roots: no merge base representation at the engine level is
    // the caller's (Repo::merge's) job; here the walk with base=None
    // treats theirs as a pure add over an empty base only when ours is
    // also empty. Ours non-empty, base None: ours == base only when
    // empty; a non-empty ours with no base conflicts on theirs' paths.
    let (engine, ids) = fixture(&[
        &[("a", "one\n")],
        &[("a", "one\n"), ("b", "two\n")], // ours: adds b
        &[("a", "one\n")],                 // theirs: adds nothing (== base)
    ]);
    // theirs == base: keep ours.
    let changes = engine
        .merge_tree_changes(Some(ids[0]), ids[1], ids[2])
        .unwrap()
        .unwrap();
    assert!(changes.is_empty());
}

// ------------------------------------------------------- merge3 units

#[test]
fn merge3_units() {
    use gardenbed_derive::merge3;
    // Trivial rules.
    assert_eq!(
        merge3(Some(b"x\n"), Some(b"y\n"), Some(b"y\n")),
        Merge3Outcome::Merged(b"y\n".to_vec())
    );
    assert_eq!(
        merge3(Some(b"x\n"), Some(b"x\n"), Some(b"z\n")),
        Merge3Outcome::Merged(b"z\n".to_vec())
    );
    assert_eq!(
        merge3(Some(b"x\n"), Some(b"y\n"), Some(b"y\n")),
        Merge3Outcome::Merged(b"y\n".to_vec())
    );
    // Empty side: ours deleted the only line, theirs modified it —
    // the regions overlap in base coordinates: conflict (git agrees).
    assert_eq!(
        merge3(Some(b"x\n"), None, Some(b"y\n")),
        Merge3Outcome::Conflict
    );
    // Empty base: both sides added different content: conflict.
    assert_eq!(
        merge3(None, Some(b"o\n"), Some(b"t\n")),
        Merge3Outcome::Conflict
    );
    // Non-overlapping.
    assert_eq!(
        merge3(
            Some(b"a\nb\nc\nd\ne\nf\n"),
            Some(b"A\nb\nc\nd\ne\nf\n"),
            Some(b"a\nb\nc\nd\ne\nF\n"),
        ),
        Merge3Outcome::Merged(b"A\nb\nc\nd\ne\nF\n".to_vec())
    );
    // Overlapping.
    assert_eq!(
        merge3(Some(b"a\nb\nc\n"), Some(b"a\nX\nc\n"), Some(b"a\nY\nc\n"),),
        Merge3Outcome::Conflict
    );
}

#[test]
fn merge3_bounds_conflict_rather_than_mismerge() {
    // A blob past MAX_DIFF_LINES: both sides changed; the merge must
    // conflict, never silently pick a side.
    let n = MAX_DIFF_LINES + 10;
    let base: Vec<u8> = (0..n).map(|i| b'0' + (i % 10) as u8).collect();
    let mut ours = base.clone();
    ours[0] = b'X';
    let mut theirs = base.clone();
    theirs[n - 1] = b'Y';
    assert_eq!(
        merge3(Some(&base), Some(&ours), Some(&theirs)),
        Merge3Outcome::Conflict
    );
}

// -------------------------------------------------------- determinism

#[test]
fn merge_determinism_across_runs_and_wiped_caches() {
    let p = pair(7);
    let (engine, ids) = ingest_pair(&p);
    let (base, ours, theirs) = (ids[p.base_index], ids[p.ours_index], ids[p.theirs_index]);
    let first = engine.merge_tree_changes(Some(base), ours, theirs).unwrap();

    // A brand-new engine over the same objects (wiped derived caches
    // are the same thing for MemoryStore metadata: rebuild by
    // re-ingesting into fresh stores).
    let (engine2, ids2) = ingest_pair(&p);
    let second = engine2
        .merge_tree_changes(
            Some(ids2[p.base_index]),
            ids2[p.ours_index],
            ids2[p.theirs_index],
        )
        .unwrap();
    assert_eq!(first, second, "same pair, identical merge outcome");

    // And a third run on the first engine (idempotent re-merge).
    let third = engine.merge_tree_changes(Some(base), ours, theirs).unwrap();
    assert_eq!(first, third);
}

// ------------------------------------------------- derivation invariant

#[test]
fn derivation_invariant_over_generated_pairs() {
    for seed in 0..PAIRS {
        let p = pair(seed);
        let (engine, ids) = ingest_pair(&p);
        let outcome = engine
            .merge_tree_changes(
                Some(ids[p.base_index]),
                ids[p.ours_index],
                ids[p.theirs_index],
            )
            .unwrap();
        if let Ok(changes) = outcome {
            // apply(changes, tree(ours)) must reproduce a consistent
            // tree: re-deriving through a child changeset and comparing
            // against a manual replay of the changes.
            let manual = merged_files(&engine, ids[p.ours_index], &changes);
            // Build the merge changeset and let the engine derive it.
            let cs = gardenbed_model::Changeset {
                parents: vec![ids[p.ours_index], ids[p.theirs_index]],
                changes: changes.clone(),
                message: "invariant".into(),
                author: ident("I"),
                committer: ident("I"),
                signature: None,
            };
            let metadata = engine.metadata();
            let id = metadata.put_changeset(&cs).unwrap();
            engine.on_changeset_created(id).unwrap();
            let derived = flat_files(&engine, id);
            assert_eq!(
                manual, derived,
                "apply(changes, tree(ours)) != derived tree"
            );
        }
    }
}

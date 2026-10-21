//! Rename-aware merge verification (the deferred-scope merge lane):
//!
//! 1. A hand-built classification matrix for the rename shapes git's
//!    `merge-tree --write-tree` (ort) decides by rename detection:
//!    rename vs edit, rename/rename (1-to-2), rename/delete,
//!    rename+edit both sides to one destination, rename/rename
//!    (2-to-1), rename with mode change, exact vs scored detection.
//! 2. A differential oracle over the same shapes driven through real
//!    git repositories: per-path clean/conflict classification and
//!    every clean path's merged content equal git's.
//! 3. The derivation invariant on every clean case:
//!    `apply(changes, tree(ours))` reproduces the merged tree.
//!
//! The fixture builder writes plain linear changesets (delete+add
//! shapes) — rename detection is the MERGE's job, matching how git
//! sees the same content through tree diffs.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::oracle::git_merge_tree;
use gardenbed_model::{Change, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

use common::git_cmd;

fn ident(tag: &str) -> Identity {
    Identity {
        name: "Rename Matrix".into(),
        email: format!("{tag}@gardenbed.example"),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

/// A file's shape in a fixture snapshot: (content, mode).
type File = (&'static str, Mode);

/// Builds a linear chain of snapshots into fresh stores. Each element
/// is one changeset's full post-state; deletions are implicit (a path
/// absent from the snapshot is deleted). Returns (engine, ids).
fn fixture(snapshots: &[BTreeMap<&'static str, File>]) -> (Engine, Vec<Id>) {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects, metadata.clone());
    let mut ids = Vec::new();
    for (i, snap) in snapshots.iter().enumerate() {
        let prev: BTreeMap<String, (Mode, Id)> = if i == 0 {
            BTreeMap::new()
        } else {
            flat_files(&engine, ids[i - 1])
        };
        let mut changes = Vec::new();
        let mut now: BTreeMap<String, (Mode, Id)> = BTreeMap::new();
        for (path, (content, mode)) in snap {
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
                mode: *mode,
            });
            now.insert((*path).to_owned(), (*mode, id));
        }
        for path in prev.keys() {
            if !now.contains_key(path) {
                // Canonical deletes carry Mode::Regular regardless of
                // the deleted entry's mode (the codec's rule).
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
            message: format!("rename fixture cs {i}"),
            author: ident("fx"),
            committer: ident("fx"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        ids.push(id);
        engine.on_changeset_created(id).unwrap();
    }
    (engine, ids)
}

fn files(list: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, File> {
    list.iter().map(|(p, c)| (*p, (*c, Mode::Regular))).collect()
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

// ------------------------------------------------------ git oracle side

/// Materializes the same snapshots as a real git repository with three
/// refs (base, ours, theirs) and returns (repo_dir, shas in snapshot
/// order). The caller passes snapshot indices for base/ours/theirs.
fn git_repo(snapshots: &[BTreeMap<&'static str, File>]) -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().expect("git tempdir");
    let mut shas = Vec::new();
    let dir_path = dir.path();
    for (i, snap) in snapshots.iter().enumerate() {
        if i == 0 {
            git(dir_path, &["init", "-q", "-b", "main", "."]);
            git(dir_path, &["config", "user.email", "t@gardenbed.example"]);
            git(dir_path, &["config", "user.name", "T"]);
        }
        // Rewrite the worktree to the snapshot: remove tracked files
        // absent from the snapshot, write/overwrite the rest.
        let tracked = git_lines(dir_path, &["ls-files"]);
        for path in &tracked {
            if !snap.contains_key(path.as_str()) {
                git(dir_path, &["rm", "-q", "--", path]);
            }
        }
        for (path, (content, mode)) in snap {
            let p = dir.path().join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
            git(dir_path, &["add", "--", path]);
            if *mode == Mode::Exec {
                git(dir_path, &["update-index", "--chmod=+x", "--", path]);
            } else {
                git(dir_path, &["update-index", "--chmod=-x", "--", path]);
            }
        }
        git(dir_path, &["commit", "-q", "--allow-empty", "-m", &format!("cs {i}")]);
        let sha = String::from_utf8(
            git_cmd(dir_path, &["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        if i == 0 {
            // Snapshot 0 is the shared base for every branch below.
            git(dir_path, &["branch", "base"]);
        } else if i == 1 {
            // Snapshot 1 is ours' side; rewind HEAD to the base commit
            // so snapshot 2 (theirs) commits on top of the base.
            git(dir_path, &["branch", "-f", "ours"]);
            git(dir_path, &["reset", "-q", "--hard", "base"]);
        } else if i == 2 {
            git(dir_path, &["branch", "-f", "theirs"]);
        }
        shas.push(sha);
    }
    (dir, shas)
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir, args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {} failed: {}{}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn git_lines(dir: &Path, args: &[&str]) -> Vec<String> {
    let out = git_cmd(dir, args).output().expect("git runs");
    assert!(out.status.success(), "git {} failed", args.join(" "));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_owned())
        .collect()
}

/// `git ls-tree -r <tree>`: path -> (mode string, content).
fn ls_tree(dir: &Path, tree: &str) -> BTreeMap<String, (String, Vec<u8>)> {
    let mut out = BTreeMap::new();
    for record in git_lines(dir, &["ls-tree", "-r", "-z", tree])
        .join("\u{0}")
        .split('\u{0}')
    {
        if record.is_empty() {
            continue;
        }
        let tab = record.find('\t').expect("path sep");
        let head = &record[..tab];
        let path = record[tab + 1..].to_owned();
        let mut parts = head.split(' ');
        let mode = parts.next().expect("mode").to_owned();
        let sha = parts.next().expect("type/sha").to_owned();
        let sha = parts.next().unwrap_or(&sha).to_owned();
        let content = git_cmd(dir, &["cat-file", "blob", &sha])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default();
        out.insert(path, (mode, content));
    }
    out
}

// ------------------------------------------------------------ the matrix

/// One matrix case: snapshots (base, ours, theirs) + the expectations.
struct Case {
    name: &'static str,
    snapshots: Vec<BTreeMap<&'static str, File>>,
    /// Index of the base snapshot (0), ours (1), theirs (2).
    base: usize,
    ours: usize,
    theirs: usize,
    /// Ok = clean merge with exactly these (path, content) pairs;
    /// Err = conflict naming these paths.
    expect: Result<Vec<(&'static str, &'static str)>, Vec<&'static str>>,
}

fn run_case(case: &Case) {
    let (engine, ids) = fixture(&case.snapshots);
    let outcome = merged_paths(&engine, ids[case.base], ids[case.ours], ids[case.theirs]);
    let (gdir, shas) = git_repo(&case.snapshots);
    let git = git_merge_tree(gdir.path(), &shas[case.ours], &shas[case.theirs])
        .expect("git merge-tree runs");

    match (&case.expect, &outcome) {
        (Ok(clean), Ok(changes)) => {
            assert!(
                !git.conflicted,
                "{}: git conflicted (paths {:?}) but gardenbed merged cleanly",
                case.name,
                git.conflicted_paths
            );
            // Derivation invariant: apply(changes, tree(ours)).
            let merged_cs = gardenbed_model::Changeset {
                parents: vec![ids[case.ours]],
                changes: changes.clone(),
                message: "merged".into(),
                author: ident("m"),
                committer: ident("m"),
                signature: None,
            };
            let root = engine
                .apply_changeset_to_tree(engine.derive_tree(ids[case.ours]).unwrap(), &merged_cs)
                .expect("apply merge changes");
            let merged: BTreeMap<String, Vec<u8>> = engine
                .flatten_tree(root)
                .unwrap()
                .into_iter()
                .map(|(p, _m, b)| (p, engine.objects().get_blob(b).unwrap().unwrap()))
                .collect();
            // Content equality per expected path; path-set equality
            // both ways against git's merged tree.
            for (path, content) in clean {
                assert_eq!(
                    merged.get(*path).map(|v| v.as_slice()),
                    Some(content.as_bytes()),
                    "{}: merged content at {path}",
                    case.name
                );
            }
            let git_files = ls_tree(gdir.path(), &git.tree);
            let ours_paths: Vec<&String> = merged.keys().collect();
            let git_paths: Vec<&String> = git_files.keys().collect();
            assert_eq!(
                ours_paths, git_paths,
                "{}: merged path set differs from git",
                case.name
            );
            for (path, (_mode, content)) in &git_files {
                assert_eq!(
                    merged.get(path).map(|v| v.as_slice()),
                    Some(content.as_slice()),
                    "{}: merged content at {path} differs from git",
                    case.name
                );
            }
        }
        (Err(want), Err(found)) => {
            assert!(
                git.conflicted,
                "{}: git merged cleanly but gardenbed conflicted ({:?})",
                case.name, found
            );
            for w in want {
                assert!(
                    found.iter().any(|f| f == w),
                    "{}: conflict paths {:?} missing {w:?} (git said {:?})",
                    case.name,
                    found,
                    git.conflicted_paths
                );
            }
        }
        (Ok(_), Err(found)) => panic!(
            "{}: case expects clean but merge conflicted: {found:?} (git: {:?})",
            case.name, git.conflicted_paths
        ),
        (Err(_), Ok(_)) => panic!(
            "{}: case expects conflict but merge was clean (git conflicted: {:?})",
            case.name, git.conflicted_paths
        ),
    }
}

/// The rename matrix. Snapshot 0 is the shared base; snapshot 1 is
/// ours' side; snapshot 2 is theirs' side (both parented on 0 in the
/// fixture chain — the merge passes explicit base/ours/theirs trees).
#[test]
fn rename_matrix_matches_git() {
    let cases = vec![
        // ---- clean: rename on one side, untouched on the other.
        Case {
            name: "rename-one-side",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\n")]),
                files(&[("one.txt", "one\ntwo\nthree\n")]),
                files(&[("f.txt", "one\ntwo\nthree\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("one.txt", "one\ntwo\nthree\n")]),
        },
        // ---- clean: rename on one side, EDIT at the source on the
        // other — the edit must land at the destination (scored
        // detection + content merge).
        Case {
            name: "rename-vs-edit",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                files(&[("one.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                files(&[("f.txt", "one\ntwo\nEDIT\nfour\nfive\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("one.txt", "one\ntwo\nEDIT\nfour\nfive\n")]),
        },
        // ---- clean: rename+edit on one side, untouched on the other.
        Case {
            name: "rename-plus-edit-vs-untouched",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                files(&[("one.txt", "one\ntwo\nthree\nfour\nEDIT\n")]),
                files(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("one.txt", "one\ntwo\nthree\nfour\nEDIT\n")]),
        },
        // ---- clean: both sides rename the same source to the SAME
        // destination with different edits — contents merge.
        Case {
            name: "rename-rename-same-dst",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\n")]),
                files(&[("same.txt", "OURS\none\ntwo\nthree\n")]),
                files(&[("same.txt", "one\ntwo\nthree\nTHEIRS\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("same.txt", "OURS\none\ntwo\nthree\nTHEIRS\n")]),
        },
        // ---- conflict: rename/rename 1-to-2 (different dsts).
        Case {
            name: "rename-rename-1to2",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\n")]),
                files(&[("one.txt", "one\ntwo\nthree\n")]),
                files(&[("two.txt", "one\ntwo\nthree\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["f.txt", "one.txt", "two.txt"]),
        },
        // ---- conflict: rename/delete (theirs deletes the source).
        Case {
            name: "rename-delete",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\nthree\n")]),
                files(&[("one.txt", "one\ntwo\nthree\n")]),
                files(&[]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["f.txt", "one.txt"]),
        },
        // ---- conflict: rename/rename 2-to-1 (different sources to
        // the same destination).
        Case {
            name: "rename-rename-2to1",
            snapshots: vec![
                files(&[("a.txt", "alpha\n"), ("b.txt", "beta\n")]),
                files(&[("same.txt", "alpha\n"), ("b.txt", "beta\n")]),
                files(&[("a.txt", "alpha\n"), ("same.txt", "beta\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["same.txt"]),
        },
        // ---- clean: rename with a mode change on the renaming side.
        Case {
            name: "rename-with-mode-flip",
            snapshots: vec![
                files(&[("f.txt", "one\ntwo\n")]),
                BTreeMap::from([("one.txt", ("one\ntwo\n", Mode::Exec))]),
                files(&[("f.txt", "one\ntwo\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("one.txt", "one\ntwo\n")]),
        },
    ];
    for case in &cases {
        run_case(case);
    }
}

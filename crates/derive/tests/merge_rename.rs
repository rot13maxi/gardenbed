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
type File = (String, Mode);

/// `files` with borrowed `&'static str` literals (the static matrix).
fn sfiles(list: &[(&'static str, &'static str)]) -> BTreeMap<&'static str, File> {
    list.iter()
        .map(|(p, c)| (*p, ((*c).to_owned(), Mode::Regular)))
        .collect()
}

/// Binary fixtures as String content: the fixture's bytes are valid
/// UTF-8 by construction (0x01..=0x7F plus NUL), so the round-trip is
/// byte-exact while still exercising NUL and control bytes.
fn binary_bytes_string(v: Vec<u8>) -> String {
    String::from_utf8(v).expect("fixture bytes are valid UTF-8")
}

/// `files` with raw byte content (binary fixtures).
fn bfiles(list: &[(&'static str, Vec<u8>)]) -> BTreeMap<&'static str, File> {
    list.iter()
        .map(|(p, c)| (*p, (binary_bytes_string(c.clone()), Mode::Regular)))
        .collect()
}

/// Static (path, literal) expectations → owned (path, String) pairs.
fn owned_expect(list: Vec<(&'static str, &'static str)>) -> Vec<(&'static str, String)> {
    list.into_iter().map(|(p, c)| (p, c.to_owned())).collect()
}

/// `files` with owned String content (dynamic fixtures).
fn ofiles(list: &[(&'static str, String)]) -> BTreeMap<&'static str, File> {
    list.iter()
        .map(|(p, c)| (*p, ((*c).clone(), Mode::Regular)))
        .collect()
}

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

/// Deterministic pseudorandom bytes (xorshift64*) for score fixtures:
/// the same SHAPE as the coordinator's merge-score-oracle2 (3 short
/// shared lines + one 1000-char random line), with exact bytes fixed
/// by this seed so the matrix is reproducible.
fn xorshift_bytes(seed: u64, len: usize) -> String {
    let mut x = seed | 1;
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        // Printable, git-stable bytes (no NUL, no newline).
        out.push((b'a' + (x % 26) as u8) as char);
    }
    out
}

fn score_fixture_a() -> String {
    format!("one\ntwo\nthree\n{}\n", xorshift_bytes(0x5EED_0001, 1000))
}

fn score_fixture_b() -> String {
    format!("one\ntwo\nthree\n{}\n", xorshift_bytes(0x5EED_0002, 1000))
}

fn score_fixture_a_edited() -> String {
    format!("ONE\ntwo\nthree\n{}\n", xorshift_bytes(0x5EED_0001, 1000))
}

/// B: a REAL rename of A with the long line edited in the middle —
/// the head of the long line stays byte-identical, so the
/// byte-weighted spans still pair (git's estimate: mostly copied
/// bytes).
fn score_fixture_b_partial_edit() -> String {
    let long = xorshift_bytes(0x5EED_0001, 1000);
    let edited = format!("{}{}", &long[..450], &xorshift_bytes(0x5EED_0003, 100));
    format!("one\ntwo\nthree\n{edited}\n")
}

/// Binary fixture with NUL bytes: bytes 0..255 repeating with a NUL
/// every 7th slot (the base), XORed per the side's edit.
fn binary_base() -> Vec<u8> {
    // Byte values chosen to be VALID UTF-8 (0x01..=0x7F; NUL included)
    // so the String round-trip is byte-exact while still exercising
    // NUL and control bytes through the span chunker.
    let mut v: Vec<u8> = Vec::new();
    for i in 0..200u32 {
        v.push((i % 127 + 1) as u8);
        if i % 7 == 3 {
            v.push(0);
        }
    }
    v
}

fn binary_fixture_ours() -> Vec<u8> {
    let mut v = binary_base();
    // Range-preserving edits (byte stays 0x01..=0x7F): the fixture's
    // String round-trip must stay byte-exact.
    v[100] = b'Z';
    v[150] = b'Y';
    v
}

fn binary_fixture_theirs() -> Vec<u8> {
    let mut v = binary_base();
    v[5] = b'X';
    v
}

/// ~40 KiB of mostly-divergent pseudorandom bytes sharing 100 tiny
/// lines ("s<N>\n"): byte weighting keeps the common mass (~500
/// bytes of ~40k, ~1%) far below the rename threshold even though
/// the LINE count overlap is high.
fn huge_fixture_base() -> String {
    let mut s = String::new();
    for i in 0..100 {
        s.push_str(&format!("s{i}\n"));
    }
    s.push_str(&xorshift_bytes(0xB16B_0001, 40_000));
    s.push('\n');
    s
}

fn huge_fixture_other() -> String {
    let mut s = String::new();
    for i in 0..100 {
        s.push_str(&format!("s{i}\n"));
    }
    s.push_str(&xorshift_bytes(0xB16B_0002, 40_000));
    s.push('\n');
    s
}

fn huge_fixture_edited() -> String {
    let mut s = String::new();
    for i in 0..100 {
        s.push_str(&format!("s{i}\n"));
    }
    s.push_str(&xorshift_bytes(0xB16B_0001, 40_000));
    s.push_str("\nEDITED\n");
    s
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
        git(
            dir_path,
            &["commit", "-q", "--allow-empty", "-m", &format!("cs {i}")],
        );
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
    expect: Result<Vec<(&'static str, String)>, Vec<&'static str>>,
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
                case.name, git.conflicted_paths
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
                sfiles(&[("f.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("f.txt", "one\ntwo\nthree\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![("one.txt", "one\ntwo\nthree\n")])),
        },
        // ---- clean: rename on one side, EDIT at the source on the
        // other — the edit must land at the destination (scored
        // detection + content merge).
        Case {
            name: "rename-vs-edit",
            snapshots: vec![
                sfiles(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                sfiles(&[("f.txt", "one\ntwo\nEDIT\nfour\nfive\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "one.txt",
                "one\ntwo\nEDIT\nfour\nfive\n",
            )])),
        },
        // ---- clean: rename+edit on one side, untouched on the other.
        Case {
            name: "rename-plus-edit-vs-untouched",
            snapshots: vec![
                sfiles(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\nfour\nEDIT\n")]),
                sfiles(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "one.txt",
                "one\ntwo\nthree\nfour\nEDIT\n",
            )])),
        },
        // ---- clean: both sides rename the same source to the SAME
        // destination with different edits — contents merge.
        Case {
            name: "rename-rename-same-dst",
            snapshots: vec![
                sfiles(&[("f.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("same.txt", "OURS\none\ntwo\nthree\n")]),
                sfiles(&[("same.txt", "one\ntwo\nthree\nTHEIRS\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "same.txt",
                "OURS\none\ntwo\nthree\nTHEIRS\n",
            )])),
        },
        // ---- conflict: rename/rename 1-to-2 (different dsts).
        Case {
            name: "rename-rename-1to2",
            snapshots: vec![
                sfiles(&[("f.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("two.txt", "one\ntwo\nthree\n")]),
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
                sfiles(&[("f.txt", "one\ntwo\nthree\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\n")]),
                sfiles(&[]),
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
                sfiles(&[("a.txt", "alpha\n"), ("b.txt", "beta\n")]),
                sfiles(&[("same.txt", "alpha\n"), ("b.txt", "beta\n")]),
                sfiles(&[("a.txt", "alpha\n"), ("same.txt", "beta\n")]),
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
                sfiles(&[("f.txt", "one\ntwo\n")]),
                BTreeMap::from([("one.txt", ("one\ntwo\n".to_owned(), Mode::Exec))]),
                sfiles(&[("f.txt", "one\ntwo\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![("one.txt", "one\ntwo\n")])),
        },
        // ---- clean: CROSS-DIRECTORY rename on one side, edit at the
        // source on the other — the edit must land at the destination
        // (dirB/g), the source (dirA/f) gone. Anchors in both dirs
        // keep the directories alive so the move crosses levels.
        Case {
            name: "cross-dir-rename-vs-edit",
            snapshots: vec![
                sfiles(&[
                    ("dirA/f", "line one\nline two\nline three\nline four\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                ]),
                sfiles(&[
                    ("dirB/g", "line one\nline two\nline three\nline four\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                ]),
                sfiles(&[
                    ("dirA/f", "line one\nline two\nEDIT BY THEIRS\nline four\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                ]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "dirB/g",
                "line one\nline two\nEDIT BY THEIRS\nline four\n",
            )])),
        },
        // ---- conflict: CROSS-DIRECTORY rename/delete (ours moves
        // dirA/f to dirB/g, theirs deletes dirA/f). Git refuses with
        // CONFLICT (rename/delete) naming both endpoints.
        Case {
            name: "cross-dir-rename-delete",
            snapshots: vec![
                sfiles(&[
                    ("dirA/f", "line one\nline two\nline three\nline four\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                ]),
                sfiles(&[
                    ("dirB/g", "line one\nline two\nline three\nline four\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                ]),
                sfiles(&[("dirA/keep", "anchorA\n"), ("dirB/keep", "anchorB\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["dirA/f", "dirB/g"]),
        },
        // ---- clean: rename on one side, EDIT at the source on the
        // other, REVERSED (theirs renames, ours edits) — the edit must
        // land at the destination and the source must be GONE.
        Case {
            name: "edit-vs-rename-reversed",
            snapshots: vec![
                sfiles(&[("f.txt", "one\ntwo\nthree\nfour\nfive\n")]),
                sfiles(&[("f.txt", "one\ntwo\nEDIT BY OURS\nfour\nfive\n")]),
                sfiles(&[("one.txt", "one\ntwo\nthree\nfour\nfive\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "one.txt",
                "one\ntwo\nEDIT BY OURS\nfour\nfive\n",
            )])),
        },
        // ---- clean: a file renamed INTO a directory with its own
        // name (A → A/sub) while the other side EDITS the file A.
        // Git merges clean: the edit lands at A/sub, nothing else.
        // The path-mask trap: A is the source FILE's path, and the
        // destination directory is also named A — the rename must not
        // be suppressed by the path overlap, and the consumed FILE
        // entry must not taint the DIRECTORY container.
        Case {
            name: "rename-into-own-dir-vs-edit",
            snapshots: vec![
                sfiles(&[("A", "line one\nline two\nline three\n")]),
                sfiles(&[("A/sub", "line one\nline two\nline three\n")]),
                sfiles(&[("A", "line one\nEDITED\nline three\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![(
                "A/sub",
                "line one\nEDITED\nline three\n",
            )])),
        },
        // ---- clean: BOTH sides rename A → A/sub (same destination)
        // and each adds a DISTINCT file under the new A directory.
        // Git merges clean with all three files: A/sub plus both
        // extras. The trap: skipping the whole directory A (because a
        // rename's source path is A) would silently drop one side's
        // extra file.
        Case {
            name: "rename-into-own-dir-both-sides-with-extras",
            snapshots: vec![
                sfiles(&[("A", "line one\nline two\nline three\n")]),
                sfiles(&[
                    ("A/sub", "line one\nline two\nline three\n"),
                    ("A/oursExtra", "ours\n"),
                ]),
                sfiles(&[
                    ("A/sub", "line one\nline two\nline three\n"),
                    ("A/theirsExtra", "theirs\n"),
                ]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![
                ("A/sub", "line one\nline two\nline three\n"),
                ("A/oursExtra", "ours\n"),
                ("A/theirsExtra", "theirs\n"),
            ])),
        },
        // ---- conflict: CROSS-DIRECTORY rename/rename 1-to-2 (both
        // sides move dirA/f to different directories). Git refuses
        // with CONFLICT (rename/rename) naming source + both dests.
        Case {
            name: "cross-dir-rename-rename-1to2",
            snapshots: vec![
                sfiles(&[
                    ("dirA/f", "line one\nline two\nline three\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                    ("dirC/keep", "anchorC\n"),
                ]),
                sfiles(&[
                    ("dirB/g", "line one\nline two\nline three\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                    ("dirC/keep", "anchorC\n"),
                ]),
                sfiles(&[
                    ("dirC/h", "line one\nline two\nline three\n"),
                    ("dirA/keep", "anchorA\n"),
                    ("dirB/keep", "anchorB\n"),
                    ("dirC/keep", "anchorC\n"),
                ]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["dirA/f", "dirB/g", "dirC/h"]),
        },
        // ---- conflict: NO rename — shared short lines with a huge
        // DIVERGENT random line each (the coordinator's
        // merge-score-oracle2 shape). Git's byte-weighted estimator
        // sees ~no common BYTES (3 short lines of 1010); the old
        // line-count estimator scored 75 and silently paired A→B,
        // relocating theirs' edit of A into B and DROPPING A. Git:
        // CONFLICT (modify/delete) on A.
        Case {
            name: "scored-no-pair-random-long-line",
            snapshots: vec![
                ofiles(&[("A", score_fixture_a())]),
                ofiles(&[("B", score_fixture_b())]),
                ofiles(&[("A", score_fixture_a_edited())]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["A"]),
        },
        // ---- conflict: same shape REVERSED (theirs adds B, ours
        // edits A) — symmetry: no pair, modify/delete on A.
        Case {
            name: "scored-no-pair-random-long-line-reversed",
            snapshots: vec![
                ofiles(&[("A", score_fixture_a())]),
                ofiles(&[("A", score_fixture_a_edited())]),
                ofiles(&[("B", score_fixture_b())]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["A"]),
        },
        // ---- clean: a real rename whose LONG line was edited in the
        // middle (byte-weighted spans still pair: the first 64-byte
        // chunk and the short lines dominate the byte mass). Git
        // merges clean, edit rides the rename.
        Case {
            name: "scored-pair-edited-long-line",
            snapshots: vec![
                ofiles(&[("A", score_fixture_a())]),
                ofiles(&[("B", score_fixture_b_partial_edit())]),
                ofiles(&[("A", score_fixture_a())]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(vec![("B", score_fixture_b_partial_edit())]),
        },
        // ---- conflict: BINARY content with NUL bytes — the rename
        // IS detected (span chunking is byte-based; NULs are ordinary
        // bytes) but git refuses to CONTENT-merge binary files
        // ("Cannot merge binary files"), so both git and gardenbed
        // conflict at the destination. The rename detection itself is
        // proven by the conflict being at moved.dat (not a
        // modify/delete on bin.dat).
        Case {
            name: "binary-nul-rename-vs-edit",
            snapshots: vec![
                bfiles(&[("bin.dat", binary_base())]),
                bfiles(&[("moved.dat", binary_fixture_ours())]),
                bfiles(&[("bin.dat", binary_fixture_theirs())]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["moved.dat"]),
        },
        // ---- clean: exact-duplicate blobs at two sources, one moved
        // — pass 1 pairs deterministically (lexicographically first
        // dst with identical blob), the other stays put.
        Case {
            name: "duplicate-blobs-one-moves",
            snapshots: vec![
                sfiles(&[("a.txt", "dup\ncontent\n"), ("b.txt", "dup\ncontent\n")]),
                sfiles(&[("moved.txt", "dup\ncontent\n"), ("b.txt", "dup\ncontent\n")]),
                sfiles(&[("a.txt", "dup\ncontent\n"), ("b.txt", "dup\ncontent\n")]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![
                ("moved.txt", "dup\ncontent\n"),
                ("b.txt", "dup\ncontent\n"),
            ])),
        },
        // ---- clean: two deletes + two adds with EQUAL scores (ties)
        // — the deterministic (dst, src) byte-order tie-break picks
        // the same pairing git's rename detection does (both sides
        // move both files; contents merge identically either way).
        Case {
            name: "scored-tie-two-pairs",
            snapshots: vec![
                sfiles(&[
                    ("f1.txt", "shared one\nshared two\nshared three\n"),
                    ("f2.txt", "shared one\nshared two\nshared three\n"),
                ]),
                sfiles(&[
                    ("g1.txt", "shared one\nshared two\nshared three\n"),
                    ("g2.txt", "shared one\nshared two\nshared three\n"),
                ]),
                sfiles(&[
                    ("f1.txt", "shared one\nshared two\nshared three\n"),
                    ("f2.txt", "shared one\nshared two\nshared three\n"),
                ]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Ok(owned_expect(vec![
                ("g1.txt", "shared one\nshared two\nshared three\n"),
                ("g2.txt", "shared one\nshared two\nshared three\n"),
            ])),
        },
        // ---- conflict: NO rename despite MANY tiny common lines —
        // byte weighting must not let 100 short shared lines outvote
        // two giant divergent blobs (false-positive guard for the
        // new estimator, the inverse of the oracle above). Git: no
        // pairing, modify/delete on big.bin.
        Case {
            name: "huge-blobs-short-common-lines-no-pair",
            snapshots: vec![
                ofiles(&[("big.bin", huge_fixture_base())]),
                ofiles(&[("other.bin", huge_fixture_other())]),
                ofiles(&[("big.bin", huge_fixture_edited())]),
            ],
            base: 0,
            ours: 1,
            theirs: 2,
            expect: Err(vec!["big.bin"]),
        },
    ];
    for case in &cases {
        run_case(case);
    }
}

// ---------------------------------------------------------------------------
// Locality (the pruned-diff rename candidates): rename detection
// must read only CHANGED subtrees. A branch that edits files in place
// across a wide unchanged tree contributes no rename candidates and
// must not scan the unchanged bulk — tree reads stay O(changed
// paths), not O(whole tree). The counting store counts every
// `get_tree`; the control proves the counting works (a rename-heavy
// branch reads many more trees).
// ---------------------------------------------------------------------------

/// An [`ObjectStore`] wrapper counting `get_tree` calls (tree gets).
struct CountingTrees {
    inner: Arc<MemoryStore>,
    tree_gets: std::sync::atomic::AtomicUsize,
}

impl gardenbed_store::ObjectStore for CountingTrees {
    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, gardenbed_store::StoreError> {
        self.inner.get_blob(id)
    }
    fn put_blob(&self, content: &[u8]) -> Result<Id, gardenbed_store::StoreError> {
        self.inner.put_blob(content)
    }
    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, gardenbed_store::StoreError> {
        self.tree_gets
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_tree(id)
    }
    fn put_tree(&self, id: Id, bytes: &[u8]) -> Result<(), gardenbed_store::StoreError> {
        self.inner.put_tree(id, bytes)
    }
    fn has(&self, id: Id) -> Result<bool, gardenbed_store::StoreError> {
        self.inner.has(id)
    }
}

/// Builds `dirs` directories with one file each. Ours edits ONE
/// file (dir 0), theirs edits a DIFFERENT file (dir 1) — same-path
/// edits confined to a couple of changed dirs over a wide UNCHANGED
/// bulk. Commits the three-snapshot chain and merges, returning the
/// merge's tree-get count (fixture reads excluded).
fn locality_merge_tree_gets(dirs: usize) -> usize {
    let objects = Arc::new(CountingTrees {
        inner: Arc::new(MemoryStore::new()),
        tree_gets: std::sync::atomic::AtomicUsize::new(0),
    });
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone() as Arc<dyn ObjectStore>, metadata.clone());

    // Snapshot k: dirs 0..dirs, all files "c"; ours edits d000, theirs
    // edits d001 (disjoint single-line edits of the same file).
    let snapshot = |k: usize| -> BTreeMap<String, String> {
        (0..dirs)
            .map(|d| {
                let content = match (k, d) {
                    (0, _) | (_, 2..) => "line\n".to_owned(),
                    (1, 0) => "OURS line\n".to_owned(),
                    (2, 1) => "line THEIRS\n".to_owned(),
                    (1, _) | (2, _) => "line\n".to_owned(),
                    _ => unreachable!(),
                };
                (format!("d{d:03}/f.txt"), content)
            })
            .collect()
    };
    let mut ids = Vec::new();
    for k in 0..3 {
        let snap = snapshot(k);
        let mut changes = Vec::new();
        let prev: BTreeMap<String, (Mode, Id)> = if k == 0 {
            BTreeMap::new()
        } else {
            engine
                .flatten_tree(engine.derive_tree(ids[k - 1]).unwrap())
                .unwrap()
                .into_iter()
                .map(|(p, m, b)| (p, (m, b)))
                .collect()
        };
        for (path, content) in &snap {
            let id = blob_id(content.as_bytes());
            engine.objects().put_blob(content.as_bytes()).unwrap();
            changes.push(Change {
                path: path.clone(),
                op: if prev.contains_key(path) {
                    Op::Modify
                } else {
                    Op::Add
                },
                blob: Some(id),
                src_path: None,
                mode: Mode::Regular,
            });
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        let cs = gardenbed_model::Changeset {
            parents: if k == 0 { vec![] } else { vec![ids[k - 1]] },
            changes,
            message: format!("locality {k}"),
            author: ident("L"),
            committer: ident("L"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        ids.push(id);
    }
    let before = objects.tree_gets.load(std::sync::atomic::Ordering::SeqCst);
    let outcome = engine
        .merge_tree_changes(Some(ids[0]), ids[1], ids[2])
        .expect("merge computes");
    let _ = outcome.expect("same-path disjoint edits merge clean");
    objects.tree_gets.load(std::sync::atomic::Ordering::SeqCst) - before
}

#[test]
fn mod_only_wide_merge_reads_changed_paths_not_the_whole_tree() {
    // Same-path edits confined to two dirs over a wide UNCHANGED
    // bulk: the merge's tree reads must be INDEPENDENT of the
    // unchanged dir count (O(changed paths), never O(tree)). The
    // old whole-flatten detection read ~4×(dirs+1) trees per merge;
    // the pruned diff reads only the changed levels.
    let small = locality_merge_tree_gets(20);
    let large = locality_merge_tree_gets(60);
    assert!(
        (large as i64 - small as i64).abs() <= 4,
        "tree reads grew with the UNCHANGED bulk: 20 dirs -> {small}, 60 dirs -> {large}"
    );
    // And the absolute count stays tiny: the two changed dirs and
    // the root, a handful of loads per level — nothing like the
    // whole-tree flattens (4 × (N+1) trees for N dirs).
    assert!(
        small < 20,
        "mod-only merge over 2 changed dirs read {small} tree gets — expected a small constant"
    );

    // CONTROL (negative proof the counter registers bulk reads): a
    // branch that renames EVERY dir's file forces reads through the
    // whole changed tree.
    let control = locality_merge_tree_gets_control(20);
    assert!(
        control > small * 3,
        "rename-heavy control read {control} — counter must exceed the mod-only {small}"
    );
}

/// The control: every directory's file is RENAMED (delete+add per
/// dir), so rename detection has candidates everywhere and reads
/// through the whole changed tree — proving the counter registers
/// bulk reads when they happen.
fn locality_merge_tree_gets_control(dirs: usize) -> usize {
    let objects = Arc::new(CountingTrees {
        inner: Arc::new(MemoryStore::new()),
        tree_gets: std::sync::atomic::AtomicUsize::new(0),
    });
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone() as Arc<dyn ObjectStore>, metadata.clone());

    // Base: dNNN/f.txt. Ours: dNNN/g.txt (every file renamed).
    // Theirs: base + one file edited.
    let mut ids = Vec::new();
    let build = |k: usize| -> BTreeMap<String, String> {
        let mut snap = BTreeMap::new();
        for d in 0..dirs {
            let path = if k == 1 {
                format!("d{d:03}/g.txt")
            } else {
                format!("d{d:03}/f.txt")
            };
            let content = if k == 2 && d == 0 {
                "edited\n".to_owned()
            } else {
                "c\n".to_owned()
            };
            snap.insert(path, content);
        }
        snap
    };
    for k in 0..3 {
        let snap = build(k);
        let mut changes = Vec::new();
        let prev: BTreeMap<String, (Mode, Id)> = if k == 0 {
            BTreeMap::new()
        } else {
            engine
                .flatten_tree(engine.derive_tree(ids[k - 1]).unwrap())
                .unwrap()
                .into_iter()
                .map(|(p, m, b)| (p, (m, b)))
                .collect()
        };
        for (path, content) in &snap {
            let id = blob_id(content.as_bytes());
            engine.objects().put_blob(content.as_bytes()).unwrap();
            changes.push(Change {
                path: path.clone(),
                op: if prev.contains_key(path) {
                    Op::Modify
                } else {
                    Op::Add
                },
                blob: Some(id),
                src_path: None,
                mode: Mode::Regular,
            });
        }
        for path in prev.keys() {
            if !snap.contains_key(path) {
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
            parents: if k == 0 { vec![] } else { vec![ids[k - 1]] },
            changes,
            message: format!("control {k}"),
            author: ident("C"),
            committer: ident("C"),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        ids.push(id);
    }
    let before = objects.tree_gets.load(std::sync::atomic::Ordering::SeqCst);
    let outcome = engine
        .merge_tree_changes(Some(ids[0]), ids[1], ids[2])
        .expect("control merge computes");
    let _ = outcome.expect("rename-heavy control merge is clean");
    objects.tree_gets.load(std::sync::atomic::Ordering::SeqCst) - before
}

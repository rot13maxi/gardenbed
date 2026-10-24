//! Recursive multi-base merge verification (the deferred-scope merge
//! lane): criss-cross histories with TWO maximal merge bases, where the
//! v0.2 single-base tie-break and git's recursive strategy disagree.
//!
//! 1. `merge_bases_all` returns exactly the maximal common ancestors
//!    (git's `merge-base --all` parity, on real git repositories).
//! 2. The recursive merge's clean/conflict classification matches git's
//!    `merge-tree --write-tree` on criss-cross pairs — including the
//!    case where a single-base tie-break merges cleanly and git (and
//!    now gardenbed) conflicts, and the reverse.
//! 3. Unique-base merges are byte-identical through the recursive entry
//!    point and the v0.2 single-base entry point (behavior preserved).
//! 4. The derivation invariant: `apply(changes, tree(ours))` reproduces
//!    git's merged tree on every clean pair.
//! 5. Determinism: identical merged bytes across repeated runs.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::oracle::git_merge_tree;
use gardenbed_model::{Change, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

use common::{git_cmd, git_out};

fn ident(tag: &str, secs: u64) -> Identity {
    Identity {
        name: "CrissCross".into(),
        email: format!("{tag}@gardenbed.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

/// One side's snapshot: path -> content (regular mode only; the
/// criss-cross corpus is content-shaped).
type Snap = BTreeMap<String, Vec<u8>>;

/// Builds a criss-cross fixture in the engine's stores:
///
/// ```text
///        base
///       /    \
///      a      b
///     / \    / \
///    m1  \  /  m2      (m1 = (a,b), m2 = (b,a): the two cross-merges)
///         \/
///       ours = child(m1)   theirs = child(m2)
/// ```
///
/// `a_edit`/`b_edit` shape the divergence at `path`; the two cross
/// merges both contain both edits (they merge cleanly by
/// construction). Returns (engine, base, m1, m2, ours, theirs).
fn criss_cross(
    path: &str,
    base_content: &[u8],
    a_edit: &[u8],
    b_edit: &[u8],
    ours_edit: &[u8],
    theirs_edit: &[u8],
) -> (Engine, Id, Id, Id, Id, Id, Id, Id) {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone(), metadata.clone());

    let snapshot = |parent: Option<Id>, tag: &str, secs: u64, snap: &Snap| -> Id {
        let mut changes = Vec::new();
        // Snapshot semantics: delete what vanished, write what's there.
        if let Some(p) = parent {
            for (ppath, _pm, _pb) in engine.flatten_tree(engine.derive_tree(p).unwrap()).unwrap() {
                if !snap.contains_key(&ppath) {
                    changes.push(Change {
                        path: ppath,
                        op: Op::Delete,
                        blob: None,
                        src_path: None,
                        mode: Mode::Regular,
                    });
                }
            }
        }
        for (p, c) in snap {
            let id = blob_id(c);
            objects.put_blob(c).unwrap();
            changes.push(Change {
                path: p.clone(),
                op: Op::Add,
                blob: Some(id),
                src_path: None,
                mode: Mode::Regular,
            });
        }
        changes.sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
        let cs = gardenbed_model::Changeset {
            parents: parent.into_iter().collect(),
            changes,
            message: format!("cc {tag}"),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };

    let mut base_snap = Snap::new();
    base_snap.insert(path.to_owned(), base_content.to_vec());
    let base = snapshot(None, "base", 0, &base_snap);

    let mut a_snap = base_snap.clone();
    a_snap.insert(path.to_owned(), a_edit.to_vec());
    let a = snapshot(Some(base), "a", 1, &a_snap);

    let mut b_snap = base_snap.clone();
    b_snap.insert(path.to_owned(), b_edit.to_vec());
    let b = snapshot(Some(base), "b", 2, &b_snap);

    // Both cross-merges carry the union content: a and b edited
    // disjoint line regions, so merge(base, a, b) is clean and its
    // result is exactly what both cross merges contain.
    let union_changes = engine
        .merge_tree_changes(Some(base), a, b)
        .expect("cross merge computes")
        .expect("a/b edits are disjoint (clean union)");
    let mk_cross = |tag: &str, secs: u64, parents: Vec<Id>| -> Id {
        let cs = gardenbed_model::Changeset {
            parents,
            changes: union_changes.clone(),
            message: format!("cc {tag}"),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };
    let m1 = mk_cross("m1", 3, vec![a, b]);
    let m2 = mk_cross("m2", 4, vec![b, a]);

    let mut ours_snap = Snap::new();
    ours_snap.insert(path.to_owned(), ours_edit.to_vec());
    let ours = snapshot(Some(m1), "ours", 5, &ours_snap);

    let mut theirs_snap = Snap::new();
    theirs_snap.insert(path.to_owned(), theirs_edit.to_vec());
    let theirs = snapshot(Some(m2), "theirs", 6, &theirs_snap);

    (engine, base, a, b, m1, m2, ours, theirs)
}

/// Materializes the criss-cross fixture as a real git repository and
/// returns (tempdir, git shas for (a, b, ours, theirs)) — the two
/// maximal common ancestors and the two tips — with refs
/// `ours`/`theirs` pointing at the tips.
fn git_criss_cross(
    path: &str,
    base_content: &[u8],
    a_edit: &[u8],
    b_edit: &[u8],
    ours_edit: &[u8],
    theirs_edit: &[u8],
    union: &[u8],
) -> (tempfile::TempDir, [String; 4]) {
    let dir = tempfile::tempdir().expect("git tempdir");
    let g = |args: &[&str]| {
        let out = git_cmd(dir.path(), args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    };
    g(&["init", "-q", "-b", "main", "."]);
    g(&["config", "user.email", "t@gardenbed.example"]);
    g(&["config", "user.name", "T"]);

    let write = |content: &[u8]| {
        std::fs::write(dir.path().join(path), content).unwrap();
    };
    let commit = |msg: &str| {
        g(&["add", "--", path]);
        g(&["commit", "-q", "--allow-empty", "-m", msg]);
        String::from_utf8(
            git_cmd(dir.path(), &["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned()
    };

    write(base_content);
    let _base = commit("base");

    write(a_edit);
    let a = commit("a");
    g(&["branch", "-f", "br-a"]);

    // Reset to base for b.
    g(&["reset", "-q", "--hard", "HEAD~1"]);
    write(b_edit);
    let b = commit("b");
    g(&["branch", "-f", "br-b"]);

    // m1 = merge(a, b) via plumbing on the union tree.
    let m1 = {
        let tree = git_union_tree(&dir, path, union);
        echo_commit_tree(&dir, &tree, &[&a, &b], "m1")
    };
    // m2 = merge(b, a).
    let m2 = {
        let tree = git_union_tree(&dir, path, union);
        echo_commit_tree(&dir, &tree, &[&b, &a], "m2")
    };

    // ours on m1, theirs on m2.
    g(&["checkout", "-q", &m1]);
    write(ours_edit);
    let ours = commit("ours");
    g(&["branch", "-f", "ours"]);

    g(&["checkout", "-q", &m2]);
    write(theirs_edit);
    let theirs = commit("theirs");
    g(&["branch", "-f", "theirs"]);

    (dir, [a, b, ours, theirs])
}

/// Writes `content` at `path` into a temporary index over the current
/// HEAD and prints the resulting tree sha (the union tree both cross
/// merges share).
fn git_union_tree(dir: &tempfile::TempDir, path: &str, content: &[u8]) -> String {
    std::fs::write(dir.path().join(path), content).unwrap();
    let out = git_cmd(dir.path(), &["add", "--", path])
        .output()
        .expect("git add runs");
    assert!(out.status.success(), "git add failed");
    String::from_utf8(
        git_cmd(dir.path(), &["write-tree"])
            .output()
            .expect("git write-tree runs")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned()
}

/// `git commit-tree <tree> -p <p1> -p <p2> -m <msg>` with a fixed
/// identity, returning the commit sha.
fn echo_commit_tree(dir: &tempfile::TempDir, tree: &str, parents: &[&str], msg: &str) -> String {
    let mut args: Vec<String> = vec!["commit-tree".into(), tree.into()];
    for p in parents {
        args.push("-p".into());
        args.push((*p).to_owned());
    }
    args.push("-m".into());
    args.push(msg.into());
    let argrefs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    String::from_utf8(
        git_cmd(dir.path(), &argrefs)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@gardenbed.example")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@gardenbed.example")
            .output()
            .expect("git commit-tree runs")
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned()
}

/// `git ls-tree -r <tree>`: path -> content.
fn ls_tree(dir: &std::path::Path, tree: &str) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let listing = String::from_utf8(
        git_cmd(dir, &["ls-tree", "-r", tree])
            .output()
            .expect("ls-tree runs")
            .stdout,
    )
    .unwrap();
    for line in listing.lines() {
        let (_meta, rest) = line.split_once('\t').expect("tab");
        let sha = rest
            .split_whitespace()
            .last()
            .or_else(|| line.split_whitespace().nth(2))
            .expect("sha");
        // Format: <mode> <type> <sha>\t<path> — the sha is in the
        // head, not the tail (the tail is the path).
        let head = line.split('\t').next().unwrap();
        let sha = head.split_whitespace().nth(2).unwrap_or(sha);
        let content = git_cmd(dir, &["cat-file", "blob", sha])
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default();
        out.insert(rest.to_owned(), content);
    }
    out
}

#[test]
fn merge_bases_all_matches_git_on_criss_cross() {
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let a_edit = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let b_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let ours_edit = b"X1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let theirs_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nY8\n".to_vec();
    let union = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();

    let (engine, _base, a, b, _m1, _m2, ours, theirs) =
        criss_cross("f.txt", &base, &a_edit, &b_edit, &ours_edit, &theirs_edit);
    let (gdir, shas) = git_criss_cross(
        "f.txt",
        &base,
        &a_edit,
        &b_edit,
        &ours_edit,
        &theirs_edit,
        &union,
    );

    // gardenbed: two maximal bases (m1, m2), in the deterministic order.
    let bases = engine.merge_bases_all(ours, theirs).unwrap();
    assert_eq!(bases.len(), 2, "criss-cross has two maximal bases");
    assert!(
        bases.contains(&a) && bases.contains(&b),
        "bases must be the two pre-merge tips: {bases:?} vs a={a} b={b}"
    );

    // git parity: `git merge-base --all ours theirs` names the same two.
    let git_bases = git_out(gdir.path(), &["merge-base", "--all", "ours", "theirs"])
        .expect("git merge-base --all succeeds");
    let git_lines: Vec<&str> = std::str::from_utf8(&git_bases).unwrap().lines().collect();
    assert_eq!(git_lines.len(), 2, "git also finds two bases");
    assert!(git_lines.contains(&shas[0].as_str()));
    assert!(git_lines.contains(&shas[1].as_str()));
}

#[test]
fn recursive_criss_cross_classification_matches_git() {
    // The shape where single-base and recursive DIFFER: ours and theirs
    // each re-edit one of the two lines the cross-merges unioned.
    // Single base (either m1 or m2) sees one side's edit as
    // base-unchanged and merges cleanly; the recursive virtual base
    // (the union) sees BOTH sides' edits as changes and conflicts —
    // git's answer.
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let a_edit = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let b_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let ours_edit = b"X1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let theirs_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nY8\n".to_vec();
    let union = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();

    let (engine, _base, _a, b, _m1, _m2, ours, theirs) =
        criss_cross("f.txt", &base, &a_edit, &b_edit, &ours_edit, &theirs_edit);
    let (gdir, _shas) = git_criss_cross(
        "f.txt",
        &base,
        &a_edit,
        &b_edit,
        &ours_edit,
        &theirs_edit,
        &union,
    );

    // Sanity: the single-base tie-break (v0.2 behavior) merges this
    // CLEANLY — the divergence this test exists to pin. The tie-break
    // picks `b` (newest committer seconds of the two maximal bases),
    // which sees ours' top-line edit as a base change and theirs'
    // bottom-line edit as the only side change.
    let single = engine.merge_tree_changes(Some(b), ours, theirs).unwrap();
    assert!(single.is_ok(), "single-base b tie-break merges cleanly");

    // git's recursive answer: CONFLICT.
    let git = git_merge_tree(gdir.path(), "ours", "theirs").expect("git merge-tree runs");
    assert!(git.conflicted, "git conflicts on the criss-cross pair");

    // gardenbed recursive: CONFLICT, naming the path.
    let recursive = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    match recursive {
        Ok(changes) => panic!("recursive merge was clean ({changes:?}) but git conflicted"),
        Err(conflicts) => {
            assert!(
                conflicts.iter().any(|c| c == "f.txt"),
                "conflict must name f.txt: {conflicts:?}"
            );
        }
    }
}

#[test]
fn recursive_clean_criss_cross_matches_git_content() {
    // A criss-cross whose tips edit DISJOINT regions after the union:
    // recursive merges cleanly and the content matches git exactly.
    let base = (1..=40)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect::<Vec<u8>>();
    let a_edit = {
        let mut v = base.clone();
        v.splice(0..13, b"A-top\n".iter().copied());
        v
    };
    let b_edit = {
        let mut v = base.clone();
        let cut = v.len() - b"base line 40\n".len();
        v.splice(cut.., b"B-bottom\n".iter().copied());
        v
    };
    // The union both cross-merges share.
    let union = {
        let mut v = a_edit.clone();
        let cut = v.len() - b"base line 40\n".len();
        v.splice(cut.., b"B-bottom\n".iter().copied());
        v
    };
    // ours edits the top again (a NEW line, below A-top), theirs
    // edits line 20 — disjoint from both the union edits and ours.
    let ours_edit = {
        let mut v = union.clone();
        v.splice(0..6, b"OURS-top\n".iter().copied());
        v
    };
    let theirs_edit = {
        let mut v = union.clone();
        // Replace "base line 20\n" somewhere in the middle.
        let needle = b"base line 20\n".to_vec();
        let pos = v
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("line 20 present");
        v.splice(pos..pos + needle.len(), b"THEIRS-20\n".iter().copied());
        v
    };

    let (engine, _base, _a, _b, _m1, _m2, ours, theirs) =
        criss_cross("f.txt", &base, &a_edit, &b_edit, &ours_edit, &theirs_edit);
    let (gdir, _shas) = git_criss_cross(
        "f.txt",
        &base,
        &a_edit,
        &b_edit,
        &ours_edit,
        &theirs_edit,
        &union,
    );

    let git = git_merge_tree(gdir.path(), "ours", "theirs").expect("git merge-tree runs");
    let recursive = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    match (git.conflicted, recursive) {
        (false, Ok(changes)) => {
            // Derivation invariant + content equality.
            let merged_cs = gardenbed_model::Changeset {
                parents: vec![ours],
                changes: changes.clone(),
                message: "merged".into(),
                author: ident("m", 9),
                committer: ident("m", 9),
                signature: None,
            };
            let root = engine
                .apply_changeset_to_tree(engine.derive_tree(ours).unwrap(), &merged_cs)
                .unwrap();
            let merged: BTreeMap<String, Vec<u8>> = engine
                .flatten_tree(root)
                .unwrap()
                .into_iter()
                .map(|(p, _m, b)| (p, engine.objects().get_blob(b).unwrap().unwrap()))
                .collect();
            let git_files = ls_tree(gdir.path(), &git.tree);
            assert_eq!(
                merged.keys().collect::<Vec<_>>(),
                git_files.keys().collect::<Vec<_>>(),
                "merged path set differs from git"
            );
            for (path, content) in &git_files {
                assert_eq!(
                    merged.get(path).map(|v| v.as_slice()),
                    Some(content.as_slice()),
                    "merged content at {path} differs from git"
                );
            }
        }
        (true, Err(conflicts)) => {
            assert!(
                conflicts.iter().any(|c| c == "f.txt"),
                "conflict must name f.txt: {conflicts:?}"
            );
        }
        (false, Err(found)) => panic!("git merged but gardenbed conflicted: {found:?}"),
        (true, Ok(_)) => panic!("git conflicted but gardenbed merged cleanly"),
    }
}

#[test]
fn unique_base_recursive_entry_is_byte_identical() {
    // Non-criss-cross: the recursive entry point must produce exactly
    // the v0.2 single-base answer (the preserved contract).
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone(), metadata.clone());
    let mut snap = Snap::new();
    snap.insert("f.txt".into(), b"one\ntwo\nthree\n".to_vec());
    let mut changes = Vec::new();
    let blob = blob_id(b"one\ntwo\nthree\n");
    objects.put_blob(b"one\ntwo\nthree\n").unwrap();
    changes.push(Change {
        path: "f.txt".into(),
        op: Op::Add,
        blob: Some(blob),
        src_path: None,
        mode: Mode::Regular,
    });
    let cs = gardenbed_model::Changeset {
        parents: vec![],
        changes,
        message: "root".into(),
        author: ident("r", 0),
        committer: ident("r", 0),
        signature: None,
    };
    let root = metadata.put_changeset(&cs).unwrap();
    engine.on_changeset_created(root).unwrap();

    let mk = |parent: Id, tag: &str, secs: u64, content: &[u8]| -> Id {
        let mut changes = Vec::new();
        let blob = blob_id(content);
        objects.put_blob(content).unwrap();
        changes.push(Change {
            path: "f.txt".into(),
            op: Op::Modify,
            blob: Some(blob),
            src_path: None,
            mode: Mode::Regular,
        });
        let cs = gardenbed_model::Changeset {
            parents: vec![parent],
            changes,
            message: tag.into(),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };
    let ours = mk(root, "ours", 1, b"OURS\ntwo\nthree\n");
    let theirs = mk(root, "theirs", 2, b"one\ntwo\nTHEIRS\n");

    let single = engine.merge_tree_changes(Some(root), ours, theirs).unwrap();
    let recursive = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    assert_eq!(
        single, recursive,
        "unique-base merges must be byte-identical through both entry points"
    );
}

#[test]
fn recursive_merge_is_deterministic_across_runs() {
    let base = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let a_edit = b"A1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n".to_vec();
    let b_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let ours_edit = b"X1\nl2\nl3\nl4\nl5\nl6\nl7\nB8\n".to_vec();
    let theirs_edit = b"l1\nl2\nl3\nl4\nl5\nl6\nl7\nY8\n".to_vec();

    let (engine, _base, _a, _b, _m1, _m2, ours, theirs) =
        criss_cross("f.txt", &base, &a_edit, &b_edit, &ours_edit, &theirs_edit);
    let first = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    let second = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    assert_eq!(first, second, "repeated recursive merges are identical");
}

// ---------------------------------------------------------------------------
// THREE maximal common ancestors (the fold's accumulated-ancestry
// contract): the fold must consume all three bases, anchoring later
// rounds on the accumulated VIRTUAL changeset (parents
// [prev_acc, b]) — not on the last sibling base, whose history drops
// everything folded before it.
// ---------------------------------------------------------------------------

/// (engine, ours, theirs, ours' post-state, theirs' post-state).
type ThreeBaseFixture = (Engine, Id, Id, Snap, Snap);

/// (git repo dir, ours' post-state, theirs' post-state).
type GitThreeBase = (tempfile::TempDir, Snap, Snap);

/// Builds the git-verified three-maximal-base shape in engine stores:
///
/// ```text
/// base -- x -- y -- z        (siblings, disjoint union content each)
/// p1 = (x, y)  p2 = (y, x)   (cross-merges carrying the union)
/// ours   = (p1, z) + ours edit
/// theirs = (p2, z) + theirs edit
/// ```
///
/// `merge_bases_all(ours, theirs)` = {x, y, z} (three maximal). The
/// union trees make a correct fold reproduce the union; the tips'
/// disjoint edits must ride on top cleanly. A fold that anchors
/// round 2 at merge_bases_all(b1, b2) alone (dropping b0's ancestry)
/// loses the union content and mis-merges.
fn three_maximal_base_fixture(disjoint: bool) -> ThreeBaseFixture {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new()) as Arc<dyn MetadataStore>;
    let engine = Engine::new(objects.clone(), metadata.clone());

    let mut union = Snap::new();
    union.insert("f.txt".to_owned(), b"base\n".to_vec());
    union.insert("fx".to_owned(), b"X\n".to_vec());
    union.insert("fy".to_owned(), b"Y\n".to_vec());
    union.insert("fz".to_owned(), b"Z\n".to_vec());

    let commit = |tag: &str, secs: u64, parents: Vec<Id>, snap: &Snap| -> Id {
        let mut changes = Vec::new();
        if parents.is_empty() {
            for (p, c) in snap {
                changes.push(Change {
                    path: p.clone(),
                    op: Op::Add,
                    blob: Some(blob_id(c)),
                    src_path: None,
                    mode: Mode::Regular,
                });
            }
        } else {
            // Full post-state snapshots: diff vs the first parent's
            // tree (all parents carry the same tree in this shape).
            let prev = engine
                .flatten_tree(engine.derive_tree(parents[0]).unwrap())
                .unwrap();
            for (ppath, _pm, _pb) in &prev {
                if !snap.contains_key(ppath) {
                    changes.push(Change {
                        path: ppath.clone(),
                        op: Op::Delete,
                        blob: None,
                        src_path: None,
                        mode: Mode::Regular,
                    });
                }
            }
            for (p, c) in snap {
                let op = if prev.iter().any(|(pp, _, _)| pp == p) {
                    Op::Modify
                } else {
                    Op::Add
                };
                changes.push(Change {
                    path: p.clone(),
                    op,
                    blob: Some(blob_id(c)),
                    src_path: None,
                    mode: Mode::Regular,
                });
            }
        }
        changes.sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
        for c in &changes {
            if c.blob.is_some() {
                let content = snap.get(&c.path).expect("changed path in snap").clone();
                objects.put_blob(&content).unwrap();
            }
        }
        let cs = gardenbed_model::Changeset {
            parents,
            changes,
            message: format!("3base {tag}"),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };

    let mut base_snap = Snap::new();
    base_snap.insert("f.txt".to_owned(), b"base\n".to_vec());
    let base = commit("base", 1, vec![], &base_snap);
    let mut x_snap = union.clone();
    x_snap.insert("fx".to_owned(), b"X\n".to_vec());
    let x = commit("x", 2, vec![base], &x_snap);
    let mut y_snap = union.clone();
    y_snap.insert("fy".to_owned(), b"Y\n".to_vec());
    let y = commit("y", 3, vec![base], &y_snap);
    let mut z_snap = union.clone();
    z_snap.insert("fz".to_owned(), b"Z\n".to_vec());
    let z = commit("z", 4, vec![base], &z_snap);
    let p1 = commit("p1", 5, vec![x, y], &union);
    let p2 = commit("p2", 6, vec![y, x], &union);
    let ours_m = commit("oursM", 7, vec![p1, z], &union);
    let theirs_m = commit("theirsM", 8, vec![p2, z], &union);

    let mut ours_snap = union.clone();
    let mut theirs_snap = union.clone();
    if disjoint {
        ours_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
        theirs_snap.insert("fy".to_owned(), b"FY4\n".to_vec());
    } else {
        // BOTH tips edit the SAME file with the SAME bytes (a shared
        // edit): still clean, and the content must be the shared one.
        ours_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
        theirs_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
    }
    let ours = commit("ourstip", 9, vec![ours_m], &ours_snap);
    let theirs = commit("theirstip", 10, vec![theirs_m], &theirs_snap);
    let _ = (ours, theirs);
    (engine, ours, theirs, ours_snap, theirs_snap)
}

/// The same three-maximal-base shape as a real git repository:
/// returns (tempdir, shas (ours, theirs)) with refs set.
fn git_three_maximal_base(disjoint: bool) -> GitThreeBase {
    let dir = tempfile::tempdir().expect("git tempdir");
    let g = |args: &[&str]| {
        let out = git_cmd(dir.path(), args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    };
    g(&["init", "-q", "-b", "main", "."]);
    g(&["config", "user.email", "t@gardenbed.example"]);
    g(&["config", "user.name", "T"]);

    let write_union = |drop: &[&str]| -> String {
        for (p, c) in [
            ("f.txt", "base\n"),
            ("fx", "X\n"),
            ("fy", "Y\n"),
            ("fz", "Z\n"),
        ] {
            if drop.contains(&p) {
                continue;
            }
            std::fs::write(dir.path().join(p), c).unwrap();
            g(&["add", "--", p]);
        }
        String::from_utf8(
            git_cmd(dir.path(), &["write-tree"])
                .output()
                .expect("write-tree")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned()
    };
    let t_base = write_union(&["fx", "fy", "fz"]);
    let t_x = write_union(&["fy", "fz"]);
    let t_y = write_union(&["fx", "fz"]);
    let t_z = write_union(&["fx", "fy"]);
    let t_union = write_union(&[]);
    let c = |tree: &str, parents: &[&str], msg: &str| -> String {
        echo_commit_tree(&dir, tree, parents, msg)
    };
    let base = c(&t_base, &[], "base");
    let x = c(&t_x, &[&base], "x");
    let y = c(&t_y, &[&base], "y");
    let z = c(&t_z, &[&base], "z");
    let p1 = c(&t_union, &[&x, &y], "p1");
    let p2 = c(&t_union, &[&y, &x], "p2");
    let ours_m = c(&t_union, &[&p1, &z], "oursM");
    let theirs_m = c(&t_union, &[&p2, &z], "theirsM");
    // Tip trees: union + the edits.
    let mut ours_snap: BTreeMap<String, Vec<u8>> = [
        ("f.txt", b"base\n".to_vec()),
        ("fx", b"X\n".to_vec()),
        ("fy", b"Y\n".to_vec()),
        ("fz", b"Z\n".to_vec()),
    ]
    .into_iter()
    .map(|(p, c)| (p.to_owned(), c))
    .collect();
    let mut theirs_snap = ours_snap.clone();
    if disjoint {
        ours_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
        theirs_snap.insert("fy".to_owned(), b"FY4\n".to_vec());
    } else {
        ours_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
        theirs_snap.insert("fx".to_owned(), b"FX4\n".to_vec());
    }
    let tree_of = |snap: &BTreeMap<String, Vec<u8>>| -> String {
        for (p, c) in snap {
            std::fs::write(dir.path().join(p), c).unwrap();
            g(&["add", "--", p]);
        }
        String::from_utf8(
            git_cmd(dir.path(), &["write-tree"])
                .output()
                .expect("write-tree")
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned()
    };
    let t_ours = tree_of(&ours_snap);
    let t_theirs = tree_of(&theirs_snap);
    let ours = c(&t_ours, &[&ours_m], "ourstip");
    let theirs = c(&t_theirs, &[&theirs_m], "theirstip");
    g(&["update-ref", "refs/heads/ours", &ours]);
    g(&["update-ref", "refs/heads/theirs", &theirs]);
    // Sanity: exactly THREE maximal bases (git merge-base --all).
    let all = git_out(dir.path(), &["merge-base", "--all", "ours", "theirs"])
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .unwrap_or_default();
    let count = all.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(count, 3, "git sees three maximal bases, got: {all}");
    (dir, ours_snap, theirs_snap)
}

#[test]
fn three_maximal_bases_disjoint_edits_merge_clean_like_git() {
    let (engine, ours, theirs, ours_snap, theirs_snap) = three_maximal_base_fixture(true);
    let (gdir, _o, _t) = git_three_maximal_base(true);
    let git = git_merge_tree(gdir.path(), "ours", "theirs").expect("git merge-tree runs");

    let recursive = engine.merge_tree_changes_recursive(ours, theirs).unwrap();
    match (git.conflicted, recursive) {
        (false, Ok(changes)) => {
            let merged_cs = gardenbed_model::Changeset {
                parents: vec![ours],
                changes: changes.clone(),
                message: "merged".into(),
                author: ident("m", 11),
                committer: ident("m", 11),
                signature: None,
            };
            let root = engine
                .apply_changeset_to_tree(engine.derive_tree(ours).unwrap(), &merged_cs)
                .unwrap();
            let merged: BTreeMap<String, Vec<u8>> = engine
                .flatten_tree(root)
                .unwrap()
                .into_iter()
                .map(|(p, _m, b)| (p, engine.objects().get_blob(b).unwrap().unwrap()))
                .collect();
            let mut expect = ours_snap.clone();
            expect.insert("fy".to_owned(), theirs_snap["fy"].clone());
            assert_eq!(
                merged, expect,
                "merged tree must carry the union plus both tips' disjoint edits"
            );
            let git_files = ls_tree(gdir.path(), &git.tree);
            assert_eq!(
                merged.keys().collect::<Vec<_>>(),
                git_files.keys().collect::<Vec<_>>(),
                "path set differs from git"
            );
            for (path, content) in &git_files {
                assert_eq!(
                    merged.get(path).map(|v| v.as_slice()),
                    Some(content.as_slice()),
                    "content at {path} differs from git"
                );
            }
        }
        (false, Err(found)) => {
            panic!("git merged cleanly (3 maximal bases) but gardenbed conflicted: {found:?}")
        }
        (true, _) => panic!("git conflicted on the disjoint 3-base shape: unexpected"),
    }
}

#[test]
fn three_maximal_bases_shared_edit_merges_clean_like_git() {
    let (engine, ours, theirs, _ours_snap, _theirs_snap) = three_maximal_base_fixture(false);
    let (gdir, _o, _t) = git_three_maximal_base(false);
    let git = git_merge_tree(gdir.path(), "ours", "theirs").expect("git merge-tree runs");
    assert!(!git.conflicted, "git must merge the shared-edit shape");

    let recursive = engine
        .merge_tree_changes_recursive(ours, theirs)
        .unwrap()
        .expect("shared edit on 3 maximal bases is clean");
    let merged_cs = gardenbed_model::Changeset {
        parents: vec![ours],
        changes: recursive.clone(),
        message: "merged".into(),
        author: ident("m", 12),
        committer: ident("m", 12),
        signature: None,
    };
    let root = engine
        .apply_changeset_to_tree(engine.derive_tree(ours).unwrap(), &merged_cs)
        .unwrap();
    let merged: BTreeMap<String, Vec<u8>> = engine
        .flatten_tree(root)
        .unwrap()
        .into_iter()
        .map(|(p, _m, b)| (p, engine.objects().get_blob(b).unwrap().unwrap()))
        .collect();
    assert_eq!(
        merged.get("fx").map(|v| v.as_slice()),
        Some(b"FX4\n".to_vec().as_slice()),
        "the shared edit must land"
    );
    let git_files = ls_tree(gdir.path(), &git.tree);
    for (path, content) in &git_files {
        assert_eq!(
            merged.get(path).map(|v| v.as_slice()),
            Some(content.as_slice()),
            "content at {path} differs from git"
        );
    }
}

#[test]
fn conflicting_multi_base_merge_persists_no_changesets() {
    // The virtual-base fold must be a PURE overlay: no synthetic or
    // authored changeset may enter the metadata store, whether the
    // outer merge is clean or conflicting. Build a three-maximal-base
    // shape whose tips CONFLICT (same file, overlapping edits), run
    // the recursive merge, and assert the store's changeset set is
    // byte-identical before and after (recent_changesets AND the
    // total put_changeset population via a full re-listing).
    let (engine, ours, theirs, _o, _t) = three_maximal_base_fixture(true);
    let metadata = engine.metadata().clone();

    let before: Vec<Id> = metadata.recent_changesets(u32::MAX).unwrap();
    assert!(before.len() >= 8, "fixture population sanity: {before:?}");

    // Force a conflicting outer merge: the tips' overlapping edits on
    // the SAME file. Reuse the fixture but edit the same region
    // differently on each side: re-parent fresh tips over the merge
    // nodes with colliding content.
    let objects = engine.objects();
    let colliding = |tag: &str, secs: u64, parent: Id, content: &[u8]| -> Id {
        let blob = blob_id(content);
        objects.put_blob(content).unwrap();
        let cs = gardenbed_model::Changeset {
            parents: vec![parent],
            changes: vec![Change {
                path: "f.txt".to_owned(),
                op: Op::Modify,
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: format!("collide {tag}"),
            author: ident(tag, secs),
            committer: ident(tag, secs),
            signature: None,
        };
        let id = metadata.put_changeset(&cs).unwrap();
        engine.on_changeset_created(id).unwrap();
        id
    };
    let ours_c = colliding("oursC", 21, ours, b"OURS-ONLY-LINE\n");
    let theirs_c = colliding("theirsC", 22, theirs, b"THEIRS-ONLY-LINE\n");

    let mid: Vec<Id> = metadata.recent_changesets(u32::MAX).unwrap();
    assert_eq!(mid.len(), before.len() + 2, "the two colliding tips");

    // The recursive merge over three maximal bases, conflicting at
    // the tips: must CONFLICT, and must not store anything.
    let outcome = engine
        .merge_tree_changes_recursive(ours_c, theirs_c)
        .unwrap();
    let conflicts = outcome.expect_err("overlapping tip edits must conflict");
    assert!(
        conflicts.iter().any(|p| p == "f.txt"),
        "conflict names f.txt: {conflicts:?}"
    );

    let after: Vec<Id> = metadata.recent_changesets(u32::MAX).unwrap();
    assert_eq!(
        after, mid,
        "the multi-base fold stored no synthetic or authored changesets"
    );

    // The CLEAN path too: the disjoint three-base merge folds all
    // bases and still must not persist anything.
    let (engine2, ours2, theirs2, _o2, _t2) = three_maximal_base_fixture(true);
    let before2 = engine2.metadata().recent_changesets(u32::MAX).unwrap();
    engine2
        .merge_tree_changes_recursive(ours2, theirs2)
        .unwrap()
        .expect("disjoint 3-base merge is clean");
    let after2 = engine2.metadata().recent_changesets(u32::MAX).unwrap();
    assert_eq!(
        after2, before2,
        "the clean multi-base fold also stored nothing"
    );
}

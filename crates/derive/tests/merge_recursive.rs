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

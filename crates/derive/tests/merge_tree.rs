//! Tree-level 3-way merge classification fixtures (M9) via
//! `Engine::merge_changesets`: both-equal, one-side-is-base (including
//! deletion), mode conflicts and exec single-side wins, delete-vs-modify
//! in both directions, add/add (identical, differing content, differing
//! modes), file↔directory, symlink/gitlink atomicity, unrelated
//! histories, determinism, and the apply property
//! `apply(merged.changes, tree(ours)) == merged tree`.
//!
//! Histories are hand-built three-tip shapes: `root → base`, then
//! `base → ours` and `base → theirs` (so the merge base is unambiguous).
//! Each side is expressed as a full path→(mode, content) snapshot and
//! the side changeset is the obvious add/modify/delete delta, keeping
//! every fixture readable and deterministic.

use std::collections::BTreeMap;
use std::sync::Arc;

use gardenbed_derive::{ConflictReason, Engine, MergeOutcome};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore as _, ObjectStore as _};

/// A side snapshot: path → (mode, content). `BTreeMap` for canonical
/// iteration order.
type Snap = BTreeMap<String, (Mode, Vec<u8>)>;

fn snap(pairs: &[(&str, Mode, &[u8])]) -> Snap {
    pairs
        .iter()
        .map(|(p, m, c)| ((*p).to_owned(), (*m, c.to_vec())))
        .collect()
}

fn ident(tag: &str) -> Identity {
    Identity {
        name: format!("Merge {tag}"),
        email: format!("merge-{tag}@bed.example"),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

// The add/modify/delete delta taking `from` to `to`, in canonical path
// order. (File↔dir swaps never appear in these fixtures at delta level;
// they appear across the three-way merge instead.)
fn delta(from: &Snap, to: &Snap) -> Vec<Change> {
    let mut out = Vec::new();
    for (path, (mode, content)) in to {
        match from.get(path) {
            None => out.push(Change {
                path: path.clone(),
                op: Op::Add,
                blob: Some(blob_id(content)),
                src_path: None,
                mode: *mode,
            }),
            Some((m, c)) if m != mode || c != content => out.push(Change {
                path: path.clone(),
                op: Op::Modify,
                blob: Some(blob_id(content)),
                src_path: None,
                mode: *mode,
            }),
            _ => {}
        }
    }
    for path in from.keys() {
        if !to.contains_key(path) {
            out.push(Change {
                path: path.clone(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            });
        }
    }
    out
}

struct Fixture {
    engine: Engine,
    store: Arc<MemoryStore>,
    base: Id,
    ours: Id,
    theirs: Id,
}

/// Stages every blob of every snapshot and builds
/// `root → base → {ours, theirs}`.
fn fixture(base: &Snap, ours: &Snap, theirs: &Snap) -> Fixture {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());
    for s in [base, ours, theirs] {
        for (mode, content) in s.values() {
            // Gitlink pointers are carried opaquely (12 zero bytes +
            // upstream sha = the id itself); staging them as blobs would
            // be rejected as non-content-addressed.
            let id = blob_id(content);
            let is_pointer = mode == &Mode::Gitlink && content.as_slice() == id.as_bytes();
            if !is_pointer {
                store.put_blob(content).expect("stage blob");
            }
        }
    }
    // Unique, monotonically increasing committer timestamps: the merge
    // base tie-break is committer-time DESC, so equal timestamps would
    // make the chosen base arbitrary among the candidates (root vs
    // base). A counter keeps the fixtures deterministic AND makes the
    // intended base (the newest common ancestor) the unique winner.
    let mut tick = 0u64;
    let mut cs = |parents: Vec<Id>, changes: Vec<Change>, tag: &str| -> Id {
        tick += 1;
        let ident = Identity {
            name: format!("Merge {tag}"),
            email: "merge@bed.example".into(),
            time_secs: 1_700_000_000 + tick,
            tz_min: 0,
        };
        let c = Changeset {
            parents,
            changes,
            message: tag.to_owned(),
            author: ident.clone(),
            committer: ident,
            signature: None,
        };
        store.put_changeset(&c).expect("changeset ingests")
    };
    let root = cs(vec![], vec![], "root");
    let base_id = cs(vec![root], delta(&Snap::new(), base), "base");
    let ours_id = cs(vec![base_id], delta(base, ours), "ours");
    let theirs_id = cs(vec![base_id], delta(base, theirs), "theirs");
    Fixture {
        engine,
        store,
        base: base_id,
        ours: ours_id,
        theirs: theirs_id,
    }
}

/// Runs the merge and returns (changes, new_blobs), asserting clean and
/// staging the new blobs so the apply property can run.
fn merged(f: &Fixture) -> (Vec<Change>, Vec<(Id, Vec<u8>)>) {
    match f.engine.merge_changesets(f.ours, f.theirs).expect("merge runs") {
        MergeOutcome::Merged { changes, new_blobs } => {
            for (id, content) in &new_blobs {
                assert_eq!(blob_id(content), *id, "staged blob is content-addressed");
                f.store.put_blob(content).expect("stage merged blob");
            }
            (changes, new_blobs)
        }
        MergeOutcome::Conflicted { conflicts } => {
            panic!("expected clean merge, got {conflicts:?}")
        }
    }
}

/// Runs the merge and returns the conflict list, asserting conflicted.
fn conflicts(f: &Fixture) -> Vec<gardenbed_derive::MergeConflict> {
    match f.engine.merge_changesets(f.ours, f.theirs).expect("merge runs") {
        MergeOutcome::Conflicted { conflicts } => conflicts,
        MergeOutcome::Merged { changes, .. } => {
            panic!("expected conflicts, merged cleanly with {changes:?}")
        }
    }
}

/// The apply property: staging `new_blobs` and applying `changes` to
/// tree(ours) must produce the merged tree; returns the flattened
/// merged tree (path, mode, blob id) for content assertions.
fn apply_and_flatten(
    f: &Fixture,
    changes: &[Change],
    new_blobs: &[(Id, Vec<u8>)],
) -> Vec<(String, Mode, Id)> {
    for (id, content) in new_blobs {
        assert_eq!(blob_id(content), *id);
        f.store.put_blob(content).expect("stage merged blob");
    }
    let cs = Changeset {
        parents: vec![f.ours, f.theirs],
        changes: changes.to_vec(),
        message: "merged".into(),
        author: ident("merged"),
        committer: ident("merged"),
        signature: None,
    };
    let merged_id = f.store.put_changeset(&cs).expect("merged changeset ingests");
    let ours_tree = f.engine.derive_tree(f.ours).expect("ours tree");
    let applied = f
        .engine
        .apply_changeset_to_tree(ours_tree, &cs)
        .expect("changes apply to ours");
    let derived = f.engine.derive_tree(merged_id).expect("merged derives");
    assert_eq!(
        applied, derived,
        "apply(changes, tree(ours)) must equal the merged tree"
    );
    f.engine.flatten_tree(derived).expect("flatten")
}

// ------------------------------------------------------------ trivial

/// Both sides identical: no changes, no new blobs.
#[test]
fn both_sides_equal() {
    let side = snap(&[("a", Mode::Regular, b"same\n")]);
    let f = fixture(&side, &side, &side);
    assert!(merged(&f).0.is_empty());
}

/// Ours unchanged from base: the merge is theirs, expressed as the
/// diff of ours → theirs.
#[test]
fn ours_is_base_takes_theirs() {
    let base = snap(&[("a", Mode::Regular, b"1\n2\n3\n")]);
    let ours = base.clone();
    let theirs = snap(&[
        ("a", Mode::Regular, b"1\n2\n3\n"),
        ("new", Mode::Exec, b"#!/bin/sh\n"),
    ]);
    let f = fixture(&base, &ours, &theirs);
    let (changes, nb) = merged(&f);
    let tree = apply_and_flatten(&f, &changes, &nb);
    assert_eq!(
        tree,
        vec![
            ("a".to_owned(), Mode::Regular, blob_id(b"1\n2\n3\n")),
            ("new".to_owned(), Mode::Exec, blob_id(b"#!/bin/sh\n")),
        ]
    );
}

/// Theirs unchanged from base (deletion on ours survives): empty change
/// list — the merged tree is ours.
#[test]
fn theirs_is_base_keeps_ours_including_deletion() {
    let base = snap(&[
        ("a", Mode::Regular, b"keep\n"),
        ("b", Mode::Regular, b"gone\n"),
    ]);
    let ours = snap(&[("a", Mode::Regular, b"keep\n")]);
    let f = fixture(&base, &ours, &base);
    assert!(merged(&f).0.is_empty());
}

// ----------------------------------------------------------- content

/// Non-overlapping edits on both sides combine (merge3 auto-combine).
#[test]
fn non_overlapping_edits_combine() {
    let base = snap(&[("f", Mode::Regular, b"1\n2\n3\n4\n5\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("f", Mode::Regular, b"A\n2\n3\n4\n5\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("f", Mode::Regular, b"1\n2\n3\n4\nE\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let (changes, nb) = merged(&f);
    let tree = apply_and_flatten(&f, &changes, &nb);
    assert_eq!(
        tree,
        vec![
            ("f".to_owned(), Mode::Regular, blob_id(b"A\n2\n3\n4\nE\n")),
            ("keep".to_owned(), Mode::Regular, blob_id(b"k\n")),
        ]
    );
}

/// Overlapping edits conflict at content level.
#[test]
fn overlapping_edits_conflict() {
    let base = snap(&[("f", Mode::Regular, b"1\n2\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("f", Mode::Regular, b"1\nOURS\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("f", Mode::Regular, b"1\nTHEIRS\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "f");
    assert!(matches!(c[0].reason, ConflictReason::Content));
}

// -------------------------------------------------------------- modes

/// Exec-bit change on one side + content change on the other: both land
/// (mode table: ours==base ⇒ take theirs' mode; content auto-combines).
#[test]
fn exec_flip_one_side_wins() {
    let base = snap(&[("s", Mode::Regular, b"1\n2\n3\n4\n5\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("s", Mode::Exec, b"1\n2\n3\n4\n5\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("s", Mode::Regular, b"1\n2\n3\n4\nE\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let (changes, nb) = merged(&f);
    let tree = apply_and_flatten(&f, &changes, &nb);
    let mut tree = tree;
    tree.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        tree,
        vec![
            ("keep".to_owned(), Mode::Regular, blob_id(b"k\n")),
            ("s".to_owned(), Mode::Exec, blob_id(b"1\n2\n3\n4\nE\n")),
        ],
        "exec flip from ours + content edit from theirs combine"
    );
}

/// Mode and content both changed on both sides in incompatible ways:
/// conflict naming the path.
#[test]
fn mode_and_content_conflict() {
    let base = snap(&[("m", Mode::Regular, b"1\n2\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("m", Mode::Exec, b"1\nOURS\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("m", Mode::Symlink, b"1\nTHEIRS\n3\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].path, "m");
}

// ----------------------------------------------------- delete/modify

/// Ours modified, theirs deleted: hard conflict (v0.2 D-list).
#[test]
fn delete_vs_modify_ours_modified() {
    let base = snap(&[("f", Mode::Regular, b"base\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("f", Mode::Regular, b"ours\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "f");
    assert!(matches!(c[0].reason, ConflictReason::DeleteModify));
}

/// The other direction: ours deleted, theirs modified.
#[test]
fn delete_vs_modify_ours_deleted() {
    let base = snap(&[("f", Mode::Regular, b"base\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("f", Mode::Regular, b"theirs\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "f");
    assert!(matches!(c[0].reason, ConflictReason::DeleteModify));
}

/// Both sides deleted: clean, stays deleted.
#[test]
fn both_deleted_is_clean() {
    let base = snap(&[
        ("f", Mode::Regular, b"x\n"),
        ("g", Mode::Regular, b"y\n"),
    ]);
    let ours = snap(&[("g", Mode::Regular, b"y\n")]);
    let f = fixture(&base, &ours, &ours);
    assert!(merged(&f).0.is_empty());
}

// ------------------------------------------------------------ add/add

/// Add/add with identical content and mode: clean, no conflict.
#[test]
fn add_add_identical() {
    let base = snap(&[("keep", Mode::Regular, b"k\n")]);
    let side = snap(&[("keep", Mode::Regular, b"k\n"), ("both", Mode::Regular, b"same\n")]);
    let f = fixture(&base, &side, &side);
    assert!(merged(&f).0.is_empty(), "identical adds are already in ours");
}

/// Add/add of the same new file with prefix-shaped content: git still
/// conflicts — the empty base makes both sides one insertion hunk at
/// position 0, which overlaps (verified against `git merge-tree`:
/// "CONFLICT (add/add): Merge conflict in n"). Our merge3 must agree.
#[test]
fn add_add_prefix_content_still_conflicts() {
    let base = snap(&[("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("keep", Mode::Regular, b"k\n"), ("n", Mode::Regular, b"1\n2\n")]);
    let theirs = snap(&[("keep", Mode::Regular, b"k\n"), ("n", Mode::Regular, b"1\n2\n3\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "n");
    assert!(matches!(
        c[0].reason,
        ConflictReason::AddAdd | ConflictReason::Content
    ));
}

/// Add/add with genuinely disjoint content at the same point conflicts.
#[test]
fn add_add_conflicting_content() {
    let base = snap(&[("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("keep", Mode::Regular, b"k\n"), ("n", Mode::Regular, b"ours\n")]);
    let theirs = snap(&[("keep", Mode::Regular, b"k\n"), ("n", Mode::Regular, b"theirs\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "n");
    assert!(matches!(
        c[0].reason,
        ConflictReason::AddAdd | ConflictReason::Content
    ));
}

// --------------------------------------------------------- file ↔ dir

/// Ours has a file where theirs has a directory: conflict.
#[test]
fn file_vs_dir_conflict() {
    let base = snap(&[("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("keep", Mode::Regular, b"k\n"), ("x", Mode::Regular, b"file\n")]);
    let theirs = snap(&[
        ("keep", Mode::Regular, b"k\n"),
        ("x/y", Mode::Regular, b"under\n"),
        ("x/z", Mode::Exec, b"#!/bin/sh\n"),
    ]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!(c[0].path, "x");
    assert!(matches!(c[0].reason, ConflictReason::FileDir));
}

// ---------------------------------------------------------- linklike

/// Symlink vs symlink with different targets: atomic conflict.
#[test]
fn symlink_both_changed_conflicts() {
    let base = snap(&[("l", Mode::Symlink, b"target-a\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("l", Mode::Symlink, b"target-b\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = snap(&[("l", Mode::Symlink, b"target-c\n"), ("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "l");
    assert!(matches!(c[0].reason, ConflictReason::Link));
}

/// Symlink unchanged on one side: the other side's change wins cleanly.
#[test]
fn symlink_one_side_change_wins() {
    let base = snap(&[("l", Mode::Symlink, b"target-a\n"), ("keep", Mode::Regular, b"k\n")]);
    let ours = snap(&[("l", Mode::Symlink, b"target-b\n"), ("keep", Mode::Regular, b"k\n")]);
    let theirs = base.clone();
    let f = fixture(&base, &ours, &theirs);
    assert!(merged(&f).0.is_empty(), "theirs==base ⇒ merged tree is ours");
}

/// Gitlink (submodule pointer) changed on both sides: atomic conflict.
#[test]
fn gitlink_both_changed_conflicts() {
    let mk = |n: u8| {
        let mut p = [0u8; 32];
        p[12] = n;
        (Id::from_bytes(p), p.to_vec())
    };
    let (_, bcontent) = mk(1);
    let (_, ocontent) = mk(2);
    let (_, tcontent) = mk(3);
    let mut base = Snap::new();
    base.insert("keep".to_owned(), (Mode::Regular, b"k\n".to_vec()));
    base.insert("sub".to_owned(), (Mode::Gitlink, bcontent));
    let mut ours = Snap::new();
    ours.insert("keep".to_owned(), (Mode::Regular, b"k\n".to_vec()));
    ours.insert("sub".to_owned(), (Mode::Gitlink, ocontent));
    let mut theirs = Snap::new();
    theirs.insert("keep".to_owned(), (Mode::Regular, b"k\n".to_vec()));
    theirs.insert("sub".to_owned(), (Mode::Gitlink, tcontent));
    let f = fixture(&base, &ours, &theirs);
    let c = conflicts(&f);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].path, "sub");
    assert!(matches!(c[0].reason, ConflictReason::Link));
}

// ----------------------------------------------------------- structural

/// Unrelated histories: named error, no merge.
#[test]
fn unrelated_histories_error() {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());
    store.put_blob(b"x\n").unwrap();
    let mk = |msg: &str| {
        let cs = Changeset {
            parents: vec![],
            changes: vec![Change {
                path: "f".to_owned(),
                op: Op::Add,
                blob: Some(blob_id(b"x\n")),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: msg.to_owned(),
            author: ident(msg),
            committer: ident(msg),
            signature: None,
        };
        store.put_changeset(&cs).unwrap()
    };
    let a = mk("left");
    let b = mk("right");
    let err = engine
        .merge_changesets(a, b)
        .expect_err("unrelated histories must error");
    let msg = err.to_string();
    assert!(msg.contains("unrelated"), "error names the situation: {msg}");
    assert!(msg.contains("merge"), "error names the operation: {msg}");
}

/// Determinism: two engines over separately-ingested identical stores
/// produce identical outcomes (BTreeMap discipline, no clocks).
#[test]
fn deterministic_across_engines() {
    let base = snap(&[
        ("a", Mode::Regular, b"1\n2\n3\n"),
        ("b", Mode::Exec, b"x\n"),
    ]);
    let ours = snap(&[
        ("a", Mode::Regular, b"OURS\n2\n3\n"),
        ("b", Mode::Exec, b"x\n"),
        ("c", Mode::Regular, b"new-ours\n"),
    ]);
    let theirs = snap(&[
        ("a", Mode::Regular, b"1\n2\nTHEIRS\n"),
        ("b", Mode::Exec, b"x\n"),
        ("d", Mode::Regular, b"new-theirs\n"),
    ]);
    let f1 = fixture(&base, &ours, &theirs);
    let f2 = fixture(&base, &ours, &theirs);
    let o1 = format!("{:?}", f1.engine.merge_changesets(f1.ours, f1.theirs));
    let o2 = format!("{:?}", f2.engine.merge_changesets(f2.ours, f2.theirs));
    assert_eq!(o1, o2);
    // And it is clean with both edits combined.
    let (changes, nb) = merged(&f1);
    let tree = apply_and_flatten(&f1, &changes, &nb);
    assert_eq!(
        tree,
        vec![
            ("a".to_owned(), Mode::Regular, blob_id(b"OURS\n2\nTHEIRS\n")),
            ("b".to_owned(), Mode::Exec, blob_id(b"x\n")),
            ("c".to_owned(), Mode::Regular, blob_id(b"new-ours\n")),
            ("d".to_owned(), Mode::Regular, blob_id(b"new-theirs\n")),
        ]
    );
}

/// Directory deleted on theirs while ours kept it unchanged: the merged
/// tree drops every file under the directory (the delete must cascade).
#[test]
fn theirs_deleted_directory_cascades() {
    let base = snap(&[
        ("dir/a", Mode::Regular, b"1\n"),
        ("dir/b", Mode::Regular, b"2\n"),
        ("keep", Mode::Regular, b"k\n"),
    ]);
    let ours = base.clone();
    let theirs = snap(&[("keep", Mode::Regular, b"k\n")]);
    let f = fixture(&base, &ours, &theirs);
    let (changes, nb) = merged(&f);
    let tree = apply_and_flatten(&f, &changes, &nb);
    assert_eq!(
        tree,
        vec![("keep".to_owned(), Mode::Regular, blob_id(b"k\n"))],
        "theirs' directory deletion must remove every file under dir/"
    );
}

/// Directory added on theirs: every file under it arrives as an Add.
#[test]
fn theirs_added_directory_arrives_wholesale() {
    let base = snap(&[("keep", Mode::Regular, b"k\n")]);
    let ours = base.clone();
    let theirs = snap(&[
        ("keep", Mode::Regular, b"k\n"),
        ("new/deep/file", Mode::Regular, b"deep\n"),
        ("new/other", Mode::Exec, b"#!/bin/sh\n"),
    ]);
    let f = fixture(&base, &ours, &theirs);
    let (changes, nb) = merged(&f);
    let tree = apply_and_flatten(&f, &changes, &nb);
    assert_eq!(
        tree,
        vec![
            ("keep".to_owned(), Mode::Regular, blob_id(b"k\n")),
            ("new/deep/file".to_owned(), Mode::Regular, blob_id(b"deep\n")),
            ("new/other".to_owned(), Mode::Exec, blob_id(b"#!/bin/sh\n")),
        ]
    );
}

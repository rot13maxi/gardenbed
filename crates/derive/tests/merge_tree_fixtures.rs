//! Tree-merge fixture matrix, part two (v0.2.md §4 rules that the
//! generated differential pairs and `merge.rs`'s fixtures do not pin):
//!
//! - symlinks and gitlinks are atomic — id equality only, no content
//!   merge (both-differ conflicts, one-side change wins cleanly);
//! - add/add with identical content on both sides resolves to one
//!   entry, not a conflict;
//! - mode divergence alone (both sides changed the mode differently)
//!   conflicts even when the content merges;
//! - deletes cascade: a whole deleted directory vs a modification
//!   inside it conflicts at the modified path, and a directory added
//!   wholesale on one side arrives wholesale.
//!
//! The engine-level contract mirrors `merge.rs`:
//! `merge_tree_changes(Some(base), ours, theirs)` returns
//! `Ok(Vec<Change>)` (clean) or `Err(Vec<String>)` (conflicting paths).

use std::collections::BTreeMap;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

fn ident(tag: &str) -> Identity {
    Identity {
        name: format!("Fixture Tester {tag}"),
        email: "fixture@gardenbed.example".to_owned(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

/// One changeset in a fresh engine; the caller supplies the full
/// change list (this fixture family needs Mode-typed entries —
/// symlinks, gitlinks, exec flips — that the (path, content) helper in
/// `merge.rs` cannot express).
fn commit(
    engine: &Engine,
    metadata: &Arc<MemoryStore>,
    changes: Vec<Change>,
    parents: Vec<Id>,
    msg: &str,
) -> Id {
    let cs = gardenbed_model::Changeset {
        parents,
        changes,
        message: msg.to_owned(),
        author: ident("F"),
        committer: ident("F"),
        signature: None,
    };
    let id = metadata.put_changeset(&cs).unwrap();
    engine.on_changeset_created(id).unwrap();
    id
}

fn engine() -> (Engine, Arc<MemoryStore>) {
    let objects = Arc::new(MemoryStore::new()) as Arc<dyn ObjectStore>;
    let metadata = Arc::new(MemoryStore::new());
    let engine = Engine::new(objects, metadata.clone());
    (engine, metadata)
}

fn put(engine: &Engine, content: &[u8]) -> Id {
    let id = blob_id(content);
    engine.objects().put_blob(content).unwrap();
    id
}

fn change(path: &str, op: Op, blob: Option<Id>, mode: Mode) -> Change {
    Change {
        path: path.to_owned(),
        op,
        blob,
        src_path: None,
        mode,
    }
}

/// Symlink vs symlink with different targets on both sides: atomic
/// conflict — merge3 must not try to merge the target strings.
#[test]
fn symlink_both_changed_conflicts() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("l", Op::Add, Some(put(&engine, b"target-a")), Mode::Symlink),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change(
            "l",
            Op::Modify,
            Some(put(&engine, b"target-b")),
            Mode::Symlink,
        )],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "l",
            Op::Modify,
            Some(put(&engine, b"target-c")),
            Mode::Symlink,
        )],
        vec![base],
        "theirs",
    );
    let conflicts = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap_err();
    assert_eq!(conflicts, vec!["l".to_owned()]);
}

/// Symlink changed on one side only: the other side's change wins
/// cleanly (atomic take-theirs / keep-ours).
#[test]
fn symlink_one_side_change_wins() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("l", Op::Add, Some(put(&engine, b"target-a")), Mode::Symlink),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change(
            "l",
            Op::Modify,
            Some(put(&engine, b"target-b")),
            Mode::Symlink,
        )],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "keep",
            Op::Modify,
            Some(put(&engine, b"theirs\n")),
            Mode::Regular,
        )],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "keep");
}

/// Gitlink (submodule pointer) changed on both sides: atomic conflict
/// — the pointers are opaque ids, never content-merged.
#[test]
fn gitlink_both_changed_conflicts() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change(
                "sub",
                Op::Add,
                Some(put(&engine, &[1u8; 20])),
                Mode::Gitlink,
            ),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change(
            "sub",
            Op::Modify,
            Some(put(&engine, &[2u8; 20])),
            Mode::Gitlink,
        )],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "sub",
            Op::Modify,
            Some(put(&engine, &[3u8; 20])),
            Mode::Gitlink,
        )],
        vec![base],
        "theirs",
    );
    let conflicts = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap_err();
    assert_eq!(conflicts, vec!["sub".to_owned()]);
}

/// Gitlink moved on one side only: clean take of the moved pointer.
#[test]
fn gitlink_one_side_change_wins() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change(
                "sub",
                Op::Add,
                Some(put(&engine, &[1u8; 20])),
                Mode::Gitlink,
            ),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change(
            "sub",
            Op::Modify,
            Some(put(&engine, &[2u8; 20])),
            Mode::Gitlink,
        )],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "keep",
            Op::Modify,
            Some(put(&engine, b"theirs\n")),
            Mode::Regular,
        )],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "keep");
}

/// Add/add with identical content (and mode) on both sides: one entry,
/// no conflict — the same-blob rule prunes before overlap testing.
#[test]
fn add_add_identical_is_clean() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![change(
            "shared",
            Op::Add,
            Some(put(&engine, b"x\n")),
            Mode::Regular,
        )],
        vec![],
        "base",
    );
    let blob = put(&engine, b"same\n");
    let ours = commit(
        &engine,
        &metadata,
        vec![change("new", Op::Add, Some(blob), Mode::Regular)],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change("new", Op::Add, Some(blob), Mode::Regular)],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    assert!(changes.is_empty(), "both sides equal → nothing to apply");
}

/// Both sides deleted the same path: clean (no delete/delete conflict).
#[test]
fn both_deleted_is_clean() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("f", Op::Add, Some(put(&engine, b"f\n")), Mode::Regular),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change("f", Op::Delete, None, Mode::Regular)],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change("f", Op::Delete, None, Mode::Regular)],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    assert!(changes.is_empty());
}

/// A directory deleted on one side while the other side modified a
/// file inside it: delete-vs-modify at the modified path (v0.2 keeps
/// these as hard conflicts).
#[test]
fn deleted_directory_vs_inner_modify_conflicts() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("dir/a", Op::Add, Some(put(&engine, b"a\n")), Mode::Regular),
            change("dir/b", Op::Add, Some(put(&engine, b"b\n")), Mode::Regular),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![
            change("dir/a", Op::Delete, None, Mode::Regular),
            change("dir/b", Op::Delete, None, Mode::Regular),
        ],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "dir/a",
            Op::Modify,
            Some(put(&engine, b"modified\n")),
            Mode::Regular,
        )],
        vec![base],
        "theirs",
    );
    let conflicts = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap_err();
    assert_eq!(conflicts, vec!["dir/a".to_owned()]);
}

/// A directory deleted on one side and untouched inside on the other:
/// the deletion lands cleanly (their unchanged inner files are not
/// resurrected).
#[test]
fn deleted_directory_untouched_inside_lands() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("dir/a", Op::Add, Some(put(&engine, b"a\n")), Mode::Regular),
            change("dir/b", Op::Add, Some(put(&engine, b"b\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![
            change("dir/a", Op::Delete, None, Mode::Regular),
            change("dir/b", Op::Delete, None, Mode::Regular),
        ],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change(
            "top",
            Op::Add,
            Some(put(&engine, b"t\n")),
            Mode::Regular,
        )],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    // theirs' top arrives; nothing of dir/ comes back.
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "top");
}

/// A directory added wholesale on one side: every inner file arrives
/// as its own change (tree merges walk to file granularity).
#[test]
fn added_directory_arrives_wholesale() {
    let (engine, metadata) = engine();
    let base = commit(
        &engine,
        &metadata,
        vec![change(
            "keep",
            Op::Add,
            Some(put(&engine, b"k\n")),
            Mode::Regular,
        )],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change(
            "ours-only",
            Op::Add,
            Some(put(&engine, b"o\n")),
            Mode::Regular,
        )],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![
            change("dir/x", Op::Add, Some(put(&engine, b"x\n")), Mode::Regular),
            change("dir/y", Op::Add, Some(put(&engine, b"y\n")), Mode::Regular),
            change(
                "dir/sub/z",
                Op::Add,
                Some(put(&engine, b"z\n")),
                Mode::Regular,
            ),
        ],
        vec![base],
        "theirs",
    );
    let changes = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap();
    let mut paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    assert_eq!(paths, vec!["dir/sub/z", "dir/x", "dir/y"]);
}

/// Mode divergence alone conflicts: both sides flipped the mode away
/// from base differently (ours → exec, theirs → symlink), same
/// content. The mode merge table has no common resolution.
#[test]
fn mode_divergence_alone_conflicts() {
    let (engine, metadata) = engine();
    let blob = put(&engine, b"content\n");
    let base = commit(
        &engine,
        &metadata,
        vec![
            change("f", Op::Add, Some(blob), Mode::Regular),
            change("keep", Op::Add, Some(put(&engine, b"k\n")), Mode::Regular),
        ],
        vec![],
        "base",
    );
    let ours = commit(
        &engine,
        &metadata,
        vec![change("f", Op::Modify, Some(blob), Mode::Exec)],
        vec![base],
        "ours",
    );
    let theirs = commit(
        &engine,
        &metadata,
        vec![change("f", Op::Modify, Some(blob), Mode::Symlink)],
        vec![base],
        "theirs",
    );
    let conflicts = engine
        .merge_tree_changes(Some(base), ours, theirs)
        .unwrap()
        .unwrap_err();
    assert_eq!(conflicts, vec!["f".to_owned()]);
}

/// The flat merged tree, as path -> (mode, blob), for assertions on
/// what the changes produce when applied to ours.
#[allow(dead_code)]
fn flat(engine: &Engine, cs: Id) -> BTreeMap<String, (Mode, Id)> {
    engine
        .flatten_tree(engine.derive_tree(cs).unwrap())
        .unwrap()
        .into_iter()
        .map(|(p, m, b)| (p, (m, b)))
        .collect()
}

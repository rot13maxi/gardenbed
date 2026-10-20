//! Recursive tree-to-tree diff (rename detection off in v0.1).
//!
//! A rename with detection off appears exactly as git's `--no-renames` shows
//! it: a Delete of the old path plus an Add of the new one. Classification
//! maps to [`Op`] as Add/Modify/Delete; there is no "type change" class — a
//! path that exists on both sides with a different blob and/or mode (git's
//! `T` for mode-class flips, e.g. when a rename lands on an existing path of
//! another type) is a gardenbed Modify. The diff-equality test maps git's
//! `T` onto Modify for exactly this reason. File↔directory swaps split into
//! a Delete of the file path plus Adds of the files under the directory (and
//! vice versa), matching `git diff-tree -r`.

use gardenbed_model::{Id, Op, TreeKind};
use gardenbed_store::StoreError;

use crate::Engine;
use crate::apply::MAX_TREE_DEPTH;

/// Recursively diffs tree `a` against tree `b` (a → b) and returns the
/// changes sorted by path bytes ascending — the changeset canonical order.
///
/// Both ids may be `None` (the empty tree), so this doubles as
/// "empty tree → tree(b)" and vice versa. Recursion is depth-bounded;
/// content-addressed trees cannot cycle, but corrupt or adversarial
/// payloads still hit [`MAX_TREE_DEPTH`] and return
/// [`StoreError::Invalid`] instead of overflowing the stack.
pub(crate) fn diff_trees(
    objects: &dyn gardenbed_store::ObjectStore,
    a: Option<Id>,
    b: Option<Id>,
) -> Result<Vec<gardenbed_model::Change>, StoreError> {
    let mut out = Vec::new();
    diff_dir(objects, a, b, "", &mut out, 0)?;
    out.sort_by(|x, y| x.path.as_bytes().cmp(y.path.as_bytes()));
    Ok(out)
}

/// Diffs one directory level, recursing into child trees.
fn diff_dir(
    objects: &dyn gardenbed_store::ObjectStore,
    a: Option<Id>,
    b: Option<Id>,
    dir: &str,
    out: &mut Vec<gardenbed_model::Change>,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(StoreError::Invalid(
            "tree nesting exceeds the maximum supported depth".into(),
        ));
    }
    let ta = match a {
        None => gardenbed_model::Tree::default(),
        Some(id) => Engine::load_tree(objects, id)?,
    };
    let tb = match b {
        None => gardenbed_model::Tree::default(),
        Some(id) => Engine::load_tree(objects, id)?,
    };
    // Entries are sorted by name bytes on both sides; a merge walk pairs
    // them in linear time.
    let (mut i, mut j) = (0usize, 0usize);
    while i < ta.entries.len() || j < tb.entries.len() {
        let ea = ta.entries.get(i);
        let eb = tb.entries.get(j);
        let ord = match (ea, eb) {
            (None, None) => unreachable!("loop guard"),
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(x), Some(y)) => x.name.as_bytes().cmp(y.name.as_bytes()),
        };
        match ord {
            std::cmp::Ordering::Less => {
                // Only in a: deleted (recursively, if a directory).
                let e = ea.expect("Less implies Some");
                push_deleted(objects, e, dir, out)?;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                // Only in b: added (recursively, if a directory).
                let e = eb.expect("Greater implies Some");
                push_added(objects, e, dir, out, depth)?;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                let (x, y) = (
                    ea.expect("Equal implies Some"),
                    eb.expect("Equal implies Some"),
                );
                match (x.kind, y.kind) {
                    (TreeKind::File, TreeKind::File) => {
                        if x.blob != y.blob || x.mode != y.mode {
                            out.push(gardenbed_model::Change {
                                path: join(dir, &x.name),
                                op: Op::Modify,
                                blob: y.blob,
                                src_path: None,
                                mode: y.mode,
                            });
                        }
                    }
                    (TreeKind::Dir, TreeKind::Dir) => {
                        if x.tree != y.tree {
                            diff_dir(objects, x.tree, y.tree, &join(dir, &x.name), out, depth + 1)?;
                        }
                    }
                    // A file↔directory swap deletes one side and adds the other.
                    (TreeKind::File, TreeKind::Dir) => {
                        push_deleted(objects, x, dir, out)?;
                        push_added(objects, y, dir, out, depth)?;
                    }
                    (TreeKind::Dir, TreeKind::File) => {
                        push_deleted(objects, x, dir, out)?;
                        push_added(objects, y, dir, out, depth)?;
                    }
                }
                i += 1;
                j += 1;
            }
        }
    }
    Ok(())
}

/// Emits Delete (+ subtree deletes) for an entry present only in `a`.
fn push_deleted(
    objects: &dyn gardenbed_store::ObjectStore,
    e: &gardenbed_model::TreeEntry,
    dir: &str,
    out: &mut Vec<gardenbed_model::Change>,
) -> Result<(), StoreError> {
    let path = join(dir, &e.name);
    match e.kind {
        TreeKind::File => out.push(gardenbed_model::Change {
            path,
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: gardenbed_model::Mode::Regular,
        }),
        TreeKind::Dir => {
            let id = e.tree.ok_or_else(|| {
                StoreError::Corrupt(format!("dir entry {:?} without a tree id", e.name))
            })?;
            for sub in Engine::load_tree(objects, id)?.entries {
                push_deleted(objects, &sub, &path, out)?;
            }
        }
    }
    Ok(())
}

/// Emits Add (+ subtree adds) for an entry present only in `b`.
fn push_added(
    objects: &dyn gardenbed_store::ObjectStore,
    e: &gardenbed_model::TreeEntry,
    dir: &str,
    out: &mut Vec<gardenbed_model::Change>,
    depth: usize,
) -> Result<(), StoreError> {
    let path = join(dir, &e.name);
    match e.kind {
        TreeKind::File => {
            let blob = e.blob.ok_or_else(|| {
                StoreError::Corrupt(format!("file entry {:?} without a blob id", e.name))
            })?;
            out.push(gardenbed_model::Change {
                path,
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode: e.mode,
            });
        }
        TreeKind::Dir => {
            let id = e.tree.ok_or_else(|| {
                StoreError::Corrupt(format!("dir entry {:?} without a tree id", e.name))
            })?;
            if depth > MAX_TREE_DEPTH {
                return Err(StoreError::Invalid(
                    "tree nesting exceeds the maximum supported depth".into(),
                ));
            }
            for sub in Engine::load_tree(objects, id)?.entries {
                push_added(objects, &sub, &path, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// Joins a directory prefix with a component.
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

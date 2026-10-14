//! Repairing git's dir→file replacement output so `derive` can apply it.
//!
//! When a first-parent tree has a *directory* at `P` and the new tree has
//! a *file* there, `git diff-tree -r --name-status` reports the new file
//! as an addition at `P` plus one deletion per old child under `P/`
//! (verified empirically; with rename detection the child deletions may
//! pair up as renames whose source is still under `P/`).
//!
//! That shape collides in `gardenbed-derive`'s applier: changes apply in
//! ascending path order (`P` < `P/child`), and an `Add`/`Modify` at `P`
//! replaces the parent's directory slot with a file *before* the
//! deletions under `P/` run — the applier then finds a file where it
//! needs to open a directory (`open_dir` collision).
//!
//! The repair exploits an applier property: a `Rename`'s `remove(src)`
//! runs *before* its `set_file(path)` — and if `src` lies under `P/`, that
//! first removal opens `P`'s directory while it is still intact. The
//! parent slot is then replaced, but the *open* directory map survives in
//! the working set, so every remaining operation under `P/` (deletes and
//! rename-source removals alike) keeps working on it. Exactly one
//! "consuming" rewrite per replaced directory suffices:
//!
//! 1. The change `X` placing the file at `P` is a `Rename` whose
//!    `src_path` is under `P/` — nothing to do (self-consuming: git paired
//!    one of the old children into `X` itself, e.g. `R P ← P/f1`).
//! 2. Otherwise, if a plain `Delete` under `P/` exists, consume the
//!    lexicographically smallest one: it is removed from the change list
//!    and `X` becomes `Rename { path: P, src_path: Q }`.
//! 3. Otherwise, if a rename's source under `P/` exists (git paired the
//!    old children into renames targeting elsewhere), consume the
//!    lexicographically smallest one: that rename loses its source and
//!    becomes an `Add` (its destination content is unchanged), and `X`
//!    becomes the consuming `Rename` as above.
//! 4. Anything else violates git's invariants (a non-empty directory
//!    always leaves at least one delete or rename source under `P/`) and
//!    is a defensive error, never a silent mangle.
//!
//! If `X` was itself a rename from outside `P/`, that old source must
//! still be removed: an explicit `Delete { src }` is emitted.
//!
//! All rewrites preserve canonical semantics: old-side paths stay unique
//! (git's diff names each first-parent path at most once), deletes carry
//! no blob and mode `Regular`, and the consumed `Q` — a first-parent child
//! of `P` — exists at apply time, satisfying the rename's `remove(src)`.
//! Each consume strictly shrinks the candidate pool (a consumed delete or
//! rename source; a rewritten rename frees its source, which re-enters as
//! exactly the `Delete` the repair emits for it), so the loop terminates.

use gardenbed_model::{Change, Mode, Op};

use crate::error::ImportError;

/// Rewrites dir→file replacements in `changes` until none remain.
///
/// `commit_hex` only feeds error context. Deterministic: consuming changes
/// are processed in ascending path order and the smallest candidate is
/// consumed each round.
pub(crate) fn repair_dir_to_file(
    changes: &mut Vec<Change>,
    commit_hex: &str,
) -> Result<(), ImportError> {
    loop {
        // The smallest-path change that places a file into a directory
        // being replaced and is not already self-consuming.
        let mut target: Option<usize> = None;
        for (i, x) in changes.iter().enumerate() {
            if !matches!(x.op, Op::Add | Op::Modify | Op::Rename) {
                continue;
            }
            if !has_under_paths(changes, i, &x.path) {
                continue;
            }
            let self_consuming = x.op == Op::Rename
                && x.src_path
                    .as_deref()
                    .is_some_and(|s| is_under(&x.path, s));
            if self_consuming {
                continue;
            }
            if target.is_none_or(|t| x.path.as_bytes() < changes[t].path.as_bytes()) {
                target = Some(i);
            }
        }
        let Some(xi) = target else { break };
        consume_one(changes, xi, commit_hex)?;
    }
    Ok(())
}

/// `child` lies strictly under directory `dir` (`dir/…`).
fn is_under(dir: &str, child: &str) -> bool {
    child.len() > dir.len()
        && child.starts_with(dir)
        && child.as_bytes()[dir.len()] == b'/'
}

/// Whether any *other* change targets a path strictly under `dir` — either
/// as its own path (a delete) or as a rename source.
fn has_under_paths(changes: &[Change], exclude: usize, dir: &str) -> bool {
    changes.iter().enumerate().any(|(j, c)| {
        j != exclude
            && (is_under(dir, &c.path) || c.src_path.as_deref().is_some_and(|s| is_under(dir, s)))
    })
}

/// Performs exactly one consuming rewrite for the file-placing change at
/// `changes[xi]`.
fn consume_one(changes: &mut Vec<Change>, xi: usize, commit_hex: &str) -> Result<(), ImportError> {
    let dir = changes[xi].path.clone();
    // Candidate pools (positions, then pick lexicographically smallest).
    let mut best_delete: Option<(String, usize)> = None;
    let mut best_src: Option<(String, usize)> = None;
    for (j, c) in changes.iter().enumerate() {
        if j == xi {
            continue;
        }
        if c.op == Op::Delete && is_under(&dir, &c.path) {
            if best_delete.as_ref().is_none_or(|(p, _)| c.path.as_bytes() < p.as_bytes()) {
                best_delete = Some((c.path.clone(), j));
            }
        }
        if c.op == Op::Rename {
            if let Some(src) = &c.src_path {
                if is_under(&dir, src)
                    && best_src.as_ref().is_none_or(|(p, _)| src.as_bytes() < p.as_bytes())
                {
                    best_src = Some((src.clone(), j));
                }
            }
        }
    }
    // Old-side path accounting: the old source of a consuming rename must
    // still be removed explicitly.
    let old_src = changes[xi].src_path.clone();

    enum Consume {
        Delete(usize),
        RenameSrc(usize),
    }
    let chosen = if let Some((_, j)) = best_delete {
        Consume::Delete(j)
    } else if let Some((_, j)) = best_src {
        Consume::RenameSrc(j)
    } else {
        return Err(ImportError::Diff {
            sha: commit_hex.to_owned(),
            detail: format!(
                "directory {dir:?} is replaced by a file but git reported no deletion or \
                 rename under it — diff-tree output violates git's invariants"
            ),
        });
    };

    // Rewrite X into the consuming rename; the consumed entry changes role.
    let q = match chosen {
        Consume::Delete(j) => {
            let q = changes[j].path.clone();
            changes.remove(j);
            q
        }
        Consume::RenameSrc(j) => {
            // The rename keeps its destination as a plain add; its source
            // moves into X (removing it once, via X's apply).
            let src = changes[j].src_path.clone().expect("rename has a source");
            changes[j] = Change {
                op: Op::Add,
                path: changes[j].path.clone(),
                blob: changes[j].blob,
                src_path: None,
                mode: changes[j].mode,
            };
            src
        }
    };
    let xi = {
        // `changes.remove` above may have shifted indices; relocate X by
        // its (unique) path.
        changes
            .iter()
            .position(|c| c.path == dir)
            .expect("the placing change is still present")
    };
    let (blob, mode) = (changes[xi].blob, changes[xi].mode);
    changes[xi] = Change {
        op: Op::Rename,
        path: dir.clone(),
        blob,
        src_path: Some(q),
        mode,
    };
    if let Some(s) = old_src {
        if s != dir {
            changes.push(Change {
                op: Op::Delete,
                path: s,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The repaired changesets are applied through the real derive engine
    //! onto a real base tree, proving both that the collision is gone and
    //! that the resulting tree matches the git snapshot the changes were
    //! derived from.
    use gardenbed_derive::Engine;
    use gardenbed_model::{blob_id, Change, Changeset, Identity, Mode, Op};
    use gardenbed_store::{MemoryStore, MetadataStore};

    use super::repair_dir_to_file;

    fn id(name: &str) -> Identity {
        Identity {
            name: name.into(),
            email: format!("{name}@example.com"),
            time_secs: 1_700_000_000,
            tz_min: 0,
        }
    }

    fn change(path: &str, op: Op, content: &str, mode: Mode) -> Change {
        Change {
            path: path.into(),
            op,
            blob: (op != Op::Delete).then(|| blob_id(content.as_bytes())),
            src_path: None,
            mode,
        }
    }

    fn rename(path: &str, src: &str, content: &str, mode: Mode) -> Change {
        let mut c = change(path, Op::Rename, content, mode);
        c.src_path = Some(src.into());
        c
    }

    /// Stores `base` (root adds) and then `child` (with parent `base`),
    /// returning the child's derived tree as a sorted `(path, mode, blob)`
    /// list. Fails the test when the applier rejects the changeset.
    fn apply(
        base_changes: Vec<Change>,
        child_changes: Vec<Change>,
    ) -> Result<Vec<(String, Mode, Option<gardenbed_model::Id>)>, String> {
        let objects = std::sync::Arc::new(MemoryStore::new());
        let metadata = std::sync::Arc::new(MemoryStore::new());
        let engine = Engine::new(objects.clone(), metadata.clone());
        let base = Changeset {
            parents: Vec::new(),
            changes: base_changes,
            message: "base".into(),
            author: id("a"),
            committer: id("a"),
            signature: None,
        };
        let base_id = metadata
            .put_changeset(&base)
            .map_err(|e| e.to_string())?;
        engine
            .on_changeset_created(base_id)
            .map_err(|e| e.to_string())?;
        let child = Changeset {
            parents: vec![base_id],
            changes: child_changes,
            message: "child".into(),
            author: id("b"),
            committer: id("b"),
            signature: None,
        };
        let child_id = metadata.put_changeset(&child).map_err(|e| e.to_string())?;
        engine
            .on_changeset_created(child_id)
            .map_err(|e| e.to_string())?;
        let tree = engine.derive_tree(child_id).map_err(|e| e.to_string())?;
        Ok(engine
            .flatten_tree(tree)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(p, m, b)| (p, m, Some(b)))
            .collect())
    }

    #[test]
    fn plain_add_over_dir_consumes_smallest_delete() {
        // git: A P, D P/f1, D P/f2 (P was a dir with f1="one", f2="two").
        let mut changes = vec![
            change("P", Op::Add, "fileP", Mode::Regular),
            change("P/f1", Op::Delete, "", Mode::Regular),
            change("P/f2", Op::Delete, "", Mode::Regular),
        ];
        repair_dir_to_file(&mut changes, "deadbeef").expect("repairs");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Rename);
        assert_eq!(changes[0].path, "P");
        assert_eq!(changes[0].src_path.as_deref(), Some("P/f1"));
        assert_eq!(changes[1], change("P/f2", Op::Delete, "", Mode::Regular));

        // Applying onto the dir-shaped base yields the file-shaped tree.
        let mut base = vec![
            change("P/f1", Op::Add, "one", Mode::Regular),
            change("P/f2", Op::Add, "two", Mode::Regular),
        ];
        base.push(change("z", Op::Add, "z", Mode::Regular));
        let tree = apply(base, changes).expect("applies without collision");
        assert_eq!(
            tree,
            vec![
                ("P".into(), Mode::Regular, Some(blob_id(b"fileP"))),
                ("z".into(), Mode::Regular, Some(blob_id(b"z"))),
            ]
        );
    }

    #[test]
    fn self_consuming_rename_needs_no_repair() {
        // git -M: R P <- P/f1, D P/f2 (new file content = old f1's).
        let mut changes = vec![
            rename("P", "P/f1", "one", Mode::Regular),
            change("P/f2", Op::Delete, "", Mode::Regular),
        ];
        repair_dir_to_file(&mut changes, "deadbeef").expect("no-op");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].src_path.as_deref(), Some("P/f1"));

        let base = vec![
            change("P/f1", Op::Add, "one", Mode::Regular),
            change("P/f2", Op::Add, "two", Mode::Regular),
        ];
        let tree = apply(base, changes).expect("applies");
        assert_eq!(tree, vec![("P".into(), Mode::Regular, Some(blob_id(b"one")))]);
    }

    #[test]
    fn moved_child_case_consumes_a_rename_source() {
        // git -M, case C: A D, R moved.txt ← D/child (no deletes under D/).
        let mut changes = vec![
            change("D", Op::Add, "fileD", Mode::Regular),
            rename("moved.txt", "D/child", "moved-content", Mode::Regular),
        ];
        repair_dir_to_file(&mut changes, "deadbeef").expect("repairs");
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Rename);
        assert_eq!(changes[0].src_path.as_deref(), Some("D/child"));
        assert_eq!(changes[1].op, Op::Add);
        assert_eq!(changes[1].path, "moved.txt");

        let base = vec![change("D/child", Op::Add, "moved-content", Mode::Regular)];
        let tree = apply(base, changes).expect("applies");
        assert_eq!(
            tree,
            vec![
                ("D".into(), Mode::Regular, Some(blob_id(b"fileD"))),
                (
                    "moved.txt".into(),
                    Mode::Regular,
                    Some(blob_id(b"moved-content"))
                ),
            ]
        );
    }

    #[test]
    fn consuming_rename_from_outside_adds_explicit_delete() {
        // git -M: the new file at P came from elsewhere (Z), dir P's
        // children were deleted: R P ← Z, D P/f1, D P/f2.
        let mut changes = vec![
            rename("P", "Z", "moved-in", Mode::Regular),
            change("P/f1", Op::Delete, "", Mode::Regular),
            change("P/f2", Op::Delete, "", Mode::Regular),
        ];
        repair_dir_to_file(&mut changes, "deadbeef").expect("repairs");
        // P consumed the smallest delete (P/f1); Z must still be removed.
        assert_eq!(changes.len(), 3);
        let p = changes.iter().find(|c| c.path == "P").expect("P present");
        assert_eq!(p.op, Op::Rename);
        assert_eq!(p.src_path.as_deref(), Some("P/f1"));
        assert!(changes
            .iter()
            .any(|c| c.op == Op::Delete && c.path == "Z"));

        let base = vec![
            change("P/f1", Op::Add, "one", Mode::Regular),
            change("P/f2", Op::Add, "two", Mode::Regular),
            change("Z", Op::Add, "zed", Mode::Regular),
        ];
        let tree = apply(base, changes).expect("applies");
        assert_eq!(
            tree,
            vec![("P".into(), Mode::Regular, Some(blob_id(b"moved-in")))]
        );
    }

    #[test]
    fn file_to_dir_needs_no_repair() {
        // git: D P, A P/a, A P/b — applies fine in path order.
        let mut changes = vec![
            change("P", Op::Delete, "", Mode::Regular),
            change("P/a", Op::Add, "a", Mode::Regular),
            change("P/b", Op::Add, "b", Mode::Regular),
        ];
        let before = changes.clone();
        repair_dir_to_file(&mut changes, "deadbeef").expect("no-op");
        assert_eq!(changes, before);

        let base = vec![change("P", Op::Add, "old", Mode::Regular)];
        let tree = apply(base, changes).expect("applies");
        assert_eq!(
            tree,
            vec![
                ("P/a".into(), Mode::Regular, Some(blob_id(b"a"))),
                ("P/b".into(), Mode::Regular, Some(blob_id(b"b"))),
            ]
        );
    }

    #[test]
    fn nested_dir_replacement() {
        // dir P stays a dir; P/sub (a dir) becomes a file.
        let mut changes = vec![
            change("P/sub", Op::Add, "file", Mode::Regular),
            change("P/sub/c", Op::Delete, "", Mode::Regular),
        ];
        repair_dir_to_file(&mut changes, "deadbeef").expect("repairs");
        assert_eq!(changes[0].op, Op::Rename);
        assert_eq!(changes[0].src_path.as_deref(), Some("P/sub/c"));

        let base = vec![change("P/sub/c", Op::Add, "c", Mode::Regular)];
        let tree = apply(base, changes).expect("applies");
        assert_eq!(tree, vec![("P/sub".into(), Mode::Regular, Some(blob_id(b"file")))]);
    }

    #[test]
    fn unchanged_trees_and_other_dirs_are_untouched() {
        // A plain add into a fresh directory has no under-paths.
        let mut changes = vec![change("new/f", Op::Add, "x", Mode::Regular)];
        let before = changes.clone();
        repair_dir_to_file(&mut changes, "deadbeef").expect("no-op");
        assert_eq!(changes, before);
    }

    #[test]
    fn defensive_error_when_nothing_to_consume() {
        // Git can never produce this shape (a file at P and entries under
        // P/ in the same tree); the repair must refuse loudly rather than
        // emit a changeset the applier would reject.
        let mut changes = vec![
            change("P", Op::Add, "fileP", Mode::Regular),
            change("P/ghost", Op::Add, "x", Mode::Regular),
        ];
        let err = repair_dir_to_file(&mut changes, "deadbeef");
        assert!(err.is_err(), "expected the defensive error");
    }
}

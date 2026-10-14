//! First-parent lineage blame (D7) with rename following and
//! segment-skipping over the path summaries.
//!
//! ## Semantics (D7, anchored to `git blame --first-parent`)
//!
//! Blame attributes each line of `path` at changeset `cs` to the last
//! changeset on the **first-parent chain** where that line's content
//! changed, following renames backwards through `src_path`. Concretely,
//! walking from the queried changeset down the chain:
//!
//! * at a changeset `C` that does not touch the current path, the content
//!   is inherited unchanged (proven by the segment bloom when skipping —
//!   see below);
//! * at a changeset `C` that touches the path, the content is compared
//!   with the parent-side content via git's own line diff (see
//!   [`crate::xdiff`]); diff-*added* lines are attributed to `C`,
//!   diff-*matched* lines are inherited and continue downward with their
//!   parent-side line numbers;
//! * a **rename** `src_path → path` is followed when `path` did not exist
//!   in the parent's tree (the moved-file case git follows): the base
//!   content is the *source path's* content in the parent and the walk
//!   continues there under the old name. When `path` DID exist in the
//!   parent (a rename onto an occupied path — a content replace), the
//!   base is the overwritten path's own content and the walk continues
//!   under the same name, matching git (git only follows a rename when
//!   the path is absent on the parent side);
//! * a path **added** at some chain changeset (or the root) has all its
//!   remaining lines attributed to that changeset.
//!
//! Merge commits are compared against their **first parent** only (their
//! recorded changes), which is exactly `--first-parent`: lines a merge
//! introduced relative to the first parent blame to the merge.
//!
//! The walk carries git's own bookkeeping: an **origin map** from each
//! line of the current version to the line of the queried (final) version
//! it came from. A diff marks new-side lines as added (their final lines
//! attribute to the touching changeset) or unchanged (their origin moves
//! to the paired old-side line, and the walk continues one level down) —
//! the same `xdl_build_script` pairing git's blame uses. Attribution is
//! therefore always recorded against final-version line numbers, however
//! deep the walk descends.
//!
//! Empirically verified against git 2.43: `git blame --first-parent`
//! follows renames by default along the chain (a renamed file's lines
//! blame to commits that touched the old name), which is what
//! `src_path`-following reproduces. Divergences (bounds, gitlink paths)
//! are documented on [`Engine::blame`].
//!
//! ## Segment skipping (the latency win)
//!
//! The first-parent chain is partitioned into segments by the skiplist
//! (roots and merges are heads). For a segment `(head, X]`, the
//! path-summary bloom of `X` covers every path touched by *any* changeset
//! in the segment. If it does **not** contain the current path, then no
//! changeset in the segment can alter that path's blob or existence, so
//! `blob(path@X) == blob(path@head)` is *proven* and the walk jumps
//! straight to the head — one derived-data read per segment instead of
//! one per commit. Bloom false positives (and any other uncertainty)
//! fall back to stepping one parent at a time; a fallback costs time,
//! never correctness. The skip is proven sound by differential tests:
//! skip-enabled blame == naive no-skip blame == git on every test
//! history.
//!
//! ## Bounds and edge cases
//!
//! * Empty files blame to no ranges (git prints nothing).
//! * Gitlink (submodule) paths have no blob content in loam; blame
//!   returns an empty vec (git errors on them — documented divergence).
//! * Symlinks blame their single target line like any other content.
//! * Deleted-then-readded paths blame the current content's lineage (the
//!   walk stops at the add that produced the queried content).
//! * Blobs beyond the diff bounds ([`crate::xdiff::MAX_DIFF_LINES`]
//!   records or the effort cap) fall back to whole-file attribution:
//!   every still-unattributed line of the current version is attributed
//!   to the youngest changeset whose blob differs from its base.

use gardenbed_model::{Id, Mode, Op, TreeKind};
use gardenbed_store::StoreError;

use crate::Engine;
use crate::xdiff;

/// Budget for blame walk steps (changeset visits). The first-parent chain
/// is finite and cycle-free under content addressing; this cap only
/// bounds hostile or corrupt graphs instead of hanging.
const BLAME_MAX_STEPS: u64 = 1 << 26;

/// One coalesced blame range: lines `lo..=hi` (1-based, inclusive) are
/// attributed to changeset `cs`. This is the type `bed blame` and the
/// gRPC `Blame` RPC surface (protocol `BlameRange{lo,hi,cs}`).
pub type BlameRange = (u64, u64, Id);

/// The walk's bookkeeping: for each line of the *current* version (1-based
/// position - 1 indexes this vec), the 0-based line of the queried
/// (final) version it carries — `None` once that final line has been
/// attributed (it must travel no further).
type Origin = Vec<Option<u32>>;

enum Step {
    /// The walk continues at the first parent under the given path.
    Continue(String),
    /// Every remaining line is attributed; the walk is done.
    Done,
}

/// Ascending coalesced blame ranges from the per-final-line interned
/// attributions (slot 0 is impossible at call time).
fn coalesce(attr: &[u32], table: &[Id]) -> Vec<BlameRange> {
    let mut out: Vec<BlameRange> = Vec::new();
    for (i, &slot) in attr.iter().enumerate() {
        let cs = table[(slot - 1) as usize];
        match out.last_mut() {
            Some((_, hi, last)) if *hi as usize == i && *last == cs => *hi = i as u64 + 1,
            _ => out.push((i as u64 + 1, i as u64 + 1, cs)),
        }
    }
    out
}

fn intern(table: &mut Vec<Id>, cs: Id) -> u32 {
    table.push(cs);
    table.len() as u32
}

impl Engine {
    /// First-parent lineage blame of `path` at changeset `cs_id`:
    /// coalesced `(lo, hi, changeset)` ranges, 1-based inclusive,
    /// ascending. See the module docs for the exact semantics.
    ///
    /// Errors with [`StoreError::NotFound`] when the changeset or the
    /// path (at that changeset) does not exist, and with
    /// [`StoreError::Corrupt`] for inconsistent history. A path that is a
    /// directory is "not found", mirroring git. Gitlink paths return an
    /// empty vec (documented divergence: git errors on them).
    pub fn blame(&self, cs_id: Id, path: &str) -> Result<Vec<BlameRange>, StoreError> {
        self.blame_impl(cs_id, path, true)
    }

    /// The naive no-skip walk: identical semantics, but never uses the
    /// segment bloom to jump. The semantic reference for the skip rule —
    /// the test suite asserts both agree on every history.
    pub fn blame_no_skip(&self, cs_id: Id, path: &str) -> Result<Vec<BlameRange>, StoreError> {
        self.blame_impl(cs_id, path, false)
    }

    fn blame_impl(
        &self,
        cs_id: Id,
        path: &str,
        allow_skip: bool,
    ) -> Result<Vec<BlameRange>, StoreError> {
        if path.is_empty() {
            return Err(StoreError::Invalid("path must be non-empty".into()));
        }
        // The queried entry must exist and be a file.
        let root = self.derive_tree(cs_id)?;
        let entry = self.tree_entry(root, path)?.ok_or(StoreError::NotFound)?;
        if entry.kind != TreeKind::File {
            // A directory path: git blame fails on it too.
            return Err(StoreError::NotFound);
        }
        if entry.mode == Mode::Gitlink {
            // Submodule pointers have no blob content in loam; git blames
            // them with a fatal error. Return the documented empty answer.
            return Ok(Vec::new());
        }
        let final_blob = entry
            .blob
            .ok_or_else(|| StoreError::Corrupt(format!("file entry {path:?} without a blob")))?;
        let content = self.objects.get_blob(final_blob)?.ok_or_else(|| {
            StoreError::Corrupt(format!("blob {final_blob} is referenced but missing"))
        })?;
        let records = xdiff::split_records(&content);
        if records.is_empty() {
            return Ok(Vec::new()); // empty file: git prints nothing
        }

        // Interned attributions: `attr[final_line]` = 1-based intern index.
        let mut table: Vec<Id> = Vec::new();
        let mut attr: Vec<u32> = vec![0; records.len()];

        // Every final line starts unattributed.
        let mut origin: Origin = (0..records.len() as u32).map(Some).collect();
        let mut cur_cs = cs_id;
        let mut cur_path = path.to_owned();
        let mut steps: u64 = 0;

        loop {
            steps += 1;
            if steps > BLAME_MAX_STEPS {
                return Err(StoreError::Corrupt(
                    "blame walk exceeded the traversal budget (corrupt graph?)".into(),
                ));
            }
            if origin.iter().all(|o| o.is_none()) {
                break;
            }
            let ps = self.pathsummary_entry(cur_cs)?;
            if ps.is_head {
                // Head (root or merge): its own changes decide. The
                // changeset is read only when the bounded exact list
                // cannot rule a touch out.
                let Some(parent) = ps.first_parent else {
                    // Root changeset: it added everything it has.
                    attribute_remaining(&mut attr, &mut origin, &mut table, cur_cs);
                    break;
                };
                let touches_here = if !ps.truncated {
                    ps.paths.iter().any(|p| p == &cur_path)
                } else {
                    let (cs, _) = self
                        .metadata()
                        .get_changeset(cur_cs)?
                        .ok_or(StoreError::NotFound)?;
                    cs.changes
                        .iter()
                        .any(|c| c.path == cur_path || c.src_path.as_deref() == Some(&cur_path))
                };
                if !touches_here {
                    // Untouched head: content inherited from the parent.
                    cur_cs = parent;
                    continue;
                }
                let (cs, _) = self
                    .metadata()
                    .get_changeset(cur_cs)?
                    .ok_or(StoreError::NotFound)?;
                match self.process_toucher(
                    cur_cs,
                    &parent,
                    &cs,
                    &cur_path,
                    &mut origin,
                    &mut attr,
                    &mut table,
                )? {
                    Step::Continue(next_path) => {
                        cur_cs = parent;
                        cur_path = next_path;
                    }
                    Step::Done => break,
                }
            } else {
                // Member of a linear segment.
                if allow_skip && !ps.bloom_maybe_contains(&cur_path) {
                    // Proven unchanged across the whole segment (head, cur].
                    cur_cs = ps.head.expect("non-head carries a head");
                    continue;
                }
                // Maybe touched: check this changeset's own touches
                // exactly (bounded list, or the changeset when truncated).
                let touched_here = if !ps.truncated {
                    ps.paths.iter().any(|p| p == &cur_path)
                } else {
                    let (cs, _) = self
                        .metadata()
                        .get_changeset(cur_cs)?
                        .ok_or(StoreError::NotFound)?;
                    cs.changes
                        .iter()
                        .any(|c| c.path == cur_path || c.src_path.as_deref() == Some(&cur_path))
                };
                if !touched_here {
                    cur_cs = ps.first_parent.expect("member has a first parent");
                    continue;
                }
                let (cs, _) = self
                    .metadata()
                    .get_changeset(cur_cs)?
                    .ok_or(StoreError::NotFound)?;
                let parent = ps
                    .first_parent
                    .ok_or_else(|| StoreError::Corrupt("member without a first parent".into()))?;
                match self.process_toucher(
                    cur_cs,
                    &parent,
                    &cs,
                    &cur_path,
                    &mut origin,
                    &mut attr,
                    &mut table,
                )? {
                    Step::Continue(next_path) => {
                        cur_cs = parent;
                        cur_path = next_path;
                    }
                    Step::Done => break,
                }
            }
        }

        // Termination guarantees attribution of every line (each step
        // attributes or descends a strictly older changeset).
        for (i, slot) in attr.iter().enumerate() {
            if *slot == 0 {
                return Err(StoreError::Corrupt(format!(
                    "line {} was never attributed (blame walk bug)",
                    i + 1
                )));
            }
        }
        Ok(coalesce(&attr, &table))
    }

    /// Processes `cur_cs` (which touches `cur_path`) against its first
    /// parent: resolves the base content (rename-aware), diffs, attributes
    /// diff-added lines (via their final-version line), and hands the
    /// still-unattributed origins to the parent side.
    // The parameters are the walk's state (who, where, what, bookkeeping);
    // bundling them would obscure the walk itself.
    #[allow(clippy::too_many_arguments)]
    fn process_toucher(
        &self,
        cur_cs: Id,
        parent: &Id,
        cs: &gardenbed_model::Changeset,
        cur_path: &str,
        origin: &mut Origin,
        attr: &mut [u32],
        table: &mut Vec<Id>,
    ) -> Result<Step, StoreError> {
        // The change that produced the current content of cur_path.
        // Prefer the dst change; a rename-away src without a dst would
        // contradict the walk invariant (cur_path exists at cur_cs).
        let change = cs
            .changes
            .iter()
            .find(|c| c.path == cur_path)
            .ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "changeset touches {cur_path:?} only as a rename source"
                ))
            })?;
        let new_blob = change
            .blob
            .ok_or_else(|| StoreError::Corrupt(format!("change on {cur_path:?} without a blob")))?;

        // Resolve the parent-side base content and the path the walk
        // continues under.
        let parent_root = self.derive_tree(*parent)?;
        let (base_blob, continue_path) = match change.op {
            Op::Rename => {
                let src = change.src_path.as_deref().ok_or_else(|| {
                    StoreError::Corrupt(format!("rename onto {cur_path:?} without a src_path"))
                })?;
                match self.tree_entry(parent_root, cur_path)? {
                    // Rename onto an occupied path: git sees a content
                    // change of that path (no rename-follow); the base is
                    // the overwritten content.
                    Some(e) => match e.kind {
                        TreeKind::File => (
                            e.blob.ok_or_else(|| {
                                StoreError::Corrupt(format!(
                                    "file entry {cur_path:?} without a blob"
                                ))
                            })?,
                            cur_path.to_owned(),
                        ),
                        TreeKind::Dir => {
                            return Err(StoreError::Corrupt(format!(
                                "rename source {cur_path:?} is a directory"
                            )));
                        }
                    },
                    None => {
                        // The moved-file case: follow the rename to src.
                        match self.tree_entry(parent_root, src)? {
                            Some(e) => match e.kind {
                                TreeKind::File => (
                                    e.blob.ok_or_else(|| {
                                        StoreError::Corrupt(format!(
                                            "file entry {src:?} without a blob"
                                        ))
                                    })?,
                                    src.to_owned(),
                                ),
                                TreeKind::Dir => {
                                    return Err(StoreError::Corrupt(format!(
                                        "rename source {src:?} is a directory"
                                    )));
                                }
                            },
                            None => {
                                return Err(StoreError::Corrupt(format!(
                                    "rename source {src:?} missing in the first parent's tree"
                                )));
                            }
                        }
                    }
                }
            }
            Op::Add | Op::Modify => match self.tree_entry(parent_root, cur_path)? {
                Some(e) => match e.kind {
                    TreeKind::File => (
                        e.blob.ok_or_else(|| {
                            StoreError::Corrupt(format!("file entry {cur_path:?} without a blob"))
                        })?,
                        cur_path.to_owned(),
                    ),
                    TreeKind::Dir => {
                        return Err(StoreError::Corrupt(format!(
                            "path {cur_path:?} is a directory in the first parent's tree"
                        )));
                    }
                },
                // Added here: all remaining lines attribute to cur_cs.
                None => {
                    attribute_remaining(attr, origin, table, cur_cs);
                    return Ok(Step::Done);
                }
            },
            Op::Delete => {
                // Impossible as the first toucher below a live path (a
                // re-add would be a younger toucher). Defensive error.
                return Err(StoreError::Corrupt(format!(
                    "delete of {cur_path:?} contradicts the walk invariant"
                )));
            }
        };

        if base_blob == new_blob {
            // Content identical (pure rename and/or mode-only change):
            // every origin is inherited 1:1.
            return Ok(Step::Continue(continue_path));
        }

        let base_content = self.objects.get_blob(base_blob)?.ok_or_else(|| {
            StoreError::Corrupt(format!("blob {base_blob} is referenced but missing"))
        })?;
        let new_content = self.objects.get_blob(new_blob)?.ok_or_else(|| {
            StoreError::Corrupt(format!("blob {new_blob} is referenced but missing"))
        })?;
        let old_recs = xdiff::split_records(&base_content);
        let new_recs = xdiff::split_records(&new_content);

        let mut next: Origin = vec![None; old_recs.len()];
        match xdiff::diff_lines(&old_recs, &new_recs) {
            Some(d) => {
                // Unchanged old and new lines pair up in order (git's
                // `xdl_build_script` invariant): the k-th unchanged new
                // line pairs with the k-th unchanged old line. Diff-added
                // new lines attribute here — through their final-version
                // origin — and travel no further.
                let n_old = old_recs.len();
                let slot = intern(table, cur_cs);
                let mut oi = 0usize;
                for (ni, changed) in d.changed_new.iter().enumerate() {
                    if *changed {
                        if let Some(fl) = origin[ni] {
                            attr[fl as usize] = slot;
                        }
                        continue;
                    }
                    // Find the next unchanged old line: the paired base.
                    while oi < n_old && d.changed_old[oi] {
                        oi += 1;
                    }
                    if oi >= n_old {
                        // Unchanged counts are equal by construction; a
                        // port bug must not mis-attribute silently.
                        return Err(StoreError::Corrupt(
                            "diff pairing invariant violated (unchanged count mismatch)".into(),
                        ));
                    }
                    next[oi] = origin[ni];
                    oi += 1;
                }
            }
            // Past the documented diff bounds: whole-file attribution.
            None => {
                let slot = intern(table, cur_cs);
                for fl in origin.iter().flatten() {
                    attr[*fl as usize] = slot;
                }
            }
        }

        *origin = next;
        if origin.iter().all(|o| o.is_none()) {
            return Ok(Step::Done);
        }
        Ok(Step::Continue(continue_path))
    }
}

/// Attributes every still-travelling line (those with an origin) to `cs`.
fn attribute_remaining(attr: &mut [u32], origin: &mut Origin, table: &mut Vec<Id>, cs: Id) {
    let slot = intern(table, cs);
    for fl in origin.iter_mut() {
        if let Some(fl) = fl.take() {
            attr[fl as usize] = slot;
        }
    }
}

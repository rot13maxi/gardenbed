//! 3-way merge: per-file line-level merge3 and the recursive tree walk
//! (v0.2 §4, D9–D11).
//!
//! ## Line level (`merge3`)
//!
//! [`merge3`] is a port of git's `xdl_merge` (the `xdl_do_merge` half of
//! xmerge.c, as of git 2.43): both sides are diffed against the base with
//! the existing xdiff port, giving two change scripts in *base* line
//! coordinates; the scripts are then walked together —
//!
//! * a change of side 1 entirely before side 2's next change (and vice
//!   versa) is taken as-is: non-overlapping regions auto-combine in either
//!   order, exactly git's rule;
//! * overlapping regions whose postimages are byte-identical coalesce into
//!   one *identical* region (both sides made the same change);
//! * anything else is a conflict.
//!
//! The postimage of a taken side-1 region is that side's lines; the merged
//! output interleaves untouched base lines (carried from side 1's
//! postimage, since side 1 = ours) with each region's chosen postimage.
//! Overlap detection is in base coordinates, matching git: `chg1`/`chg2`
//! are lengths in the *base* of each side's change (git's "both sides
//! replaced these base lines with the same number of lines" bookkeeping
//! from `xdl_do_merge`'s one-side-only branches).
//!
//! Levels: git's default is `XDL_MERGE_ZEALOUS`, which refines conflicts
//! by diffing the two conflicting postimages against each other and
//! simplifies non-conflicts separated by ≤ 3 lines. Both steps are ported
//! ([`refine_conflicts`], [`simplify_non_conflicts`]) because the
//! differential oracle (M9.1) compares against `git merge-tree`, which
//! runs zealous — omitting them would diverge on real edit patterns.
//!
//! Binary contents (a NUL byte before the first LF) never line-merge:
//! git treats them as atomic, so both-sides-changed binary is a conflict
//! and one-side-changed binary is the trivial take-the-changed-side rule
//! (which the tree walk resolves before merge3 is consulted).
//!
//! ## Tree level
//!
//! The recursive walk over `(base, ours, theirs)` trees applies §4's
//! rules in order: content-addressed equality pruning, trivial rules
//! (two sides equal → that value; one side == base → the other), merge3
//! on regular/exec leaves that differ on both sides, the mode merge
//! table, atomic symlinks/gitlinks, and the applier's directory conflict
//! rules for file↔dir and delete-vs-modify. The merge is **rename-blind**
//! (documented divergence): rename/rename surfaces as add/add,
//! rename/delete as delete/modify.
//!
//! ## Determinism
//!
//! Everything is a pure function of `(base, ours, theirs)` tree contents:
//! trees and blobs are content-addressed, maps are `BTree*`, no clocks.
//! The same triple yields byte-identical merged trees and therefore an
//! identical changeset id (D9: concurrent identical merges are benign).

use std::collections::BTreeMap;

use gardenbed_model::{Change, Changeset, Id, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

use crate::apply::MAX_TREE_DEPTH;
use crate::merge3::{Merge3Outcome, merge3};
use crate::xdiff::MAX_DIFF_LINES;

/// One conflicting path: the full path and why it conflicted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
    /// Full path from the tree root, `/`-separated.
    pub path: String,
    /// Human-readable reason (which rule fired).
    pub reason: ConflictReason,
}

/// Why a path conflicted (the classification the caller reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// Both sides changed overlapping line regions (or binary contents).
    Content,
    /// Both sides changed the mode differently.
    Mode,
    /// Both sides changed content and the mode differently.
    ContentAndMode,
    /// One side deleted, the other modified (hard conflict per D11).
    DeleteModify,
    /// Both sides added different content/mode at the same new path.
    AddAdd,
    /// The path is a file on one side and a directory on the other, with
    /// both sides changed (or the side that kept it a file also changed
    /// content under the directory's path).
    FileDir,
    /// Both sides changed a symlink/gitlink (atomic: id equality only).
    Link,
    /// Content merge would exceed the xdiff line bound.
    TooLarge,
}

impl ConflictReason {
    /// Short human phrase used in the conflict listing.
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::Content => "content",
            ConflictReason::Mode => "mode",
            ConflictReason::ContentAndMode => "content+mode",
            ConflictReason::DeleteModify => "delete/modify",
            ConflictReason::AddAdd => "add/add",
            ConflictReason::FileDir => "file/directory",
            ConflictReason::Link => "symlink/gitlink",
            ConflictReason::TooLarge => "too large to merge",
        }
    }
}

/// The result of a 3-way tree merge (against the "ours" side).
#[derive(Debug)]
pub enum TreeMergeOutcome {
    /// Clean merge: the change list vs `ours` that produces the merged
    /// tree (apply(changes, tree(ours)) == merged tree), plus the merged
    /// root tree id itself.
    Merged {
        /// Diff `tree(ours) → merged_tree`, canonical order.
        changes: Vec<Change>,
        /// The merged root tree id (`None` = empty tree).
        tree: Option<Id>,
    },
    /// Conflict: every conflicting path, collected before returning (the
    /// rebase contract — nothing partial is ever stored).
    Conflict(Vec<MergeConflict>),
}

impl TreeMergeOutcome {
    /// The change list vs ours, or `None` on conflict.
    pub fn changes(&self) -> Option<&[Change]> {
        match self {
            TreeMergeOutcome::Merged { changes, .. } => Some(changes),
            TreeMergeOutcome::Conflict(_) => None,
        }
    }
}

/// One side's file entry in the tree walk: blob id + mode.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Side {
    blob: Id,
    mode: Mode,
}

/// The outcome of merging one path's leaf entry.
enum LeafOutcome {
    /// Take this entry as-is.
    Take(Side),
    /// Delete the path.
    Delete,
    /// Conflict with this reason.
    Conflict(ConflictReason),
    /// Content-merge: use these merged bytes with this mode.
    Merged { content: Vec<u8>, mode: Mode },
}

/// A slot in the merged directory being built: a resolved file, a resolved
/// subdirectory, or a pending conflict marker.
enum MergedSlot {
    File { blob: Id, mode: Mode },
    Dir { tree: Option<Id> },
}

/// Merges the trees of `(base, ours, theirs)` and returns the outcome
/// against ours. Pure: reads trees/blobs, writes nothing.
///
/// `base == None` is the empty tree (an add/add everywhere).
pub(crate) fn merge_trees(
    objects: &dyn ObjectStore,
    base: Option<Id>,
    ours: Option<Id>,
    theirs: Option<Id>,
) -> Result<TreeMergeOutcome, StoreError> {
    let mut ctx = TreeMerge::new(objects);
    let mut conflicts: Vec<MergeConflict> = Vec::new();
    match ctx.merge_dir(base, ours, theirs, "", 0, &mut conflicts)? {
        DirOutcome::Tree(tree) => {
            let changes = crate::diff::diff_trees(objects, ours, tree)?;
            Ok(TreeMergeOutcome::Merged { changes, tree })
        }
        // Conflicts are collected in `conflicts`; an empty merged tree
        // with no conflicts is a legitimate result (both sides deleted
        // everything, or ours was empty and theirs deleted).
        DirOutcome::Conflict => Ok(TreeMergeOutcome::Conflict(conflicts)),
    }
}

/// The result of merging one directory level.
#[derive(Debug)]
enum DirOutcome {
    /// The merged sub-tree id (`None` = empty).
    Tree(Option<Id>),
    /// A conflict fired under this directory.
    Conflict,
}

struct TreeMerge<'a> {
    objects: &'a dyn ObjectStore,
}

impl<'a> TreeMerge<'a> {
    fn new(objects: &'a dyn ObjectStore) -> Self {
        TreeMerge { objects }
    }

    /// Loads a tree, or the empty tree for `None`.
    fn tree(&self, id: Option<Id>) -> Result<Tree, StoreError> {
        match id {
            None => Ok(Tree::default()),
            Some(id) => crate::Engine::load_tree(self.objects, id),
        }
    }

    fn blob(&self, id: Id) -> Result<Vec<u8>, StoreError> {
        match self.objects.get_blob(id)? {
            Some(b) => Ok(b),
            None => Err(StoreError::Corrupt(format!(
                "blob {id} is referenced but missing from the object store"
            ))),
        }
    }

    /// Merges one directory level: a three-way merge join over the
    /// entries of `(base, ours, theirs)` at `dir`, recursing into
    /// directories both sides may have touched.
    #[allow(clippy::too_many_arguments)]
    fn merge_dir(
        &mut self,
        base: Option<Id>,
        ours: Option<Id>,
        theirs: Option<Id>,
        dir: &str,
        depth: usize,
        conflicts: &mut Vec<MergeConflict>,
    ) -> Result<DirOutcome, StoreError> {
        if depth > MAX_TREE_DEPTH {
            return Err(StoreError::Invalid(
                "tree nesting exceeds the maximum supported depth".into(),
            ));
        }
        let tb = self.tree(base)?;
        let to = self.tree(ours)?;
        let tt = self.tree(theirs)?;
        // Content-addressed pruning at the directory level: ours and
        // theirs share the subtree id ⇒ nothing to decide.
        if ours == theirs {
            return Ok(DirOutcome::Tree(ours));
        }
        let mut merged: BTreeMap<String, MergedSlot> = BTreeMap::new();
        let mut conflicted = false;
        // Three-way join over the union of entry names. Entries are sorted
        // by name bytes in a canonical tree; pairing is linear.
        let names = union_names(&tb, &to, &tt);
        for name in names {
            let eb = entry_of(&tb, &name);
            let eo = entry_of(&to, &name);
            let et = entry_of(&tt, &name);
            let path = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            match self.merge_entry(eb, eo, et, &path, depth, conflicts)? {
                EntryOutcome::Resolved(slot) => {
                    merged.insert(name, slot);
                }
                EntryOutcome::Missing => {}
                EntryOutcome::Conflicted => {
                    conflicted = true;
                }
            }
        }
        if conflicted {
            return Ok(DirOutcome::Conflict);
        }
        // Serialize: entries in canonical (byte-ascending) name order.
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(merged.len());
        for (name, slot) in merged {
            match slot {
                MergedSlot::File { blob, mode } => entries.push(TreeEntry {
                    name,
                    kind: TreeKind::File,
                    mode,
                    blob: Some(blob),
                    tree: None,
                }),
                MergedSlot::Dir { tree } => match tree {
                    // An empty sub-tree vanishes (the applier's rule).
                    None => {}
                    Some(tree) => entries.push(TreeEntry {
                        name,
                        kind: TreeKind::Dir,
                        mode: Mode::Regular,
                        blob: None,
                        tree: Some(tree),
                    }),
                },
            }
        }
        entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let tree = Tree { entries };
        let id = if tree.entries.is_empty() {
            None
        } else {
            let bytes = tree
                .canonical_bytes()
                .map_err(crate::apply::codec_err)?;
            let id = tree.id().map_err(crate::apply::codec_err)?;
            self.objects.put_tree(id, &bytes)?;
            Some(id)
        };
        Ok(DirOutcome::Tree(id))
    }

    /// Merges one entry name across the three sides. A file on some side
    /// and a directory on another is handled here; the deep rules run in
    /// [`TreeMerge::merge_leaf`].
    fn merge_entry(
        &mut self,
        eb: Option<&TreeEntry>,
        eo: Option<&TreeEntry>,
        et: Option<&TreeEntry>,
        path: &str,
        depth: usize,
        conflicts: &mut Vec<MergeConflict>,
    ) -> Result<EntryOutcome, StoreError> {
        // Trivial pruning first (cheap, content-addressed): any two sides
        // equal ⇒ answer follows.
        if eo == et {
            // Both non-ours sides agree: that value (or both deleted).
            return Ok(match eo {
                Some(e) => resolved_file(e),
                None => EntryOutcome::Missing,
            });
        }
        if eb == eo {
            // Ours is unchanged from base: take theirs (including delete).
            return Ok(match et {
                Some(e) => resolved_file(e),
                None => EntryOutcome::Missing,
            });
        }
        if eb == et {
            // Theirs is unchanged from base: keep ours (including delete).
            return Ok(match eo {
                Some(e) => resolved_file(e),
                None => EntryOutcome::Missing,
            });
        }
        // All three pairwise distinct from here on... except the case
        // where the two differing sides are (file, dir) shapes.
        let kb = eb.map(kind_of);
        let ko = eo.map(kind_of);
        let kt = et.map(kind_of);
        // Kind of the merged entry: when a side is missing, the kind is
        // whatever remains; when all present, shapes must be reconciled.
        let ours_file = eo.map(|e| e.kind == TreeKind::File);
        let theirs_file = et.map(|e| e.kind == TreeKind::File);
        let base_file = eb.map(|e| e.kind == TreeKind::File);
        match (ko, kt) {
            // Both sides deleted: handled by the eo==et rule above.
            (None, None) => Ok(EntryOutcome::Missing),
            // Ours deleted, theirs modified: delete/modify conflict.
            (None, Some(_)) => {
                // theirs != base (they changed it) and ours deleted it.
                conflicts.push(MergeConflict {
                    path: path.to_owned(),
                    reason: ConflictReason::DeleteModify,
                });
                Ok(EntryOutcome::Conflicted)
            }
            // Ours modified, theirs deleted: delete/modify conflict.
            (Some(_), None) => {
                conflicts.push(MergeConflict {
                    path: path.to_owned(),
                    reason: ConflictReason::DeleteModify,
                });
                Ok(EntryOutcome::Conflicted)
            }
            (Some(_), Some(_)) => {
                // Both sides have an entry: file/dir shapes decide.
                let _ = (kb, base_file, ours_file, theirs_file);
                match (eo.unwrap().kind, et.unwrap().kind) {
                    (TreeKind::File, TreeKind::File) => {
                        self.merge_leaf(eb, eo.unwrap(), et.unwrap(), path, conflicts)
                    }
                    (TreeKind::Dir, TreeKind::Dir) => {
                        let merged_sub = self.merge_dir(
                            eb.and_then(|e| e.tree),
                            eo.unwrap().tree,
                            et.unwrap().tree,
                            path,
                            depth + 1,
                            conflicts,
                        )?;
                        Ok(match merged_sub {
                            DirOutcome::Tree(t) => EntryOutcome::Resolved(MergedSlot::Dir {
                                tree: t,
                            }),
                            DirOutcome::Conflict => EntryOutcome::Conflicted,
                        })
                    }
                    // file↔dir across ours/theirs: conflict (both sides
                    // changed the path's shape).
                    _ => {
                        conflicts.push(MergeConflict {
                            path: path.to_owned(),
                            reason: ConflictReason::FileDir,
                        });
                        Ok(EntryOutcome::Conflicted)
                    }
                }
            }
        }
    }

    /// Merges a file leaf that differs on both sides: mode merge table,
    /// atomic kinds, then merge3 on contents. `eb` may be `None` (add/add).
    fn merge_leaf(
        &mut self,
        eb: Option<&TreeEntry>,
        eo: &TreeEntry,
        et: &TreeEntry,
        path: &str,
        conflicts: &mut Vec<MergeConflict>,
    ) -> Result<EntryOutcome, StoreError> {
        let so = Side {
            blob: eo.blob.expect("file entry carries a blob"),
            mode: eo.mode,
        };
        let st = Side {
            blob: et.blob.expect("file entry carries a blob"),
            mode: et.mode,
        };
        let sb = eb.map(|e| Side {
            blob: e.blob.expect("file entry carries a blob"),
            mode: e.mode,
        });
        // Content-equality pruning: blob ids are content addresses.
        if so.blob == st.blob {
            // Same content: only the mode can differ; the mode table
            // decides (a single-side exec flip wins; both-flip-differently
            // conflicts).
            return match merge_modes(sb, so.mode, st.mode) {
                Some(mode) => Ok(EntryOutcome::Resolved(MergedSlot::File {
                    blob: so.blob,
                    mode,
                })),
                None => {
                    conflicts.push(MergeConflict {
                        path: path.to_owned(),
                        reason: ConflictReason::Mode,
                    });
                    Ok(EntryOutcome::Conflicted)
                }
            };
        }
        // Atomic kinds: symlinks and gitlinks merge by id equality only.
        // A regular ↔ symlink flip is itself a mode-class change, so it
        // lands here too (both sides changed shape differently).
        if so.mode != st.mode && !compatible_modes(so.mode, st.mode) {
            // Mode-class conflict (regular↔symlink, regular↔gitlink, …):
            // if one side just kept the base's class, take the other; a
            // genuine class conflict needs both sides' classes examined.
            return match merge_mode_classes(sb, so, st) {
                Some(side) => Ok(EntryOutcome::Resolved(MergedSlot::File {
                    blob: side.blob,
                    mode: side.mode,
                })),
                None => {
                    conflicts.push(MergeConflict {
                        path: path.to_owned(),
                        reason: ConflictReason::Mode,
                    });
                    Ok(EntryOutcome::Conflicted)
                }
            };
        }
        // Same mode class, different blobs: content merge.
        let mode = merge_modes(sb, so.mode, st.mode);
        let base_content = match sb {
            Some(sb) => Some(self.blob(sb.blob)?),
            None => None, // add/add: empty base content
        };
        let ours_content = self.blob(so.blob)?;
        let theirs_content = self.blob(st.blob)?;
        let (mode, reason_mode) = match mode {
            Some(m) => (m, false),
            None => (so.mode, true),
        };
        // Binary contents never line-merge (git's rule): conflict when
        // both sides changed them differently (same-id equality was
        // pruned above).
        if is_binary(&ours_content) || is_binary(&theirs_content)
            || base_content.as_deref().is_some_and(is_binary)
        {
            conflicts.push(MergeConflict {
                path: path.to_owned(),
                reason: reason_reason(true, reason_mode),
            });
            return Ok(EntryOutcome::Conflicted);
        }
        match merge3(
            base_content.as_deref().unwrap_or(&[]),
            &ours_content,
            &theirs_content,
        ) {
            Merge3Outcome::Merged(content) => {
                if reason_mode {
                    conflicts.push(MergeConflict {
                        path: path.to_owned(),
                        reason: ConflictReason::Mode,
                    });
                    return Ok(EntryOutcome::Conflicted);
                }
                Ok(EntryOutcome::Resolved(match put_blob(self.objects, &content)? {
                    blob => MergedSlot::File { blob, mode },
                }))
            }
            Merge3Outcome::Conflict(_) => {
                conflicts.push(MergeConflict {
                    path: path.to_owned(),
                    reason: reason_reason(true, reason_mode),
                });
                Ok(EntryOutcome::Conflicted)
            }
            Merge3Outcome::TooLarge => {
                conflicts.push(MergeConflict {
                    path: path.to_owned(),
                    reason: if reason_mode {
                        ConflictReason::ContentAndMode
                    } else {
                        ConflictReason::TooLarge
                    },
                });
                Ok(EntryOutcome::Conflicted)
            }
        }
    }
}

/// Reason synthesis for content/mode combos.
fn reason_reason(content: bool, mode: bool) -> ConflictReason {
    match (content, mode) {
        (true, false) => ConflictReason::Content,
        (false, true) => ConflictReason::Mode,
        (true, true) => ConflictReason::ContentAndMode,
        (false, false) => ConflictReason::Content,
    }
}

enum EntryOutcome {
    /// A resolved slot for the merged directory.
    Resolved(MergedSlot),
    /// The path is absent in the merge result.
    Missing,
    /// A conflict was recorded for this path.
    Conflicted,
}

/// Wraps a resolved file entry into a slot.
fn resolved_file(e: &TreeEntry) -> EntryOutcome {
    EntryOutcome::Resolved(MergedSlot::File {
        blob: e.blob.expect("file entry carries a blob"),
        mode: e.mode,
    })
}

/// The union of entry names across the three trees (canonical order is
/// preserved: names are collected in byte-ascending order).
fn union_names(tb: &Tree, to: &Tree, tt: &Tree) -> Vec<String> {
    let mut names: Vec<String> = Vec::with_capacity(to.entries.len() + tt.entries.len());
    for t in [tb, to, tt] {
        for e in &t.entries {
            names.push(e.name.clone());
        }
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names.dedup();
    names
}

fn entry_of<'e>(t: &'e Tree, name: &str) -> Option<&'e TreeEntry> {
    t.entries.iter().find(|e| e.name == name)
}

fn kind_of(e: &TreeEntry) -> TreeKind {
    e.kind
}

/// True when two modes can hold the same content shape for merging
/// purposes: the regular/exec pair (git's 100644/100755 class), or equal
/// modes. Symlinks, gitlinks, and cross-class pairs are atomic.
fn compatible_modes(a: Mode, b: Mode) -> bool {
    matches!(
        (a, b),
        (Mode::Regular, Mode::Regular)
            | (Mode::Exec, Mode::Exec)
            | (Mode::Regular, Mode::Exec)
            | (Mode::Exec, Mode::Regular)
    )
}

/// The mode merge table (§4.3): single-side exec-bit change wins; both
/// sides changing mode differently conflicts.
fn merge_modes(base: Option<Side>, ours: Mode, theirs: Mode) -> Option<Mode> {
    if ours == theirs {
        return Some(ours);
    }
    let bm = base.map(|s| s.mode);
    match bm {
        Some(b) if b == ours => Some(theirs),
        Some(b) if b == theirs => Some(ours),
        _ => None, // both changed mode differently
    }
}

/// Mode-class conflict resolution: when the two sides' mode *classes*
/// differ, one side keeping base's class means the other side's flip wins.
fn merge_mode_classes(base: Option<Side>, ours: Side, theirs: Side) -> Option<Side> {
    let bo = base.map(|s| s.mode);
    match bo {
        Some(b) if b == ours.mode => Some(theirs),
        Some(b) if b == theirs.mode => Some(ours),
        _ => None,
    }
}

/// git's `buffer_is_binary` (a NUL byte before the first newline).
fn is_binary(content: &[u8]) -> bool {
    match content.iter().position(|&b| b == b'\n') {
        Some(nl) => content[..nl].contains(&0u8),
        None => content.contains(&0u8),
    }
}

/// Writes a merged blob and returns its content address.
fn put_blob(objects: &dyn ObjectStore, content: &[u8]) -> Result<Id, StoreError> {
    objects.put_blob(content)
}

/// `true` when either content side exceeds the xdiff line bound (the
/// caller conflicts rather than mis-merging; M9.5).
pub(crate) fn exceeds_diff_bound(content: &[u8]) -> bool {
    content.len() > MAX_DIFF_LINES.saturating_mul(MAX_LINE_BYTES)
}

/// Longest line we still split (guards the saturating bound above).
const MAX_LINE_BYTES: usize = 1 << 20;

// ============================================================ engine API

impl crate::Engine {
    /// Merges the trees of `(base, ours, theirs)` changeset ids and
    /// returns the outcome against ours, without consulting or writing
    /// any derived cache. `base == None` (the empty tree) is the
    /// add/add-everywhere shape.
    pub fn merge_tree_changes(
        &self,
        base: Option<Id>,
        ours: Id,
        theirs: Id,
    ) -> Result<TreeMergeOutcome, StoreError> {
        let t_ours = self.derive_tree(ours)?;
        let t_theirs = self.derive_tree(theirs)?;
        merge_trees(&*self.objects, base, t_ours, t_theirs)
    }

    /// The 3-way merge of changesets `ours` and `theirs` (D9–D11).
    ///
    /// The merge base is [`Engine::merge_base`]; unrelated histories
    /// (no common ancestor) fail with [`StoreError::Invalid`] naming the
    /// operation and both ids. The merged changeset carries
    /// `parents = [ours, theirs]` and `changes = diff(tree(ours) →
    /// merged_tree)`, so the unchanged first-parent derivation rule
    /// reproduces the merge.
    ///
    /// Message and identities are supplied by the caller — this method
    /// fills them with placeholders (empty strings, zero timestamps) so
    /// the changeset's *content* (tree + changes + parents) is a pure
    /// function of the triple; the caller layers real message/identities
    /// on top at integration (M10) and recomputes the id.
    ///
    /// Nothing is written to the store's changeset tables: trees and
    /// merged blobs land in the object store (content-addressed,
    /// unreferenced until the caller commits — harmless by D11), but no
    /// changeset row, derived row, or certificate is created.
    pub fn merge_changesets(
        &self,
        ours: Id,
        theirs: Id,
    ) -> Result<MergeOutcome, StoreError> {
        let base = self
            .merge_base(ours, theirs)?
            .ok_or_else(|| {
                StoreError::Invalid(format!(
                    "merge of {ours} and {theirs}: unrelated histories (no merge base)"
                ))
            })?;
        let outcome = self.merge_tree_changes(Some(base), ours, theirs)?;
        match outcome {
            TreeMergeOutcome::Merged { changes, tree } => {
                let cs = Changeset {
                    parents: vec![ours, theirs],
                    changes,
                    message: String::new(),
                    author: placeholder_identity(),
                    committer: placeholder_identity(),
                    signature: None,
                };
                Ok(MergeOutcome::Merged {
                    changeset: cs,
                    tree,
                })
            }
            TreeMergeOutcome::Conflict(list) => Ok(MergeOutcome::Conflict(list)),
        }
    }
}

/// Zeroed identity: content-neutral, so merge output depends only on the
/// triple (determinism, M9.4); integration fills real identities.
fn placeholder_identity() -> gardenbed_model::Identity {
    gardenbed_model::Identity {
        name: String::new(),
        email: String::new(),
        time_secs: 0,
        tz_min: 0,
    }
}

/// The result of [`Engine::merge_changesets`].
pub enum MergeOutcome {
    /// Clean merge: the merged changeset (parents `[ours, theirs]`,
    /// changes vs ours, placeholder identities/message for the caller to
    /// overwrite) and the merged root tree id.
    Merged {
        /// A changeset missing only message/identities.
        changeset: Changeset,
        /// The merged root tree (`None` = empty).
        tree: Option<Id>,
    },
    /// Every conflicting path, collected before returning.
    Conflict(Vec<MergeConflict>),
}

impl MergeOutcome {
    /// The merged changeset, or `None` on conflict.
    pub fn changeset(&self) -> Option<&Changeset> {
        match self {
            MergeOutcome::Merged { changeset, .. } => Some(changeset),
            MergeOutcome::Conflict(_) => None,
        }
    }

    /// The conflict list, or `None` when clean.
    pub fn conflicts(&self) -> Option<&[MergeConflict]> {
        match self {
            MergeOutcome::Merged { .. } => None,
            MergeOutcome::Conflict(list) => Some(list),
        }
    }
}

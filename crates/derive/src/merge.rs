//! Three-way merge (v0.2 §3, decisions D9–D11, milestone M9).
//!
//! Two layers, both pure computations over the engine's store handles —
//! nothing in this module writes blobs, trees, or changesets; storage is
//! the server's job at integration time:
//!
//! - [`merge3`]: git-faithful line-level 3-way text merge, a port of
//!   xdiff's `xdl_merge` at `XDL_MERGE_ZEALOUS` with no favor and no
//!   marker style (merge-ort's `ll_xdl_merge` defaults, which is what
//!   `git merge-tree --write-tree` uses). Non-overlapping base→ours and
//!   base→theirs change regions auto-combine; overlapping regions with
//!   identical outcomes coalesce; everything else is a conflict.
//!
//! - [`Engine::merge_changesets`]: the 3-way tree walk over
//!   `(merge_base, ours, theirs)`, emitting the merged result as
//!   *changes relative to ours* plus the raw bytes of any newly merged
//!   blob (content-addressed by [`blob_id`], staged by the caller).
//!   Applying the returned changes to `tree(ours)` reproduces the merged
//!   tree exactly — the unchanged-first-parent derivation rule holds, so
//!   a committed merge is reproducible from its own changeset.
//!
//! Semantics deliberately chosen in v0.2 (documented divergences from
//! git's merge-ort, §12):
//!
//! - **Rename-blindness.** The walk pairs entries by name only. A
//!   rename surfaces as delete+add, so rename/rename becomes add/add and
//!   rename/delete becomes delete/modify. No rename detection, ever.
//! - **Hard conflicts.** Modify/delete is always a conflict (no
//!   "keep the modified version and continue" resolution); D/F conflicts
//!   are conflicts, not git's `path~branch` rearrangements. The merged
//!   tree of a conflicted merge is therefore *ours* — conflict paths are
//!   reported, not staged.
//! - **Binary and oversized inputs.** git's `ll_xdl_merge` routes any
//!   input containing a NUL in its first 8000 bytes (or exceeding 1 GiB)
//!   to the binary driver, which without a favor is a conflict. We
//!   match that classification: both-sides-changed binary content
//!   conflicts with ours' bytes as the reported postimage.
//! - **Histories are gitlike, not identical**: merge-ort diffs with the
//!   histogram algorithm while our xdiff port is Myers-with-indent-
//!   heuristic; on ambiguous inputs hunk placement (and thus
//!   clean-vs-conflict on knife-edge overlaps) can differ. The
//!   differential suite quantifies this; see `tests/merge_oracle.rs`.

use std::ops::Range;

use gardenbed_model::{blob_id, Change, Id, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

use crate::Engine;
use crate::apply::MAX_TREE_DEPTH;
use crate::diff;
use crate::xdiff;

// ================================================================ merge3

/// The line-level 3-way merge outcome.
///
/// `Merged` carries the merged bytes (a pure interleaving of ours' and
/// theirs' records — with no favor and no marker style, xdiff emits no
/// synthetic bytes). `Conflict` carries the conflicting regions as
/// base/ours/theirs *record* ranges (record = a line including its
/// trailing `\n`; the last record may lack one), for reporting and for
/// any future conflict-marker materialization the server chooses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3Outcome {
    /// Clean merge: the merged bytes.
    Merged(Vec<u8>),
    /// Conflicting regions, in file order. Empty only for the
    /// "same content on both changed sides" degenerate, which merges
    /// clean by construction (see [`merge3`]).
    Conflict(Vec<Merge3Conflict>),
}

/// One conflicting region, in record coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge3Conflict {
    /// The base preimage range.
    pub base: Range<usize>,
    /// Our postimage range.
    pub ours: Range<usize>,
    /// Their postimage range.
    pub theirs: Range<usize>,
}

/// git's `FIRST_FEW_BYTES`: how much of a buffer `buffer_is_binary`
/// scans for a NUL before declaring the file binary.
const FIRST_FEW_BYTES: usize = 8000;

/// git's `MAX_XDIFF_SIZE`: inputs larger than this skip xdiff for the
/// binary driver (in ll_xdl_merge, merge-ll.c).
const MAX_XDIFF_SIZE: usize = 1024 * 1024 * 1024;

/// git's `buffer_is_binary`: a NUL byte within the first 8 KiB.
fn is_binary(content: &[u8]) -> bool {
    let scan = &content[..content.len().min(FIRST_FEW_BYTES)];
    scan.contains(&0)
}

/// One change script hunk: `i1..i1+chg1` of the base lines is replaced
/// by `i2..i2+chg2` of the other side's lines (xmerge's `xdchange_t`;
/// here side 1 = base, side 2 = ours or theirs). Runs are built by
/// pairing the k-th contiguous changed-old run with the k-th changed-new
/// run of a [`xdiff::LineDiff`] — exactly what `xdl_build_script` does
/// over xdiff's changed flags.
#[derive(Clone, Copy, Debug)]
struct Hunk {
    i1: usize,
    chg1: usize,
    i2: usize,
    chg2: usize,
}

/// Builds the change script of `a → b` from a line diff. Returns `None`
/// when the diff hit its documented bounds (caller: conflict).
fn hunks(a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<Hunk>> {
    let d = xdiff::diff_lines(a, b)?;
    let mut out = Vec::new();
    // Pair changed runs positionally, the way xdiff's change script
    // does: a pure insertion has an empty old run, a pure deletion an
    // empty new run, and both belong to the same hunk at that base
    // position. Unchanged records are the matched LCS records — equal
    // in count and content between hunks — so walking them in lockstep
    // keeps the two sides aligned; whenever either side sits on a
    // changed run (or is exhausted, which makes its run empty), one
    // hunk consumes both sides' changed runs from the current
    // positions.
    let (mut i, mut j) = (0usize, 0usize);
    while i < d.changed_old.len() || j < d.changed_new.len() {
        if i < d.changed_old.len()
            && j < d.changed_new.len()
            && !d.changed_old[i]
            && !d.changed_new[j]
        {
            i += 1;
            j += 1;
            continue;
        }
        let start_old = i;
        let start_new = j;
        while i < d.changed_old.len() && d.changed_old[i] {
            i += 1;
        }
        while j < d.changed_new.len() && d.changed_new[j] {
            j += 1;
        }
        out.push(Hunk {
            i1: start_old,
            chg1: i - start_old,
            i2: start_new,
            chg2: j - start_new,
        });
    }
    Some(out)
}

/// A merge region (xmerge's `xdmerge_t`). `mode` 0 = conflict, 1 = take
/// ours, 2 = take theirs, 3 = take both (only produced by favor
/// combinations we never use, kept for the C port's fidelity), 4 =
/// conflict refined to "both sides changed identically" (treated as
/// take-ours during output: the postimages are equal).
#[derive(Clone, Copy, Debug)]
struct Region {
    mode: u8,
    /// Base preimage range.
    i0: usize,
    chg0: usize,
    /// Our postimage range.
    i1: usize,
    chg1: usize,
    /// Their postimage range.
    i2: usize,
    chg2: usize,
}

/// git's `xdl_append_merge`: coalesce a region into the previous one
/// when either postimage runs touch or overlap; a mode mismatch turns
/// the coalesced region into a conflict.
fn append_merge(changes: &mut Vec<Region>, r: Region) {
    if let Some(m) = changes.last_mut() {
        if r.i1 <= m.i1 + m.chg1 || r.i2 <= m.i2 + m.chg2 {
            if r.mode != m.mode {
                m.mode = 0;
            }
            // Extend to cover both the old and the new region, exactly
            // C's `chg = end(new) - i(old)` arithmetic (regions arrive
            // in ascending order, so this never shrinks).
            m.chg0 = r.i0 + r.chg0 - m.i0;
            m.chg1 = r.i1 + r.chg1 - m.i1;
            m.chg2 = r.i2 + r.chg2 - m.i2;
            return;
        }
    }
    changes.push(r);
}

/// True when ours' and theirs' changed lines at the two hunks are
/// record-for-record equal (xmerge's `xdl_merge_cmp_lines`).
fn postimages_equal(
    ours: &[&[u8]],
    ours_at: usize,
    theirs: &[&[u8]],
    theirs_at: usize,
    count: usize,
) -> bool {
    (0..count).all(|k| ours[ours_at + k] == theirs[theirs_at + k])
}

/// git's `xdl_refine_conflicts` at ZEALOUS: for each conflict where both
/// sides changed, re-diff our postimage against their postimage. An
/// empty diff means both sides made the same change — clean (mode 4).
/// Otherwise the conflict shrinks to the actually-differing subranges,
/// splitting into several conflicts when the re-diff has multiple hunks.
fn refine_conflicts(
    ours: &[&[u8]],
    theirs: &[&[u8]],
    changes: &mut Vec<Region>,
) {
    let mut i = 0;
    while i < changes.len() {
        if changes[i].mode != 0 || changes[i].chg1 == 0 || changes[i].chg2 == 0 {
            i += 1;
            continue;
        }
        let m = changes[i];
        let sub = match diff_lines_or_none(
            &ours[m.i1..m.i1 + m.chg1],
            &theirs[m.i2..m.i2 + m.chg2],
        ) {
            None => {
                i += 1;
                continue;
            }
            Some(sub) => sub,
        };
        if sub.is_empty() {
            // Both sides made the identical change.
            changes[i].mode = 4;
            i += 1;
            continue;
        }
        // The first re-diff hunk shrinks this region; every subsequent
        // hunk becomes a new conflict right after it (xmerge splices
        // them into the linked list; the vector insert mirrors that).
        changes[i] = Region {
            mode: 0,
            i0: m.i0,
            chg0: m.chg0,
            i1: m.i1 + sub[0].i1,
            chg1: sub[0].chg1,
            i2: m.i2 + sub[0].i2,
            chg2: sub[0].chg2,
        };
        let mut at = i + 1;
        for h in &sub[1..] {
            changes.insert(
                at,
                Region {
                    mode: 0,
                    i0: m.i0,
                    chg0: m.chg0,
                    i1: m.i1 + h.i1,
                    chg1: h.chg1,
                    i2: m.i2 + h.i2,
                    chg2: h.chg2,
                },
            );
            at += 1;
        }
        i = at;
    }
}

/// [`hunks`] that never fails: outside merge3's guaranteed-bounded
/// re-diffs, a `None` (pathological size) keeps the region as-is.
fn diff_lines_or_none(a: &[&[u8]], b: &[&[u8]]) -> Option<Vec<Hunk>> {
    hunks(a, b)
}

/// git's `xdl_simplify_non_conflicts` at ZEALOUS (level 2: the
/// contains-alnum refinement is level-3 only and stays off): when two
/// conflicts are separated by ≤ 3 of ours' unchanged records, swallow
/// the gap into one conflict — fewer, larger conflict hunks.
fn simplify_non_conflicts(changes: &mut Vec<Region>) {
    let mut i = 0;
    while i + 1 < changes.len() {
        let (m, next) = (&changes[i], changes[i + 1]);
        let begin = m.i1 + m.chg1;
        let end = next.i1;
        if m.mode != 0 || next.mode != 0 || end.saturating_sub(begin) > 3 {
            i += 1;
        } else {
            let next = changes[i + 1];
            let m = &mut changes[i];
            m.chg1 = (next.i1 + next.chg1).saturating_sub(m.i1);
            m.chg2 = (next.i2 + next.chg2).saturating_sub(m.i2);
            changes.remove(i + 1);
        }
    }
}

/// Three-way line merge of `base`, `ours`, `theirs` at git's default
/// (`ll_xdl_merge`: ZEALOUS, no favor, no marker style).
///
/// Both-sides-equal and one-side-unchanged inputs return the changed
/// side verbatim (xmerge's empty-script early-outs), which also makes
/// `merge3` safe for add/add: pass an empty base.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Merge3Outcome {
    // ll_xdl_merge routes binary or oversized inputs to the binary
    // driver; without a favor that is a conflict (we take no side).
    if ours == theirs {
        return Merge3Outcome::Merged(ours.to_vec());
    }
    if base.len() > MAX_XDIFF_SIZE || ours.len() > MAX_XDIFF_SIZE || theirs.len() > MAX_XDIFF_SIZE
    {
        return Merge3Outcome::Conflict(vec![Merge3Conflict {
            base: 0..0,
            ours: 0..record_count(ours),
            theirs: 0..record_count(theirs),
        }]);
    }
    if is_binary(base) || is_binary(ours) || is_binary(theirs) {
        return Merge3Outcome::Conflict(vec![Merge3Conflict {
            base: 0..record_count(base),
            ours: 0..record_count(ours),
            theirs: 0..record_count(theirs),
        }]);
    }

    let base_recs = xdiff::split_records(base);
    let our_recs = xdiff::split_records(ours);
    let their_recs = xdiff::split_records(theirs);

    // xdl_merge's early-outs: an empty change script on either side
    // means the other side is the verbatim result.
    let xscr1 = match hunks(&base_recs, &our_recs) {
        Some(h) => h,
        None => return conflict_all(&base_recs, &our_recs, &their_recs),
    };
    let xscr2 = match hunks(&base_recs, &their_recs) {
        Some(h) => h,
        None => return conflict_all(&base_recs, &our_recs, &their_recs),
    };
    if xscr1.is_empty() {
        return Merge3Outcome::Merged(theirs.to_vec());
    }
    if xscr2.is_empty() {
        return Merge3Outcome::Merged(ours.to_vec());
    }

    // ---- xdl_do_merge (level = ZEALOUS, favor = 0, style = 0) ----
    let mut changes: Vec<Region> = Vec::new();
    let (mut p, mut q) = (0usize, 0usize); // cursors into xscr1 / xscr2
    while p < xscr1.len() && q < xscr2.len() {
        let s1 = xscr1[p];
        let s2 = xscr2[q];
        if s1.i1 + s1.chg1 < s2.i1 {
            // Our hunk ends before theirs begins (base coordinates):
            // take ours. i2/chg2 project the untouched base span into
            // their postimage so the region bookkeeping stays aligned.
            append_merge(
                &mut changes,
                Region {
                    mode: 1,
                    i0: s1.i1,
                    chg0: s1.chg1,
                    i1: s1.i2,
                    chg1: s1.chg2,
                    i2: s2.i2.saturating_sub(s2.i1) + s1.i1,
                    chg2: s1.chg1,
                },
            );
            p += 1;
            continue;
        }
        if s2.i1 + s2.chg1 < s1.i1 {
            // Their hunk is strictly before ours: take theirs.
            append_merge(
                &mut changes,
                Region {
                    mode: 2,
                    i0: s2.i1,
                    chg0: s2.chg1,
                    i1: s1.i2.saturating_sub(s1.i1) + s2.i1,
                    chg1: s2.chg1,
                    i2: s2.i2,
                    chg2: s2.chg2,
                },
            );
            q += 1;
            continue;
        }
        // Overlapping or adjacent hunks: identical changes coalesce
        // silently; anything else is a conflict spanning both hunks'
        // base ranges (xdl_do_merge's conflict geometry).
        let identical = s1.i1 == s2.i1
            && s1.chg1 == s2.chg1
            && s1.chg2 == s2.chg2
            && postimages_equal(our_recs.as_slice(), s1.i2, their_recs.as_slice(), s2.i2, s1.chg2);
        if !identical {
            let off = s1.i1 as i64 - s2.i1 as i64;
            let ffo = off + s1.chg1 as i64 - s2.chg1 as i64;
            let (mut i0, mut i1, mut i2) = (s1.i1 as i64, s1.i2 as i64, s2.i2 as i64);
            if off > 0 {
                i0 -= off;
                i1 -= off;
            } else {
                i2 += off;
            }
            let mut chg0 = s1.i1 as i64 + s1.chg1 as i64 - i0;
            let mut chg1 = s1.i2 as i64 + s1.chg2 as i64 - i1;
            let mut chg2 = s2.i2 as i64 + s2.chg2 as i64 - i2;
            if ffo < 0 {
                chg0 -= ffo;
                chg1 -= ffo;
            } else {
                chg2 += ffo;
            }
            append_merge(
                &mut changes,
                Region {
                    mode: 0,
                    i0: i0.max(0) as usize,
                    chg0: chg0.max(0) as usize,
                    i1: i1.max(0) as usize,
                    chg1: chg1.max(0) as usize,
                    i2: i2.max(0) as usize,
                    chg2: chg2.max(0) as usize,
                },
            );
        }
        // Advance whichever hunk's base range ends first (both on a tie).
        let end1 = s1.i1 + s1.chg1;
        let end2 = s2.i1 + s2.chg1;
        if end2 >= end1 {
            p += 1;
        }
        if end1 >= end2 {
            q += 1;
        }
    }
    while p < xscr1.len() {
        let s1 = xscr1[p];
        // i2 projects the base span into theirs' postimage; the
        // line-count delta may be negative when theirs deleted lines
        // (C computes this in int and never lets it go below the
        // region's own bookkeeping needs — saturate at 0).
        let i2 = (s1.i1 as i64 + their_recs.len() as i64 - base_recs.len() as i64).max(0) as usize;
        append_merge(
            &mut changes,
            Region {
                mode: 1,
                i0: s1.i1,
                chg0: s1.chg1,
                i1: s1.i2,
                chg1: s1.chg2,
                i2,
                chg2: s1.chg1,
            },
        );
        p += 1;
    }
    while q < xscr2.len() {
        let s2 = xscr2[q];
        let i1 = (s2.i1 as i64 + our_recs.len() as i64 - base_recs.len() as i64).max(0) as usize;
        append_merge(
            &mut changes,
            Region {
                mode: 2,
                i0: s2.i1,
                chg0: s2.chg1,
                i1,
                chg1: s2.chg1,
                i2: s2.i2,
                chg2: s2.chg2,
            },
        );
        q += 1;
    }

    refine_conflicts(&our_recs, &their_recs, &mut changes);
    simplify_non_conflicts(&mut changes);

    // Output (xdl_fill_merge_buffer, favor = 0, style = 0): every region
    // copies ours' untouched records before it, then the region's chosen
    // postimage records. Mode 4's postimages are equal, so "ours" is a
    // faithful rendering. A surviving mode-0 region is a conflict.
    let mut conflicts = Vec::new();
    for r in &changes {
        if r.mode == 0 {
            conflicts.push(Merge3Conflict {
                base: r.i0..r.i0 + r.chg0,
                ours: r.i1..r.i1 + r.chg1,
                theirs: r.i2..r.i2 + r.chg2,
            });
        }
    }
    if !conflicts.is_empty() {
        return Merge3Outcome::Conflict(conflicts);
    }
    let mut out = Vec::with_capacity(ours.len().max(theirs.len()));
    let mut i = 0usize;
    for r in &changes {
        out.extend_from_slice(&join_records(&our_recs[i..r.i1]));
        if r.mode & 1 != 0 || r.mode == 4 {
            out.extend_from_slice(&join_records(&our_recs[r.i1..r.i1 + r.chg1]));
        }
        if r.mode & 2 != 0 {
            out.extend_from_slice(&join_records(&their_recs[r.i2..r.i2 + r.chg2]));
        }
        i = r.i1 + r.chg1;
    }
    out.extend_from_slice(&join_records(&our_recs[i..]));
    Merge3Outcome::Merged(out)
}

/// Whole-file fallback when a diff exceeds the documented bounds: one
/// conflict spanning everything. A divergence from git's uncapped xdiff
/// (documented with [`MAX_DIFF_LINES`]), kept conservative on purpose.
fn conflict_all(
    base: &[&[u8]],
    ours: &[&[u8]],
    theirs: &[&[u8]],
) -> Merge3Outcome {
    Merge3Outcome::Conflict(vec![Merge3Conflict {
        base: 0..base.len(),
        ours: 0..ours.len(),
        theirs: 0..theirs.len(),
    }])
}

/// Number of records in `content` (records include their `\n`).
fn record_count(content: &[u8]) -> usize {
    xdiff::split_records(content).len()
}

/// Concatenates records into owned bytes.
fn join_records(records: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(records.iter().map(|r| r.len()).sum());
    for r in records {
        out.extend_from_slice(r);
    }
    out
}

// ============================================================ tree merge

/// Why a path (or path prefix) conflicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictReason {
    /// merge3 found overlapping, non-identical change regions (or the
    /// content is binary/oversized and both sides changed it).
    Content,
    /// Both sides changed the mode differently (regular ↔ exec).
    Mode,
    /// Both the contents and the modes conflict.
    ContentAndMode,
    /// One side deleted the path, the other modified it (v0.2: hard
    /// conflict, no resolution).
    DeleteModify,
    /// Both sides added the path with different contents/modes
    /// (also where a rename/rename or rename/add surfaces, since the
    /// merge is rename-blind: it sees two adds).
    AddAdd,
    /// A file exists on one side where a directory exists on the other
    /// (including inside the directory's subtree). v0.2: hard conflict,
    /// no `path~branch` rearrangement.
    FileDir,
    /// A symlink or gitlink changed on both sides (atomic: ids only,
    /// never content-merged).
    Link,
}

/// The 3-way tree-merge outcome.
///
/// `Merged` is everything the caller needs to commit: stage each
/// `new_blobs` entry with `put_blob` (ids are content addresses, so
/// staging is idempotent), then persist a changeset with
/// `parents = [ours, theirs]` and these `changes` — replaying them over
/// `tree(ours)` reproduces the merged tree, satisfying the
/// unchanged-first-parent derivation rule.
///
/// `Conflicted` reports each conflicting path with its reason. The merged
/// tree is *not* reconstructed for conflicted merges (v0.2 chose hard
/// conflicts; git's stage-the-conflicted-tree behavior belongs to the
/// integration layer, if ever).
#[derive(Clone, Debug)]
pub enum MergeOutcome {
    /// Clean merge.
    Merged {
        /// Changes taking `tree(ours)` to the merged tree, canonical:
        /// strictly ascending by path bytes, no duplicates.
        changes: Vec<Change>,
        /// Raw bytes of every blob the merge synthesized (merged file
        /// contents), as `(content address, content)`. The caller stages
        /// them; the merge itself writes nothing.
        new_blobs: Vec<(Id, Vec<u8>)>,
    },
    /// Conflicted merge: each conflicting path with its reason, sorted
    /// by path bytes.
    Conflicted {
        conflicts: Vec<MergeConflict>,
    },
}

/// One conflicted path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeConflict {
    /// The conflicting path (full, slash-joined).
    pub path: String,
    /// Why it conflicted.
    pub reason: ConflictReason,
}

/// Working state threaded through the recursive walk.
struct MergeWalk<'a> {
    objects: &'a dyn ObjectStore,
    /// The merged-tree bytes that differ from ours, in canonical order
    /// (the walk visits names in byte order, so appends land sorted —
    /// a final sort keeps the invariant cheap to audit).
    changes: Vec<Change>,
    /// New blob contents by id, byte-sorted at the end.
    new_blobs: Vec<(Id, Vec<u8>)>,
    /// Conflicted paths, sorted by path bytes.
    conflicts: Vec<MergeConflict>,
}

impl<'a> MergeWalk<'a> {
    fn new(objects: &'a dyn ObjectStore) -> Self {
        MergeWalk {
            objects,
            changes: Vec::new(),
            new_blobs: Vec::new(),
            conflicts: Vec::new(),
        }
    }

    /// Records a newly merged blob's bytes. Duplicate ids (two paths
    /// merged to identical content) are collapsed by the final sort +
    /// dedup pass.
    fn stage_blob(&mut self, content: Vec<u8>) -> Id {
        let id = blob_id(&content);
        self.new_blobs.push((id, content));
        id
    }

    /// Emits a change taking ours to the merged result at `path`, unless
    /// it is a no-op relative to ours.
    fn emit(&mut self, path: &str, op: Op, blob: Option<Id>, mode: Mode) {
        self.changes.push(Change {
            path: path.to_owned(),
            op,
            blob,
            src_path: None,
            mode,
        });
    }

    /// Deletes `path` (and, if ours has a directory there, everything
    /// under it) from the merged tree.
    fn emit_delete(&mut self, path: &str) {
        self.changes.push(Change {
            path: path.to_owned(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        });
    }

    fn conflict(&mut self, path: &str, reason: ConflictReason) {
        self.conflicts.push(MergeConflict {
            path: path.to_owned(),
            reason,
        });
    }

    /// Records every file under theirs' subtree `tree` (at directory
    /// prefix `dir`) as added. The blob ids already exist in the store —
    /// theirs' tree is stored — so nothing is staged.
    fn add_subtree(&mut self, dir: &str, tree: Id, depth: usize) -> Result<(), StoreError> {
        if depth > MAX_TREE_DEPTH {
            return Err(StoreError::Invalid(
                "tree nesting exceeds the maximum supported depth".into(),
            ));
        }
        for e in Engine::load_tree(self.objects, tree)?.entries {
            match e.kind {
                TreeKind::File => {
                    let blob = e.blob.ok_or_else(|| {
                        StoreError::Corrupt(format!("file entry {:?} without a blob id", e.name))
                    })?;
                    self.emit(&join(dir, &e.name), Op::Add, Some(blob), e.mode);
                }
                TreeKind::Dir => {
                    let id = e.tree.ok_or_else(|| {
                        StoreError::Corrupt(format!("dir entry {:?} without a tree id", e.name))
                    })?;
                    self.add_subtree(&join(dir, &e.name), id, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// Joins a directory prefix with a component (diff.rs's `join`).
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// One side's view of a name: a file leaf (mode + blob id) or a subtree.
#[derive(Clone, PartialEq)]
enum Side {
    /// Absent.
    None,
    /// A file entry (any mode).
    File { mode: Mode, blob: Id },
    /// A directory (subtree id).
    Dir { tree: Id },
}

impl Side {
    fn of(e: Option<&TreeEntry>) -> Result<Self, StoreError> {
        match e {
            None => Ok(Side::None),
            Some(e) => match e.kind {
                TreeKind::File => {
                    let blob = e.blob.ok_or_else(|| {
                        StoreError::Corrupt(format!("file entry {:?} without a blob id", e.name))
                    })?;
                    Ok(Side::File { mode: e.mode, blob })
                }
                TreeKind::Dir => {
                    let tree = e.tree.ok_or_else(|| {
                        StoreError::Corrupt(format!("dir entry {:?} without a tree id", e.name))
                    })?;
                    Ok(Side::Dir { tree })
                }
            },
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, Side::None)
    }
}

/// Merges one directory level. `dir` is the prefix for emitted paths;
/// depth bounds recursion (corrupt trees cannot cycle legitimately).
fn merge_dir(
    w: &mut MergeWalk<'_>,
    base: Option<Id>,
    ours: Option<Id>,
    theirs: Option<Id>,
    dir: &str,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(StoreError::Invalid(
            "tree nesting exceeds the maximum supported depth".into(),
        ));
    }
    let load = |id: Option<Id>| -> Result<Tree, StoreError> {
        match id {
            None => Ok(Tree::default()),
            Some(id) => Engine::load_tree(w.objects, id),
        }
    };
    let tb = load(base)?;
    let to = load(ours)?;
    let tt = load(theirs)?;

    // Union of names, each side looked up by name (entries are sorted by
    // name bytes, so a three-way sorted merge is possible; the maps keep
    // the code readable and the iteration order deterministic either way).
    let mut names: Vec<&str> = Vec::new();
    for t in [&tb, &to, &tt] {
        for e in &t.entries {
            names.push(&e.name);
        }
    }
    names.sort_unstable();
    names.dedup();

    for name in names {
        let path = join(dir, name);
        let eb = tb.entries.iter().find(|e| e.name == name);
        let eo = to.entries.iter().find(|e| e.name == name);
        let et = tt.entries.iter().find(|e| e.name == name);
        merge_entry(
            w,
            Side::of(eb)?,
            Side::of(eo)?,
            Side::of(et)?,
            &path,
            depth,
        )?;
    }
    Ok(())
}

/// Resolves one name's entry on the three sides into changes relative
/// to ours. All the trivial pairwise-equality resolutions happen first;
/// what remains is genuinely three-way.
fn merge_entry(
    w: &mut MergeWalk<'_>,
    eb: Side,
    eo: Side,
    et: Side,
    path: &str,
    depth: usize,
) -> Result<(), StoreError> {
    // Trivial resolution by content-addressed equality: two equal sides
    // resolve the third.
    if eo == et {
        // Same thing on both sides (including both absent): the merged
        // result is exactly ours — no change.
        return Ok(());
    }
    if eb == eo {
        // Ours matches base: take theirs — including deletion, and
        // including replacing a file with a directory or vice versa.
        return take_theirs(w, &eb, &et, path, depth);
    }
    if eb == et {
        // Theirs matches base: the merged result stays ours — no change.
        return Ok(());
    }

    // All three sides differ pairwise. What remains depends on the
    // kinds ours and theirs hold (base can be anything).
    match (&eo, &et) {
        // A file on both changed sides: content merge (or add/add when
        // base has nothing — or something else — here).
        (
            Side::File { mode: mo, blob: o },
            Side::File { mode: mt, blob: t },
        ) => merge_file(w, &eb, *mo, o, *mt, t, path),

        // Directories on both sides: recurse. Base contributes its
        // subtree only when it is a directory here too (a base file or
        // a missing base entry is the empty tree for the sub-merge,
        // exactly like git's filemask semantics).
        (Side::Dir { tree: to_id }, Side::Dir { tree: tt_id }) => {
            let tb_id = match eb {
                Side::Dir { tree } => Some(tree),
                _ => None,
            };
            merge_dir(w, tb_id, Some(*to_id), Some(*tt_id), path, depth + 1)
        }

        // A file on one side, a directory on the other: file/directory
        // conflict. v0.2 chose hard conflicts — the merged tree is
        // neither side's arrangement, so the path is reported and ours
        // stays (no `path~branch` rearrangement, which is a git
        // convenience that invents paths the caller never wrote).
        (Side::File { .. }, Side::Dir { .. }) | (Side::Dir { .. }, Side::File { .. }) => {
            w.conflict(path, ConflictReason::FileDir);
            Ok(())
        }

        // Ours has nothing here (and base ≠ ours, theirs ≠ ours): theirs
        // changed while ours was deleted or never had the name —
        // modify/delete. v0.2: hard conflict naming the path and side.
        // (eb is never None here: a base matching ours was already
        // resolved by `eb == eo` above.)
        // Theirs has nothing here (and base ≠ theirs, ours ≠ theirs):
        // ours changed while theirs was deleted or never had the name —
        // modify/delete from the other direction. v0.2: hard conflict.
        // (et == Some but ours changed; eb == et was already resolved.)
        // Theirs deleted this subtree while ours changed inside it:
        // git surfaces modify/delete at the FILE level (it walks the
        // surviving directory and reports each modified file), not at
        // the directory top path — mirror that granularity so conflict
        // path sets compare equal. Ours' unchanged files under the dir
        // are simply deleted (take-theirs), exactly like `eb == eo`.
        (Side::Dir { .. }, Side::None) => {
            if let Side::Dir { tree: base_tree } = &eb {
                dir_delete_modify(w, *base_tree, eo, path, depth, true)
            } else {
                // Base was not a directory here (file or absent): the
                // whole ours-side subtree is effectively added content
                // theirs deleted — report the top path.
                w.conflict(path, ConflictReason::DeleteModify);
                Ok(())
            }
        }
        // Ours deleted this subtree while theirs changed inside it:
        // the mirror of the arm above — git reports the surviving
        // side's modified files, not the directory top path.
        (Side::None, Side::Dir { .. }) => {
            if let Side::Dir { tree: base_tree } = &eb {
                dir_delete_modify(w, *base_tree, et, path, depth, false)
            } else {
                // Base was not a directory here: theirs' whole subtree
                // is new content ours deleted — report the top path.
                w.conflict(path, ConflictReason::DeleteModify);
                Ok(())
            }
        }
        // Ours has nothing here while theirs changed the file:
        // modify/delete naming the file's own path.
        (Side::None, Side::File { .. })
        // Ours has a file where theirs is absent: modify/delete at the
        // file's own path.
        | (Side::File { .. }, Side::None) => {
            w.conflict(path, ConflictReason::DeleteModify);
            Ok(())
        }
        // Both sides absent cannot reach here (`eo == et` resolved it).
        (Side::None, Side::None) => Ok(()),
    }
}

/// One side deleted the directory at `path` while the other kept it
/// with changes inside: per-file modify/delete conflicts for every
/// entry the surviving side changed relative to base, per-file deletes
/// for everything it left untouched. This mirrors git's file-level
/// conflict granularity — merge-ort reports `dir/file` paths, not the
/// directory top path. (The emitted deletes only matter when the walk
/// stays conflict-free, which cannot happen past the first conflict;
/// they are bookkeeping for symmetric reuse of this helper.)
fn dir_delete_modify(
    w: &mut MergeWalk<'_>,
    base_tree: Id,
    surviving: Side,
    path: &str,
    depth: usize,
    surviving_is_ours: bool,
) -> Result<(), StoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(StoreError::Invalid(
            "tree nesting exceeds the maximum supported depth".into(),
        ));
    }
    let Side::Dir { tree: ours_tree } = surviving else {
        // Unreachable by construction (the caller matched Dir/None).
        w.conflict(path, ConflictReason::DeleteModify);
        return Ok(());
    };

    let base_entries = Engine::load_tree(w.objects, base_tree)?.entries;
    let ours_entries = Engine::load_tree(w.objects, ours_tree)?.entries;

    for e in &ours_entries {
        let sub_path = join(path, &e.name);
        let base_e = base_entries.iter().find(|b| b.name == e.name);
        match (&e.kind, base_e.map(|b| b.kind)) {
            // The surviving side's entry matches base (unchanged): the
            // deleting side wins silently — but the emitted Delete is
            // only meaningful (and only valid for the apply property)
            // when OURS holds the path. When theirs survives, ours
            // never had these files, and the merge is conflicted
            // regardless.
            (TreeKind::File, Some(TreeKind::File)) if base_e.is_some_and(|b| b.blob == e.blob && b.mode == e.mode) => {
                if surviving_is_ours {
                    w.emit_delete(&sub_path);
                }
            }
            // The surviving side modified the base file: the
            // modify/delete conflict names the file itself.
            (TreeKind::File, Some(TreeKind::File)) => {
                w.conflict(&sub_path, ConflictReason::DeleteModify);
            }
            // The surviving side ADDED this file (base absent): an
            // addition beats the other side's parent-directory deletion
            // (git agrees: `merge-tree` keeps it). No conflict — when
            // the surviving side is ours the Add is already in place,
            // and when it is theirs the merge is conflicted elsewhere
            // anyway.
            (TreeKind::File, None) => {}
            // Base had a directory where the surviving side now has a
            // file: a replace; report it (rare in generated shapes).
            (TreeKind::File, Some(TreeKind::Dir)) => {
                w.conflict(&sub_path, ConflictReason::DeleteModify);
            }
            // A subdirectory the base also had: recurse (its unchanged
            // files delete, its changed files conflict).
            (TreeKind::Dir, Some(TreeKind::Dir)) => {
                if let Some(bt) = base_e.and_then(|b| b.tree) {
                    dir_delete_modify(w, bt, Side::of(Some(e))?, &sub_path, depth + 1, surviving_is_ours)?;
                }
            }
            // The surviving side added this whole subdirectory (base
            // had nothing or a file here): the addition beats the other
            // side's parent-directory deletion, same rule as a plain
            // added file above.
            (TreeKind::Dir, None | Some(TreeKind::File)) => {}
        }
    }
    Ok(())
}

/// Merges a file that exists on both changed sides (regular/exec leaves;
/// symlinks and gitlinks arrive here too and are handled atomically).
fn merge_file(
    w: &mut MergeWalk<'_>,
    eb: &Side,
    mo: Mode,
    o: &Id,
    mt: Mode,
    t: &Id,
    path: &str,
) -> Result<(), StoreError> {
    // Symlinks and gitlinks are atomic: id equality only, never a
    // content merge. (Link-target text is not mergeable text in git
    // either, and a gitlink's stored "blob" is a padded pointer that is
    // never in the object store.)
    if is_linklike(mo) || is_linklike(mt) {
        let reason = if mo == mt {
            ConflictReason::Link
        } else {
            // Distinct types on the two sides (symlink vs gitlink, or
            // link vs regular): no content merge is even possible.
            ConflictReason::Content
        };
        w.conflict(path, reason);
        return Ok(());
    }
    // Regular/exec on both sides: git's "distinct types" never arises
    // (Regular↔Exec is the mode table's domain).

    // merge-ort's handle_content_merge mode table: equal sides or
    // ours-matches-base resolve to theirs' mode; otherwise ours wins,
    // clean iff theirs matches base.
    // Add/add (base absent or not a file here): git treats differing
    // modes as a conflict — there is no base mode to break the tie, so
    // neither side's mode "wins". A file base goes through the normal
    // mode table.
    let (mode, mode_clean) = match eb {
        Side::File { mode, .. } => merge_modes(*mode, mo, mt),
        _ => {
            if mo == mt {
                (mo, true)
            } else {
                (mo, false)
            }
        }
    };

    // The content merge. For add/add (base absent or non-regular here)
    // git passes the empty blob as base — the two sides then merge as
    // independent inserts.
    let base_bytes = match eb {
        Side::File { mode, blob } if !is_linklike(*mode) => read_blob(w.objects, blob)?,
        _ => Vec::new(),
    };
    let ours_bytes = read_blob(w.objects, o)?;
    let theirs_bytes = read_blob(w.objects, t)?;
    let (content_clean, merged) = match merge3(&base_bytes, &ours_bytes, &theirs_bytes) {
        Merge3Outcome::Merged(bytes) => (true, bytes),
        Merge3Outcome::Conflict(_) => (false, ours_bytes.clone()),
    };
    let reason = match (content_clean, mode_clean) {
        (true, true) => {
            // Clean: emit relative to ours (a no-op when both the blob
            // and the mode already match ours — merge3 can return ours
            // verbatim when ours' postimage was the resolution).
            if merged == ours_bytes && mode == mo {
                return Ok(());
            }
            let blob = if merged == ours_bytes {
                *o
            } else {
                w.stage_blob(merged)
            };
            let op = if eb.is_none() { Op::Add } else { Op::Modify };
            w.emit(path, op, Some(blob), mode);
            return Ok(());
        }
        (false, true) => ConflictReason::Content,
        (true, false) => ConflictReason::Mode,
        (false, false) => ConflictReason::ContentAndMode,
    };
    w.conflict(path, reason);
    Ok(())
}

/// Takes theirs at `path`: ours matched base, so their side's value —
/// file, directory, or absence — is the merged result, emitted as
/// changes relative to ours. Deletion of a directory recurses to
/// per-file `Delete` changes (the changeset vocabulary has no
/// "delete tree").
fn take_theirs(
    w: &mut MergeWalk<'_>,
    eb: &Side,
    et: &Side,
    path: &str,
    depth: usize,
) -> Result<(), StoreError> {
    match et {
        Side::None => {
            // Theirs deleted (or never had it): the merged tree drops
            // what ours holds. Ours held something (else `eo == et`
            // had already resolved this name).
            w.emit_delete(path);
            Ok(())
        }
        Side::File { mode, blob } => {
            let op = if eb.is_none() { Op::Add } else { Op::Modify };
            w.emit(path, op, Some(*blob), *mode);
            Ok(())
        }
        Side::Dir { tree } => {
            // Their directory (sub)tree arrives wholesale: every file
            // under it is an Add relative to ours. Ours held a file or
            // a different subtree here, so this lands cleanly.
            w.add_subtree(path, *tree, depth)
        }
    }
}

/// merge-ort's mode table (handle_content_merge): equal sides or
/// ours-matches-base resolve to theirs' mode; otherwise (the
/// regular/exec pair) ours wins and the merge is clean iff theirs
/// matches base.
fn merge_modes(base: Mode, ours: Mode, theirs: Mode) -> (Mode, bool) {
    if ours == theirs || ours == base {
        (theirs, true)
    } else {
        (ours, theirs == base)
    }
}

/// Symlinks and gitlinks: merged by id, never by content.
fn is_linklike(mode: Mode) -> bool {
    matches!(mode, Mode::Symlink | Mode::Gitlink)
}

/// Reads a blob, mapping absence to corruption (trees only reference
/// stored blobs).
fn read_blob(objects: &dyn ObjectStore, id: &Id) -> Result<Vec<u8>, StoreError> {
    objects
        .get_blob(*id)?
        .ok_or_else(|| StoreError::Corrupt(format!("blob {id} is referenced but missing")))
}

impl Engine {
    /// Three-way merge of two changesets' trees against their merge base
    /// (D9–D11).
    ///
    /// Pure computation: reads trees and blobs through the engine's
    /// store handles and returns the merged tree as *changes relative to
    /// `tree(ours)`* plus the bytes of any newly merged blob. Nothing is
    /// written; staging `new_blobs` and persisting the changeset (with
    /// `parents = [ours, theirs]`, caller-supplied message and
    /// identities) is the integration layer's job.
    ///
    /// Determinism: the same `(base, ours, theirs)` triple always yields
    /// byte-identical output — trees are walked in name-byte order,
    /// changes are emitted in canonical order, and blob ids are content
    /// addresses. No clocks, no map-iteration-order dependence.
    ///
    /// Errors: unrelated histories (no merge base) are
    /// [`StoreError::Invalid`] naming the operation and both ids;
    /// store corruption propagates.
    pub fn merge_changesets(&self, ours: Id, theirs: Id) -> Result<MergeOutcome, StoreError> {
        let base = self.merge_base(ours, theirs)?.ok_or_else(|| {
            StoreError::Invalid(format!(
                "merge of {ours} and {theirs}: unrelated histories (no merge base)"
            ))
        })?;
        let ours_tree = self.derive_tree(ours)?;
        let theirs_tree = self.derive_tree(theirs)?;
        let base_tree = self.derive_tree(base)?;

        // Dir-level content-addressed pruning: identical subtrees on any
        // two sides resolve the third without walking. (Root-level only;
        // deeper pruning happens naturally when equal subtrees make the
        // per-name equality checks trivial.)
        if ours_tree == theirs_tree {
            return Ok(MergeOutcome::Merged {
                changes: Vec::new(),
                new_blobs: Vec::new(),
            });
        }
        let mut w = MergeWalk::new(&*self.objects);
        if base_tree == ours_tree {
            // Ours is the base: the merged tree is exactly theirs,
            // expressed as diff(ours → theirs) — the existing tree diff
            // emits the change list directly.
            w.changes = diff::diff_trees(&*self.objects, ours_tree, theirs_tree)?;
            return Ok(MergeOutcome::Merged {
                changes: w.changes,
                new_blobs: Vec::new(),
            });
        }
        if base_tree == theirs_tree {
            // Theirs is the base: the merged tree is ours.
            return Ok(MergeOutcome::Merged {
                changes: Vec::new(),
                new_blobs: Vec::new(),
            });
        }

        merge_dir(&mut w, base_tree, ours_tree, theirs_tree, "", 0)?;

        if w.conflicts.is_empty() {
            w.changes
                .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
            w.changes.dedup_by(|a, b| a.path == b.path);
            w.new_blobs
                .sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            w.new_blobs.dedup_by(|a, b| a.0 == b.0);
            Ok(MergeOutcome::Merged {
                changes: w.changes,
                new_blobs: w.new_blobs,
            })
        } else {
            w.conflicts
                .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
            w.conflicts.dedup_by(|a, b| a.path == b.path);
            Ok(MergeOutcome::Conflicted {
                conflicts: w.conflicts,
            })
        }
    }
}

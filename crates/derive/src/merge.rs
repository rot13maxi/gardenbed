//! Server-side two-parent merge (v0.2 D9–D11): merge3 content merge,
//! the 3-way tree walk, and `Engine::merge_changesets`.
//!
//! Merge is a **derived operation with zero new storage and zero new
//! truth**: it computes the merged tree from (merge base, ours, theirs)
//! and emits the change list `diff(tree(ours) → merged_tree)`, so the
//! resulting changeset is an ordinary changeset that derives through the
//! unchanged first-parent rule (`apply(changes, tree(parents[0]))`).
//!
//! ## Content merge (`merge3`)
//!
//! A per-file 3-way line merge in the shape of git's `xdl_merge` (the
//! marker-less, non-zealous form): both sides are diffed against the
//! base on the xdiff port, change regions are overlap-tested in base
//! coordinates, non-overlapping regions auto-combine, any overlap is a
//! conflict. Since the same diff engine (with the same compaction and
//! indent heuristic) produced both sides' regions, an unchanged-side
//! region never collides with a changed-side one — the auto-combine
//! cases are exactly git's. Regions that merely touch (one side's
//! replacement starts where the other's base span ends) conflict too,
//! matching `git merge-tree --write-tree` (adjacent single-line changes
//! from the two sides are a content conflict).
//!
//! Blobs past the xdiff caps ([`xdiff::MAX_DIFF_LINES`], the search
//! effort cap) conflict rather than mis-merge — same reason git's own
//! cost caps exist (M9.5).
//!
//! ## Tree merge
//!
//! A recursive walk over `(base, ours, theirs)` trees emitting a change
//! list vs OURS. Trivial rules first (two sides equal → that value; one
//! side == base → the other side); content-addressed equality prunes
//! untouched subtrees before any blob read. Regular/exec leaves
//! differing on both sides get `merge3`; mode merge follows the same
//! trivial rules; symlinks and gitlinks are atomic (id equality).
//! Directory-level conflicts (file↔directory on both-changed sides,
//! delete-vs-modify, add/add) are collected and reported together, at
//! file-path granularity wherever the walk can name the file.
//!
//! Everything is deterministic: content addressing plus a pure function
//! of immutable inputs — wiped and rebuilt caches reproduce
//! byte-identical merges (M9.4).

use std::collections::{BTreeMap, HashMap, HashSet};

use gardenbed_model::{Change, Id, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

use crate::Engine;
use crate::apply::MAX_TREE_DEPTH;
use crate::xdiff;

/// One side's detected rename, in WHOLE-TREE path space: `src` was
/// removed and `dst` added anywhere in the tree (same directory or
/// not), carrying either the identical blob (exact rename) or a
/// similar one (scored rename). `src_base` is the source entry in
/// the BASE tree (pre-rename); detection requires the same mode
/// class on both ends (a regular→exec flip is a mode change riding
/// the rename; a class flip to symlink/gitlink is not a rename
/// candidate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SideRename {
    /// Removed path (repo-relative).
    pub(crate) src: String,
    /// Added path (repo-relative).
    pub(crate) dst: String,
    /// The source entry in the BASE tree (pre-rename).
    pub(crate) src_base: TreeEntry,
    /// Similarity score in percent (100 = identical bytes).
    pub(crate) score: u32,
}

/// Detects one side's renames over the WHOLE TREE (git's
/// `diffcore_rename` scope): a rename requires the source path absent
/// from the side, present in the base, and the destination path
/// present on the side, absent from the base, with matching mode
/// class (Regular/Exec both count as files). Whole-tree pairing is
/// what makes CROSS-DIRECTORY moves detectable: the deletion of
/// `dirA/f` pairs with the addition of `dirB/g` even though they
/// surface at different levels of the tree walk.
///
/// Inputs are flattened, path-sorted file listings (path → entry) of
/// the base and the side. Resolution order matches git's
/// `diffcore_rename` shape:
/// 1. **Exact renames** — destination blob id equals the source's.
///    Each source pairs with the lexicographically first exact
///    destination (git prefers lower stage/path order too); ties on
///    the destination side pick the first source in path order.
/// 2. **Scored renames** — remaining candidates are scored by
///    line-based similarity (`similarity_score`) and paired greedily
///    best-first; a pair must clear [`MIN_RENAME_SCORE`] (git's
///    default 50%).
///
/// Both passes are bounded by [`MAX_RENAME_CANDIDATES`] per side over
/// the whole tree; beyond that bound the candidates stay unpaired
/// (the walk then treats them as plain delete/add, the rename-blind
/// behavior) — the same shape as git's `diff.renameLimit` falling
/// back.
fn detect_side_renames(
    objects: &dyn ObjectStore,
    base_entries: &BTreeMap<String, TreeEntry>,
    side_entries: &BTreeMap<String, TreeEntry>,
) -> Result<Vec<SideRename>, StoreError> {
    let mut adds: Vec<(&String, &TreeEntry)> = Vec::new();
    let mut dels: Vec<(&String, &TreeEntry)> = Vec::new();
    for (path, entry) in side_entries {
        if !base_entries.contains_key(path) {
            adds.push((path, entry));
        }
    }
    for (path, entry) in base_entries {
        if !side_entries.contains_key(path) {
            dels.push((path, entry));
        }
    }
    if adds.is_empty() || dels.is_empty() {
        return Ok(Vec::new());
    }
    if adds.len() > MAX_RENAME_CANDIDATES || dels.len() > MAX_RENAME_CANDIDATES {
        return Ok(Vec::new()); // bounded: too many candidates to score
    }

    let files_only = |e: &TreeEntry| {
        e.kind == TreeKind::File && e.mode != Mode::Symlink && e.mode != Mode::Gitlink
    };

    // Pass 1: exact renames (blob id equality). Both listings are
    // sorted by path bytes, so iteration order is the deterministic
    // (dst, src) path order.
    let mut used_dsts: HashSet<&String> = HashSet::new();
    let mut used_srcs: HashSet<&String> = HashSet::new();
    let mut found: Vec<SideRename> = Vec::new();
    for (dp, de) in &adds {
        if !files_only(de) {
            continue;
        }
        // First unused source with the identical blob, in path order.
        for (sp, se) in &dels {
            if used_srcs.contains(sp) || !files_only(se) {
                continue;
            }
            if se.blob == de.blob {
                used_srcs.insert(sp);
                used_dsts.insert(dp);
                found.push(SideRename {
                    src: (*sp).clone(),
                    dst: (*dp).clone(),
                    src_base: (*se).clone(),
                    score: 100,
                });
                break;
            }
        }
    }

    // Pass 2: scored renames over the leftovers.
    let open_adds: Vec<(&String, &TreeEntry)> = adds
        .iter()
        .filter(|(p, _)| !used_dsts.contains(*p))
        .cloned()
        .collect();
    let open_dels: Vec<(&String, &TreeEntry)> = dels
        .iter()
        .filter(|(p, _)| !used_srcs.contains(*p))
        .cloned()
        .collect();
    if !open_adds.is_empty() && !open_dels.is_empty() {
        let mut pairs: Vec<(u32, usize, usize)> = Vec::new(); // (score, di, ai)
        for (di, (_sp, se)) in open_dels.iter().enumerate() {
            if !files_only(se) {
                continue;
            }
            let src_content = objects.get_blob(se.blob.expect("file entry carries a blob"))?;
            for (ai, (_dp, de)) in open_adds.iter().enumerate() {
                if !files_only(de) {
                    continue;
                }
                let Some(score) = similarity_score(
                    src_content.as_deref(),
                    objects
                        .get_blob(de.blob.expect("file entry carries a blob"))?
                        .as_deref(),
                ) else {
                    continue;
                };
                if score >= MIN_RENAME_SCORE {
                    pairs.push((score, di, ai));
                }
            }
        }
        // Best score first; ties resolved deterministically by
        // (dst path, src path) byte order.
        pairs.sort_by(|x, y| {
            y.0.cmp(&x.0).then_with(|| {
                let xd = open_adds[x.2].0.as_bytes();
                let yd = open_adds[y.2].0.as_bytes();
                xd.cmp(yd)
                    .then_with(|| open_dels[x.1].0.as_bytes().cmp(open_dels[y.1].0.as_bytes()))
            })
        });
        let mut taken_dst: HashSet<usize> = HashSet::new();
        let mut taken_src: HashSet<usize> = HashSet::new();
        for (score, di, ai) in pairs {
            if taken_dst.contains(&ai) || taken_src.contains(&di) {
                continue;
            }
            taken_dst.insert(ai);
            taken_src.insert(di);
            let (sp, se) = open_dels[di];
            let (dp, _de) = open_adds[ai];
            found.push(SideRename {
                src: (*sp).clone(),
                dst: (*dp).clone(),
                src_base: (*se).clone(),
                score,
            });
        }
    }
    found.sort_by(|a, b| a.src.as_bytes().cmp(b.src.as_bytes()));
    Ok(found)
}

/// Minimum similarity for a scored rename (git's default `-M50%`).
const MIN_RENAME_SCORE: u32 = 50;
/// Bound on adds/dels considered per side over the whole tree (git's
/// `diff.renameLimit` default is 1000; we keep the same order).
const MAX_RENAME_CANDIDATES: usize = 1000;

/// Line-based similarity between two blobs, in percent (0..=100):
/// `100 * common_lines / max(lines_a, lines_b)` where common lines are
/// the unchanged half of the line diff — git's `diffcore_rename`
/// estimate for text files. `None` when either side is past the xdiff
/// bounds (no score, never a mis-detected rename).
fn similarity_score(a: Option<&[u8]>, b: Option<&[u8]>) -> Option<u32> {
    let a = a.unwrap_or(&[]);
    let b = b.unwrap_or(&[]);
    if a.is_empty() || b.is_empty() {
        return None; // an empty side has no lines to share
    }
    let ra = xdiff::split_records(a);
    let rb = xdiff::split_records(b);
    let max = ra.len().max(rb.len());
    if max == 0 {
        return None;
    }
    let diff = xdiff::diff_lines(&ra, &rb)?;
    let removed = diff.changed_old.iter().filter(|c| **c).count();
    let added = diff.changed_new.iter().filter(|c| **c).count();
    let common = ra
        .len()
        .saturating_sub(removed)
        .min(rb.len().saturating_sub(added));
    Some((100 * common as u64 / max as u64) as u32)
}

/// The outcome of one 3-way file merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Merge3Outcome {
    /// The merged bytes (auto-merged, or one side trivially won).
    Merged(Vec<u8>),
    /// The sides' changes overlap in base coordinates: a conflict.
    Conflict,
}

/// Per-file 3-way content merge over the xdiff port.
///
/// Change regions from both sides are overlap-tested in base
/// coordinates like git's `xdl_merge`: non-overlapping regions
/// auto-combine into the merged bytes; any overlap (or either diff past
/// the xdiff bounds) is a conflict. `None` inputs (an empty side)
/// behave as zero records, exactly as [`xdiff::split_records`] renders
/// them.
pub fn merge3(base: Option<&[u8]>, ours: Option<&[u8]>, theirs: Option<&[u8]>) -> Merge3Outcome {
    let b = base.unwrap_or(&[]);
    let o = ours.unwrap_or(&[]);
    let t = theirs.unwrap_or(&[]);
    // Content-addressed trivial rules first: identical bytes need no
    // diff (this is also what makes the tree walk's caller cheap).
    if o == t {
        return Merge3Outcome::Merged(o.to_vec());
    }
    if o == b {
        return Merge3Outcome::Merged(t.to_vec());
    }
    if t == b {
        return Merge3Outcome::Merged(o.to_vec());
    }
    let recs_b = xdiff::split_records(b);
    let recs_o = xdiff::split_records(o);
    let recs_t = xdiff::split_records(t);
    let Some(diff_o) = xdiff::diff_lines(&recs_b, &recs_o) else {
        return Merge3Outcome::Conflict;
    };
    let Some(diff_t) = xdiff::diff_lines(&recs_b, &recs_t) else {
        return Merge3Outcome::Conflict;
    };
    let ours_regions = regions_base(&recs_b, &recs_o, &diff_o);
    let theirs_regions = regions_base(&recs_b, &recs_t, &diff_t);
    // Regions both sides changed identically (same base span, same
    // replacement bytes) apply once: git's xdl_merge takes a change
    // made identically on both sides without conflict.
    let shared = |r: &Region| {
        theirs_regions
            .iter()
            .any(|t| t.base_lo == r.base_lo && t.base_hi == r.base_hi && t.lines == r.lines)
    };
    let ours_kept: Vec<Region> = ours_regions
        .iter()
        .filter(|r| !shared(r))
        .cloned()
        .collect();
    let theirs_unique: Vec<Region> = theirs_regions
        .iter()
        .filter(|t| {
            !ours_regions
                .iter()
                .any(|o| o.base_lo == t.base_lo && o.base_hi == t.base_hi && o.lines == t.lines)
        })
        .cloned()
        .collect();
    let shared_regions: Vec<Region> = ours_regions.iter().filter(|r| shared(r)).cloned().collect();
    if conflicts_with(&ours_kept, &theirs_unique) {
        return Merge3Outcome::Conflict;
    }
    // Non-overlapping: interleave the region sets in base order.
    let mut all: Vec<Region> = ours_kept
        .into_iter()
        .chain(theirs_unique)
        .chain(shared_regions)
        .collect();
    all.sort_by(|a, b| {
        a.base_lo
            .cmp(&b.base_lo)
            .then(a.base_hi.cmp(&b.base_hi))
            .then(a.side_lo.cmp(&b.side_lo))
    });
    let mut out: Vec<u8> = Vec::with_capacity(o.len().max(t.len()));
    let mut base_i = 0usize;
    for region in &all {
        while base_i < region.base_lo {
            out.extend_from_slice(recs_b[base_i]);
            base_i += 1;
        }
        out.extend_from_slice(&region.lines);
        base_i = region.base_hi;
    }
    while base_i < recs_b.len() {
        out.extend_from_slice(recs_b[base_i]);
        base_i += 1;
    }
    Merge3Outcome::Merged(out)
}

/// One change region in base coordinates: base lines `[base_lo,
/// base_hi)` are replaced by `lines`.
#[derive(Clone, Debug)]
struct Region {
    base_lo: usize,
    base_hi: usize,
    /// The region's replacement bytes (from the owning side).
    lines: Vec<u8>,
    /// The replacement's first record index on the owning side — a
    /// deterministic tie-break for the interleave sort only.
    side_lo: usize,
}

/// Extracts a side's changed regions in base coordinates from its diff
/// against the base (git's `xdl_merge` builds the same "changed versus
/// base" span list per side before overlap testing).
fn regions_base<'a>(base: &[&'a [u8]], side: &[&'a [u8]], diff: &xdiff::LineDiff) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut base_i = 0usize;
    let mut side_i = 0usize;
    // Walk both record cursors in lockstep; whenever either side's
    // changed flags fire, accumulate one maximal region. Unchanged
    // records pair up 1:1 in order (the diff's invariant), so both
    // cursors advance together outside regions.
    while base_i < base.len() && side_i < side.len() {
        let base_chg = diff.changed_old[base_i];
        let side_chg = diff.changed_new[side_i];
        if !base_chg && !side_chg {
            base_i += 1;
            side_i += 1;
            continue;
        }
        let base_lo = base_i;
        let side_lo = side_i;
        let mut lines: Vec<u8> = Vec::new();
        // Consume the maximal run of changed records on both sides; it
        // ends when both cursors sit on unchanged records.
        loop {
            let bchg = base_i < base.len() && diff.changed_old[base_i];
            let schg = side_i < side.len() && diff.changed_new[side_i];
            if !bchg && !schg {
                break;
            }
            if schg {
                lines.extend_from_slice(side[side_i]);
                side_i += 1;
            }
            if bchg {
                base_i += 1;
            }
        }
        regions.push(Region {
            base_lo,
            base_hi: base_i,
            lines,
            side_lo,
        });
    }
    // Trailing records: the diff pairs unchanged records 1:1, so any
    // record left on either cursor at loop exit is changed. All of them
    // form one final region spanning to the end of base (the side's
    // trailing records are its replacement lines; trailing base records
    // are a deletion - a region with no lines).
    if base_i < base.len() || side_i < side.len() {
        let base_lo = base_i;
        let side_lo = side_i;
        let mut lines: Vec<u8> = Vec::new();
        while side_i < side.len() {
            lines.extend_from_slice(side[side_i]);
            side_i += 1;
        }
        regions.push(Region {
            base_lo,
            base_hi: base.len(),
            lines,
            side_lo,
        });
    }
    regions
}

/// True if any region of `a` conflicts with any region of `b` in base
/// coordinates — overlapping base spans, or spans that touch (one
/// side's replacement begins exactly where the other's base span ends).
/// Touching regions conflict in git's `xdl_merge` too: the merged hunks
/// would interleave replacements of consecutive base lines from
/// different sides, which `CONFLICT (content)` covers (verified against
/// `git merge-tree --write-tree`: adjacent single-line changes from the
/// two sides conflict).
fn conflicts_with(a: &[Region], b: &[Region]) -> bool {
    for x in a {
        for y in b {
            if x.base_lo <= y.base_hi && y.base_lo <= x.base_hi {
                return true; // overlapping or touching base spans
            }
        }
    }
    false
}

/// The recursive 3-way tree walk: `(base, ours, theirs)` → the change
/// list vs OURS, or every conflicting path.
///
/// Blob bytes are read only for leaves that differ on both sides (the
/// trivial rules prune everything else). Directories equal by id are
/// never loaded. The merged directory is never materialized here — it
/// is reconstructed by applying the emitted changes, which the
/// derivation invariant asserts.
#[allow(clippy::too_many_arguments)]
fn walk(
    engine: &Engine,
    base: Option<Id>,
    ours: Option<Id>,
    theirs: Option<Id>,
    dir: &str,
    o_root: Option<Id>,
    t_root: Option<Id>,
    ours_renames: &[SideRename],
    theirs_renames: &[SideRename],
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(StoreError::Invalid(
            "tree nesting exceeds the maximum supported depth".into(),
        ));
    }
    if ours == theirs {
        return Ok(()); // two sides equal: that value (ours) — untouched
    }
    if ours == base {
        // Ours unchanged from base: adopt theirs wholesale. The emitted
        // change list is the tree diff ours → theirs, pruned by
        // content-addressed equality inside `diff_trees`.
        extend_prefixed(&*engine.objects, changes, dir, ours, theirs)?;
        return Ok(());
    }
    if theirs == base {
        return Ok(()); // theirs unchanged: keep ours
    }
    // `None` here is the empty tree, never a deleted subtree: the root
    // call passes `derive_tree`'s `Option<Id>` (None = empty root), and
    // the child recursion passes `Dir` entries' tree ids, which are
    // always `Some` (`apply` omits empty subtrees from parent entries).
    // A side that emptied its root therefore merges entry by entry
    // below — an absent name is ours' delete, and a name theirs added
    // on top of an absent base is a clean add — exactly like any other
    // divergence. (Treating a None root as "deleted the subtree" would
    // conflict theirs' root-level adds against ours' empty tree.)
    // Both sides have a directory here; merge entry by entry.
    let b_tree = load_or_default(&*engine.objects, base)?;
    let o_tree = load_or_default(&*engine.objects, ours)?;
    let t_tree = load_or_default(&*engine.objects, theirs)?;
    merge_dir(
        engine,
        &b_tree,
        &o_tree,
        &t_tree,
        dir,
        o_root,
        t_root,
        ours_renames,
        theirs_renames,
        changes,
        conflicts,
        depth,
    )
}

/// Collects the file paths at which `side`'s tree differs from `base`,
/// recursing into changed subdirectories. Used on the
/// deleted-subtree-versus-changed-subtree branches of the walk so
/// conflicts are named at file granularity, exactly like git's
/// `CONFLICT (modify/delete)` listings. Returns the collected paths.
fn changed_paths(
    engine: &Engine,
    base: Option<Id>,
    side: Option<Id>,
    dir: &str,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<Vec<String>, StoreError> {
    changed_paths_opts(engine, base, side, dir, conflicts, depth, false)
}

/// `skip_deletes`: paths the side outright DELETED (present in base,
/// absent in the side) are not reported — used when the OTHER side
/// deleted the whole subtree, making those deletions mutual and clean.
fn changed_paths_opts(
    engine: &Engine,
    base: Option<Id>,
    side: Option<Id>,
    dir: &str,
    conflicts: &mut Vec<String>,
    depth: usize,
    skip_deletes: bool,
) -> Result<Vec<String>, StoreError> {
    if depth > MAX_TREE_DEPTH {
        return Err(StoreError::Invalid(
            "tree nesting exceeds the maximum supported depth".into(),
        ));
    }
    let start = conflicts.len();
    if side == base {
        return Ok(Vec::new());
    }
    let b_tree = load_or_default(&*engine.objects, base)?;
    let s_tree = load_or_default(&*engine.objects, side)?;
    let mut names: Vec<&str> = b_tree
        .entries
        .iter()
        .chain(s_tree.entries.iter())
        .map(|e| e.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let eb = b_tree.entries.iter().find(|e| e.name == name);
        let es = s_tree.entries.iter().find(|e| e.name == name);
        if entries_eq(eb, es) {
            continue; // unchanged here and beneath (ids equal)
        }
        match (eb.map(|e| e.kind), es.map(|e| e.kind)) {
            (Some(TreeKind::Dir), Some(TreeKind::Dir)) => {
                changed_paths_opts(
                    engine,
                    eb.and_then(|e| e.tree),
                    es.and_then(|e| e.tree),
                    &join(dir, name),
                    conflicts,
                    depth + 1,
                    skip_deletes,
                )?;
            }
            (_, None) if skip_deletes => {} // mutual delete: clean
            _ => conflicts.push(join(dir, name)),
        }
    }
    Ok(conflicts[start..].to_vec())
}

/// One directory level of the walk: merges ours' entries against base
/// and theirs, appending changes vs ours and conflicts.
///
/// Rename-aware (the v0.3 merge lane): the walk is seeded with each
/// side's WHOLE-TREE renames (detected once per merge by
/// [`detect_side_renames`] over flattened trees — cross-directory
/// moves pair `dirA/f` → `dirB/g`). This level keeps the renames
/// whose source or destination resolves INSIDE this directory:
/// a rename on one side against an edit at the source path on the
/// other merges the edit into the destination; two sides renaming
/// the same source to different destinations, or one side renaming
/// what the other deleted, are conflicts (git's `rename/rename` and
/// `rename/delete`).
#[allow(clippy::too_many_arguments)]
fn merge_dir(
    engine: &Engine,
    b_tree: &Tree,
    o_tree: &Tree,
    t_tree: &Tree,
    dir: &str,
    o_root: Option<Id>,
    t_root: Option<Id>,
    tree_ours_renames: &[SideRename],
    tree_theirs_renames: &[SideRename],
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<(), StoreError> {
    // This level's renames: whole-tree paths whose parent directory
    // is exactly `dir` (the source's rename rule fires at the level
    // that owns the source name; the destination rule at the level
    // that owns the destination name).
    fn parent_of(path: &str) -> &str {
        match path.rfind('/') {
            Some(i) => &path[..i],
            None => "",
        }
    }
    let ours_renames: Vec<SideRename> = tree_ours_renames
        .iter()
        .filter(|r| parent_of(&r.src) == dir || parent_of(&r.dst) == dir)
        .cloned()
        .collect();
    let theirs_renames: Vec<SideRename> = tree_theirs_renames
        .iter()
        .filter(|r| parent_of(&r.src) == dir || parent_of(&r.dst) == dir)
        .cloned()
        .collect();
    if !ours_renames.is_empty() || !theirs_renames.is_empty() {
        return merge_dir_rename_aware(
            engine,
            b_tree,
            o_tree,
            t_tree,
            dir,
            o_root,
            t_root,
            &ours_renames,
            &theirs_renames,
            tree_ours_renames,
            tree_theirs_renames,
            changes,
            conflicts,
            depth,
        );
    }
    merge_dir_plain(
        engine,
        b_tree,
        o_tree,
        t_tree,
        dir,
        o_root,
        t_root,
        tree_ours_renames,
        tree_theirs_renames,
        changes,
        conflicts,
        depth,
    )
}

/// The rename-blind per-level walk (the v0.2 behavior, now the
/// fallback when no renames were detected on either side).
#[allow(clippy::too_many_arguments)]
fn merge_dir_plain(
    engine: &Engine,
    b_tree: &Tree,
    o_tree: &Tree,
    t_tree: &Tree,
    dir: &str,
    o_root: Option<Id>,
    t_root: Option<Id>,
    tree_ours_renames: &[SideRename],
    tree_theirs_renames: &[SideRename],
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<(), StoreError> {
    let mut names: Vec<&str> = b_tree
        .entries
        .iter()
        .chain(o_tree.entries.iter())
        .chain(t_tree.entries.iter())
        .map(|e| e.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        let eb = b_tree.entries.iter().find(|e| e.name == name);
        let eo = o_tree.entries.iter().find(|e| e.name == name);
        let et = t_tree.entries.iter().find(|e| e.name == name);
        merge_entry(
            engine,
            eb,
            eo,
            et,
            dir,
            name,
            o_root,
            t_root,
            tree_ours_renames,
            tree_theirs_renames,
            changes,
            conflicts,
            depth,
        )?;
    }
    Ok(())
}

/// The rename-aware per-level walk: `ours_renames`/`theirs_renames`
/// are this level's WHOLE-TREE moves (filtered by [`merge_dir`] to
/// ones with an endpoint in this directory); every other entry merges
/// by the ordinary rules. Rename keys match this level's NAMES (the
/// last path component); emitted paths are the whole-tree paths.
#[allow(clippy::too_many_arguments)]
fn merge_dir_rename_aware(
    engine: &Engine,
    b_tree: &Tree,
    o_tree: &Tree,
    t_tree: &Tree,
    dir: &str,
    o_root: Option<Id>,
    t_root: Option<Id>,
    ours_renames: &[SideRename],
    theirs_renames: &[SideRename],
    tree_ours_renames: &[SideRename],
    tree_theirs_renames: &[SideRename],
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<(), StoreError> {
    // Index the renames by this level's NAME (the last path component
    // of the whole-tree source/destination path).
    fn base_name(p: &str) -> &str {
        p.rsplit('/').next().unwrap_or(p)
    }
    let ours_by_src: HashMap<&str, &SideRename> = ours_renames
        .iter()
        .map(|r| (base_name(&r.src), r))
        .collect();
    let theirs_by_src: HashMap<&str, &SideRename> = theirs_renames
        .iter()
        .map(|r| (base_name(&r.src), r))
        .collect();
    let ours_by_dst: HashMap<&str, &SideRename> = ours_renames
        .iter()
        .map(|r| (base_name(&r.dst), r))
        .collect();
    let theirs_by_dst: HashMap<&str, &SideRename> = theirs_renames
        .iter()
        .map(|r| (base_name(&r.dst), r))
        .collect();

    // Names to walk: base ∪ ours ∪ theirs (all three levels' names).
    let mut names: Vec<&str> = b_tree
        .entries
        .iter()
        .chain(o_tree.entries.iter())
        .chain(t_tree.entries.iter())
        .map(|e| e.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();

    // A name consumed by a rename rule is skipped in the plain walk.
    let mut consumed: HashSet<&str> = HashSet::new();

    // ---- rename/rename (same source, different destinations): the
    // 1-to-2 conflict. Git reports CONFLICT (rename/rename) and names
    // source + both destinations.
    for (src, or_) in &ours_by_src {
        if let Some(tr) = theirs_by_src.get(src)
            && or_.dst != tr.dst {
                conflicts.push(or_.src.clone());
                conflicts.push(or_.dst.clone());
                conflicts.push(tr.dst.clone());
                consumed.insert(base_name(src));
                consumed.insert(base_name(&or_.dst));
                consumed.insert(base_name(&tr.dst));
            }
    }

    for name in &names {
        if consumed.contains(name) {
            continue;
        }
        let eb = b_tree.entries.iter().find(|e| e.name == *name);
        let eo = o_tree.entries.iter().find(|e| e.name == *name);
        let et = t_tree.entries.iter().find(|e| e.name == *name);
        let path = join(dir, name);

        // ---- rename/delete: one side renamed the source, the other
        // deleted it (and did not re-add at the destination). Git:
        // CONFLICT (rename/delete), the renamed copy left in the tree
        // and both endpoints reported. Gardenbed refuses with both
        // endpoints named.
        if let Some(or_) = ours_by_src.get(name) {
            // Ours renamed `name` -> or_.dst. Theirs deleted `name`?
            // A theirs rename FROM the same source is not a delete
            // (the 1-to-2 case above already consumed different
            // destinations; the same destination merges below).
            if et.is_none()
                && eb.is_some()
                && !theirs_by_dst.contains_key(base_name(&or_.dst))
                && !theirs_by_src.contains_key(name)
            {
                conflicts.push(path.clone());
                conflicts.push(or_.dst.clone());
                consumed.insert(base_name(&or_.dst));
                continue;
            }
        }
        if let Some(tr) = theirs_by_src.get(name)
            && eo.is_none()
                && eb.is_some()
                && !ours_by_dst.contains_key(base_name(&tr.dst))
                && !ours_by_src.contains_key(name)
            {
                conflicts.push(path.clone());
                conflicts.push(tr.dst.clone());
                consumed.insert(base_name(&tr.dst));
                continue;
            }

        // ---- rename vs edit at the source path (ours renamed, theirs
        // edited the same source content): the edit merges INTO the
        // destination via a 3-way content merge over (base content at
        // src, ours' dst content, theirs' src content). Theirs leaving
        // the source UNCHANGED is not this case — the plain rules at
        // both endpoints already express ours' rename (deleted source,
        // added destination).
        if let Some(or_) = ours_by_src.get(name)
            && et.is_some() && et.expect("checked").kind == TreeKind::File && !entries_eq(et, eb)
                && let Some(dst_entry) = engine.entry_at(o_root, &or_.dst)? {
                    let dst_path = or_.dst.clone();
                    // OURS' entry at the DESTINATION (None when the
                    // destination is not in ours' tree): the change is
                    // vs ours, so the no-op prune compares there.
                    merge_rename_edit(
                        engine,
                        &or_.src_base,
                        &dst_entry,
                        et.expect("checked above"),
                        engine.entry_at(o_root, &dst_path)?.as_ref(),
                        &dst_path,
                        changes,
                        conflicts,
                    )?;
                    consumed.insert(base_name(&or_.dst));
                    continue;
                }
        if let Some(tr) = theirs_by_src.get(name)
            && eo.is_some() && eo.expect("checked").kind == TreeKind::File && !entries_eq(eo, eb)
                && let Some(dst_entry) = engine.entry_at(t_root, &tr.dst)? {
                    let dst_path = tr.dst.clone();
                    // OURS still holds the edited source (`eo`); the
                    // merged content lands at the destination, and the
                    // source must GO — ours' tree still has it, so the
                    // change list (which is vs OURS) needs the delete.
                    let src_path = path.clone();
                    let src_eo = eo.expect("checked above");
                    merge_rename_edit(
                        engine,
                        &tr.src_base,
                        src_eo,
                        &dst_entry,
                        engine.entry_at(o_root, &dst_path)?.as_ref(),
                        &dst_path,
                        changes,
                        conflicts,
                    )?;
                    delete(Some(src_eo), &src_path, changes);
                    consumed.insert(base_name(&tr.dst));
                    continue;
                }

        // ---- both sides renamed the same source to the SAME
        // destination: a 3-way content merge at the destination with
        // the source's base entry as the base (the rename's common
        // ancestor — without it the destination looks like add/add).
        if let Some(or_) = ours_by_dst.get(name)
            && let Some(tr) = theirs_by_dst.get(name)
                && or_.src == tr.src
                    && let (Some(dst_o), Some(dst_t)) = (
                        o_tree.entries.iter().find(|e| e.name == *name),
                        t_tree.entries.iter().find(|e| e.name == *name),
                    ) {
                        merge_rename_edit(
                            engine,
                            &or_.src_base,
                            dst_o,
                            dst_t,
                            eo,
                            &or_.dst,
                            changes,
                            conflicts,
                        )?;
                        continue;
                    }

        // ---- a rename destination colliding with a path the other
        // side has or added (rename/rename 2-to-1, or rename onto an
        // occupied path): conflict at the destination.
        if let Some(or_) = ours_by_dst.get(name)
            && et.is_some()
                && theirs_by_dst.get(name).map(|r| r.src.as_str()) != Some(or_.src.as_str())
            {
                conflicts.push(path.clone());
                continue;
            }
        if let Some(tr) = theirs_by_dst.get(name)
            && eo.is_some()
                && ours_by_dst.get(name).map(|r| r.src.as_str()) != Some(tr.src.as_str())
            {
                conflicts.push(path.clone());
                continue;
            }

        merge_entry(
            engine,
            eb,
            eo,
            et,
            dir,
            name,
            o_root,
            t_root,
            tree_ours_renames,
            tree_theirs_renames,
            changes,
            conflicts,
            depth,
        )?;
    }
    Ok(())
}

/// The 3-way file merge behind a rename: `base_entry` is the pre-rename
/// source entry, `ours_entry` and `theirs_entry` the two sides' entries
/// at the surviving path (the renaming side's destination, the other
/// side's edited source). Mode merges by the trivial rules; content
/// merges by `merge3` over the three blobs. The change is emitted at
/// `dst_path` vs OURS (`eo` is ours' entry at that path, `None` when
/// ours' tree lacks the destination).
#[allow(clippy::too_many_arguments)]
fn merge_rename_edit(
    engine: &Engine,
    base_entry: &TreeEntry,
    ours_entry: &TreeEntry,
    theirs_entry: &TreeEntry,
    eo: Option<&TreeEntry>,
    dst_path: &str,
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
) -> Result<(), StoreError> {
    let eb = Some(base_entry);
    let o = ours_entry;
    let t = theirs_entry;
    merge_file(engine, eb, eo, o, t, dst_path, changes, conflicts)
}

/// The per-entry merge rules (spec §4). Emits the change vs OURS for
/// non-conflicting outcomes; conflicts are recorded by the caller from
/// the returned `Err(path)`-free [`Verdict::Conflict`].
#[allow(clippy::too_many_arguments)]
fn merge_entry(
    engine: &Engine,
    eb: Option<&TreeEntry>,
    eo: Option<&TreeEntry>,
    et: Option<&TreeEntry>,
    dir: &str,
    name: &str,
    o_root: Option<Id>,
    t_root: Option<Id>,
    tree_ours_renames: &[SideRename],
    tree_theirs_renames: &[SideRename],
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
    depth: usize,
) -> Result<(), StoreError> {
    let path = join(dir, name);
    // Two sides equal: that value.
    if entries_eq(eo, et) {
        return Ok(());
    }
    // One side == base: the other side.
    if entries_eq(eo, eb) {
        // Ours unchanged from base: adopt theirs wholesale.
        return match (eo, et) {
            // Directory -> directory: the tree diff of the two subtree
            // ids (pruned by content addressing inside `diff_trees`).
            (Some(_), Some(e)) if e.kind == TreeKind::Dir => {
                extend_prefixed(
                    &*engine.objects,
                    changes,
                    &path,
                    eo.and_then(|o| o.tree),
                    e.tree,
                )?;
                Ok(())
            }
            // Theirs added a directory where neither base nor ours had
            // one (ours == base == absent): a pure add — diff it in
            // from nothing (`take` is file-only: a Dir entry carries a
            // tree id, not a blob).
            (None, Some(e)) if e.kind == TreeKind::Dir => {
                extend_prefixed(&*engine.objects, changes, &path, None, e.tree)?;
                Ok(())
            }
            // Anything else present on theirs' side is a file entry
            // (ours matched base, so kinds agree unless theirs changed
            // them — and a changed entry on an unchanged ours is simply
            // adopted): one Modify/Add.
            (_, Some(e)) => {
                take(eo, e, &path, changes);
                Ok(())
            }
            (Some(e), None) => {
                // This arm only runs with ours == base (the wholesale
                // guard above), so theirs' delete is clean. A file is
                // one Delete; a directory must diff out (a single
                // dir-path Delete would remove the whole subtree in
                // apply, which is legal but not the canonical flat
                // form diff itself emits).
                match e.kind {
                    TreeKind::File => {
                        delete(Some(e), &path, changes);
                    }
                    TreeKind::Dir => {
                        extend_prefixed(&*engine.objects, changes, &path, e.tree, None)?;
                    }
                }
                Ok(())
            }
            (None, None) => Ok(()),
        };
    }
    if entries_eq(et, eb) {
        return Ok(()); // theirs unchanged: keep ours
    }
    // Both sides changed this name.
    match (eo.map(|e| (e.kind, e)), et.map(|e| (e.kind, e))) {
        (Some((TreeKind::Dir, o)), Some((TreeKind::Dir, t))) => {
            // Both directories: recurse. Equal subtree ids were pruned
            // by the trivial rules above (entries_eq compares tree ids).
            walk(
                engine,
                eb.and_then(|e| e.tree),
                o.tree,
                t.tree,
                &path,
                o_root,
                t_root,
                tree_ours_renames,
                tree_theirs_renames,
                changes,
                conflicts,
                depth + 1,
            )
        }
        (Some((TreeKind::File, o)), Some((TreeKind::File, t))) => {
            merge_file(engine, eb, eo, o, t, &path, changes, conflicts)
        }
        // File on one side, directory on the other, both changed:
        // git's `CONFLICT (file/directory)`. Git reports the conflict
        // at file granularity — the changed paths under the directory
        // side (its `~branch`-renamed counterpart path is not listed).
        (Some((TreeKind::Dir, d)), Some((TreeKind::File, _))) => {
            changed_paths(
                engine,
                eb.and_then(|e| e.tree),
                d.tree,
                &path,
                conflicts,
                depth,
            )?;
            Ok(())
        }
        (Some((TreeKind::File, _)), Some((TreeKind::Dir, d))) => {
            changed_paths(
                engine,
                eb.and_then(|e| e.tree),
                d.tree,
                &path,
                conflicts,
                depth,
            )?;
            Ok(())
        }
        (None, Some((TreeKind::Dir, t))) => {
            // Ours lacks it; theirs has a directory here and changed it
            // vs base (added or replaced): conflict at file granularity.
            // Paths theirs also deleted are mutual deletes — clean
            // (ours deleted the whole entry).
            changed_paths_opts(
                engine,
                eb.and_then(|e| e.tree),
                t.tree,
                &path,
                conflicts,
                depth,
                true,
            )?;
            if eb.is_none() {
                // Base lacked the directory too but ours deleted...
                // unreachable (eo is None and base None means both
                // absent; the trivial rules returned early). Keep a
                // defensive conflict on the entry itself.
                conflicts.push(path);
            }
            Ok(())
        }
        (None, Some((TreeKind::File, _))) => {
            // Ours lacks it; theirs has a file here and changed it vs
            // base: delete (ours) vs modify (theirs): conflict.
            conflicts.push(path);
            Ok(())
        }
        (Some(eo_entry), None) => {
            // Ours has it (changed vs base), theirs lacks it.
            if eb.is_none() {
                // Ours added it, theirs never had it: keep ours.
                return Ok(());
            }
            // modify (ours) vs delete (theirs): conflict at file-path
            // granularity — a directory entry recurses (git reports
            // modify/delete per file).
            let eo_entry = eo_entry.1;
            match eo_entry.kind {
                TreeKind::Dir => {
                    // Paths ours also deleted are mutual deletes —
                    // clean (theirs deleted the whole entry).
                    changed_paths_opts(
                        engine,
                        eb.and_then(|e| e.tree),
                        eo_entry.tree,
                        &path,
                        conflicts,
                        depth,
                        true,
                    )?;
                    Ok(())
                }
                TreeKind::File => {
                    conflicts.push(path);
                    Ok(())
                }
            }
        }
        (None, None) => Ok(()),
    }
}

/// The both-changed file-leaf merge: mode merge by the trivial rules,
/// then content merge for regular/exec leaves; symlinks and gitlinks
/// are atomic (id equality only).
#[allow(clippy::too_many_arguments)]
fn merge_file(
    engine: &Engine,
    eb: Option<&TreeEntry>,
    eo: Option<&TreeEntry>,
    o: &TreeEntry,
    t: &TreeEntry,
    path: &str,
    changes: &mut Vec<Change>,
    conflicts: &mut Vec<String>,
) -> Result<(), StoreError> {
    let ob = o.blob.expect("file entry carries a blob");
    let tb = t.blob.expect("file entry carries a blob");
    let omode = o.mode;
    let tmode = t.mode;
    let bmode = eb.filter(|e| e.kind == TreeKind::File).map(|e| e.mode);
    // Mode merge: the trivial rules. Atomic classes never blend — a
    // class flip on one side with the other side changed conflicts
    // (git's `CONFLICT (distinct types)`).
    let Some(mode) = merge_mode(bmode, omode, tmode) else {
        record_conflict(path, conflicts);
        return Ok(());
    };
    let atomic = |m: Mode| matches!(m, Mode::Symlink | Mode::Gitlink);
    if atomic(mode) || atomic(omode) || atomic(tmode) {
        // Atomic entries: id equality only. The trivial rules already
        // handled equality, so both-changed atomic entries conflict.
        if ob == tb {
            take(eo, t, path, changes);
            return Ok(());
        }
        record_conflict(path, conflicts);
        return Ok(());
    }
    // Regular/exec leaves differing on both sides: per-file 3-way
    // content merge. The base blob participates only when the base
    // entry is a file of a non-gitlink class (a file↔symlink flip has
    // no line-mergeable base).
    let base_content = match eb {
        Some(b)
            if b.kind == TreeKind::File
                && b.mode != Mode::Gitlink
                && omode != Mode::Symlink
                && tmode != Mode::Symlink =>
        {
            engine
                .objects
                .get_blob(b.blob.expect("file entry carries a blob"))?
        }
        _ => None,
    };
    let ours_content = engine.objects.get_blob(ob)?;
    let theirs_content = engine.objects.get_blob(tb)?;
    match merge3(
        base_content.as_deref(),
        ours_content.as_deref(),
        theirs_content.as_deref(),
    ) {
        Merge3Outcome::Merged(bytes) => {
            let blob = engine.objects.put_blob(&bytes)?;
            let merged = TreeEntry {
                name: t.name.clone(),
                kind: TreeKind::File,
                mode,
                blob: Some(blob),
                tree: None,
            };
            take(eo, &merged, path, changes);
            Ok(())
        }
        Merge3Outcome::Conflict => {
            record_conflict(path, conflicts);
            Ok(())
        }
    }
}

/// Records a conflict at `path`.
fn record_conflict(path: &str, conflicts: &mut Vec<String>) {
    conflicts.push(path.to_owned());
}

/// The mode-merge trivial rules for file leaves.
///
/// `None` = conflicting modes. Modes merge by the same table as
/// content: equal sides win, a side equal to base yields the other.
fn merge_mode(base: Option<Mode>, ours: Mode, theirs: Mode) -> Option<Mode> {
    if ours == theirs {
        return Some(ours);
    }
    if Some(ours) == base {
        return Some(theirs);
    }
    if Some(theirs) == base {
        return Some(ours);
    }
    // Both flipped the mode differently (including class flips): a mode
    // that is neither side's would be new truth — conflict.
    None
}

/// Emits the change vs ours adopting `new` at `path` (a no-op when the
/// adoption is byte-identical to ours).
fn take(eo: Option<&TreeEntry>, new: &TreeEntry, path: &str, changes: &mut Vec<Change>) {
    let blob = new.blob.expect("merged file entry carries a blob");
    if let Some(old) = eo
        && old.blob == Some(blob)
        && old.mode == new.mode
    {
        return;
    }
    let op = if eo.is_some() { Op::Modify } else { Op::Add };
    changes.push(Change {
        path: path.to_owned(),
        op,
        blob: Some(blob),
        src_path: None,
        mode: new.mode,
    });
}

/// Emits the Delete change vs ours at `path` (ours has it; the merged
/// tree does not).
fn delete(eo: Option<&TreeEntry>, path: &str, changes: &mut Vec<Change>) {
    if eo.is_none() {
        return;
    }
    changes.push(Change {
        path: path.to_owned(),
        op: Op::Delete,
        blob: None,
        src_path: None,
        mode: Mode::Regular,
    });
}

/// Appends a subtree-relative tree diff to the change list, rewriting
/// each path onto the `prefix` root (diff operands are subtree ids;
/// the emitted change list is root-relative).
fn extend_prefixed(
    objects: &dyn ObjectStore,
    changes: &mut Vec<Change>,
    prefix: &str,
    from: Option<Id>,
    to: Option<Id>,
) -> Result<(), StoreError> {
    let diff = crate::diff::diff_trees(objects, from, to)?;
    if prefix.is_empty() {
        changes.extend(diff);
        return Ok(());
    }
    for mut c in diff {
        c.path = format!("{prefix}/{}", c.path);
        if let Some(src) = &mut c.src_path {
            *src = format!("{prefix}/{src}");
        }
        changes.push(c);
    }
    Ok(())
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn entries_eq(a: Option<&TreeEntry>, b: Option<&TreeEntry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.kind == y.kind && x.mode == y.mode && x.blob == y.blob && x.tree == y.tree
        }
        _ => false,
    }
}

fn load_or_default(objects: &dyn ObjectStore, id: Option<Id>) -> Result<Tree, StoreError> {
    match id {
        None => Ok(Tree::default()),
        Some(id) => Engine::load_tree(objects, id),
    }
}

/// Bound on the recursive virtual-base fold's descent (criss-cross
/// stacks of stacks). Beyond it the fold falls back to the first base
/// in the deterministic order — a documented limit, not a mis-merge.
const MAX_RECURSIVE_BASE_DEPTH: usize = 16;

impl Engine {
    /// The 3-way merge of the trees of `ours` and `theirs` over their
    /// merge base, as a change list vs ours. Conflicts (content or
    /// tree-level) are collected across the whole walk and returned
    /// together, sorted; nothing is stored for a conflicting merge
    /// except merged blob content (unreferenced, harmless by
    /// construction).
    ///
    /// This method computes and **writes** merged blobs
    /// (content-addressed, idempotent) but never writes a changeset
    /// row — the caller stores the changeset through the ordinary
    /// create path.
    pub fn merge_tree_changes(
        &self,
        base: Option<Id>,
        ours: Id,
        theirs: Id,
    ) -> Result<Result<Vec<Change>, Vec<String>>, StoreError> {
        let o_tree = self.derive_tree(ours)?;
        let t_tree = self.derive_tree(theirs)?;
        let b_tree = match base {
            Some(b) => self.derive_tree(b)?,
            None => None,
        };
        self.merge_tree_ids(b_tree, o_tree, t_tree)
    }

    /// The 3-way walk over raw TREE ids (`None` = empty): the body of
    /// [`Engine::merge_tree_changes`] split out so the recursive
    /// virtual-base fold (which produces a tree, not a changeset) can
    /// run the exact same merge code.
    /// Resolves a whole-tree file path to its entry in the tree rooted
    /// at `root` (None when absent, or when a path component is not a
    /// directory). Used by the rename rules to reach a rename's
    /// destination when it lives in a DIFFERENT directory than the
    /// source (the per-level walk only holds this level's entries).
    fn entry_at(&self, root: Option<Id>, path: &str) -> Result<Option<TreeEntry>, StoreError> {
        let mut tree_id = root;
        let components: Vec<&str> = path.split('/').collect();
        for (i, comp) in components.iter().enumerate() {
            let Some(id) = tree_id else { return Ok(None) };
            let tree = Engine::load_tree(&*self.objects, id)?;
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                return Ok(None);
            };
            if i == components.len() - 1 {
                return Ok(Some(entry.clone()));
            }
            tree_id = entry.tree;
        }
        Ok(None)
    }

    /// Whole-tree rename/delete screening (git's CONFLICT
    /// (rename/delete)): one side renamed `src` → `dst` while the
    /// other side deleted `src` outright (it has no rename of its own
    /// from that source). Both endpoints are named.
    fn screen_rename_deletes(
        &self,
        o_tree: Option<Id>,
        t_tree: Option<Id>,
        ours_renames: &[SideRename],
        theirs_renames: &[SideRename],
        conflicts: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        for r in ours_renames {
            if self.entry_at(t_tree, &r.src)?.is_none()
                && !theirs_renames.iter().any(|x| x.src == r.src)
            {
                conflicts.push(r.src.clone());
                conflicts.push(r.dst.clone());
            }
        }
        for r in theirs_renames {
            if self.entry_at(o_tree, &r.src)?.is_none()
                && !ours_renames.iter().any(|x| x.src == r.src)
            {
                conflicts.push(r.src.clone());
                conflicts.push(r.dst.clone());
            }
        }
        Ok(())
    }

    /// Whole-tree rename/rename 1-to-2 screening (git's CONFLICT
    /// (rename/rename)): both sides renamed the same source to
    /// different destinations. Source and both destinations named.
    /// (Same-destination renames are NOT a conflict — they merge at
    /// the destination by the level rules.)
    fn screen_rename_rename_1to2(
        &self,
        ours_renames: &[SideRename],
        theirs_renames: &[SideRename],
        conflicts: &mut Vec<String>,
    ) {
        for or_ in ours_renames {
            if let Some(tr) = theirs_renames.iter().find(|x| x.src == or_.src)
                && or_.dst != tr.dst {
                    conflicts.push(or_.src.clone());
                    conflicts.push(or_.dst.clone());
                    conflicts.push(tr.dst.clone());
                }
        }
    }

    /// One side's whole-tree renames over (base, side) trees: flattened
    /// file listings keyed by path, screened by [`detect_side_renames`].
    /// Returns empty for equal trees (the common no-rename case).
    fn side_renames(
        &self,
        base: Option<Id>,
        side: Option<Id>,
    ) -> Result<Vec<SideRename>, StoreError> {
        if base == side {
            return Ok(Vec::new());
        }
        let flatten = |root: Option<Id>| -> Result<BTreeMap<String, TreeEntry>, StoreError> {
            let mut out = BTreeMap::new();
            for (path, mode, blob) in self.flatten_tree(root)? {
                out.insert(
                    path,
                    TreeEntry {
                        name: String::new(),
                        kind: TreeKind::File,
                        mode,
                        blob: Some(blob),
                        tree: None,
                    },
                );
            }
            Ok(out)
        };
        let b = flatten(base)?;
        let o = flatten(side)?;
        detect_side_renames(&*self.objects, &b, &o)
    }

    fn merge_tree_ids(
        &self,
        b_tree: Option<Id>,
        o_tree: Option<Id>,
        t_tree: Option<Id>,
    ) -> Result<Result<Vec<Change>, Vec<String>>, StoreError> {
        let mut changes: Vec<Change> = Vec::new();
        let mut conflicts: Vec<String> = Vec::new();
        // Whole-tree rename screening (once per merge, git's
        // diffcore_rename scope): each side's (deleted, added) FILE
        // paths over the flattened trees, paired exact-first then
        // scored. Cross-directory moves pair here — the per-level walk
        // below consumes them at whichever level owns each endpoint.
        let ours_renames = self.side_renames(b_tree, o_tree)?;
        let theirs_renames = self.side_renames(b_tree, t_tree)?;
        // Whole-tree rename-conflict screening (rename/delete and
        // rename/rename 1-to-2). These are WHOLE-TREE facts the
        // per-level rules cannot always see: the source's directory
        // can come out identical on both sides (each side's rename or
        // delete removes the same source entry), so the level walk
        // short-circuits on `ours == theirs` before any rename rule
        // runs. Screening once here (against the full trees) names
        // every endpoint; the walk below still collects any further
        // conflicts.
        self.screen_rename_deletes(
            o_tree,
            t_tree,
            &ours_renames,
            &theirs_renames,
            &mut conflicts,
        )?;
        self.screen_rename_rename_1to2(&ours_renames, &theirs_renames, &mut conflicts);
        walk(
            self,
            b_tree,
            o_tree,
            t_tree,
            "",
            o_tree,
            t_tree,
            &ours_renames,
            &theirs_renames,
            &mut changes,
            &mut conflicts,
            0,
        )?;
        if !conflicts.is_empty() {
            conflicts.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            conflicts.dedup();
            return Ok(Err(conflicts));
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(Ok(changes))
    }

    /// The 3-way tree merge with **recursive multi-base resolution**
    /// (git's `merge-recursive`/`merge-ort` shape): computes every
    /// maximal common ancestor of `ours` and `theirs`
    /// ([`Engine::merge_bases_all`], deterministically ordered) and,
    /// when there is more than one, folds them into one **virtual
    /// base tree** before the ordinary walk.
    ///
    /// The fold is itself a sequence of 3-way tree merges —
    /// `virtual = merge(merge(bases[0], bases[1] over base-of-bases),
    /// bases[2], …)` — computed against the bases' own merge base,
    /// recursively, with [`MAX_RECURSIVE_BASE_DEPTH`] bounding the
    /// descent. A conflicting base fold embeds the conflict by
    /// *dropping the conflicting path from the virtual base* (the path
    /// then merges as if absent from the base, which surfaces the
    /// real disagreement to the outer merge instead of hiding it
    /// behind an arbitrary pick): the virtual base is a derived,
    /// never-stored tree, so nothing partial is ever persisted.
    ///
    /// A unique merge base skips the fold entirely — the behavior is
    /// byte-identical to [`Engine::merge_tree_changes`] for every
    /// non-criss-cross merge (the v0.2 contract preserved).
    ///
    /// Determinism: the base order is `(committer seconds desc, id
    /// bytes desc)`; the fold is a pure function of immutable trees;
    /// wiped caches reproduce identical merges.
    pub fn merge_tree_changes_recursive(
        &self,
        ours: Id,
        theirs: Id,
    ) -> Result<Result<Vec<Change>, Vec<String>>, StoreError> {
        let bases = self.merge_bases_all(ours, theirs)?;
        match bases.len() {
            0 => {
                // Unrelated histories: the caller (Repo::merge)
                // refuses before reaching here; at the engine level
                // an empty base is the empty-tree merge.
                self.merge_tree_changes(None, ours, theirs)
            }
            1 => self.merge_tree_changes(Some(bases[0]), ours, theirs),
            _ => {
                let virtual_base = self.fold_virtual_base(&bases, 0)?;
                let o_tree = self.derive_tree(ours)?;
                let t_tree = self.derive_tree(theirs)?;
                self.merge_tree_ids(virtual_base, o_tree, t_tree)
            }
        }
    }

    /// The octopus fold round: a 3-way merge of raw TREE ids over an
    /// EXPLICIT base list (the accumulated references' common
    /// ancestors — the caller computes them, because the accumulated
    /// merge-result tree has no changeset of its own to walk ancestry
    /// from). One base is the ordinary merge; several fold into a
    /// virtual base first, exactly as [`Engine::merge_tree_changes_recursive`].
    pub fn merge_tree_changes_over_trees(
        &self,
        bases: &[Id],
        ours_tree: Option<Id>,
        theirs_tree: Option<Id>,
    ) -> Result<Result<Vec<Change>, Vec<String>>, StoreError> {
        // `bases` are CHANGESET ids (octopus accumulates references);
        // derive each base's tree before the tree-level walk.
        let base_trees: Vec<Option<Id>> = bases
            .iter()
            .map(|&b| self.derive_tree(b))
            .collect::<Result<_, _>>()?;
        match base_trees.len() {
            0 => self.merge_tree_ids(None, ours_tree, theirs_tree),
            1 => self.merge_tree_ids(base_trees[0], ours_tree, theirs_tree),
            _ => {
                let virtual_base = self.fold_virtual_base(bases, 0)?;
                self.merge_tree_ids(virtual_base, ours_tree, theirs_tree)
            }
        }
    }

    /// Folds `bases` (deterministically ordered) into one virtual base
    /// tree id, left-to-right: `acc = merge(acc, bases[i])` over
    /// `merge_base(acc, bases[i])`. Conflicting paths drop out of the
    /// virtual base (see [`Engine::merge_tree_changes_recursive`]).
    /// Folds `bases` (deterministically ordered, ALL changeset ids)
    /// into one virtual base TREE id (`None` = empty), left-to-right:
    /// `acc = merge(acc, bases[i])` over `merge_bases_all(acc_cs,
    /// bases[i])`, with conflicts resolved by dropping the conflicting
    /// paths from the virtual base (see
    /// [`Engine::merge_tree_changes_recursive`]).
    ///
    /// NOTE on ids: the fold works in TREE space internally
    /// (`acc_tree`), but `merge_tree_changes` and
    /// `merge_bases_all` take CHANGESET ids — the fold tracks the
    /// changeset whose tree `acc_tree` currently is (`acc_cs`) so
    /// ancestry stays real. The first base's changeset anchors it.
    fn fold_virtual_base(&self, bases: &[Id], depth: usize) -> Result<Option<Id>, StoreError> {
        if depth > MAX_RECURSIVE_BASE_DEPTH {
            // Bounded descent: fall back to the first base in the
            // deterministic order (a documented limit; git has no
            // such cap but pathological criss-cross stacks are not
            // constructible through ordinary merges).
            return self.derive_tree(bases[0]);
        }
        let mut acc_tree = self.derive_tree(bases[0])?;
        let mut acc_cs = bases[0];
        for &b in &bases[1..] {
            let inner_bases = self.merge_bases_all(acc_cs, b)?;
            let inner_base_cs = inner_bases.first().copied();
            // The tree id of the inner base (None = empty tree).
            let inner_base_tree = match inner_base_cs {
                Some(x) => self.derive_tree(x)?,
                None => None,
            };
            let b_tree = self.derive_tree(b)?;
            let acc_b = self.merge_tree_ids(inner_base_tree, acc_tree, b_tree)?;
            let changes = match acc_b {
                Ok(changes) => changes,
                Err(conflicts) => {
                    // Conflicting fold: recompute the fold WITHOUT the
                    // conflicting paths — each conflicting path drops
                    // from the virtual base, so the outer merge sees it
                    // as base-absent (both sides changed) instead of
                    // anchored to an arbitrary side.
                    self.virtual_base_dropping(inner_base_tree, acc_tree, b_tree, &conflicts)?
                }
            };
            // Apply the fold's changes onto the accumulator tree.
            let folded = gardenbed_model::Changeset {
                parents: vec![acc_cs],
                changes,
                message: "virtual base fold".into(),
                author: gardenbed_model::Identity {
                    name: "gardenbed".into(),
                    email: "virtual@base".into(),
                    time_secs: 0,
                    tz_min: 0,
                },
                committer: gardenbed_model::Identity {
                    name: "gardenbed".into(),
                    email: "virtual@base".into(),
                    time_secs: 0,
                    tz_min: 0,
                },
                signature: None,
            };
            acc_tree = self.apply_changeset_to_tree(acc_tree, &folded)?;
            // The accumulator's tree is now the merged fold; the
            // changeset anchor stays the LAST merged base for the
            // next round's ancestry (the fold's result is
            // tree-identical to that merge, and its ancestry is a
            // superset through the merged pair).
            acc_cs = b;
        }
        Ok(acc_tree)
    }

    /// The fold step over a conflicting base pair: the change list
    /// that turns the accumulator TREE into the pruned union —
    /// `b`'s non-conflicting changes vs the inner base ride along,
    /// and every conflicting path is DELETED from the result (the
    /// virtual base simply lacks those paths, so the outer merge
    /// treats them as base-absent). All arguments are TREE ids
    /// (`None` = empty tree).
    fn virtual_base_dropping(
        &self,
        inner_base: Option<Id>,
        acc_tree: Option<Id>,
        t_tree: Option<Id>,
        conflicts: &[String],
    ) -> Result<Vec<Change>, StoreError> {
        let mut changes = Vec::new();
        // Theirs' side of the diff vs the inner base, minus conflicts.
        let diff = crate::diff::diff_trees(&*self.objects, inner_base, t_tree)?;
        for mut c in diff {
            if conflicts.iter().any(|p| p == &c.path) {
                continue;
            }
            // A rename fact from the tree diff is irrelevant to the
            // fold's change list; normalize to add/modify.
            c.src_path = None;
            changes.push(c);
        }
        // Conflicting paths present in the accumulator are DELETED
        // from the virtual base.
        let o_files = self.flatten_tree(acc_tree)?;
        for (path, _mode, _blob) in o_files {
            if conflicts.iter().any(|p| p == &path) {
                changes.push(Change {
                    path,
                    op: Op::Delete,
                    blob: None,
                    src_path: None,
                    // Canonical deletes carry Mode::Regular.
                    mode: Mode::Regular,
                });
            }
        }
        changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        changes.dedup_by(|a, b| a.path == b.path);
        Ok(changes)
    }
}

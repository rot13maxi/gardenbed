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

use gardenbed_model::{Change, Id, Mode, Op, Tree, TreeEntry, TreeKind};
use gardenbed_store::{ObjectStore, StoreError};

use crate::Engine;
use crate::apply::MAX_TREE_DEPTH;
use crate::xdiff;

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
    if ours.is_none() {
        // Ours lacks the directory. Base did too: theirs added content
        // here — a pure add, adopted wholesale.
        if base.is_none() {
            extend_prefixed(&*engine.objects, changes, dir, None, theirs)?;
            return Ok(());
        }
        // Ours deleted this subtree; theirs changed something inside.
        // Conflict at file-path granularity: the paths theirs changed
        // vs base are the modify/delete conflicts git reports. Paths
        // theirs also deleted are mutual deletes — clean.
        changed_paths_opts(engine, base, theirs, dir, conflicts, depth, true)?;
        return Ok(());
    }
    if theirs.is_none() {
        // Theirs deleted this subtree (base had it); ours changed it.
        // If ours changed nothing vs base, the merge is theirs' delete;
        // otherwise the changed paths conflict.
        if changed_paths(engine, base, ours, dir, &mut Vec::new(), depth)?.is_empty() {
            extend_prefixed(&*engine.objects, changes, dir, ours, None)?;
        } else {
            changed_paths_opts(engine, base, ours, dir, conflicts, depth, true)?;
        }
        return Ok(());
    }
    // Both sides have a directory here; merge entry by entry.
    let b_tree = load_or_default(&*engine.objects, base)?;
    let o_tree = load_or_default(&*engine.objects, ours)?;
    let t_tree = load_or_default(&*engine.objects, theirs)?;
    merge_dir(
        engine, &b_tree, &o_tree, &t_tree, dir, changes, conflicts, depth,
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
#[allow(clippy::too_many_arguments)]
fn merge_dir(
    engine: &Engine,
    b_tree: &Tree,
    o_tree: &Tree,
    t_tree: &Tree,
    dir: &str,
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
        merge_entry(engine, eb, eo, et, dir, name, changes, conflicts, depth)?;
    }
    Ok(())
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
        let mut changes: Vec<Change> = Vec::new();
        let mut conflicts: Vec<String> = Vec::new();
        walk(
            self,
            b_tree,
            o_tree,
            t_tree,
            "",
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
}

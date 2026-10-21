//! The import driver: walk the graph, classify diffs, ingest changesets.
//!
//! Per commit (in topological order, batched for diff-tree):
//!
//! 1. blobs referenced by the commit's changes are mirrored into objects
//!    (content-addressed: written exactly once per distinct content),
//! 2. the changeset is encoded, validated and stored in the metadata,
//! 3. the git-SHA mapping row is recorded (bijection, enforced by the
//!    store),
//! 4. `Engine::on_changeset_created` derives the tree and skip-list entry
//!    eagerly (D3) — tree objects flow through the counting proxy and
//!    count toward [`ImportReport::objects_written`].
//!
//! Changes are computed against the **first parent** only
//! (`--diff-merges=first-parent`), matching Gardenbed's changeset
//! semantics; other parents are recorded as additional changeset parents.
//!
//! Re-rooting exists only for `--limit` slices: a slice-boundary commit
//! whose first parent lies outside the slice is stored as a root
//! changeset carrying its full tree as adds; additional out-of-slice
//! parents are dropped. Full imports never re-root — every reachable
//! parent is either ingested (earlier in topological order) or mapped.
//!
//! Object writes and validation happen before atomic metadata ingestion:
//! changeset, Git mapping, derived rows and durable validation certificate
//! land together. Interrupted imports resume at the last certified frontier.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use sha2::Digest;
use sha2::Sha256;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Mode, Op};
use gardenbed_store::{MetadataStore, ObjectStore, StoreError};

use crate::counting::CountingObjects;
use crate::difftree::{DIFF_BATCH, DiffRecord, DiffStatus, run_batch};
use crate::error::ImportError;
use crate::graph::{Arena, Node, read_commit};
use crate::gtree::{GEntry, GKind};
use crate::oid::GitOid;
use crate::options::ImportOptions;
use crate::rawtree::RawTreeCapture;
use crate::report::{ImportReport, RefRewind};
use crate::resolve::{GitObjects, MAX_TREE_DEPTH, gitlink_blob_id};
use crate::transform::repair_dir_to_file;

/// Imports every selected branch of the git repository at `git_dir` into
/// the Gardenbed stores.
///
/// `git_dir` may be a bare repository or a working tree. `objects` (objects)
/// and `metadata` (metadata) are the destination stores; `engine` is
/// accepted for API shape but superseded internally by an engine over a
/// counting wrapper of `objects` (the engine is stateless over its store
/// pair, so behavior is identical — only write counting is added).
///
/// See [`ImportOptions`] for the knobs and the crate-level docs for the
/// import semantics (topological ingestion, first-parent deltas, force
/// pushes, incrementality).
pub fn import_from_path(
    git_dir: &Path,
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    _engine: Engine,
    opts: &ImportOptions,
) -> Result<ImportReport, ImportError> {
    let started = Instant::now();
    // gix is used for object reads (D6); open it isolated so no ambient
    // config, worktree hooks or environment leak into the import.
    let mut repo = gix::open_opts(git_dir, gix::open::Options::isolated())
        .map_err(|e| ImportError::Git(format!("{}: {e}", git_dir.display())))?;
    if repo.object_hash() != gix::hash::Kind::Sha1 {
        return Err(ImportError::Unsupported(
            "only SHA-1 repositories are supported (SHA-256 object format rejected)",
        ));
    }
    if repo.git_dir().join("shallow").exists() {
        return Err(ImportError::Unsupported(
            "shallow clones are not supported (history must be complete)",
        ));
    }
    // Keep hot object reads cheap without letting the cache grow
    // unboundedly over huge imports.
    repo.object_cache_size(64 * 1024 * 1024);

    // Subprocesses run inside the repository (work dir for working trees,
    // the git dir itself for bare repositories).
    let cwd = repo
        .workdir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| git_dir.to_path_buf());

    // ---- refs: select and map to tips --------------------------------
    let selected = select_refs(&repo, opts)?;
    let mut tips: Vec<GitOid> = Vec::new();
    {
        let mut seen = HashSet::new();
        for (_, oid) in &selected {
            if seen.insert(*oid) {
                tips.push(*oid);
            }
        }
    }

    // ---- pass 1: walk the graph --------------------------------------
    let mut arena = Arena::walk(&repo, &tips, metadata.as_ref(), opts.limit.is_none())?;
    let order = arena.ingest_order(opts.limit)?;
    let slice: HashSet<u32> = order.iter().copied().collect();

    // ---- pass 2: ingest ----------------------------------------------
    let counting = Arc::new(CountingObjects::new(objects));
    // The caller's engine is superseded by one over the counting wrapper:
    // derived trees then count toward `objects_written`.
    let engine = Engine::new(counting.clone(), metadata.clone());
    // Raw-tree capture state (one memo for the whole run: subtrees
    // shared across commits are decided once).
    let mut raw_tree_capture = RawTreeCapture::new();
    let mut git_objects = GitObjects::new(&repo, counting.as_ref());

    let mut report = ImportReport::default();
    // Re-rooted slice boundaries (limit imports only; full imports never
    // re-root): their changes are their full tree, not a diff.
    let reroot: HashSet<u32> = if opts.limit.is_some() {
        order
            .iter()
            .copied()
            .filter(|idx| is_reroot(&arena.nodes[*idx as usize], &arena.nodes, &slice))
            .collect()
    } else {
        HashSet::new()
    };

    for chunk in order.chunks(DIFF_BATCH) {
        // Which commits need a diff-tree classification?
        let mut shas: Vec<GitOid> = Vec::with_capacity(chunk.len());
        for idx in chunk {
            let node = &arena.nodes[*idx as usize];
            if node.mapped {
                report.commits_skipped += 1;
                continue;
            }
            if reroot.contains(idx) {
                continue;
            }
            shas.push(node.oid);
        }
        let mut diffs = if shas.is_empty() {
            HashMap::new()
        } else {
            run_batch(&cwd, &shas, opts.detect_renames)?
        };
        for idx in chunk {
            let node = &arena.nodes[*idx as usize];
            if node.mapped {
                continue; // already counted in the skip pass above
            }
            let sha_hex = node.oid.to_hex();
            let (raw, raw_bytes) = read_commit(&repo, node.oid, &sha_hex)?;
            let (parents, changes) = build_changes(
                &mut git_objects,
                &arena.nodes,
                *idx,
                &reroot,
                diffs.remove(&node.oid).unwrap_or_default(),
                &sha_hex,
            )?;
            let cs = Changeset {
                parents,
                changes,
                message: raw.message.clone(),
                author: raw.author.clone(),
                committer: raw.committer.clone(),
                signature: raw.signature.clone(),
            };
            // Sparse raw-commit capture (v0.2 D21): reconstruct the commit
            // bytes the model can produce and byte-compare with the
            // original. The parents used here are the ones the model will
            // link — the same filter `build_changes` applied — because
            // that is what the export reconstructs from. Byte-identical ⇒
            // provably reconstructable, no row. Anything else (a
            // signature, an unmodeled header such as encoding/mergetag,
            // non-UTF-8 identity bytes, header-order drift, or a re-rooted
            // limit-slice boundary whose model parents were dropped) ⇒
            // capture the original verbatim so export can emit it
            // SHA-exact — or, for slice boundaries whose raw bytes name
            // parents below the slice, refuse loudly instead of emitting
            // a dangling pack.
            let model_parent_hexes: Vec<String> = if reroot.contains(idx) {
                // Re-rooted slice boundary: the model stores no parents.
                Vec::new()
            } else {
                node.parents
                    .iter()
                    .filter_map(|p| {
                        let pn = &arena.nodes[*p as usize];
                        pn.cs.map(|_| pn.oid.to_hex())
                    })
                    .collect()
            };
            let reconstructed =
                crate::commit::reconstruct_bytes(&raw, &raw.tree.to_hex(), &model_parent_hexes);
            // Raw-tree capture (raw-tree fidelity): walk the commit's
            // tree closure; divergent subtrees (pre-canonical modes,
            // non-UTF-8 names) get verbatim rows keyed by their original
            // git SHA, and a divergent ROOT forces raw-commit capture —
            // the raw commit's `tree` header then carries the original
            // root SHA the export needs. Rows land additively before the
            // ingest transaction below (crash-orphan-safe: a row is only
            // consumed when a captured commit's tree header names it).
            let (root, written_trees) = engine.validated_tree_for_new_with_writes(&cs)?;
            let raw_tree_rows = raw_tree_capture.capture_commit_tree(
                &repo,
                &engine,
                raw.tree,
                root,
                metadata.as_ref(),
                &sha_hex,
            )?;
            report.raw_trees_captured += raw_tree_rows as u64;
            let raw_capture = if reconstructed == raw_bytes && raw_tree_rows == 0 {
                None
            } else {
                // Unreconstructable headers, or a divergent root tree:
                // the original commit bytes carry what the model cannot
                // — extra headers, and the original root SHA in the
                // `tree` header (which the export resolves through
                // raw-tree rows).
                Some(raw_bytes.as_slice())
            };
            // Batched ingest: the changeset row, the git mapping and the
            // derived-data rows (tree cache, skiplist, path summary) land in
            // ONE metadata transaction — a crash leaves the commit entirely
            // absent, which the incremental skip pass treats exactly like a
            // commit never reached (parents before children still holds).
            // The derived values are byte-identical to the eager
            // on_changeset_created path.
            let canonical = cs.canonical_bytes()?;
            let cs_id = Id::from_bytes(Sha256::digest(&canonical).into());
            // Normally parents were certified atomically by earlier ingests.
            // Mapped legacy frontiers are migrated once, including all parents.
            for parent in &cs.parents {
                engine.certify_history(*parent)?;
            }
            let rows = engine.derived_rows_for_validated_tree(cs_id, &cs, root)?;
            // Same last-chance re-check as the server's CreateChangeset:
            // the import holds repo.lock only shared, so a --force gc can
            // sweep concurrently and unlink a mirrored blob between the
            // mirror above and this row's commit. The in-transaction
            // verify is indivisible against gc's write lock, so the row
            // either lands with its objects present or the import errors
            // and the next import re-mirrors it (imports are resumable).
            // Gitlink ids are padded upstream SHAs, never objects files,
            // so they are exempt.
            //
            // The required set is the blob ids plus the DERIVATION'S
            // WRITE SET — every tree id the replay stored, idempotent
            // re-puts included (round-2 P2-1; round-4 redesign for
            // commit size-independence, same as the server side). Trees
            // the replay did not store are untouched subtrees reachable
            // from certified rows, which the gc mark never collects (V1,
            // D14); the write set enumerates exactly the race surface at
            // O(touched directories).
            let mut required: Vec<Id> = cs
                .changes
                .iter()
                .filter(|c| c.mode != Mode::Gitlink)
                .filter_map(|c| c.blob)
                .collect();
            required.extend(written_trees.iter().copied());
            let verify_objects = counting.clone();
            let verify = move || {
                // Plain existence checks for blobs and replayed trees
                // alike: one path shape per content hash, has() covers
                // both kinds.
                for id in &required {
                    if !verify_objects.has(*id)? {
                        let key = id.to_hex();
                        return Err(StoreError::Invalid(format!(
                            "object {key} was collected by a concurrent gc; \
                             the next import re-mirrors it"
                        )));
                    }
                }
                Ok(())
            };
            metadata.put_validated_changeset(
                &cs,
                root,
                &rows,
                Some(&sha_hex),
                raw_capture,
                Some(&verify),
            )?;
            arena.nodes[*idx as usize].cs = Some(cs_id);
            report.commits_new += 1;
            if report.commits_new % 25_000 == 0 {
                log::info!(
                    "import: {} commits ingested ({:.0}/s)",
                    report.commits_new,
                    report.commits_new as f64 / started.elapsed().as_secs_f64().max(1e-9)
                );
            }
        }
    }
    report.commits_total = if opts.limit.is_some() {
        order.len() as u64
    } else {
        arena.nodes.len() as u64
    };
    if opts.limit.is_none() {
        // Mapped commits pruned at the walk frontier never reach the
        // chunk loop (the Kahn order covers unmapped nodes only); count
        // them so `total = new + skipped` holds.
        report.commits_skipped = arena.nodes.iter().filter(|n| n.mapped).count() as u64;
    }

    // ---- refs: compare-and-swap onto the fresh tips -------------------
    // Two phases, so that a refusal moves NOTHING: phase 1 validates every
    // selected ref (rewind detection included) and collects all would-be
    // refusals; only if none exist does phase 2 perform the moves. Running
    // the gate per-ref inside a single loop would leave refs sorted before
    // the offending one already moved when the import aborts.
    struct PlannedRef {
        name: String,
        current: Option<Id>,
        target: Id,
        rewind: Option<RefRewind>,
    }
    // One would-be refusal: a non-descendant ref move that `allow_rewind`
    // does not permit. All of them are aggregated into a single error, so
    // no offending ref is dropped from the report when an earlier one
    // already tripped the gate.
    struct Refusal {
        name: String,
        old: Id,
        target: Id,
        // `None` under `--limit` (slice re-rooting breaks real ancestry).
        fell_off: Option<u64>,
    }
    let mut planned: Vec<PlannedRef> = Vec::with_capacity(selected.len());
    let mut refusals: Vec<Refusal> = Vec::new();
    for (name, tip) in &selected {
        let Some(idx) = arena.index.get(tip) else {
            return Err(ImportError::Other(format!(
                "ref {name} tip {} was not walked",
                tip.to_hex()
            )));
        };
        let Some(target) = arena.nodes[*idx as usize].cs else {
            // limit-slice tips outside the slice have no mapping; the ref
            // is left alone (documented on ImportOptions::limit).
            log::debug!("ref {name}: tip outside the import slice; not recorded");
            continue;
        };
        // Also certify an unchanged mapped tip on legacy re-import. A Git
        // mapping or generic derived row is never itself a validation proof.
        engine.certify_history(target)?;
        let current = metadata.resolve_ref(name)?;
        if current == Some(target) {
            continue; // already there: unchanged re-import
        }
        // Backward-move detection (force-push reporting): when the ref had
        // a previous value and the new tip is not a descendant of it, the
        // ref moves backward (or sideways onto rewritten history) and the
        // operator must see it — published work can silently fall off the
        // ref otherwise. The move stays permitted with `allow_rewind`
        // (force-push parity); without it the import fails before any ref
        // is touched. `--limit` imports refuse too (the count is not
        // computable there, but refusing the non-descendant move itself is
        // the safe default); full imports refuse only when changesets
        // would actually be lost, so sideways moves onto shared history
        // still only warn.
        let rewind = if let Some(old) = current
            && !engine.is_ancestor(old, target)?
        {
            // Count what falls off: changesets reachable from the old
            // tip but not from the new one. Both sets come from the
            // metadata's own parent edges — no git reads, no object
            // reads — and each changeset row is fetched once (visited
            // set), only on the rewind path itself.
            let fell_off = if opts.limit.is_some() {
                // Slice re-rooting stores out-of-slice parents as root
                // changesets, so real ancestry is not recoverable from
                // the imported graph: report, do not count.
                None
            } else {
                Some(count_fell_off(metadata.as_ref(), old, target)?)
            };
            if !opts.allow_rewind && (fell_off.is_none() || fell_off.unwrap_or(0) > 0) {
                refusals.push(Refusal {
                    name: name.clone(),
                    old,
                    target,
                    fell_off,
                });
            }
            Some(RefRewind {
                name: name.clone(),
                old_tip: old,
                new_tip: target,
                fell_off,
            })
        } else {
            None
        };
        planned.push(PlannedRef {
            name: name.clone(),
            current,
            target,
            rewind,
        });
    }
    if !refusals.is_empty() {
        // The loss phrase per refusal: exact count for full imports,
        // "uncounted" under `--limit`.
        let loss = |r: &Refusal| match r.fell_off {
            Some(n) => format!("losing {n} changeset(s)"),
            None => "losing changesets (uncounted under --limit)".to_owned(),
        };
        let hatch = "re-run with --allow-rewind to permit this";
        let moves = refusals
            .iter()
            .map(|r| {
                format!(
                    "{}, backward from {} to {}, {}",
                    r.name,
                    r.old.to_hex(),
                    r.target.to_hex(),
                    loss(r)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ImportError::Ref {
            name: refusals[0].name.clone(),
            detail: format!("import would move refs backward: {moves}; {hatch}"),
        });
    }
    for m in planned {
        if let Some(rewind) = m.rewind {
            report.refs_rewound.push(rewind);
        }
        match metadata.cas_ref(&m.name, m.current, m.target)? {
            gardenbed_store::RefCas::Won { .. } => report.refs_updated.push((m.name, m.target)),
            gardenbed_store::RefCas::Conflict { current } => {
                return Err(ImportError::Ref {
                    name: m.name,
                    detail: format!("lost a concurrent update: the ref now points at {current}"),
                });
            }
        }
    }
    // Legacy frontier certification may recreate missing tree objects.
    report.objects_written = counting.written();
    report.refs_updated.sort_by(|a, b| a.0.cmp(&b.0));
    report.refs_rewound.sort_by(|a, b| a.name.cmp(&b.name));
    report.elapsed = started.elapsed();
    Ok(report)
}

/// How many changesets are reachable from `old` but not from `new` — the
/// work that falls off a rewound ref. Both directions' ancestor sets are
/// collected by two independent plain BFS walks over the metadata's parent
/// edges (changeset rows only; no objects, no git), each row fetched once
/// per walk — the shared subgraph is fetched twice, once by each walk, so
/// the cost is O(old-reachable + new-reachable) sqlite row reads.
fn count_fell_off(
    metadata: &dyn gardenbed_store::MetadataStore,
    old: Id,
    new: Id,
) -> Result<u64, ImportError> {
    let mut from_old: HashSet<Id> = HashSet::new();
    let mut queue = vec![old];
    from_old.insert(old);
    while let Some(x) = queue.pop() {
        let (cs, _) = metadata
            .get_changeset(x)?
            .ok_or_else(|| ImportError::Other(format!("dangling changeset {x} under a ref")))?;
        for p in &cs.parents {
            if from_old.insert(*p) {
                queue.push(*p);
            }
        }
    }
    let mut from_new: HashSet<Id> = HashSet::new();
    let mut queue = vec![new];
    from_new.insert(new);
    while let Some(x) = queue.pop() {
        let (cs, _) = metadata
            .get_changeset(x)?
            .ok_or_else(|| ImportError::Other(format!("dangling changeset {x} under a ref")))?;
        for p in &cs.parents {
            if from_new.insert(*p) {
                queue.push(*p);
            }
        }
    }
    Ok(from_old.difference(&from_new).count() as u64)
}

/// Whether this node must be re-rooted: it has parents in git, but its
/// first parent is neither mapped nor part of this run's slice.
fn is_reroot(node: &Node, nodes: &[Node], slice: &HashSet<u32>) -> bool {
    match node.parents.first() {
        None => false, // a genuine root: --root diffing adds the full tree
        Some(first) => {
            let parent = &nodes[*first as usize];
            !(parent.mapped || slice.contains(first))
        }
    }
}

/// Selects the branches to import: `refs/heads/*`, full names, symbolic
/// refs skipped, filtered through the `opts.refs` glob patterns (empty
/// means all). Returned sorted by name for deterministic walks.
fn select_refs(
    repo: &gix::Repository,
    opts: &ImportOptions,
) -> Result<Vec<(String, GitOid)>, ImportError> {
    let platform = repo
        .references()
        .map_err(|e| ImportError::Git(format!("cannot list references: {e}")))?;
    let branches = platform
        .local_branches()
        .map_err(|e| ImportError::Git(format!("cannot list local branches: {e}")))?;
    let mut selected: Vec<(String, GitOid)> = Vec::new();
    for reference in branches {
        let reference =
            reference.map_err(|e| ImportError::Git(format!("reading a reference: {e}")))?;
        let name = String::from_utf8_lossy(reference.name().as_bstr()).into_owned();
        // Only direct object targets are imported; symbolic refs (e.g.
        // `refs/heads/HEAD`) carry no commit of their own.
        let tip = match reference.target() {
            gix::refs::TargetRef::Object(id) => GitOid::try_from(id.as_bytes())
                .map_err(|_| ImportError::Git(format!("ref {name}: bad object id")))?,
            _ => {
                log::debug!("ref {name}: symbolic; skipped");
                continue;
            }
        };
        let matched = opts.refs.is_empty()
            || opts
                .refs
                .iter()
                .any(|pattern| crate::options::glob_match(pattern, &name));
        if matched {
            selected.push((name, tip));
        }
    }
    selected.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(selected)
}

/// Builds one changeset's parent ids and changes from the diff records.
fn build_changes(
    git_objects: &mut GitObjects<'_>,
    nodes: &[Node],
    idx: u32,
    reroot: &HashSet<u32>,
    records: Vec<DiffRecord>,
    sha_hex: &str,
) -> Result<(Vec<Id>, Vec<Change>), ImportError> {
    let node = &nodes[idx as usize];
    if reroot.contains(&idx) {
        // Slice boundary: store the full tree as adds, no parents.
        let files = git_objects.flatten_tree(node.tree, sha_hex)?;
        let changes = files
            .into_iter()
            .map(|(path, mode, blob)| {
                Ok(Change {
                    op: Op::Add,
                    path,
                    blob: Some(blob_id_for(git_objects, mode, blob, sha_hex)?),
                    src_path: None,
                    mode,
                })
            })
            .collect::<Result<Vec<_>, ImportError>>()?;
        return Ok((Vec::new(), changes));
    }

    // Sanity-check empty diffs against the trees (see difftree docs): git
    // emits no output for an empty diff, and it must not skip anything
    // else silently — with one documented exception git itself cannot
    // see: git's diff machinery canonicalizes tree entry modes before
    // comparing (`canon_mode`), so a commit that only normalizes a
    // pre-canonical mode (early-2005 git wrote `100664`; the fixup makes
    // it `100644`, blob ids unchanged) produces no output although the
    // raw trees differ. Such commits are classified by the raw,
    // mode-aware fallback below; every other inconsistency stays a hard
    // error.
    let mut changes: Vec<Change> = Vec::with_capacity(records.len());
    if records.is_empty() {
        match node.parents.first() {
            Some(first) => {
                if node.tree != nodes[*first as usize].tree {
                    changes = raw_first_parent_diff(
                        git_objects,
                        nodes[*first as usize].tree,
                        node.tree,
                        sha_hex,
                    )?;
                }
            }
            None => {
                if !git_objects.tree_entries(node.tree, sha_hex)?.is_empty() {
                    return Err(ImportError::Diff {
                        sha: sha_hex.to_owned(),
                        detail: "root commit produced no diff although its tree is not empty"
                            .into(),
                    });
                }
            }
        }
    } else if let Some(first) = node.parents.first()
        && node.tree == nodes[*first as usize].tree
    {
        return Err(ImportError::Diff {
            sha: sha_hex.to_owned(),
            detail: "diff-tree reported changes although the tree equals the first \
                         parent's tree"
                .into(),
        });
    }

    for record in &records {
        let change = match record.status {
            DiffStatus::Delete => Change {
                path: record.path.clone(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
            DiffStatus::Add | DiffStatus::Modify | DiffStatus::TypeChange => {
                let (mode, blob) = git_objects.resolve_path(node.tree, &record.path, sha_hex)?;
                let id = blob_id_for(git_objects, mode, blob, sha_hex)?;
                Change {
                    path: record.path.clone(),
                    op: if record.status == DiffStatus::Add {
                        Op::Add
                    } else {
                        Op::Modify
                    },
                    blob: Some(id),
                    src_path: None,
                    mode,
                }
            }
            DiffStatus::Rename | DiffStatus::Copy => {
                let (mode, blob) = git_objects.resolve_path(node.tree, &record.path, sha_hex)?;
                let id = blob_id_for(git_objects, mode, blob, sha_hex)?;
                Change {
                    path: record.path.clone(),
                    op: Op::Rename,
                    blob: Some(id),
                    src_path: record.src.clone(),
                    mode,
                }
            }
        };
        changes.push(change);
    }
    repair_dir_to_file(&mut changes, sha_hex)?;
    // Canonical order: strictly ascending by path bytes; duplicates are
    // impossible for well-formed git output — enforced defensively.
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    if let Some(pair) = changes
        .windows(2)
        .find(|w| w[0].path.as_bytes() >= w[1].path.as_bytes())
    {
        return Err(ImportError::Diff {
            sha: sha_hex.to_owned(),
            detail: format!("duplicate path {:?} after repair", pair[1].path),
        });
    }
    // Parents: mapped or already-ingested parents link; others (limit
    // slices only) are dropped.
    let parents = node
        .parents
        .iter()
        .filter_map(|p| nodes[*p as usize].cs)
        .collect();
    Ok((parents, changes))
}

// ------------------------------------------------------- raw tree fallback

/// Safety cap for [`raw_first_parent_diff`]: the maximum number of leaf
/// entries (adds + deletes + modifies) a single raw tree comparison may
/// produce. Mode-normalization commits touch a handful of paths; the cap
/// only ever fires on adversarial or corrupt trees, where a clean error
/// beats unbounded memory use (the linux kernel's ~90k files leave two
/// orders of magnitude of headroom).
const RAW_DIFF_LEAF_CAP: usize = 1 << 20;

/// Computes a commit's first-parent changes by comparing its tree with
/// the first parent's tree **at the raw level** — entry by entry,
/// recursively — without git's diff machinery.
///
/// This is the fallback for commits whose `git diff-tree` run (batch and
/// per-commit re-run alike) yields no output although the tree differs
/// from the first parent's tree. git's diff machinery canonicalizes tree
/// entry modes before comparing them (`canon_mode`), so commits that only
/// normalize pre-canonical modes are invisible to it: early-2005 git
/// wrote regular files with the group-writable mode `100664`, and the
/// "new git world order" fixup commits that turn those into `100644`
/// (identical blob ids, differing raw modes) produce **zero diff output**
/// — verified empirically, `--raw` included. The same collapse covers
/// permission-bit variants of every other entry kind (`100775` ≡
/// `100755`, `120777` ≡ `120000`, `160123` ≡ `160000`, directory `040750`
/// ≡ `040000`), so this comparison works on the canonicalized model
/// entries ([`GEntry`]: kind, canonical [`Mode`], oid) and handles all of
/// them uniformly.
///
/// Semantics, mirroring the classification of the normal path:
///
/// * present only in the child → [`Op::Add`], only in the parent →
///   [`Op::Delete`] (a directory flattens into one delete per leaf),
/// * both sides with the same oid and the same canonical mode → nothing:
///   a `100664` → `100644` normalization is a **no-op** (both sides are
///   [`Mode::Regular`], the blob id is identical), so the changeset has
///   zero changes and the derived tree inherits the parent's — by
///   construction its derived id is canonical and legitimately differs
///   from git's raw tree sha for such history,
/// * both sides with the same oid but differing canonical modes →
///   [`Op::Modify`] carrying the new mode and the normal-path blob id of
///   the unchanged content (mode-only; Gardenbed's mode table is finer
///   than git's `canon_mode` for exotic permission bits),
/// * both sides with differing oids → [`Op::Modify`] with the new blob,
/// * file↔dir swaps → Delete(s) of the old subtree + Add(s) of the new.
///
/// No renames are invented: git reports none for these commits, so
/// nothing can be detected here — Add+Delete/Modify only.
///
/// Descent is lazy (identical subtree oids are pruned unread), entries
/// are compared by sorted name, and the leaf count is capped at
/// [`RAW_DIFF_LEAF_CAP`]: anything larger is a clean [`ImportError::Diff`],
/// never unbounded memory.
fn raw_first_parent_diff(
    git_objects: &mut GitObjects<'_>,
    parent_tree: GitOid,
    child_tree: GitOid,
    sha_hex: &str,
) -> Result<Vec<Change>, ImportError> {
    let mut changes = Vec::new();
    let mut leaves = 0usize;
    raw_diff_dir(
        git_objects,
        "",
        Some(parent_tree),
        Some(child_tree),
        0,
        RAW_DIFF_LEAF_CAP,
        sha_hex,
        &mut changes,
        &mut leaves,
    )?;
    Ok(changes)
}

/// Recursive worker of [`raw_first_parent_diff`]: compares one directory
/// pair (`None` = that side does not exist) under `prefix` and appends
/// the canonical changes; `leaves` counts them against `cap`.
#[allow(clippy::too_many_arguments)]
fn raw_diff_dir(
    git_objects: &mut GitObjects<'_>,
    prefix: &str,
    parent: Option<GitOid>,
    child: Option<GitOid>,
    depth: usize,
    cap: usize,
    sha_hex: &str,
    changes: &mut Vec<Change>,
    leaves: &mut usize,
) -> Result<(), ImportError> {
    if depth > MAX_TREE_DEPTH {
        return Err(ImportError::Object {
            oid: sha_hex.to_owned(),
            detail: "tree nesting exceeds the maximum supported depth".into(),
        });
    }
    if let (Some(p), Some(c)) = (parent, child)
        && p == c
    {
        return Ok(()); // identical subtree: pruned without reading it
    }
    let old = match parent {
        Some(t) => sorted_tree_entries(git_objects, t, sha_hex)?,
        None => Vec::new(),
    };
    let new = match child {
        Some(t) => sorted_tree_entries(git_objects, t, sha_hex)?,
        None => Vec::new(),
    };
    let mut i = 0usize;
    let mut j = 0usize;
    while i < old.len() || j < new.len() {
        let ord = match (old.get(i), new.get(j)) {
            (Some(p), Some(c)) => p.name.as_bytes().cmp(c.name.as_bytes()),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => unreachable!("the loop condition keeps one side non-empty"),
        };
        match ord {
            Ordering::Less => {
                raw_emit_side(
                    git_objects,
                    prefix,
                    &old[i],
                    false,
                    sha_hex,
                    changes,
                    leaves,
                )?;
                i += 1;
            }
            Ordering::Greater => {
                raw_emit_side(git_objects, prefix, &new[j], true, sha_hex, changes, leaves)?;
                j += 1;
            }
            Ordering::Equal => {
                let (p, c) = (&old[i], &new[j]);
                match (p.kind, c.kind) {
                    (GKind::Dir, GKind::Dir) => {
                        if p.oid != c.oid {
                            // Different content: descend. Identical oids
                            // are pruned (the lazy rule).
                            let sub = raw_join(prefix, &p.name);
                            raw_diff_dir(
                                git_objects,
                                &sub,
                                Some(p.oid),
                                Some(c.oid),
                                depth + 1,
                                cap,
                                sha_hex,
                                changes,
                                leaves,
                            )?;
                        }
                    }
                    (GKind::File, GKind::File) => {
                        if p.oid != c.oid || p.mode != c.mode {
                            // Equal canonical modes with equal oids were
                            // filtered out above (the pre-canonical-mode
                            // no-op); anything else is a modify — mode
                            // only when the blob is unchanged.
                            let id = blob_id_for(git_objects, c.mode, c.oid, sha_hex)?;
                            changes.push(Change {
                                path: raw_join(prefix, &c.name),
                                op: Op::Modify,
                                blob: Some(id),
                                src_path: None,
                                mode: c.mode,
                            });
                            *leaves += 1;
                        }
                    }
                    _ => {
                        // File↔dir swap: delete the old side's leaves,
                        // add the new side's. git's diff always reports
                        // this shape, so it never reaches the fallback in
                        // practice; kept for completeness.
                        raw_emit_side(git_objects, prefix, p, false, sha_hex, changes, leaves)?;
                        raw_emit_side(git_objects, prefix, c, true, sha_hex, changes, leaves)?;
                    }
                }
                i += 1;
                j += 1;
            }
        }
        if *leaves > cap {
            return Err(ImportError::Diff {
                sha: sha_hex.to_owned(),
                detail: format!(
                    "raw tree diff against the first parent exceeds the {cap}-leaf safety cap \
                     ({leaves} leaves counted); refusing to build an oversized changeset"
                ),
            });
        }
    }
    Ok(())
}

/// Emits one side of a raw comparison for `entry`, which exists on only
/// one side of the diff: child-side entries become Adds (their blobs are
/// mirrored through the normal path), parent-side entries become Deletes.
/// A directory produces one change per leaf ([`GitObjects::flatten_tree`]).
fn raw_emit_side(
    git_objects: &mut GitObjects<'_>,
    prefix: &str,
    entry: &GEntry,
    child_side: bool,
    sha_hex: &str,
    changes: &mut Vec<Change>,
    leaves: &mut usize,
) -> Result<(), ImportError> {
    match entry.kind {
        GKind::File => {
            *leaves += 1;
            let path = raw_join(prefix, &entry.name);
            if child_side {
                let id = blob_id_for(git_objects, entry.mode, entry.oid, sha_hex)?;
                changes.push(Change {
                    path,
                    op: Op::Add,
                    blob: Some(id),
                    src_path: None,
                    mode: entry.mode,
                });
            } else {
                changes.push(Change {
                    path,
                    op: Op::Delete,
                    blob: None,
                    src_path: None,
                    mode: Mode::Regular,
                });
            }
        }
        GKind::Dir => {
            // flatten_tree returns paths relative to the subtree root;
            // the entry's own name is the missing path component.
            let sub = raw_join(prefix, &entry.name);
            for (path, mode, blob) in git_objects.flatten_tree(entry.oid, sha_hex)? {
                *leaves += 1;
                let full = if sub.is_empty() {
                    path
                } else {
                    format!("{sub}/{path}")
                };
                if child_side {
                    let id = blob_id_for(git_objects, mode, blob, sha_hex)?;
                    changes.push(Change {
                        path: full,
                        op: Op::Add,
                        blob: Some(id),
                        src_path: None,
                        mode,
                    });
                } else {
                    changes.push(Change {
                        path: full,
                        op: Op::Delete,
                        blob: None,
                        src_path: None,
                        mode: Mode::Regular,
                    });
                }
            }
        }
    }
    Ok(())
}

/// The entries of one git tree, sorted by name bytes. Tree objects are
/// stored in git's own order (directory names compare as if suffixed
/// with `/`, which differs from plain byte order); sorting both sides
/// makes the entry-wise comparison exact.
fn sorted_tree_entries(
    git_objects: &mut GitObjects<'_>,
    tree: GitOid,
    sha_hex: &str,
) -> Result<Vec<GEntry>, ImportError> {
    let mut entries: Vec<GEntry> = git_objects.tree_entries(tree, sha_hex)?.as_ref().clone();
    entries.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    Ok(entries)
}

/// `prefix/name` (`name` alone when `prefix` is empty).
fn raw_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// The gardenbed blob id for a tree entry: gitlinks carry the padded
/// upstream SHA and are never read or written; everything else is read
/// from git and mirrored into objects (once per distinct content).
fn blob_id_for(
    git_objects: &mut GitObjects<'_>,
    mode: Mode,
    blob: GitOid,
    context_oid: &str,
) -> Result<gardenbed_model::Id, ImportError> {
    if mode == Mode::Gitlink {
        Ok(gitlink_blob_id(blob))
    } else {
        git_objects.ensure_blob(blob, context_oid)
    }
}

#[cfg(test)]
mod raw_fallback_tests {
    //! Focused tests for the raw mode-aware tree-diff fallback ([`super`]):
    //! git's diff machinery reports nothing for these trees, so they are
    //! built with plumbing only (`hash-object --literally` writes the
    //! non-canonical modes; see the edgecase integration test for the
    //! end-to-end import behavior).

    use std::io::Write;
    use std::process::{Command, Stdio};

    use gardenbed_model::{Change, Mode, Op};
    use gardenbed_store::MemoryStore;

    use super::{RAW_DIFF_LEAF_CAP, raw_diff_dir};
    use crate::oid::GitOid;
    use crate::resolve::GitObjects;

    /// A plumbing-only git repository (hermetic, throwaway).
    struct Repo {
        _dir: tempfile::TempDir,
        dir: std::path::PathBuf,
    }

    impl Repo {
        fn new() -> Repo {
            let _dir = tempfile::tempdir().expect("tempdir");
            let out = Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(_dir.path().join("repo"))
                .output()
                .expect("git init runs");
            assert!(out.status.success(), "git init failed");
            Repo {
                dir: _dir.path().join("repo"),
                _dir,
            }
        }

        fn git(&self, args: &[&str], stdin: &[u8]) -> String {
            let mut cmd = Command::new("git");
            cmd.current_dir(&self.dir);
            for (k, _) in std::env::vars_os() {
                if k.to_string_lossy().starts_with("GIT_") {
                    cmd.env_remove(&k);
                }
            }
            cmd.env("GIT_CONFIG_NOSYSTEM", "1");
            cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
            cmd.args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn().expect("spawn git");
            child
                .stdin
                .as_mut()
                .expect("stdin")
                .write_all(stdin)
                .expect("write stdin");
            let out = child.wait_with_output().expect("wait");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        }

        fn blob(&self, content: &[u8]) -> String {
            self.git(&["hash-object", "-w", "--stdin"], content)
        }

        /// A tree object with literal entry modes (`--literally` bypasses
        /// the fsck check that refuses fresh non-canonical modes).
        fn tree(&self, entries: &[(&str, &str, &str)]) -> String {
            let mut bytes = Vec::new();
            for (mode, sha, name) in entries {
                bytes.extend_from_slice(mode.as_bytes());
                bytes.push(b' ');
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(0);
                bytes.extend_from_slice(&hex_bytes(sha));
            }
            self.git(
                &["hash-object", "-t", "tree", "-w", "--stdin", "--literally"],
                &bytes,
            )
        }
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0, "odd hex: {s}");
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
            .collect()
    }

    /// Runs the fallback between two hand-crafted trees.
    fn diff_trees(
        repo: &Repo,
        parent: &str,
        child: &str,
        cap: usize,
    ) -> Result<Vec<Change>, crate::error::ImportError> {
        let gix_repo =
            gix::open_opts(&repo.dir, gix::open::Options::isolated()).expect("open repo");
        let store = MemoryStore::new();
        let mut git_objects = GitObjects::new(&gix_repo, &store);
        let mut changes = Vec::new();
        let mut leaves = 0usize;
        raw_diff_dir(
            &mut git_objects,
            "",
            Some(GitOid::from_hex(parent).expect("hex")),
            Some(GitOid::from_hex(child).expect("hex")),
            0,
            cap,
            "deadbeef",
            &mut changes,
            &mut leaves,
        )?;
        Ok(changes)
    }

    /// A commit that only normalizes `100664` → `100644` (identical blob
    /// ids) is a no-op: both sides are `Mode::Regular` in the model.
    #[test]
    fn mode_only_664_normalization_is_a_no_op() {
        let r = Repo::new();
        let b = r.blob(b"same\n");
        let parent = r.tree(&[("100664", &b, "f")]);
        let child = r.tree(&[("100644", &b, "f")]);
        assert_ne!(parent, child, "fixture: the raw tree ids must differ");
        let changes = diff_trees(&r, &parent, &child, RAW_DIFF_LEAF_CAP).expect("diffs");
        assert!(changes.is_empty(), "zero-change changeset, got {changes:?}");
    }

    /// Exotic file modes git's `canon_mode` collapses (e.g. `100675` ≡
    /// `100644`) but Gardenbed's mode table distinguishes become a
    /// mode-only Modify carrying the new mode and the unchanged blob.
    #[test]
    fn exotic_modes_distinguished_by_the_model_become_modifies() {
        let r = Repo::new();
        let b = r.blob(b"same\n");
        let parent = r.tree(&[("100675", &b, "f")]); // exec bit set ⇒ Exec
        let child = r.tree(&[("100644", &b, "f")]); // ⇒ Regular
        let changes = diff_trees(&r, &parent, &child, RAW_DIFF_LEAF_CAP).expect("diffs");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].op, Op::Modify);
        assert_eq!(changes[0].path, "f");
        assert_eq!(changes[0].mode, Mode::Regular, "the child's mode wins");
        assert_eq!(changes[0].blob, Some(gardenbed_model::blob_id(b"same\n")));
    }

    /// A content change under an unchanged (non-canonical) mode is an
    /// ordinary Modify with the new blob — git reports this case itself,
    /// but the fallback must agree if it is ever reached.
    #[test]
    fn content_change_is_a_modify_with_the_new_blob() {
        let r = Repo::new();
        let a = r.blob(b"a\n");
        let b2 = r.blob(b"b\n");
        let parent = r.tree(&[("100664", &a, "f")]);
        let child = r.tree(&[("100664", &b2, "f")]);
        let changes = diff_trees(&r, &parent, &child, RAW_DIFF_LEAF_CAP).expect("diffs");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].op, Op::Modify);
        assert_eq!(changes[0].blob, Some(gardenbed_model::blob_id(b"b\n")));
        assert_eq!(changes[0].mode, Mode::Regular);
    }

    /// Structural differences (add, delete, dir flattening, file↔dir
    /// swap) come out as git's diff would shape them — never as renames.
    #[test]
    fn structural_changes_are_add_delete_pairs() {
        let r = Repo::new();
        let a = r.blob(b"a\n");
        let b2 = r.blob(b"b\n");
        let c = r.blob(b"c\n");
        let d = r.blob(b"d\n");
        // Parent: P (a file), d/(x, y), f1; child: P/ (a dir with one
        // inner file), f1 (kept identical), g — and `d` gone entirely.
        let dsub = r.tree(&[("100644", &c, "x"), ("100644", &d, "y")]);
        let parent = r.tree(&[
            ("100644", &b2, "P"),
            ("40000", &dsub, "d"),
            ("100644", &a, "f1"),
        ]);
        let psub = r.tree(&[("100644", &a, "inner")]);
        let child = r.tree(&[
            ("40000", &psub, "P"),
            ("100644", &a, "f1"),
            ("100644", &c, "g"),
        ]);
        let changes = diff_trees(&r, &parent, &child, RAW_DIFF_LEAF_CAP).expect("diffs");
        let shown: Vec<(Op, String)> = changes.iter().map(|c| (c.op, c.path.clone())).collect();
        assert_eq!(
            shown,
            vec![
                // The file→dir swap: one delete for the old file, one add
                // per new leaf (git's own shape; the dir→file repair pass
                // consumes the deletes when applied in the other order).
                (Op::Delete, "P".into()),
                (Op::Add, "P/inner".into()),
                // The removed directory flattens into per-leaf deletes.
                (Op::Delete, "d/x".into()),
                (Op::Delete, "d/y".into()),
                // The kept file is filtered (same oid, same mode).
                (Op::Add, "g".into()),
            ],
        );
        assert!(
            changes.iter().all(|c| c.src_path.is_none()),
            "no invented renames"
        );
        assert_eq!(
            changes
                .iter()
                .find(|c| c.path == "P/inner")
                .and_then(|c| c.blob),
            Some(gardenbed_model::blob_id(b"a\n")),
        );
    }

    /// The leaf cap produces a clean error naming the cap, never a panic
    /// or an oversized changeset.
    #[test]
    fn cap_bails_out_cleanly() {
        let r = Repo::new();
        let contents = ["1\n", "2\n", "3\n", "1x\n", "2x\n", "3x\n"];
        let blobs: Vec<String> = contents.iter().map(|c| r.blob(c.as_bytes())).collect();
        let parent = r.tree(&[
            ("100644", &blobs[0], "f1"),
            ("100644", &blobs[1], "f2"),
            ("100644", &blobs[2], "f3"),
        ]);
        let child = r.tree(&[
            ("100644", &blobs[3], "f1"),
            ("100644", &blobs[4], "f2"),
            ("100644", &blobs[5], "f3"),
        ]);
        let err = diff_trees(&r, &parent, &child, 2).expect_err("over the cap");
        let text = err.to_string();
        assert!(
            text.contains("2-leaf safety cap"),
            "error must name the cap: {text}"
        );
        // Under the real cap the same diff is three ordinary modifies.
        let changes = diff_trees(&r, &parent, &child, RAW_DIFF_LEAF_CAP).expect("diffs");
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().all(|c| c.op == Op::Modify));
    }
}

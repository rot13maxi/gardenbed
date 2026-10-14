//! The import driver: walk the graph, classify diffs, ingest changesets.
//!
//! Per commit (in topological order, batched for diff-tree):
//!
//! 1. blobs referenced by the commit's changes are mirrored into loam
//!    (content-addressed: written exactly once per distinct content),
//! 2. the changeset is encoded, validated and stored in the seedbed,
//! 3. the git-SHA mapping row is recorded (bijection, enforced by the
//!    store),
//! 4. `Engine::on_changeset_created` derives the tree and skip-list entry
//!    eagerly (D3) — tree objects flow through the counting proxy and
//!    count toward [`ImportReport::objects_written`].
//!
//! Changes are computed against the **first parent** only
//! (`--diff-merges=first-parent`), matching gardenbed's changeset
//! semantics; other parents are recorded as additional changeset parents.
//!
//! Re-rooting exists only for `--limit` slices: a slice-boundary commit
//! whose first parent lies outside the slice is stored as a root
//! changeset carrying its full tree as adds; additional out-of-slice
//! parents are dropped. Full imports never re-root — every reachable
//! parent is either ingested (earlier in topological order) or mapped.
//!
//! Object writes happen strictly before the changeset that references
//! them, and the mapping row precedes the derived-data pass, so a crash
//! leaves only recomputable state behind.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Mode, Op};use gardenbed_store::{MetadataStore, ObjectStore};

use crate::counting::CountingObjects;
use crate::difftree::{run_batch, DiffRecord, DiffStatus, DIFF_BATCH};
use crate::error::ImportError;
use crate::graph::{read_commit, Arena, Node};
use crate::oid::GitOid;
use crate::options::ImportOptions;
use crate::report::ImportReport;
use crate::resolve::{gitlink_blob_id, GitObjects};
use crate::transform::repair_dir_to_file;

/// Imports every selected branch of the git repository at `git_dir` into
/// the gardenbed stores.
///
/// `git_dir` may be a bare repository or a working tree. `objects` (loam)
/// and `metadata` (seedbed) are the destination stores; `engine` is
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
            let raw = read_commit(&repo, node.oid, &sha_hex)?;
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
                message: raw.message,
                author: raw.author,
                committer: raw.committer,
                signature: raw.signature,
            };
            let cs_id = metadata.put_changeset(&cs)?;
            metadata.record_git_mapping(&sha_hex, cs_id)?;
            engine.on_changeset_created(cs_id)?;
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
    report.objects_written = counting.written();

    // ---- refs: compare-and-swap onto the fresh tips -------------------
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
        let current = metadata.resolve_ref(name)?;
        if current == Some(target) {
            continue; // already there: unchanged re-import
        }
        match metadata.cas_ref(name, current, target)? {
            gardenbed_store::RefCas::Won { .. } => {
                report.refs_updated.push((name.clone(), target))
            }
            gardenbed_store::RefCas::Conflict { current } => {
                return Err(ImportError::Ref {
                    name: name.clone(),
                    detail: format!(
                        "lost a concurrent update: the ref now points at {current}"
                    ),
                })
            }
        }
    }
    report.refs_updated.sort_by(|a, b| a.0.cmp(&b.0));
    report.elapsed = started.elapsed();
    Ok(report)
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
    // else silently.
    if records.is_empty() {
        match node.parents.first() {
            Some(first) => {
                if node.tree != nodes[*first as usize].tree {
                    return Err(ImportError::Diff {
                        sha: sha_hex.to_owned(),
                        detail: "diff-tree produced no output although the commit's tree \
                                 differs from its first parent's tree"
                            .into(),
                    });
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
    } else if let Some(first) = node.parents.first() {
        if node.tree == nodes[*first as usize].tree {
            return Err(ImportError::Diff {
                sha: sha_hex.to_owned(),
                detail: "diff-tree reported changes although the tree equals the first \
                         parent's tree"
                    .into(),
            });
        }
    }

    let mut changes: Vec<Change> = Vec::with_capacity(records.len());
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

/// The gardenbed blob id for a tree entry: gitlinks carry the padded
/// upstream SHA and are never read or written; everything else is read
/// from git and mirrored into loam (once per distinct content).
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

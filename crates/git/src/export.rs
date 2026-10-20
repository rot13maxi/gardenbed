//! Export — rendering Gardenbed's changeset stores as a real git
//! repository (the bridge's write direction), **natively** (v0.2 D20).
//!
//! No git subprocess is involved: the pack (`.pack` + version-2 `.idx`)
//! is written by [`crate::pack`], loose refs and `HEAD` are plain files,
//! and the repository skeleton (`objects/`, `refs/`, minimal `config`)
//! is created by hand. Every object name is **computed** here, never
//! observed — and every computed commit SHA is asserted against the
//! store's git-mapping row (drift is a hard error, never silent).
//!
//! # Object reconstruction
//!
//! 1. **Blobs** first, ascending by blob id: `SHA-1("blob <len>\0" +
//!    content)`. Gitlink pointers are *not* blobs — their model blob id
//!    already *is* the upstream git SHA-1 (zero-padded), used directly as
//!    a tree entry's 20-byte name.
//! 2. **Trees** bottom-up per changeset (topological order makes the
//!    chain-walk amortized: `derive_tree` finds the first parent's tree
//!    cached). Git tree entries are re-sorted git-style — directories
//!    compare as `name + "/"` (e.g. file `foo.txt` sorts *before* dir
//!    `foo` in git, *after* it in the model's plain name order) — and
//!    rendered `<mode-octal> <name>\0<20-byte sha>`. Each distinct model
//!    tree id maps to one git tree object (memoized).
//! 3. **Commits** in topological order: `tree <sha>` + `parent <sha>`×N +
//!    `author`/`committer` lines with verbatim identity bytes and raw
//!    `±hhmm` dates, blank line, message. The reconstruction reproduces
//!    unsigned, UTF-8, header-order-canonical commits byte-exactly —
//!    that is what makes roundtrips SHA-exact.
//! 4. **Refs last** as loose files under `refs/`, plus `HEAD` pointing at
//!    the first exported ref.
//!
//! # Import fidelity (v0.2 D21, M18)
//!
//! Commits that came from a git import may carry bytes the model cannot
//! reconstruct (a `gpgsig`/`mergetag`/`encoding` header, non-UTF-8
//! identity bytes, …). At import, any such commit's **raw object bytes**
//! are captured into a sparse side table; export writes those bytes
//! **verbatim** (asserting their SHA-1 against the git-mapping row), so
//! imported history roundtrips SHA-exact instead of forking ids. The
//! `signature` model field survives for the API; `signed_dropped` is 0 on
//! every successful export — a signed changeset with *no* raw row (a v0.1
//! store) refuses loudly at the SHA assertion instead (retired by
//! re-import).
//!
//! # Divergences (see `docs/divergences.md`)
//!
//! * Pre-canonical git modes (`100664` history) and non-UTF-8 **path
//!   names** diverge at the tree level; the raw-*tree* capture that would
//!   fix them is gated on the kernel throughput gate (§2/§4). For a
//!   mapped commit whose reconstruction drifts, export **fails loudly**
//!   naming the commit — the divergence is never silent.
//! * Empty trees: a changeset with `root_tree == None` exports git's
//!   canonical empty tree object (`4b825dc6…`), which every git
//!   understands without packing it — it is packed anyway for
//!   self-containedness.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use gardenbed_derive::Engine;
use gardenbed_model::{Changeset, Id, Mode, TreeKind};
use gardenbed_store::{MetadataStore, ObjectStore, StoreError};
use sha1::{Digest, Sha1};

use crate::error::ExportError;
use crate::options::glob_match;
use crate::pack::{PackObject, PackWriter, write_idx};

/// Knobs for an export.
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    /// Glob patterns (`*`, `?`) matched against full ref names such as
    /// `refs/heads/main`; empty means every ref in the metadata.
    pub refs: Vec<String>,
}

/// What an export produced.
///
/// `commits` is in export (topological) order. `refs` pairs each exported
/// ref with the git SHA it was computed to point at. Every SHA in the
/// report is computed by the export itself and (for mapped changesets)
/// asserted against the git-mapping table.
#[derive(Clone, Debug)]
pub struct ExportReport {
    /// Commit objects written.
    pub commits_exported: u64,
    /// Distinct blob objects written (deduplicated by blob id).
    pub blobs_exported: u64,
    /// Kept for report-shape compatibility: in v0.2 the export REFUSES a
    /// signed changeset that has no raw commit row (the SHA assertion
    /// fires before anything ships — never a silent divergence), so this
    /// is 0 on every successful export. Retire v0.1 stores by re-import;
    /// see `docs/divergences.md`.
    pub signed_dropped: u64,
    /// `(full ref name, git sha)` for every exported ref, sorted by name.
    pub refs: Vec<(String, String)>,
    /// `(changeset id, git sha)` for every exported commit, in export
    /// order. SHAs are computed by the export and asserted against the
    /// git mapping where one exists.
    pub commits: Vec<(Id, String)>,
    /// Path of the written pack file (the `.idx` sits beside it).
    pub pack_path: std::path::PathBuf,
    /// Wall-clock duration of the whole export.
    pub elapsed: std::time::Duration,
}

impl std::fmt::Display for ExportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} commits ({} signed dropped), {} blobs, {} ref(s), {:.2}s",
            self.commits_exported,
            self.signed_dropped,
            self.blobs_exported,
            self.refs.len(),
            self.elapsed.as_secs_f64(),
        )
    }
}

/// Exports the changeset graph reachable from the metadata's refs into a
/// git repository at `git_dir` (created as a bare SHA-1 repository when
/// missing; an existing bare SHA-1 repository is accepted as-is).
///
/// A non-bare existing target is refused before anything is written: its
/// index and worktree would be left on the old HEAD while the ref moved,
/// staging the whole checkout as deletions (dogfood #11). Updating them is
/// deliberately out of scope — use a bare mirror (or export elsewhere).
///
/// Blobs are read from `objects`, changesets and refs from `metadata`.
/// The pack, index, loose refs and `HEAD` are written natively; no git
/// subprocess runs. Every mapped changeset's computed SHA is asserted
/// against the git-mapping row — drift fails the export.
pub fn export_to_path(
    git_dir: &Path,
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    opts: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    let started = Instant::now();

    // ---- refs: filter, validate, keep metadata's sorted-by-name order.
    let all_refs = metadata.list_refs()?;
    let tips: Vec<(String, Id)> = all_refs
        .iter()
        .filter(|(name, _)| opts.refs.is_empty() || opts.refs.iter().any(|p| glob_match(p, name)))
        .cloned()
        .collect();
    for (name, _) in &tips {
        validate_ref_name(name)?;
    }

    // ---- the reachable graph, parents before children.
    let (order, cache) = topo_order(&tips, metadata.as_ref())?;
    let blob_ids = collect_blob_ids(&order, &cache);

    // ---- the target repository skeleton (bare SHA-1).
    prepare_target(git_dir)?;

    // ---- the pack is streamed straight to disk as objects are produced
    //      (blobs ascending by id, then trees and commits in topological
    //      order): no multi-GB in-memory buffer, and zlib runs on all
    //      cores over bounded chunks.
    let pack_dir = git_dir.join("objects/pack");
    std::fs::create_dir_all(&pack_dir)
        .map_err(|e| ExportError::Git(format!("creating {}: {e}", pack_dir.display())))?;
    let tmp_path = pack_dir.join(format!("pack-tmp-{:016x}", std::process::id()));
    let pack_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)
        .map_err(|e| ExportError::Git(format!("creating {}: {e}", tmp_path.display())))?;
    let mut pack = PackWriter::new(std::io::BufWriter::with_capacity(1 << 20, pack_file))
        .map_err(|e| ExportError::Git(format!("writing the pack: {e}")))?;

    let engine = Engine::new(objects.clone(), metadata.clone());

    // ---- commits are rendered in topological windows: derive the
    //      window's root trees, decode every not-yet-rendered tree they
    //      reach on all cores (breadth-first by level — a level's child
    //      ids are only known once the level is decoded), then render the
    //      window from memory. The sha memo (`tree_shas`, carried across
    //      windows) keeps a window's decode set to its *new* trees —
    //      consecutive commits share almost all subtrees — so the decoded
    //      map stays small regardless of history size.
    let mut prefetched_trees: u64 = 0;
    /// A decoded tree, shared with the render pass.
    type TreePair = (Id, Arc<gardenbed_model::Tree>);
    /// Commits per decode window.
    const COMMIT_WINDOW: usize = 512;
    /// Decodes `level` in parallel, returning `(id, tree)` pairs.
    fn decode_level(
        objects: &Arc<dyn ObjectStore>,
        level: &[Id],
    ) -> Result<Vec<(Id, Arc<gardenbed_model::Tree>)>, ExportError> {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        let per = level.len().div_ceil(threads);
        let results: Vec<Result<Vec<TreePair>, ExportError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = level
                .chunks(per)
                .map(|slice| {
                    let objects = objects.clone();
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|id| {
                                let bytes = objects.get_tree(*id)?.ok_or_else(|| {
                                    StoreError::Corrupt(format!(
                                        "tree {id} is referenced but missing from the object store"
                                    ))
                                })?;
                                Ok((
                                    *id,
                                    Arc::new(gardenbed_model::Tree::decode(&bytes).map_err(
                                        |e| StoreError::Corrupt(format!("tree {id}: {e}")),
                                    )?),
                                ))
                            })
                            .collect::<Result<Vec<_>, _>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Err(ExportError::Other("a tree worker panicked".into()))
                    })
                })
                .collect()
        });
        let mut out = Vec::with_capacity(level.len());
        for result in results {
            out.extend(result?);
        }
        Ok(out)
    }
    /// Collects the tree ids `roots` reach that are not yet rendered
    /// (absent from `tree_shas`), decoding in parallel as it walks.
    fn prefetch_window_trees(
        objects: &Arc<dyn ObjectStore>,
        roots: impl Iterator<Item = Option<Id>>,
        tree_shas: &HashMap<Id, [u8; 20]>,
    ) -> Result<HashMap<Id, Arc<gardenbed_model::Tree>>, ExportError> {
        let mut trees: HashMap<Id, Arc<gardenbed_model::Tree>> = HashMap::new();
        let mut level: Vec<Id> = roots
            .flatten()
            .filter(|id| !tree_shas.contains_key(id))
            .collect();
        level.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        level.dedup();
        while !level.is_empty() {
            let decoded = decode_level(objects, &level)?;
            level.clear();
            for (id, tree) in decoded {
                for e in &tree.entries {
                    if let Some(child) = e.tree
                        && !tree_shas.contains_key(&child)
                        && !trees.contains_key(&child)
                    {
                        level.push(child);
                    }
                }
                trees.insert(id, tree);
            }
            level.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            level.dedup();
        }
        Ok(trees)
    }

    let mut blob_shas: HashMap<Id, [u8; 20]> = HashMap::with_capacity(blob_ids.len());
    let mut tree_shas: HashMap<Id, [u8; 20]> = HashMap::new();
    let mut commit_shas: HashMap<Id, [u8; 20]> = HashMap::with_capacity(order.len());
    let mut written: HashSet<[u8; 20]> = HashSet::new();
    let t_blobs = Instant::now();
    // Blobs are independent: read + hash them on all cores, window by
    // window (window = a few pack chunks, so decompressed content is
    // bounded and the pack writer stays fed). Windows keep blob order —
    // ascending by id — so the pack stays deterministic.
    const BLOB_WINDOW: usize = 4 * 4096;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    let per_thread = BLOB_WINDOW.div_ceil(threads);
    /// One decoded blob: id, git SHA-1, content.
    type BlobTriple = (Id, [u8; 20], Vec<u8>);
    for window in blob_ids.chunks(BLOB_WINDOW) {
        let results: Vec<Result<Vec<BlobTriple>, ExportError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = window
                .chunks(per_thread)
                .map(|slice| {
                    let objects = objects.clone();
                    scope.spawn(move || {
                        let mut out = Vec::with_capacity(slice.len());
                        for id in slice {
                            let content = objects
                                .get_blob(*id)?
                                .ok_or(ExportError::MissingBlob { id: *id })?;
                            let sha = git_sha1(3, &content);
                            out.push((*id, sha, content));
                        }
                        Ok(out)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Err(ExportError::Other("a blob worker panicked".into()))
                    })
                })
                .collect()
        });
        for result in results {
            for (id, sha, content) in result? {
                blob_shas.insert(id, sha);
                if written.insert(sha) {
                    pack.push(PackObject {
                        kind: 3,
                        data: content,
                        sha1: sha,
                    })
                    .map_err(|e| ExportError::Git(format!("writing the pack: {e}")))?;
                }
            }
        }
    }
    log::info!(
        "phase blobs: {:.2}s ({} blobs)",
        t_blobs.elapsed().as_secs_f64(),
        blob_ids.len()
    );

    let t_prefetch_total = Instant::now();
    let t_commits = Instant::now();
    let mut t_derive_total = std::time::Duration::ZERO;
    let mut t_render_total = std::time::Duration::ZERO;
    let mut commits: Vec<(Id, String)> = Vec::with_capacity(order.len());
    let mut signed_dropped: u64 = 0;
    for window in order.chunks(COMMIT_WINDOW) {
        // Derive the window's roots, then decode its not-yet-rendered
        // trees in parallel before rendering anything.
        let mut roots: Vec<Option<Id>> = Vec::with_capacity(window.len());
        for id in window {
            let t0 = Instant::now();
            roots.push(
                engine
                    .derive_tree(*id)
                    .map_err(|e| ExportError::Changeset {
                        id: id.to_hex(),
                        detail: format!("deriving the root tree: {e}"),
                    })?,
            );
            t_derive_total += t0.elapsed();
        }
        let window_trees = prefetch_window_trees(&objects, roots.iter().copied(), &tree_shas)?;
        prefetched_trees += window_trees.len() as u64;

        for (id, root) in window.iter().zip(&roots) {
            let cs = &cache[id];
            let t1 = Instant::now();
            let tree_sha = TreeRenderer {
                engine: &engine,
                blob_shas: &blob_shas,
                tree_shas: &mut tree_shas,
                trees: &window_trees,
                pack: &mut pack,
                written: &mut written,
            }
            .render(*root)?;
            t_render_total += t1.elapsed();

            // The commit bytes: raw row (verbatim, asserted) or reconstruction.
            let raw = metadata.raw_commit(*id).map_err(ExportError::from)?;
            let (commit_bytes, from_raw) = match raw {
                Some(bytes) => {
                    // Raw bytes are identity truth, not pack closure truth:
                    // every tree and parent they name must exist in what this
                    // export writes, or the pack would dangle (a `limit`-slice
                    // boundary names git parents below the slice — refused
                    // loudly rather than emitted as a broken pack).
                    validate_raw_closure(*id, &bytes, &tree_sha, &commit_shas)?;
                    (bytes, true)
                }
                None => (reconstruct_commit(*id, cs, &tree_sha, &commit_shas)?, false),
            };
            let commit_sha = git_sha1(1, &commit_bytes);

            // The load-bearing assertion (D20): for mapped changesets the
            // computed SHA must equal the git-mapping row. Drift means either
            // store corruption or a reconstruction divergence — never silent.
            if let Some(mapped) = metadata.git_for_cs(*id).map_err(ExportError::from)? {
                let computed_hex = to_hex(&commit_sha);
                if mapped != computed_hex {
                    return Err(ExportError::Changeset {
                        id: id.to_hex(),
                        detail: format!(
                            "computed git SHA {computed_hex} but the git mapping says {mapped}{}",
                            if from_raw {
                                " (raw commit bytes disagree with the mapping row — corrupt side table)"
                            } else {
                                " (reconstruction diverges from the imported commit — pre-canonical \
                                 modes or non-UTF-8 paths; see docs/divergences.md §2/§4)"
                            }
                        ),
                    });
                }
            }

            if !from_raw && cs.signature.is_some() {
                signed_dropped += 1;
            }
            if written.insert(commit_sha) {
                pack.push(PackObject {
                    kind: 1,
                    data: commit_bytes,
                    sha1: commit_sha,
                })
                .map_err(|e| ExportError::Git(format!("writing the pack: {e}")))?;
            }
            commit_shas.insert(*id, commit_sha);
            commits.push((*id, to_hex(&commit_sha)));
        }
    }
    log::info!(
        "phase trees+commits: {:.2}s (derive {:.2}s, render {:.2}s, prefetch {:.2}s for {} \
         trees, {} commits)",
        t_commits.elapsed().as_secs_f64(),
        t_derive_total.as_secs_f64(),
        t_render_total.as_secs_f64(),
        t_prefetch_total.elapsed().as_secs_f64(),
        prefetched_trees,
        order.len()
    );

    // ---- finalize the pack (trailer + count patch), rename into place,
    //      and write the idx + refs.
    let t_pack = Instant::now();
    let (pack_sha, entries) = pack
        .finish()
        .map_err(|e| ExportError::Git(format!("writing the pack: {e}")))?;
    let hex = to_hex(&pack_sha);
    let pack_path = pack_dir.join(format!("pack-{hex}.pack"));
    let idx_path = pack_dir.join(format!("pack-{hex}.idx"));
    // Idempotent re-export: same content ⇒ same names; skip when present.
    if !pack_path.exists() {
        fsync_rename(&tmp_path, &pack_path)?;
    } else {
        let _ = std::fs::remove_file(&tmp_path);
    }
    if !idx_path.exists() {
        let mut idx = Vec::new();
        write_idx(&mut idx, entries, pack_sha)
            .map_err(|e| ExportError::Git(format!("writing the idx: {e}")))?;
        atomic_write(&idx_path, &idx)?;
    }
    log::info!(
        "phase pack+idx write: {:.2}s",
        t_pack.elapsed().as_secs_f64()
    );
    write_refs(git_dir, &tips, &commit_shas)?;

    let refs: Vec<(String, String)> = tips
        .iter()
        .map(|(name, tip)| {
            let sha = commit_shas
                .get(tip)
                .map(|s| to_hex(s))
                .ok_or_else(|| ExportError::Ref {
                    name: name.clone(),
                    detail: "its tip changeset was not exported".into(),
                });
            sha.map(|sha| (name.clone(), sha))
        })
        .collect::<Result<_, _>>()?;

    Ok(ExportReport {
        commits_exported: order.len() as u64,
        blobs_exported: blob_ids.len() as u64,
        signed_dropped,
        refs,
        commits,
        pack_path,
        elapsed: started.elapsed(),
    })
}

/// `SHA-1("<type> <len>\0" + data)` — git's object name. `kind` is git's
/// numeric type (1=commit, 2=tree, 3=blob, 4=tag).
pub(crate) fn git_sha1(kind: u8, data: &[u8]) -> [u8; 20] {
    let mut sha = Sha1::new();
    sha.update(kind_name(kind));
    sha.update(b" ");
    sha.update(data.len().to_string().as_bytes());
    sha.update(b"\0");
    sha.update(data);
    sha.finalize().into()
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        4 => "tag",
        _ => unreachable!("kind is a git object type"),
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Depth guard mirroring the importer's.
const MAX_TREE_DEPTH: usize = 4096;

/// Renders git tree objects from model trees, memoizing by model tree id
/// (a stable subtree renders once per export). The per-window decoded
/// trees are consulted before the engine's on-demand path.
struct TreeRenderer<'a> {
    engine: &'a Engine,
    blob_shas: &'a HashMap<Id, [u8; 20]>,
    tree_shas: &'a mut HashMap<Id, [u8; 20]>,
    trees: &'a HashMap<Id, Arc<gardenbed_model::Tree>>,
    pack: &'a mut PackWriter,
    written: &'a mut HashSet<[u8; 20]>,
}

impl TreeRenderer<'_> {
    /// Renders the tree `root` (`None` = the canonical empty tree),
    /// returning its git SHA-1.
    fn render(&mut self, root: Option<Id>) -> Result<[u8; 20], ExportError> {
        match root {
            None => {
                // git's canonical empty tree. Write it (idempotently) so
                // the pack is self-contained even though every git knows
                // it.
                let sha = git_sha1(2, b"");
                if self.written.insert(sha) {
                    self.pack.push(PackObject {
                        kind: 2,
                        data: Vec::new(),
                        sha1: sha,
                    })?;
                }
                Ok(sha)
            }
            Some(id) => self.render_node(id, 0),
        }
    }

    fn render_node(&mut self, id: Id, depth: usize) -> Result<[u8; 20], ExportError> {
        if depth > MAX_TREE_DEPTH {
            return Err(ExportError::Changeset {
                id: id.to_hex(),
                detail: "tree nesting exceeds the maximum supported depth".into(),
            });
        }
        if let Some(sha) = self.tree_shas.get(&id) {
            return Ok(*sha);
        }
        // Prefetched during the window's parallel decode; unexpected ids
        // (or a window past the memo) fall back to an on-demand read.
        let tree: Arc<gardenbed_model::Tree> = match self.trees.get(&id) {
            Some(tree) => tree.clone(),
            None => self
                .engine
                .cached_tree_arc(id)?
                .ok_or_else(|| ExportError::Changeset {
                    id: id.to_hex(),
                    detail: "the derived tree is missing from the object store".into(),
                })?,
        };

        // git tree entry order: entries compare on name bytes, but a
        // directory's name compares as `name + "/"`.
        let mut entries: Vec<(&gardenbed_model::TreeEntry, Vec<u8>)> =
            Vec::with_capacity(tree.entries.len());
        for e in &tree.entries {
            let mut key = Vec::with_capacity(e.name.len() + 1);
            key.extend_from_slice(e.name.as_bytes());
            if e.kind == TreeKind::Dir {
                key.push(b'/');
            }
            entries.push((e, key));
        }
        entries.sort_by(|a, b| a.1.cmp(&b.1));

        let mut body: Vec<u8> = Vec::new();
        for (e, _) in &entries {
            let (mode, sha) = match e.kind {
                TreeKind::File => {
                    let blob = e.blob.ok_or_else(|| ExportError::Changeset {
                        id: id.to_hex(),
                        detail: format!("file entry {} carries no blob id", e.name),
                    })?;
                    let sha = if e.mode == Mode::Gitlink {
                        // The model blob id *is* the upstream git SHA-1
                        // zero-padded to 32 bytes; strip the padding.
                        gitlink_oid(blob)?
                    } else {
                        *self
                            .blob_shas
                            .get(&blob)
                            .ok_or(ExportError::MissingBlob { id: blob })?
                    };
                    (git_mode(e.mode), sha)
                }
                TreeKind::Dir => {
                    let child = e.tree.ok_or_else(|| ExportError::Changeset {
                        id: id.to_hex(),
                        detail: format!("dir entry {} carries no tree id", e.name),
                    })?;
                    ("40000", self.render_node(child, depth + 1)?)
                }
            };
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(e.name.as_bytes());
            body.push(b'\0');
            body.extend_from_slice(&sha);
        }

        let sha = git_sha1(2, &body);
        if self.written.insert(sha) {
            self.pack.push(PackObject {
                kind: 2,
                data: body,
                sha1: sha,
            })?;
        }
        self.tree_shas.insert(id, sha);
        Ok(sha)
    }
}

/// A raw commit's bytes must only reference what this export writes:
/// the tree the model derives (byte-equal header) and parents that are
/// already in the exported set. Otherwise the pack would dangle.
fn validate_raw_closure(
    cs: Id,
    raw: &[u8],
    computed_tree_sha: &[u8; 20],
    exported: &HashMap<Id, [u8; 20]>,
) -> Result<(), ExportError> {
    let tree_hex = to_hex(computed_tree_sha);
    if !raw
        .windows(tree_hex.len() + 6)
        .any(|w| w.starts_with(b"tree ") && &w[5..5 + tree_hex.len()] == tree_hex.as_bytes())
    {
        return Err(ExportError::Changeset {
            id: cs.to_hex(),
            detail: format!(
                "the raw commit bytes do not reference the derived tree {tree_hex} — \
                 pre-canonical modes or non-UTF-8 paths diverge at the tree level; export \
                 refuses rather than emit a broken pack (docs/divergences.md §2/§4; the \
                 raw-tree capture that resolves this is gated on the kernel throughput gate)"
            ),
        });
    }
    let parent_shas: HashSet<[u8; 20]> = exported.values().copied().collect();
    // Headers run to the first empty line; parent lines may sit anywhere
    // among them, so every header line is inspected, not just a prefix.
    let mut pos = 0usize;
    while pos < raw.len() {
        let line_end = match raw[pos..].iter().position(|b| *b == b'\n') {
            Some(i) => pos + i,
            None => break, // no terminating newline: the SHA assertion fires
        };
        let line = &raw[pos..line_end];
        if line.is_empty() {
            break; // end of headers
        }
        if let Some(hex) = line.strip_prefix(b"parent ") {
            if hex.len() != 40 || hex.iter().any(|b| !b.is_ascii_hexdigit()) {
                return Err(ExportError::Changeset {
                    id: cs.to_hex(),
                    detail: format!(
                        "the raw commit names a malformed parent {:?} — export refuses \
                         rather than emit a broken pack (corrupt raw row)",
                        String::from_utf8_lossy(hex)
                    ),
                });
            }
            let sha = hex_sha(hex);
            if !parent_shas.contains(&sha) {
                return Err(ExportError::Changeset {
                    id: cs.to_hex(),
                    detail: format!(
                        "the raw commit names parent {} which this export does not contain \
                         (a limit-slice boundary or an unmapped frontier); export refuses \
                         rather than emit a dangling pack",
                        String::from_utf8_lossy(hex)
                    ),
                });
            }
        }
        pos = line_end + 1;
    }
    Ok(())
}

fn hex_sha(hex: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    for i in 0..20 {
        let hi = (hex[2 * i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (hex[2 * i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out[i] = (hi << 4) | lo;
    }
    out
}
/// Reconstructs a commit object's bytes from the model: the inverse of
/// the importer's header parsing for the reconstructable subset (no
/// extra headers, UTF-8 identities, canonical header order). Mapped
/// commits whose original bytes differ are caught by the SHA assertion
/// (or served verbatim from a raw row — see the module docs).
fn reconstruct_commit(
    cs_id: Id,
    cs: &Changeset,
    tree_sha: &[u8; 20],
    commit_shas: &HashMap<Id, [u8; 20]>,
) -> Result<Vec<u8>, ExportError> {
    let mut body: Vec<u8> = Vec::with_capacity(256 + cs.message.len());
    body.extend_from_slice(b"tree ");
    body.extend_from_slice(&to_hex(tree_sha).into_bytes());
    body.push(b'\n');
    for parent in &cs.parents {
        let sha = commit_shas
            .get(parent)
            .ok_or_else(|| ExportError::Changeset {
                id: parent.to_hex(),
                detail: "parent was not exported before this commit (not in topological order)"
                    .into(),
            })?;
        body.extend_from_slice(b"parent ");
        body.extend_from_slice(&to_hex(sha).into_bytes());
        body.push(b'\n');
    }
    write_identity(&mut body, &cs_id, "author", &cs.author)?;
    write_identity(&mut body, &cs_id, "committer", &cs.committer)?;
    body.push(b'\n');
    body.extend_from_slice(cs.message.as_bytes());
    Ok(body)
}

/// Appends one git ident line: `author Name <email> secs ±hhmm`. The name
/// and email are written **verbatim** — reproducing the original commit
/// bytes is what makes unsigned roundtrips SHA-exact. Bytes the model
/// cannot hold (<, >, newlines) are refused.
fn write_identity(
    out: &mut impl Write,
    cs: &Id,
    role: &str,
    identity: &gardenbed_model::Identity,
) -> Result<(), ExportError> {
    for (field, value) in [("name", &identity.name), ("email", &identity.email)] {
        if value.chars().any(|c| matches!(c, '<' | '>' | '\n' | '\r')) {
            return Err(ExportError::Identity {
                cs: cs.to_hex(),
                field: format!("{role} {field}"),
                detail: format!(
                    "{value:?} carries '<', '>' or a newline; the commit object could not be \
                     re-created"
                ),
            });
        }
    }
    write!(
        out,
        "{role} {} <{}> {} {}",
        identity.name,
        identity.email,
        identity.time_secs,
        format_tz(identity.tz_min)
    )?;
    out.write_all(b"\n")?;
    Ok(())
}

/// Formats `tz_min` as git's raw `±hhmm` form (mirrors the harness:
/// -330 → "-0530", 90 → "+0130", 345 → "+0545").
fn format_tz(tz_min: i32) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let abs = tz_min.unsigned_abs();
    format!("{}{:02}{:02}", sign, abs / 60, abs % 60)
}

/// The model's `Mode` → git's octal mode string. Trees use git's
/// abbreviated directory form `40000` (never `040000`).
fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// The 20-byte upstream git SHA-1 of a gitlink pointer: the model stores
/// it zero-padded to 32 bytes (spec §3); the padding is stripped here.
/// Anything not zero-padded is not a pointer and is refused.
fn gitlink_oid(id: Id) -> Result<[u8; 20], ExportError> {
    let bytes = id.as_bytes();
    if !bytes[..12].iter().all(|b| *b == 0) {
        return Err(ExportError::Changeset {
            id: id.to_hex(),
            detail: "gitlink blob id is not a zero-padded git SHA-1".into(),
        });
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes[12..]);
    Ok(out)
}

/// Creates a bare SHA-1 repository skeleton when `git_dir` is missing;
/// an existing directory must be a bare SHA-1 git repository.
fn prepare_target(git_dir: &Path) -> Result<(), ExportError> {
    let mkdir = |p: &Path| {
        std::fs::create_dir_all(p)
            .map_err(|e| ExportError::Git(format!("creating {}: {e}", p.display())))
    };
    if !git_dir.exists() {
        mkdir(git_dir)?;
        mkdir(&git_dir.join("objects/info"))?;
        mkdir(&git_dir.join("objects/pack"))?;
        mkdir(&git_dir.join("refs/heads"))?;
        mkdir(&git_dir.join("refs/tags"))?;
        std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/main\n")
            .map_err(|e| ExportError::Git(format!("writing HEAD: {e}")))?;
        // Minimal bare repository config: everything else is git's
        // default. `bare = true` plus the sha1 object format.
        std::fs::write(
            git_dir.join("config"),
            b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n",
        )
        .map_err(|e| ExportError::Git(format!("writing config: {e}")))?;
        return Ok(());
    }
    // An existing target must be a bare SHA-1 git repository. Read the
    // config the way any consumer would: gix, isolated.
    let repo = gix::open_opts(git_dir, gix::open::Options::isolated()).map_err(|e| {
        ExportError::Git(format!(
            "{} is not a git repository: {e}",
            git_dir.display()
        ))
    })?;
    if repo.object_hash() != gix::hash::Kind::Sha1 {
        return Err(ExportError::Git(format!(
            "the target repository uses the {:?} object format; exports are sha1 only",
            repo.object_hash()
        )));
    }
    if !repo.is_bare() {
        // Refuse before a byte is written (dogfood #11): advancing the ref
        // of a non-bare repository leaves its index and worktree on the
        // old HEAD, staging the whole checkout as deletions. Updating
        // index/worktree is the sanctioned no-go; refuse, loudly.
        let worktree = repo
            .workdir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| git_dir.display().to_string());
        return Err(ExportError::Git(format!(
            "the target repository is not bare (worktree at {worktree}); \
             use a bare target (git init --bare) or export elsewhere"
        )));
    }
    Ok(())
}

/// Renames a fully written, already-fsynced pack file into place.
/// (`finish` synced its bytes; only the directory entry needs syncing.)
fn fsync_rename(from: &Path, to: &Path) -> Result<(), ExportError> {
    let result = std::fs::rename(from, to).and_then(|_| {
        if let Some(parent) = to.parent() {
            std::fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    });
    result.map_err(|e| ExportError::Git(format!("publishing {}: {e}", to.display())))
}

/// Loose refs + HEAD: `refs/heads/<name> = <40-hex>\n` (nested names get
/// directories), `HEAD = ref: <first ref>` when any ref was exported.
fn write_refs(
    git_dir: &Path,
    tips: &[(String, Id)],
    commit_shas: &HashMap<Id, [u8; 20]>,
) -> Result<(), ExportError> {
    for (name, tip) in tips {
        let sha = commit_shas.get(tip).ok_or_else(|| ExportError::Ref {
            name: name.clone(),
            detail: "its tip changeset was not exported".into(),
        })?;
        let path = git_dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ExportError::Git(format!("creating {}: {e}", parent.display())))?;
        }
        atomic_write(&path, format!("{}\n", to_hex(sha)).as_bytes())?;
    }
    if let Some((first, _)) = tips.first() {
        atomic_write(&git_dir.join("HEAD"), format!("ref: {first}\n").as_bytes())?;
    }
    Ok(())
}

/// tmp + rename, fsynced: a reader never observes a partial ref or pack.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    let tmp = path.with_extension("tmp");
    let write_one = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write_one().map_err(|e| ExportError::Git(format!("writing {}: {e}", path.display())))
}

/// Depth-first post-order over every changeset reachable from `tips`,
/// deduplicating shared ancestry and refusing cycles. Deterministic:
/// tips in ref-name order, parents in stored order.
fn topo_order(
    tips: &[(String, Id)],
    metadata: &dyn MetadataStore,
) -> Result<(Vec<Id>, HashMap<Id, Changeset>), ExportError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        /// On the current DFS path (an ancestor of whatever is being
        /// expanded) — meeting it again is a cycle.
        Visiting,
        /// Fully emitted.
        Done,
    }

    enum Frame {
        Enter(Id),
        Emit(Id),
    }

    let mut state: HashMap<Id, State> = HashMap::new();
    let mut cache: HashMap<Id, Changeset> = HashMap::new();
    let mut order: Vec<Id> = Vec::new();

    for (_, tip) in tips {
        let mut stack = vec![Frame::Enter(*tip)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(id) => match state.get(&id) {
                    Some(State::Done) => {}
                    Some(State::Visiting) => {
                        return Err(ExportError::Changeset {
                            id: id.to_hex(),
                            detail: "the changeset graph has a cycle (a changeset is its own \
                                     ancestor)"
                                .into(),
                        });
                    }
                    None => {
                        state.insert(id, State::Visiting);
                        let cs = load_changeset(metadata, id, &mut cache)?;
                        stack.push(Frame::Emit(id));
                        // Push in reverse so the first parent's subtree is
                        // expanded (and finished) first. A parent listed
                        // twice by one changeset is walked once.
                        let mut pushed = HashSet::new();
                        for p in cs.parents.iter().rev() {
                            if !pushed.insert(*p) {
                                continue;
                            }
                            match state.get(p) {
                                None => stack.push(Frame::Enter(*p)),
                                Some(State::Done) => {}
                                Some(State::Visiting) => {
                                    return Err(ExportError::Changeset {
                                        id: p.to_hex(),
                                        detail: "the changeset graph has a cycle (a changeset \
                                                 is its own ancestor)"
                                            .into(),
                                    });
                                }
                            }
                        }
                    }
                },
                Frame::Emit(id) => {
                    state.insert(id, State::Done);
                    order.push(id);
                }
            }
        }
    }
    Ok((order, cache))
}

/// Fetches a changeset into the walk cache, refusing dangling refs/parents.
fn load_changeset<'a>(
    metadata: &dyn MetadataStore,
    id: Id,
    cache: &'a mut HashMap<Id, Changeset>,
) -> Result<&'a Changeset, ExportError> {
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(id) {
        let (cs, _) = metadata
            .get_changeset(id)?
            .ok_or_else(|| ExportError::Changeset {
                id: id.to_hex(),
                detail: "missing from the metadata (dangling ref or parent)".into(),
            })?;
        e.insert(cs);
    }
    Ok(&cache[&id])
}

/// Every blob a non-gitlink change references, sorted by id
/// (deduplicated — several paths share one blob).
fn collect_blob_ids(order: &[Id], cache: &HashMap<Id, Changeset>) -> Vec<Id> {
    let mut set: HashSet<Id> = HashSet::new();
    for id in order {
        for c in &cache[id].changes {
            if let Some(blob) = c.blob
                && c.mode != Mode::Gitlink
            {
                set.insert(blob);
            }
        }
    }
    let mut ids: Vec<Id> = set.into_iter().collect();
    ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    ids
}

/// Validates a full ref name against git's `check-ref-format` rules;
/// anything git would reject is refused before a bad loose ref lands.
fn validate_ref_name(name: &str) -> Result<(), ExportError> {
    let fail = |detail: String| ExportError::Ref {
        name: name.to_owned(),
        detail,
    };
    if !name.starts_with("refs/") {
        return Err(fail("the name must start with refs/".into()));
    }
    if name.ends_with('/') || name.ends_with('.') {
        return Err(fail("the name must not end with '/' or '.'".into()));
    }
    if name.contains("..") || name.contains("@{") || name == "@" {
        return Err(fail("the name contains '..', '@{' or is '@'".into()));
    }
    if name.contains("//") || name.contains('\\') {
        return Err(fail("the name contains '//' or a backslash".into()));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(fail("the name has an empty component".into()));
        }
        if component.starts_with('.') {
            return Err(fail(format!("the component {component:?} starts with '.'")));
        }
        if component.ends_with(".lock") {
            return Err(fail(format!("the component {component:?} ends with .lock")));
        }
    }
    for &b in name.as_bytes() {
        if b < 0x20 || b == 0x7f {
            return Err(fail("the name contains a control character".into()));
        }
        if matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[') {
            return Err(fail(format!(
                "the name contains {:?} (not allowed in ref names)",
                b as char
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tz_matches_git_raw_dates() {
        assert_eq!(format_tz(0), "+0000");
        assert_eq!(format_tz(330), "+0530");
        assert_eq!(format_tz(-330), "-0530");
        assert_eq!(format_tz(90), "+0130");
        assert_eq!(format_tz(345), "+0545");
        assert_eq!(format_tz(-420), "-0700");
    }

    #[test]
    fn git_object_hashes_match_git() {
        // `printf 'hi\n' | git hash-object --stdin`
        assert_eq!(
            to_hex(&git_sha1(3, b"hi\n")),
            "45b983be36b73c0788dc9cbcb76cbb80fc7bb057"
        );
        // `printf '' | git hash-object -t tree --stdin` (git's well-known empty tree)
        assert_eq!(
            to_hex(&git_sha1(2, b"")),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        );
    }

    #[test]
    fn gitlink_ids_strip_the_zero_padding() {
        let mut bytes = [0u8; 32];
        let sha = [
            0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab,
            0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
        ];
        bytes[12..].copy_from_slice(&sha);
        let id = Id::from_bytes(bytes);
        assert_eq!(gitlink_oid(id).unwrap(), sha);
        // Not zero-padded: not a pointer, refuse it.
        let mut bad = bytes;
        bad[0] = 1;
        assert!(gitlink_oid(Id::from_bytes(bad)).is_err());
    }

    #[test]
    fn ref_names_follow_gits_rules() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/heads/feature/x").is_ok());
        assert!(validate_ref_name("refs/tags/v1.0").is_ok());
        assert!(validate_ref_name("main").is_err());
        assert!(validate_ref_name("HEAD").is_err());
        assert!(validate_ref_name("refs/.bed/tip").is_err(), "dot component");
        assert!(validate_ref_name("refs/heads/a.lock").is_err());
        assert!(validate_ref_name("refs/heads/a..b").is_err());
        assert!(validate_ref_name("refs/heads/a b").is_err(), "space");
        assert!(validate_ref_name("refs/heads/a~b").is_err());
        assert!(validate_ref_name("refs/heads/a^b").is_err());
        assert!(validate_ref_name("refs/heads/a:b").is_err());
        assert!(validate_ref_name("refs/heads/a?b").is_err());
        assert!(validate_ref_name("refs/heads/a*b").is_err());
        assert!(validate_ref_name("refs/heads/a[b").is_err());
        assert!(validate_ref_name("refs/heads/a\\b").is_err());
        assert!(validate_ref_name("refs/heads/@{x}").is_err());
        assert!(validate_ref_name("refs/heads//x").is_err());
        assert!(validate_ref_name("refs/heads/x/").is_err());
        assert!(validate_ref_name("refs/heads/x.").is_err());
    }
}

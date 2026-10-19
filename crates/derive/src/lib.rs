//! gardenbed-derive — the derived data engine: derived data over the stores.
//!
//! [`Engine`] turns the changeset log into answers. Trees are *derived*:
//! `tree(cs) = apply(changes(cs), tree(first parent))` — only directories a
//! change touches are re-created, and every computed tree is cached in objects
//! (D3: materialized eagerly on write, recomputable lazily anywhere else).
//! On top of trees sit the recursive tree diff and the segmented-ancestry
//! skip list backing `merge_base` / `is_ancestor`.
//!
//! On top of the trees sit first-parent lineage **blame** (D7): the walk
//! from the queried changeset down the first-parent chain, following
//! renames through `src_path`, attributing each line to the last changeset
//! that changed it. Blame is accelerated by the segmented-ancestry skip
//! list plus per-changeset **path summaries**: a bounded touched-path set
//! and a cumulative segment Bloom filter prove whole linear segments
//! unchanged in one derived-data read (a false positive costs a step,
//! never a wrong answer).
//!
//! Everything derived is a **cache**: a missing or unreadable entry costs a
//! recompute, never a wrong answer, and every derivation is deterministic
//! (content addressing makes identical inputs converge on identical ids).

mod ancestry;
mod apply;
mod blame;
mod cuttings;
mod diff;
mod merge;
mod pathsummary;
mod xdiff;

pub use ancestry::SegmentEntry;
pub use blame::BlameRange;
pub use cuttings::{Cutting, CuttingKind, Cuttings};
pub use merge::{Merge3Outcome, merge3};
pub use pathsummary::{BLOOM_BITS, MAX_SUMMARY_PATHS, PathSummary};
pub use xdiff::MAX_DIFF_LINES;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use gardenbed_model::{Change, Changeset, Id, Mode, Tree, TreeEntry, TreeKind};
use gardenbed_store::{MetadataStore, ObjectStore, StoreError};

use apply::codec_err;

/// Derived-data kind tag for the tree cache (`cs → tree id`).
pub const DERIVED_TREE: &str = "tree";
/// Derived-data kind tag for the segmented-ancestry skip list.
pub const DERIVED_SKIPLIST: &str = "skiplist";
/// Derived-data kind tag for the per-changeset path summary (blame's
/// segment-skip data: bounded touched-path set + segment bloom).
pub const DERIVED_PATHSUMMARY: &str = "pathsummary";
/// Derived-data kind tag for the per-changeset exact-blob copy/rename
/// facts.
pub const DERIVED_CUTTINGS: &str = "cuttings";

/// One registered derived-data kind — the single registration point a
/// kind passes through. Both eager paths iterate the registry
/// ([`derived_kinds`]): [`Engine::on_changeset_created`] runs each `eager`
/// hook in order, and [`Engine::derived_rows_for_validated_tree`] runs
/// each `for_new` hook. Adding a kind is one module plus one entry in
/// [`DERIVED_KINDS`]; no other wiring exists to miss.
///
/// A kind whose derivation cost scales with the **parent tree size**
/// (not just the changeset's own changes) must not run on the write
/// path — the M5 size-independence budget forbids commit latency that
/// grows with repository size. Such a kind registers as lazy: no eager
/// hook, no `for_new` row; its lazy getter computes and persists on
/// first query instead. `cuttings` is the current lazy kind.
pub struct DerivedKind {
    /// The kind tag persisted in the `derived` table.
    pub name: &'static str,
    /// Eagerly computes and persists this kind's entry for an
    /// already-stored changeset (idempotent; the kind's lazy getter's
    /// compute-and-write half doubles as this hook).
    pub(crate) eager: Option<EagerHook>,
    /// Contributes this kind's rows for a not-yet-stored changeset to the
    /// shared [`DerivedFlow`] (batched import), in registry order.
    pub(crate) for_new: Option<ForNewHook>,
}

/// An eager hook: computes and persists one kind's entry for an
/// already-stored changeset.
pub(crate) type EagerHook = fn(&Engine, Id) -> Result<(), StoreError>;

/// A batched hook: contributes one kind's row to a shared
/// [`DerivedFlow`] for a not-yet-stored changeset.
pub(crate) type ForNewHook = for<'a> fn(&Engine, &mut DerivedFlow<'a>) -> Result<(), StoreError>;

// ---------------------------------------------------------- kind hooks

/// Tree eager hook: derive (or restore from a certificate) and cache.
fn tree_eager(engine: &Engine, cs_id: Id) -> Result<(), StoreError> {
    let tree = engine.derive_tree(cs_id)?;
    // A durable certificate can answer derive_tree without consulting
    // derived data. Explicit eager rebuilds must still restore its tree row.
    engine.cache_tree(cs_id, tree)
}

/// Tree batched hook: the already-replayed root is the whole payload.
fn tree_for_new(_engine: &Engine, flow: &mut DerivedFlow<'_>) -> Result<(), StoreError> {
    flow.push(DERIVED_TREE, cache_payload(flow.root));
    Ok(())
}

/// Skiplist eager hook: the lazy getter computes and persists.
fn skiplist_eager(engine: &Engine, cs_id: Id) -> Result<(), StoreError> {
    engine.skiplist_entry(cs_id).map(|_| ())
}

/// Skiplist batched hook: the head's entry is computed for the caller to
/// store with the changeset row (batched import), never written here.
fn skiplist_for_new(engine: &Engine, flow: &mut DerivedFlow<'_>) -> Result<(), StoreError> {
    let bytes = flow.segment_entry(engine)?.encode();
    flow.push(DERIVED_SKIPLIST, bytes);
    Ok(())
}

/// Pathsummary eager hook: the lazy getter computes and persists.
fn pathsummary_eager(engine: &Engine, cs_id: Id) -> Result<(), StoreError> {
    engine.pathsummary_entry(cs_id).map(|_| ())
}

/// Pathsummary batched hook: consumes the skiplist kind's segment entry
/// from the flow (computed once, shared — never recomputed).
fn pathsummary_for_new(engine: &Engine, flow: &mut DerivedFlow<'_>) -> Result<(), StoreError> {
    let bytes = {
        let cs = flow.cs;
        let seg = flow.segment_entry(engine)?;
        pathsummary::pathsummary_entry_for_new(engine, cs, seg)?.encode()
    };
    flow.push(DERIVED_PATHSUMMARY, bytes);
    Ok(())
}

// Cuttings is **lazy-only**: `build_cuttings` flattens the whole first
// parent's tree (O(changes + parent size)), which is fine for a query
// but is exactly what the M5 size-independence budget forbids on the
// commit path. The eager and batched hooks stay `None`; the lazy getter
// [`Engine::cuttings_entry`] computes and persists on first query.

/// The derived-data kind registry, in eager-write and row order: tree,
/// skiplist, pathsummary, cuttings. The order is part of the
/// batched-import contract (the row list keeps this exact sequence); the
/// skiplist's segment entry flows to the pathsummary through
/// [`DerivedFlow`], so it is computed exactly once per flow.
static DERIVED_KINDS: &[DerivedKind] = &[
    DerivedKind {
        name: DERIVED_TREE,
        eager: Some(tree_eager),
        for_new: Some(tree_for_new),
    },
    DerivedKind {
        name: DERIVED_SKIPLIST,
        eager: Some(skiplist_eager),
        for_new: Some(skiplist_for_new),
    },
    DerivedKind {
        name: DERIVED_PATHSUMMARY,
        eager: Some(pathsummary_eager),
        for_new: Some(pathsummary_for_new),
    },
    DerivedKind {
        name: DERIVED_CUTTINGS,
        eager: None,
        for_new: None,
    },
];

/// The registered derived-data kinds, in eager-write order — the order
/// [`Engine::on_changeset_created`] writes them and
/// [`Engine::derived_rows_for_validated_tree`] lists them.
pub fn derived_kinds() -> &'static [DerivedKind] {
    DERIVED_KINDS
}

/// The derived data engine over one repository's stores.
///
/// Cheap to share: both stores are behind `Arc` and every method takes
/// `&self`. Holds trait objects so the server (and any other tier) can keep
/// one regardless of the backend implementations.
///
/// Two process-lifetime caches hang off the engine, both strictly bounded
/// and keyed by content addresses (which never decode to different bytes):
/// decoded trees for [`Engine::tree_entry`] and finished blame walks for
/// [`Engine::blame`]. History is immutable under content addressing, so a
/// cached answer can never go stale; entries only save recomputation.
/// Memoized finished blame walks, keyed `(changeset, path)`. History is
/// immutable under content addressing, so a finished walk's answer is final.
type BlameMemo = CacheMap<(Id, Box<str>), Arc<Vec<BlameRange>>>;

#[derive(Clone)]
pub struct Engine {
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    /// Decoded trees by tree id (`tree_entry`'s hot path).
    trees: Arc<Mutex<CacheMap<Id, Arc<Tree>>>>,
    /// Blame results by `(changeset, path)` (the walk's hot path).
    blame: Arc<Mutex<BlameMemo>>,
}

/// A bounded FIFO map used for the engine's decoded-tree and blame caches.
/// Insertion-ordered eviction keeps the footprint capped; a poisoned lock
/// is recovered from (the entries themselves stay consistent — a panic can
/// only strike between a map insert and its queue push, which at worst
/// delays one eviction).
struct CacheMap<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
    cap_entries: usize,
    cap_bytes: usize,
    bytes: usize,
    /// The value's approximate memory weight, for the byte cap.
    weight: fn(&V) -> usize,
}

impl<K: Clone + Eq + std::hash::Hash, V: Clone> CacheMap<K, V> {
    fn new(cap_entries: usize, cap_bytes: usize, weight: fn(&V) -> usize) -> Self {
        CacheMap {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap_entries,
            cap_bytes,
            bytes: 0,
            weight,
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.map.get(key).cloned()
    }

    fn put(&mut self, key: K, value: V) {
        let w = (self.weight)(&value);
        match self.map.insert(key.clone(), value) {
            None => {
                // Fresh key: FIFO order entry and full weight.
                self.order.push_back(key);
                self.bytes += w;
            }
            Some(old) => {
                // Re-insert over an existing key: the old value's weight
                // must come off (it is no longer cached) and the new one
                // on, or `bytes` drifts upward on every refresh. The key
                // keeps its original place in the FIFO order — pushing a
                // duplicate would make later eviction pop a key that is
                // still cached, leaking both its entry and its weight.
                self.bytes += w;
                self.bytes -= (self.weight)(&old);
            }
        }
        while self.map.len() > self.cap_entries || self.bytes > self.cap_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(v) = self.map.remove(&oldest) {
                self.bytes -= (self.weight)(&v);
            }
        }
    }
}

/// Caps for the decoded-tree cache (`tree_entry`): root trees of a corpus
/// like git.git are a few KiB each.
const TREE_CACHE_ENTRIES: usize = 1024;
const TREE_CACHE_BYTES: usize = 32 << 20;
/// Caps for the blame-result cache (`blame`): bounded both in walks and in
/// total cached ranges.
const BLAME_CACHE_ENTRIES: usize = 256;
const BLAME_CACHE_BYTES: usize = 32 << 20;

fn blame_weight(ranges: &Arc<Vec<BlameRange>>) -> usize {
    ranges.len() * std::mem::size_of::<BlameRange>()
}

fn tree_weight(tree: &Arc<Tree>) -> usize {
    std::mem::size_of::<Tree>()
        + tree.entries.capacity() * std::mem::size_of::<TreeEntry>()
        + tree
            .entries
            .iter()
            .map(|e| e.name.capacity())
            .sum::<usize>()
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    /// An engine over `objects` (objects) and `metadata` (metadata).
    pub fn new(objects: Arc<dyn ObjectStore>, metadata: Arc<dyn MetadataStore>) -> Self {
        Engine {
            objects,
            metadata,
            trees: Arc::new(Mutex::new(CacheMap::new(
                TREE_CACHE_ENTRIES,
                TREE_CACHE_BYTES,
                tree_weight,
            ))),
            blame: Arc::new(Mutex::new(CacheMap::new(
                BLAME_CACHE_ENTRIES,
                BLAME_CACHE_BYTES,
                blame_weight,
            ))),
        }
    }

    /// The object store (objects).
    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    /// The metadata store (metadata).
    pub fn metadata(&self) -> &Arc<dyn MetadataStore> {
        &self.metadata
    }

    // ------------------------------------------------------------ trees

    /// Replay directly against an explicit tree, without reading mutable
    /// derived caches. Used by validation certificates and legacy migration.
    pub fn apply_changeset_to_tree(
        &self,
        base: Option<Id>,
        cs: &Changeset,
    ) -> Result<Option<Id>, StoreError> {
        apply::apply_changeset(&*self.objects, base, cs)
    }

    /// Validate a candidate against certified parents. No generic derived
    /// cache participates in this proof. The returned root can be committed
    /// with `MetadataStore::put_validated_changeset` after derived rows are
    /// prepared with `derived_rows_for_validated_tree`.
    pub fn validated_tree_for_new(&self, cs: &Changeset) -> Result<Option<Id>, StoreError> {
        gardenbed_model::codec::validate_changeset(cs, true).map_err(codec_err)?;
        let mut base = None;
        for (index, parent) in cs.parents.iter().enumerate() {
            let root = self
                .metadata
                .validation_tree(*parent)?
                .ok_or_else(|| StoreError::Invalid(format!("parent {parent} is not certified")))?;
            if index == 0 {
                base = root;
            }
        }
        for change in &cs.changes {
            if let Some(blob) = change.blob
                && change.mode != Mode::Gitlink
                && !self.objects.has(blob)?
            {
                return Err(StoreError::Invalid(format!(
                    "referenced blob {blob} is missing"
                )));
            }
        }
        self.apply_changeset_to_tree(base, cs)
    }

    /// Validate legacy history bottom-up until durable v1 certificates form
    /// a frontier. Every parent, including non-first parents, is checked.
    /// Valid ancestors are certified immediately, so retries and restarts
    /// resume at that frontier. No history-sized process cache is retained.
    pub fn certify_history(&self, head: Id) -> Result<(), StoreError> {
        let mut pending = vec![(head, false)];
        while let Some((id, ready)) = pending.pop() {
            if self.metadata.validation_tree(id)?.is_some() {
                continue;
            }
            let cs = self.get_changeset(id)?;
            if cs.id().map_err(codec_err)? != id {
                return Err(StoreError::Corrupt(format!(
                    "changeset content does not match addressed id {id}"
                )));
            }
            if !ready {
                pending.push((id, true));
                pending.extend(cs.parents.iter().map(|parent| (*parent, false)));
                continue;
            }
            let root = self.validated_tree_for_new(&cs)?;
            // An EXPLICIT no-op verify (round-2 review, P3): certify
            // history re-certifies already-committed rows whose objects
            // the gc mark already sees live (they are reachable from a
            // certified changeset), so there is nothing to re-check — but
            // passing the no-op makes that a visible decision at the call
            // site instead of a silent `None` skip a future caller could
            // copy without justification.
            let no_op = || Ok(());
            self.metadata
                .put_validated_changeset(&cs, root, &[], None, None, Some(&no_op))?;
        }
        Ok(())
    }

    /// The root tree of changeset `cs_id`: `apply(changes(cs), tree(first
    /// parent))`. `None` when the changeset's tree is empty (no files
    /// anywhere in history).
    ///
    /// Only touched directories are re-created; untouched subtrees are kept
    /// by id and never loaded. For cold history the first-parent chain is
    /// walked upward until a cached tree is found (or the root), then the
    /// changes are applied back down, caching every intermediate tree
    /// best-effort. Idempotent and crash-safe: a missing cache entry only
    /// costs a recompute.
    ///
    /// Errors with [`StoreError::NotFound`] when the changeset (or a
    /// referenced object) does not exist, and with
    /// [`StoreError::Invalid`] when a changeset is inconsistent with its
    /// first parent's tree (e.g. deleting a path that is not there).
    pub fn derive_tree(&self, cs_id: Id) -> Result<Option<Id>, StoreError> {
        if let Some(cached) = self.cached_tree(cs_id)? {
            return Ok(cached);
        }
        // Walk the first-parent chain upward until a cached tree shows up
        // (or the root), collecting the uncached changesets on the way.
        let mut chain: Vec<(Id, Changeset)> = Vec::new();
        let mut cur = cs_id;
        let base: Option<Id> = loop {
            if let Some(cached) = self.cached_tree(cur)? {
                break cached;
            }
            let cs = self.get_changeset(cur)?;
            let parent = cs.parents.first().copied();
            chain.push((cur, cs));
            match parent {
                // Root: derive from the empty tree.
                None => break None,
                Some(p) => cur = p,
            }
        };
        // Apply back down the chain, oldest first, caching as we go.
        let mut tree = base;
        for (id, cs) in chain.iter().rev() {
            tree = apply::apply_changeset(&*self.objects, tree, cs)?;
            self.cache_tree(*id, tree)?;
        }
        Ok(tree)
    }

    /// Computes the tree of a **not yet stored** changeset `cs_id`/`cs`,
    /// writing cache rows only for uncached ancestors (rare: a gap in the
    /// cache under an already-committed changeset). The head's cache payload
    /// is *returned*, not written — the caller stores it in the same
    /// transaction as the changeset row itself, so the cache entry can never
    /// name a missing changeset. Returns `(tree id, cache payload)` where the
    /// payload mirrors [`Engine::cache_tree`]'s encoding (empty = empty
    /// tree).
    fn derive_tree_for_new(
        &self,
        cs_id: Id,
        cs: &Changeset,
    ) -> Result<(Option<Id>, Vec<u8>), StoreError> {
        if let Some(cached) = self.cached_tree(cs_id)? {
            return Ok((cached, cache_payload(cached)));
        }
        // The head's content is in hand; only ancestors are read from the
        // store (they are committed).
        let mut chain: Vec<(Id, Changeset)> = vec![(cs_id, cs.clone())];
        // Walk the first-parent chain upward until a cached tree shows up
        // (or the root, whose base is the empty tree).
        let mut base: Option<Id> = None;
        let mut cur = cs.parents.first().copied();
        while let Some(p) = cur {
            if let Some(cached) = self.cached_tree(p)? {
                base = cached;
                break;
            }
            let anc = self.get_changeset(p)?;
            cur = anc.parents.first().copied();
            chain.push((p, anc));
        }
        // Apply back down the chain, oldest first. Intermediates cache
        // immediately (their changeset rows are already committed); the
        // head's payload goes back to the caller.
        let mut tree = base;
        for (i, (id, anc)) in chain.iter().enumerate().rev() {
            tree = apply::apply_changeset(&*self.objects, tree, anc)?;
            if i == 0 {
                return Ok((tree, cache_payload(tree)));
            }
            self.cache_tree(*id, tree)?;
        }
        unreachable!("the head is always chain[0]")
    }

    /// Alias of [`Engine::derive_tree`] (the RPC name).
    pub fn root_tree(&self, cs_id: Id) -> Result<Option<Id>, StoreError> {
        self.derive_tree(cs_id)
    }

    /// Decodes the stored tree payload for `id`; `None` when absent.
    pub fn tree(&self, id: Id) -> Result<Option<Tree>, StoreError> {
        match self.objects.get_tree(id)? {
            None => Ok(None),
            Some(bytes) => Tree::decode(&bytes).map(Some).map_err(codec_err),
        }
    }

    /// The entry at `path` under root tree `root` (`None` = empty tree);
    /// `None` when the path does not exist.
    pub fn tree_entry(
        &self,
        root: Option<Id>,
        path: &str,
    ) -> Result<Option<TreeEntry>, StoreError> {
        if path.is_empty() {
            return Err(StoreError::Invalid("path must be non-empty".into()));
        }
        let mut cur = root;
        let comps: Vec<&str> = path.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = match cur {
                None => return Ok(None),
                Some(id) => self.decoded_tree(id)?,
            };
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                return Ok(None);
            };
            if i + 1 == comps.len() {
                return Ok(Some(entry.clone()));
            }
            match entry.kind {
                TreeKind::Dir => cur = entry.tree,
                TreeKind::File => return Ok(None), // a file cannot contain paths
            }
        }
        unreachable!("loop returns on the final component")
    }

    /// The decoded tree `id`, through the engine's decoded-tree cache:
    /// consecutive blame/diff steps on neighboring changesets repeatedly
    /// ask for the same tree ids (a directory's id is stable across commits
    /// that do not touch it), so a bounded cache turns most loads into
    /// cache hits. Content addressing makes the entry immortal — the same
    /// id always decodes to the same entries.
    ///
    /// The public twin of [`Engine::tree`] for hot paths: same content,
    /// shared `Arc` instead of a fresh clone, and served from the cache
    /// (the export's per-commit tree rendering hits the same stable ids
    /// thousands of times in a row). A missing object surfaces as the
    /// store's `Corrupt` error — a referenced tree id that is absent is
    /// corruption, not an absence to report — so no existence probe runs
    /// first: one store roundtrip per uncached id.
    pub fn cached_tree_arc(&self, id: Id) -> Result<Option<Arc<Tree>>, StoreError> {
        Ok(Some(self.decoded_tree(id)?))
    }

    fn decoded_tree(&self, id: Id) -> Result<Arc<Tree>, StoreError> {
        if let Some(tree) = self.lock_trees().get(&id) {
            return Ok(tree);
        }
        let tree = Arc::new(Self::load_tree(&*self.objects, id)?);
        self.lock_trees().put(id, tree.clone());
        Ok(tree)
    }

    fn lock_trees(&self) -> std::sync::MutexGuard<'_, CacheMap<Id, Arc<Tree>>> {
        match self.trees.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_blame(&self) -> std::sync::MutexGuard<'_, BlameMemo> {
        match self.blame.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Flattened recursive listing of a tree: `(path, mode, blob id)` per
    /// file, sorted by path bytes. `None` lists nothing (empty tree).
    pub fn flatten_tree(&self, root: Option<Id>) -> Result<Vec<(String, Mode, Id)>, StoreError> {
        let mut out = Vec::new();
        self.flatten_into(root, "", &mut out, 0)?;
        out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        Ok(out)
    }

    fn flatten_into(
        &self,
        root: Option<Id>,
        dir: &str,
        out: &mut Vec<(String, Mode, Id)>,
        depth: usize,
    ) -> Result<(), StoreError> {
        if depth > apply::MAX_TREE_DEPTH {
            return Err(StoreError::Invalid(
                "tree nesting exceeds the maximum supported depth".into(),
            ));
        }
        let Some(id) = root else { return Ok(()) };
        for e in Self::load_tree(&*self.objects, id)?.entries {
            let path = if dir.is_empty() {
                e.name.clone()
            } else {
                format!("{dir}/{}", e.name)
            };
            match e.kind {
                TreeKind::File => {
                    let blob = e.blob.ok_or_else(|| {
                        StoreError::Corrupt(format!("file entry {:?} without a blob id", e.name))
                    })?;
                    out.push((path, e.mode, blob));
                }
                TreeKind::Dir => {
                    self.flatten_into(e.tree, &path, out, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------- diff

    /// Recursive tree diff between the trees of changesets `a` and `b`
    /// (a → b): ops Add/Modify/Delete, rename detection off, paths in
    /// changeset canonical order (ascending path bytes).
    pub fn diff(&self, a: Id, b: Id) -> Result<Vec<Change>, StoreError> {
        let ta = self.derive_tree(a)?;
        let tb = self.derive_tree(b)?;
        self.diff_trees(ta, tb)
    }

    /// Tree diff between two tree ids (`None` = the empty tree).
    pub fn diff_trees(&self, a: Option<Id>, b: Option<Id>) -> Result<Vec<Change>, StoreError> {
        diff::diff_trees(&*self.objects, a, b)
    }

    // ---------------------------------------------------------- internal

    /// The changeset `id`, or [`StoreError::NotFound`].
    pub(crate) fn get_changeset(&self, id: Id) -> Result<Changeset, StoreError> {
        self.metadata
            .get_changeset(id)?
            .map(|(cs, _)| cs)
            .ok_or(StoreError::NotFound)
    }

    /// Decodes the tree stored under `id`; a missing referenced tree is
    /// store corruption (every referenced tree is written before the
    /// reference that names it).
    pub(crate) fn load_tree(objects: &dyn ObjectStore, id: Id) -> Result<Tree, StoreError> {
        match objects.get_tree(id)? {
            None => Err(StoreError::Corrupt(format!(
                "tree {id} is referenced but missing from the object store"
            ))),
            Some(bytes) => Tree::decode(&bytes).map_err(codec_err),
        }
    }

    /// The cached tree for `cs`: `Some(Some(id))` / `Some(None)` (cached
    /// empty tree) on hit, `None` on miss. A cached id whose object has
    /// vanished is treated as a miss (recompute), never trusted blindly.
    fn cached_tree(&self, cs: Id) -> Result<Option<Option<Id>>, StoreError> {
        if let Some(certified) = self.metadata.validation_tree(cs)? {
            return match certified {
                None => Ok(Some(None)),
                Some(root) if self.objects.has(root)? => Ok(Some(Some(root))),
                Some(_) => Ok(None), // Missing object: recompute from history.
            };
        }
        match self.metadata.derived_get(DERIVED_TREE, cs)? {
            None => Ok(None),
            Some(bytes) if bytes.is_empty() => Ok(Some(None)),
            Some(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                let id = Id::from_bytes(arr);
                if self.objects.has(id)? {
                    Ok(Some(Some(id)))
                } else {
                    Ok(None)
                }
            }
            // Malformed cache payload: treat as a miss and recompute.
            Some(_) => Ok(None),
        }
    }

    /// Best-effort tree cache write. Called after the tree object itself is
    /// in objects, so the cache entry never names a missing object.
    fn cache_tree(&self, cs: Id, tree: Option<Id>) -> Result<(), StoreError> {
        self.metadata
            .derived_put(DERIVED_TREE, cs, &cache_payload(tree))
    }

    /// Computes the derived-data rows for a **not yet stored** changeset
    /// without writing them: the derived tree, the segmented-ancestry skip
    /// list entry and the path summary — exactly what
    /// [`Engine::on_changeset_created`] would persist, but as plain rows for
    /// the caller to store atomically together with the changeset (batched
    /// import). Ancestor backfill rows are written directly (their
    /// changeset rows are already committed); only rows keyed by `cs_id` are
    /// deferred.
    ///
    /// The row values are byte-identical to what the eager path writes, so
    /// import-through-`ingest_changeset` and import-through-
    /// `on_changeset_created` produce the same store contents.
    pub fn derived_rows_for_new(
        &self,
        cs_id: Id,
        cs: &Changeset,
    ) -> Result<Vec<gardenbed_store::DerivedRow>, StoreError> {
        let (root, _) = self.derive_tree_for_new(cs_id, cs)?;
        self.derived_rows_for_validated_tree(cs_id, cs, root)
    }

    /// Prepare derived rows using an already replayed root. This avoids a
    /// second tree application on native and import validation paths. This
    /// method only prepares caches; it does not grant a validation certificate.
    pub fn derived_rows_for_validated_tree(
        &self,
        cs_id: Id,
        cs: &Changeset,
        root: Option<Id>,
    ) -> Result<Vec<gardenbed_store::DerivedRow>, StoreError> {
        let mut flow = DerivedFlow {
            cs_id,
            cs,
            root,
            rows: Vec::with_capacity(DERIVED_KINDS.len()),
            segment: None,
        };
        for kind in DERIVED_KINDS {
            if let Some(for_new) = kind.for_new {
                for_new(self, &mut flow)?;
            }
        }
        Ok(flow.rows)
    }
}

/// The shared state of one batched-import derivation: the changeset, the
/// already-replayed root, the rows accumulated so far (registry order),
/// and the lazily-shared skiplist segment entry (computed at most once,
/// consumed by both the skiplist and the pathsummary kinds).
pub(crate) struct DerivedFlow<'a> {
    cs_id: Id,
    cs: &'a Changeset,
    root: Option<Id>,
    rows: Vec<gardenbed_store::DerivedRow>,
    segment: Option<ancestry::SegmentEntry>,
}

impl DerivedFlow<'_> {
    /// Appends one row (registry order; the batched-import contract).
    fn push(&mut self, kind: &str, bytes: Vec<u8>) {
        self.rows.push(gardenbed_store::DerivedRow {
            kind: kind.to_owned(),
            cs: self.cs_id,
            bytes,
        });
    }

    /// The changeset's segment entry, computed once per flow and shared
    /// by every kind that needs it (skiplist row + pathsummary).
    fn segment_entry(&mut self, engine: &Engine) -> Result<&ancestry::SegmentEntry, StoreError> {
        if self.segment.is_none() {
            self.segment = Some(ancestry::skiplist_entry_for_new(
                engine, self.cs_id, self.cs,
            )?);
        }
        Ok(self.segment.as_ref().expect("just set"))
    }
}

/// The tree-cache payload for a tree id (`None` = empty tree): the raw id
/// bytes, or nothing. Mirrors the on-store encoding of `cache_tree`.
fn cache_payload(tree: Option<Id>) -> Vec<u8> {
    match tree {
        None => Vec::new(),
        Some(id) => id.as_bytes().to_vec(),
    }
}

//! gardenbed-derive — the compost engine: derived data over the stores.
//!
//! [`Engine`] turns the changeset log into answers. Trees are *derived*:
//! `tree(cs) = apply(changes(cs), tree(first parent))` — only directories a
//! change touches are re-created, and every computed tree is cached in loam
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
mod diff;
mod pathsummary;
mod xdiff;

pub use ancestry::SegmentEntry;
pub use blame::BlameRange;
pub use pathsummary::{BLOOM_BITS, MAX_SUMMARY_PATHS, PathSummary};

use std::sync::Arc;

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

/// The compost engine over one repository's stores.
///
/// Cheap to share: both stores are behind `Arc` and every method takes
/// `&self`. Holds trait objects so the server (and any other tier) can keep
/// one regardless of the backend implementations.
#[derive(Clone)]
pub struct Engine {
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    /// An engine over `objects` (loam) and `metadata` (seedbed).
    pub fn new(objects: Arc<dyn ObjectStore>, metadata: Arc<dyn MetadataStore>) -> Self {
        Engine { objects, metadata }
    }

    /// The object store (loam).
    pub fn objects(&self) -> &Arc<dyn ObjectStore> {
        &self.objects
    }

    /// The metadata store (seedbed).
    pub fn metadata(&self) -> &Arc<dyn MetadataStore> {
        &self.metadata
    }

    // ------------------------------------------------------------ trees

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
                Some(id) => Self::load_tree(&*self.objects, id)?,
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
    /// in loam, so the cache entry never names a missing object.
    fn cache_tree(&self, cs: Id, tree: Option<Id>) -> Result<(), StoreError> {
        let payload = match tree {
            None => Vec::new(),
            Some(id) => id.as_bytes().to_vec(),
        };
        self.metadata.derived_put(DERIVED_TREE, cs, &payload)
    }
}

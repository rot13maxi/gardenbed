//! Git object access for the import: trees, path resolution, blobs.
//!
//! Everything gitoxide-based (decision D6). Two caches keep the hot paths
//! cheap: parsed trees by git id (path lookups and re-root walks hit the
//! same directories across commits) and, more importantly, the mapping
//! from a git blob id to the gardenbed blob `Id` — importing a blob needs
//! its SHA-256, so raw content is read (and written to loam) exactly once
//! per git blob, even when hundreds of commits touch it.
//!
//! Cache bounds are hard caps with a clear-on-overflow policy: memory stays
//! bounded on huge imports (the linux kernel has millions of unique blobs;
//! caches rebuild themselves for the working locality instead of growing
//! unboundedly). Gitlink pointers are never cached, never read, and never
//! written as blobs — their gardenbed blob field is the upstream git SHA-1
//! left-padded to 32 bytes (canonical-encoding spec §3).

use std::collections::HashMap;
use std::sync::Arc;

use gardenbed_model::{Id, Mode, blob_id};
use gardenbed_store::ObjectStore;

use crate::error::ImportError;
use crate::gtree::{GEntry, GKind, parse_tree};
use crate::oid::GitOid;

/// Maximum cached parsed trees.
const TREE_CACHE_CAP: usize = 100_000;
/// Maximum cached git-blob → gardenbed-Id mappings.
const BLOB_CACHE_CAP: usize = 4_000_000;
/// Maximum path nesting when walking trees (matches the apply/diff guard).
/// Shared with the raw tree-diff fallback in `import`.
pub(crate) const MAX_TREE_DEPTH: usize = 4096;

/// Reads git objects through gix and mirrors blobs into the loam.
pub(crate) struct GitObjects<'r> {
    repo: &'r gix::Repository,
    /// The counting object store: blob writes are reported as
    /// `objects_written` only when they create a loam entry.
    objects: &'r dyn ObjectStore,
    trees: HashMap<GitOid, Arc<Vec<GEntry>>>,
    blobs: HashMap<GitOid, Id>,
}

impl<'r> GitObjects<'r> {
    pub(crate) fn new(repo: &'r gix::Repository, objects: &'r dyn ObjectStore) -> Self {
        GitObjects {
            repo,
            objects,
            trees: HashMap::new(),
            blobs: HashMap::new(),
        }
    }

    /// The entries of a git tree (parsed and cached).
    pub(crate) fn tree_entries(
        &mut self,
        tree: GitOid,
        sha_hex: &str,
    ) -> Result<Arc<Vec<GEntry>>, ImportError> {
        if let Some(cached) = self.trees.get(&tree) {
            return Ok(Arc::clone(cached));
        }
        let obj = self
            .repo
            .find_object(crate::graph::gix_id(tree))
            .map_err(|e| ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: format!("cannot read tree: {e}"),
            })?;
        if obj.kind != gix::object::Kind::Tree {
            return Err(ImportError::Object {
                oid: sha_hex.to_owned(),
                detail: format!("expected a tree object, found {:?}", obj.kind),
            });
        }
        let entries = parse_tree(&obj.data).map_err(|detail| ImportError::Object {
            oid: sha_hex.to_owned(),
            detail,
        })?;
        if self.trees.len() >= TREE_CACHE_CAP {
            self.trees.clear();
        }
        let entries = Arc::new(entries);
        self.trees.insert(tree, Arc::clone(&entries));
        Ok(entries)
    }

    /// Resolves `path` (a file or gitlink in git's model) under tree
    /// `root` to its entry. Errors when the path is missing or turns out
    /// to be a directory — the diff said this path is a blob-level entry,
    /// so anything else means git's output and the tree disagree.
    pub(crate) fn resolve_path(
        &mut self,
        root: GitOid,
        path: &str,
        commit_hex: &str,
    ) -> Result<(Mode, GitOid), ImportError> {
        let components: Vec<&str> = path.split('/').collect();
        let mut cur = root;
        for (i, comp) in components.iter().enumerate() {
            let entries = self.tree_entries(cur, commit_hex)?;
            let Some(entry) = entries.iter().find(|e| e.name == *comp) else {
                return Err(ImportError::Diff {
                    sha: commit_hex.to_owned(),
                    detail: format!("path {path:?} is missing from the commit's tree"),
                });
            };
            let last = i + 1 == components.len();
            match (&entry.kind, last) {
                (GKind::File, true) => return Ok((entry.mode, entry.oid)),
                (GKind::Dir, false) => cur = entry.oid,
                (GKind::File, false) => {
                    return Err(ImportError::Diff {
                        sha: commit_hex.to_owned(),
                        detail: format!("path {path:?} crosses a file entry ({:?})", components[i]),
                    });
                }
                (GKind::Dir, true) => {
                    return Err(ImportError::Diff {
                        sha: commit_hex.to_owned(),
                        detail: format!("path {path:?} is a directory, not a file"),
                    });
                }
            }
        }
        unreachable!("empty paths are rejected before resolution")
    }

    /// The gardenbed blob `Id` for a git blob id, writing the blob into
    /// loam on first sight. Gitlink pointers never reach this: their
    /// synthetic padded id is computed by the caller.
    ///
    /// `context_oid` names the *referencing* tree/commit for error
    /// messages (the blob itself may legitimately be absent from this
    /// repository, e.g. a shallow or partial clone — an error either way).
    pub(crate) fn ensure_blob(
        &mut self,
        blob: GitOid,
        context_oid: &str,
    ) -> Result<Id, ImportError> {
        if let Some(id) = self.blobs.get(&blob) {
            return Ok(*id);
        }
        let hex = blob.to_hex();
        let obj = self
            .repo
            .find_object(crate::graph::gix_id(blob))
            .map_err(|e| ImportError::Object {
                oid: hex.clone(),
                detail: format!("cannot read blob (referenced by {context_oid}): {e}"),
            })?;
        if obj.kind != gix::object::Kind::Blob {
            return Err(ImportError::Object {
                oid: hex.clone(),
                detail: format!("expected a blob object, found {:?}", obj.kind),
            });
        }
        let id = blob_id(&obj.data);
        let put = self.objects.put_blob(&obj.data)?;
        if put != id {
            return Err(ImportError::Other(format!(
                "object store addressed blob {hex} as {put} instead of {id}"
            )));
        }
        if self.blobs.len() >= BLOB_CACHE_CAP {
            self.blobs.clear();
        }
        self.blobs.insert(blob, id);
        Ok(id)
    }

    /// Recursively flattens a git tree into `(path, mode, blob id)` file
    /// entries — the seed of a re-rooted changeset (a `limit` slice
    /// boundary). Sorted by path bytes on return.
    pub(crate) fn flatten_tree(
        &mut self,
        root: GitOid,
        commit_hex: &str,
    ) -> Result<Vec<(String, Mode, GitOid)>, ImportError> {
        let mut out = Vec::new();
        self.flatten_into(root, "", commit_hex, 0, &mut out)?;
        out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        Ok(out)
    }

    fn flatten_into(
        &mut self,
        tree: GitOid,
        prefix: &str,
        commit_hex: &str,
        depth: usize,
        out: &mut Vec<(String, Mode, GitOid)>,
    ) -> Result<(), ImportError> {
        if depth > MAX_TREE_DEPTH {
            return Err(ImportError::Object {
                oid: commit_hex.to_owned(),
                detail: "tree nesting exceeds the maximum supported depth".into(),
            });
        }
        let entries = self.tree_entries(tree, commit_hex)?;
        for entry in entries.iter() {
            let path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{prefix}/{}", entry.name)
            };
            match entry.kind {
                GKind::File => out.push((path, entry.mode, entry.oid)),
                GKind::Dir => {
                    self.flatten_into(entry.oid, &path, commit_hex, depth + 1, out)?;
                }
            }
        }
        Ok(())
    }
}

/// The gardenbed blob id of a gitlink pointer: 12 zero bytes followed by
/// the 20-byte upstream git SHA-1 (canonical-encoding spec §3).
pub(crate) fn gitlink_blob_id(upstream: GitOid) -> Id {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(upstream.as_bytes());
    Id::from_bytes(bytes)
}

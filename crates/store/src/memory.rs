//! In-memory implementations of both storage traits, for tests in later
//! milestones (and as the semantic reference for the on-disk backends).

use crate::StoreError;
use crate::traits::MetadataStore;
use crate::traits::ObjectStore;
use crate::traits::RefCas;
use gardenbed_model::Changeset;
use gardenbed_model::Id;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::RwLock;

/// One interior state guarded by a single `RwLock`: correct and plenty fast
/// for tests.
#[derive(Debug, Default)]
struct MemoryInner {
    /// Blob id → raw (decompressed) content.
    blobs: HashMap<Id, Vec<u8>>,
    /// Tree id → canonical tree bytes.
    trees: HashMap<Id, Vec<u8>>,
    /// Changeset id → (decoded, canonical bytes).
    changesets: HashMap<Id, (Changeset, Vec<u8>)>,
    /// Insertion order of `changesets`: the arrival log behind
    /// `recent_changesets`. First arrival wins (idempotent re-puts keep
    /// the original position), mirroring SQLite's rowid.
    changeset_arrival: std::collections::VecDeque<Id>,
    /// Ref name → changeset id.
    refs: BTreeMap<String, Id>,
    /// Git SHA (lowercase) → changeset id.
    git_by_sha: HashMap<String, Id>,
    /// Changeset id → git SHA.
    git_by_cs: HashMap<Id, String>,
    /// Derived-data cache keyed by `(kind, changeset id)`.
    derived: HashMap<(String, Id), Vec<u8>>,
    validated_v1: HashMap<Id, Option<Id>>,
    /// Sparse raw-commit capture (v0.2 D21): changeset id → verbatim
    /// git commit object bytes.
    raw_commits: HashMap<Id, Vec<u8>>,
    /// Sparse raw-tree capture (raw-tree fidelity): original git tree
    /// SHA → (verbatim git tree bytes, canonical model tree id — `None`
    /// for an empty normalized tree).
    raw_trees: HashMap<[u8; 20], (Vec<u8>, Option<Id>)>,
}

/// An entirely in-memory store implementing both [`ObjectStore`] and
/// [`MetadataStore`]. Single-threaded semantics under a lock, identical
/// verification and immutability behavior to the on-disk backends.
#[derive(Debug, Default)]
pub struct MemoryStore {
    inner: RwLock<MemoryInner>,
}

impl MemoryStore {
    /// A fresh, empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn read<T>(&self, f: impl FnOnce(&MemoryInner) -> T) -> Result<T, StoreError> {
        match self.inner.read() {
            Ok(guard) => Ok(f(&guard)),
            Err(_) => Err(StoreError::Db("memory store lock poisoned".into())),
        }
    }

    fn write<T>(
        &self,
        f: impl FnOnce(&mut MemoryInner) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match self.inner.write() {
            Ok(mut guard) => f(&mut guard),
            Err(_) => Err(StoreError::Db("memory store lock poisoned".into())),
        }
    }

    /// Test hook: seeds the blob map directly with `bytes` under `id`,
    /// bypassing content addressing. This is the in-memory analogue of
    /// hand-crafting an on-disk envelope at a shard path, and exists to
    /// exercise immutability/corruption handling. Not part of the storage
    /// contract.
    pub fn insert_raw(&self, id: Id, bytes: Vec<u8>) {
        self.write(|s| {
            s.blobs.insert(id, bytes);
            Ok(())
        })
        .ok(); // only fails on lock poisoning; nothing to propagate in a hook
    }

    /// Test hook: like [`MemoryStore::insert_raw`] but for the tree map.
    pub fn insert_tree_raw(&self, id: Id, bytes: Vec<u8>) {
        self.write(|s| {
            s.trees.insert(id, bytes);
            Ok(())
        })
        .ok();
    }
}

impl ObjectStore for MemoryStore {
    fn put_blob(&self, content: &[u8]) -> Result<Id, StoreError> {
        let id = gardenbed_model::blob_id(content);
        self.write(|s| match s.blobs.get(&id) {
            Some(existing) if existing == content => Ok(()),
            Some(_) => Err(StoreError::ImmutabilityViolation(id)),
            None => {
                s.blobs.insert(id, content.to_vec());
                Ok(())
            }
        })?;
        Ok(id)
    }

    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|s| s.blobs.get(&id).cloned())
    }

    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), StoreError> {
        // Same invariant as the fs backend: the bytes must be the canonical
        // payload for `id`.
        let actual = Id::from_bytes(Sha256::digest(canonical_bytes).into());
        if actual != id {
            return Err(StoreError::Invalid(format!(
                "tree canonical bytes hash to {actual}, not {id}"
            )));
        }
        self.write(|s| match s.trees.get(&id) {
            Some(existing) if existing == canonical_bytes => Ok(()),
            Some(_) => Err(StoreError::ImmutabilityViolation(id)),
            None => {
                s.trees.insert(id, canonical_bytes.to_vec());
                Ok(())
            }
        })
    }

    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|s| s.trees.get(&id).cloned())
    }

    fn has(&self, id: Id) -> Result<bool, StoreError> {
        self.read(|s| s.blobs.contains_key(&id) || s.trees.contains_key(&id))
    }
}

impl MetadataStore for MemoryStore {
    fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError> {
        let canonical = cs.canonical_bytes().map_err(|e| {
            StoreError::Invalid(format!("changeset is not canonically encodable: {e}"))
        })?;
        let id = Id::from_bytes(Sha256::digest(&canonical).into());
        self.write(|s| match s.changesets.get(&id) {
            Some((_, bytes)) if bytes == &canonical => Ok(()),
            Some(_) => Err(StoreError::ImmutabilityViolation(id)),
            None => {
                s.changesets.insert(id, (cs.clone(), canonical));
                s.changeset_arrival.push_back(id);
                Ok(())
            }
        })?;
        Ok(id)
    }

    fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError> {
        self.read(|s| s.changesets.get(&id).cloned())
    }

    fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError> {
        self.read(|s| s.refs.get(name).copied())
    }

    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
        self.read(|s| {
            s.refs
                .iter()
                .map(|(name, id)| (name.clone(), *id))
                .collect()
        })
    }

    fn cas_ref(&self, name: &str, expected: Option<Id>, new: Id) -> Result<RefCas, StoreError> {
        if name.is_empty() {
            return Err(StoreError::Invalid("ref name must not be empty".into()));
        }
        self.write(|s| match (expected, s.refs.get(name).copied()) {
            (None, Some(current)) => Ok(RefCas::Conflict { current }),
            (None, None) => {
                s.refs.insert(name.to_owned(), new);
                Ok(RefCas::Won { current: new })
            }
            (Some(expected), Some(current)) => {
                if current == expected {
                    s.refs.insert(name.to_owned(), new);
                    Ok(RefCas::Won { current: new })
                } else {
                    Ok(RefCas::Conflict { current })
                }
            }
            (Some(_), None) => Err(StoreError::NotFound),
        })
    }

    fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        self.write(
            |s| match (s.git_by_sha.get(&sha).copied(), s.git_by_cs.get(&cs)) {
                (Some(existing_cs), Some(existing_sha))
                    if existing_cs == cs && existing_sha.as_str() == sha =>
                {
                    Ok(()) // idempotent
                }
                (Some(existing_cs), _) => Err(StoreError::Invalid(format!(
                    "git sha {sha} already maps to changeset {}",
                    existing_cs.to_hex()
                ))),
                (_, Some(existing_sha)) => Err(StoreError::Invalid(format!(
                    "changeset {} already mapped from git sha {existing_sha}",
                    cs.to_hex()
                ))),
                (None, None) => {
                    s.git_by_sha.insert(sha.clone(), cs);
                    s.git_by_cs.insert(cs, sha);
                    Ok(())
                }
            },
        )
    }

    fn record_export_mappings(
        &self,
        pairs: &[(String, Id)],
    ) -> Result<crate::traits::ExportMappingReport, StoreError> {
        use crate::traits::{ExportMappingConflict, ExportMappingReport};
        // Normalize before the write lock: malformed shas are caller
        // bugs and must not land any pair.
        let mut normalized = Vec::with_capacity(pairs.len());
        for (sha, cs) in pairs {
            normalized.push((normalize_git_sha(sha)?, *cs));
        }
        // One write-lock critical section = one atomic batch (the RwLock
        // is this backend's transaction): classification completes before
        // any insert, so conflicts never block accepted pairs, and a crash
        // leaves the batch absent (pairs are idempotent; a re-run repairs).
        self.write(|s| {
            let mut report = ExportMappingReport::default();
            let mut insert: Vec<bool> = Vec::with_capacity(normalized.len());
            // In-batch view of already-accepted pairs: a later pair
            // repeating an accepted sha or cs (a twin: same git bytes,
            // different cs) classifies against it, never re-inserts.
            let mut seen_sha: std::collections::HashMap<String, Id> =
                std::collections::HashMap::new();
            let mut seen_cs: std::collections::HashMap<Id, String> =
                std::collections::HashMap::new();
            for (sha, cs) in &normalized {
                let by_sha = s
                    .git_by_sha
                    .get(sha)
                    .copied()
                    .or_else(|| seen_sha.get(sha).copied());
                let by_cs = s
                    .git_by_cs
                    .get(cs)
                    .cloned()
                    .or_else(|| seen_cs.get(cs).cloned());
                match (by_sha, by_cs) {
                    (Some(existing_cs), Some(existing_sha))
                        if existing_cs == *cs && existing_sha == *sha =>
                    {
                        report.unchanged += 1; // idempotent
                        insert.push(false);
                    }
                    (None, None) => {
                        // Fresh pair: neither side bound. The insert
                        // happens in the second pass, after the whole
                        // batch is classified.
                        insert.push(true);
                        seen_sha.insert(sha.clone(), *cs);
                        seen_cs.insert(*cs, sha.clone());
                    }
                    (existing_cs, existing_sha) => {
                        // Refused, never overwritten (twin semantics,
                        // mirroring record_git_mapping's bijection).
                        report.conflicts.push(ExportMappingConflict {
                            git_sha: sha.clone(),
                            cs: *cs,
                            existing_cs_for_sha: existing_cs,
                            existing_sha_for_cs: existing_sha,
                        });
                        insert.push(false);
                    }
                }
            }
            for (i, (sha, cs)) in normalized.iter().enumerate() {
                if !insert[i] {
                    continue;
                }
                s.git_by_sha.insert(sha.clone(), *cs);
                s.git_by_cs.insert(*cs, sha.clone());
                report.recorded += 1;
            }
            Ok(report)
        })
    }

    fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError> {
        let sha = normalize_git_sha(git_sha_hex)?;
        self.read(|s| s.git_by_sha.get(&sha).copied())
    }

    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
        self.read(|s| s.git_by_cs.get(&cs).cloned())
    }

    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
        if kind.is_empty() || kind.len() > 64 {
            return Err(StoreError::Invalid(
                "derived-data kind must be 1..=64 characters".into(),
            ));
        }
        self.write(|s| {
            s.derived.insert((kind.to_owned(), cs), bytes.to_vec());
            Ok(())
        })
    }

    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|s| s.derived.get(&(kind.to_owned(), cs)).cloned())
    }

    fn validation_tree(&self, cs: Id) -> Result<Option<Option<Id>>, StoreError> {
        self.read(|s| s.validated_v1.get(&cs).copied())
    }

    fn put_validated_changeset(
        &self,
        cs: &Changeset,
        root_tree: Option<Id>,
        derived: &[crate::traits::DerivedRow],
        git_sha_hex: Option<&str>,
        raw_commit: Option<&[u8]>,
        _verify: Option<&dyn Fn() -> Result<(), StoreError>>,
    ) -> Result<Id, StoreError> {
        let canonical = cs
            .canonical_bytes()
            .map_err(|e| StoreError::Invalid(e.to_string()))?;
        let id = cs.id().map_err(|e| StoreError::Invalid(e.to_string()))?;
        let sha = git_sha_hex.map(normalize_git_sha).transpose()?;
        self.write(|s| {
            for parent in &cs.parents {
                if !s.validated_v1.contains_key(parent) {
                    return Err(StoreError::Invalid(format!(
                        "parent {parent} is not certified"
                    )));
                }
            }
            if let Some((_, previous)) = s.changesets.get(&id)
                && *previous != canonical
            {
                return Err(StoreError::ImmutabilityViolation(id));
            }
            if let Some(previous) = s.validated_v1.get(&id)
                && *previous != root_tree
            {
                return Err(StoreError::ImmutabilityViolation(id));
            }
            if let Some(sha) = sha.as_ref()
                && (s
                    .git_by_sha
                    .get(sha)
                    .is_some_and(|existing| *existing != id)
                    || s.git_by_cs.get(&id).is_some_and(|existing| existing != sha))
            {
                return Err(StoreError::Invalid("Git mapping is not a bijection".into()));
            }
            if let Some(raw) = raw_commit
                && s.raw_commits
                    .get(&id)
                    .is_some_and(|existing| existing != raw)
            {
                return Err(StoreError::ImmutabilityViolation(id));
            }
            for row in derived {
                if row.kind.is_empty() || row.kind.len() > 64 {
                    return Err(StoreError::Invalid(
                        "derived-data kind must be 1..=64 characters".into(),
                    ));
                }
            }
            if !s.changesets.contains_key(&id) {
                s.changeset_arrival.push_back(id); // first arrival only
            }
            s.changesets.insert(id, (cs.clone(), canonical));
            s.validated_v1.insert(id, root_tree);
            if let Some(sha) = sha {
                s.git_by_sha.insert(sha.clone(), id);
                s.git_by_cs.insert(id, sha);
            }
            if let Some(raw) = raw_commit {
                s.raw_commits.insert(id, raw.to_vec());
            }
            for row in derived {
                s.derived
                    .insert((row.kind.clone(), row.cs), row.bytes.clone());
            }
            Ok(id)
        })
    }

    fn raw_commit(&self, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|s| s.raw_commits.get(&cs).cloned())
    }

    fn raw_tree(&self, git_tree_sha: [u8; 20]) -> Result<Option<Vec<u8>>, StoreError> {
        self.read(|s| s.raw_trees.get(&git_tree_sha).map(|(b, _)| b.clone()))
    }

    fn raw_tree_model_id(&self, git_tree_sha: [u8; 20]) -> Result<Option<Option<Id>>, StoreError> {
        self.read(|s| s.raw_trees.get(&git_tree_sha).map(|(_, m)| *m))
    }

    fn put_raw_tree(
        &self,
        git_tree_sha: [u8; 20],
        model_tree: Option<Id>,
        bytes: &[u8],
    ) -> Result<(), StoreError> {
        // Content-addressed by the key: the bytes must SHA-1 to it.
        let actual = crate::sqlite::git_tree_sha1(bytes);
        if actual != git_tree_sha {
            return Err(StoreError::Invalid(format!(
                "raw tree bytes SHA-1 to {}, not {}",
                crate::sqlite::hex_sha(&actual),
                crate::sqlite::hex_sha(&git_tree_sha)
            )));
        }
        self.write(|s| match s.raw_trees.get(&git_tree_sha) {
            // Identical bytes: idempotent no-op. The stored (first-seen)
            // model association stays untouched — one git tree can
            // normalize to different model trees across import contexts
            // (legacy-Exec frontier vs fresh-Regular; see the trait doc),
            // and the export resolves the model tree from changeset
            // context, never from this column.
            Some((existing, _)) if existing.as_slice() == bytes => Ok(()),
            Some(_) => Err(StoreError::Invalid(format!(
                "raw tree {} already captured with different bytes — \
                 content-addressed rows are immutable",
                crate::sqlite::hex_sha(&git_tree_sha)
            ))),
            None => {
                s.raw_trees
                    .insert(git_tree_sha, (bytes.to_vec(), model_tree));
                Ok(())
            }
        })
    }

    fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError> {
        self.read(|s| {
            s.changeset_arrival
                .iter()
                .rev()
                .take(limit as usize)
                .copied()
                .collect()
        })
    }

    fn changeset_ids_in_id_range(
        &self,
        lo: &[u8],
        hi_exclusive: Option<&[u8]>,
    ) -> Result<Vec<Id>, StoreError> {
        self.read(|s| {
            let mut ids: Vec<Id> = s
                .changesets
                .keys()
                .copied()
                .filter(|id| {
                    let bytes = id.as_bytes().as_slice();
                    bytes >= lo && hi_exclusive.is_none_or(|hi| bytes < hi)
                })
                .collect();
            ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            ids
        })
    }
}

/// Accepts a 40-character git SHA in either case; returns the lowercase
/// canonical form. Mirrors the sqlite backend's rule.
fn normalize_git_sha(git_sha_hex: &str) -> Result<String, StoreError> {
    if git_sha_hex.len() != 40 || !git_sha_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::Invalid(format!(
            "git sha must be 40 hex characters, got {git_sha_hex:?}"
        )));
    }
    Ok(git_sha_hex.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_store_is_empty() {
        let s = MemoryStore::new();
        let id = gardenbed_model::blob_id(b"nothing");
        assert_eq!(s.get_blob(id).unwrap(), None);
        assert!(!s.has(id).unwrap());
        assert_eq!(s.list_refs().unwrap(), vec![]);
    }
}

//! Storage contracts shared by every backend.
//!
//! Two traits partition the repository state (see `DESIGN.md`):
//!
//! - [`ObjectStore`] — **objects**, the content-addressed object store: blobs
//!   and trees, immutable once written.
//! - [`MetadataStore`] — the **metadata**: changesets, refs (compare-and-swap),
//!   the git-SHA mapping table, and the derived-data cache.

use crate::StoreError;
use gardenbed_model::Changeset;
use gardenbed_model::Id;

/// Content-addressed object store (objects). Immutable once written.
///
/// Blobs are addressed by `SHA-256(raw content)`; trees by `SHA-256(canonical
/// tree bytes)`. Writes are atomic and durable: a reader observes either the
/// full, verified object or nothing — a torn write is never observable.
pub trait ObjectStore: Send + Sync {
    /// Store raw blob content under `SHA-256(content)`. Returns the [`Id`].
    ///
    /// Storing the same Id again with identical bytes is an idempotent
    /// no-op; putting an existing Id with DIFFERENT bytes is an error
    /// (immutability).
    fn put_blob(&self, content: &[u8]) -> Result<Id, StoreError>;

    /// Raw, decompressed content for a blob id; `None` when absent. The
    /// on-disk envelope is transparently handled, and the content is
    /// verified (length + SHA-256) against the addressed id before it is
    /// returned.
    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError>;

    /// Store a tree as its canonical payload bytes (already including
    /// format/version bytes) under its tree Id.
    ///
    /// The bytes must hash to `id` (that is what a tree Id *is*); anything
    /// else is rejected with [`StoreError::Invalid`].
    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), StoreError>;

    /// Canonical tree payload bytes for a tree id; `None` when absent. The
    /// bytes are verified against the addressed id before being returned.
    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError>;

    /// Cheap existence check. Never reads object content.
    fn has(&self, id: Id) -> Result<bool, StoreError>;
}

/// Outcome of a compare-and-swap on a ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefCas {
    /// The swap happened. `current` is the ref's value *after* the swap
    /// (the newly installed id).
    Won {
        /// The ref's value after the swap.
        current: Id,
    },
    /// The swap did not happen. `current` is the value that blocked it.
    Conflict {
        /// The ref's current value (not the value the caller expected).
        current: Id,
    },
}

/// Changesets, refs, the git-SHA mapping, and derived-data caches — the
/// "metadata".
pub trait MetadataStore: Send + Sync {
    /// Encodes `cs` into its canonical bytes, stores them together with a
    /// decoded row, and returns the changeset's Id. Re-putting an identical
    /// changeset is an idempotent no-op.
    fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError>;

    /// The decoded changeset plus its exact canonical bytes, or `None`.
    fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError>;

    /// The id a ref currently points at, or `None` if the ref does not exist.
    fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError>;

    /// All refs as `(name, id)`, sorted by name.
    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError>;

    /// Compare-and-swap. `expected = None` means "the ref must not exist";
    /// `Some(id)` means "the ref must exist and point at `id`".
    ///
    /// Atomic: concurrent CAS calls on the same ref produce exactly one
    /// [`RefCas::Won`] per swap. A lost race returns [`RefCas::Conflict`]
    /// carrying the ref's current value; expecting `Some` on an absent ref
    /// returns [`StoreError::NotFound`].
    fn cas_ref(&self, name: &str, expected: Option<Id>, new: Id) -> Result<RefCas, StoreError>;

    /// Record that git commit `git_sha_hex` corresponds to changeset `cs`.
    ///
    /// The mapping is a bijection: neither the SHA nor the changeset may
    /// already be mapped to a different counterpart, otherwise
    /// [`StoreError::Invalid`]. Recording an already-recorded pair is an
    /// idempotent no-op. `git_sha_hex` is a 40-character git SHA; it is
    /// normalized to lowercase before storage.
    fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError>;

    /// The changeset id recorded for a git SHA, if any.
    fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError>;

    /// The git SHA recorded for a changeset id, if any.
    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError>;

    /// Ids of stored changesets whose 32-byte id sorts in the half-open
    /// byte range `[lo, hi_exclusive)` — the storage primitive behind
    /// truncated-id (hex prefix) resolution.
    ///
    /// Ids are compared as raw big-endian byte strings: SQLite orders BLOBs
    /// with `memcmp` (and the changeset id is the primary key, so both
    /// bounds seek one index range), and the memory backend orders byte
    /// slices identically. Callers derive the bounds from a user-typed hex
    /// prefix `p` of `k` nibbles: `lo = p ‖ 0x00…` and
    /// `hi_exclusive = p+1 ‖ 0x00…` (the prefix incremented in its last
    /// nibble, with carry; absent when every nibble is `0xF`, where the
    /// successor overflows and the range is open-ended). The minimum prefix
    /// is 8 hex chars (4 bytes), so even a million-changeset store matches
    /// a handful of rows per seek. Results are ascending by id.
    ///
    /// The default implementation refuses; backends that can seek by id
    /// override it (both in-tree backends do).
    fn changeset_ids_in_id_range(
        &self,
        lo: &[u8],
        hi_exclusive: Option<&[u8]>,
    ) -> Result<Vec<Id>, StoreError> {
        let _ = (lo, hi_exclusive);
        Err(StoreError::Db(
            "this metadata store cannot scan changeset ids by range".into(),
        ))
    }

    /// The most recently *stored* changeset ids, newest arrival first,
    /// capped at `limit` (`0` = none).
    ///
    /// This is arrival order — when the row first landed in the store —
    /// with **no graph semantics**: no ancestry filtering, no ref
    /// filtering, no date ordering. Unpublished changesets (rows no ref
    /// can reach, e.g. working copies committed straight to the server)
    /// are included; surfacing them is the point. A re-put idempotent
    /// changeset keeps its original arrival position.
    ///
    /// Required method: a store that cannot answer this must say so at
    /// compile time, not silently return an empty history.
    fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError>;

    /// Store bytes in the derived-data cache ("derived data"). `kind` is a short
    /// tag naming the derived-data type, e.g. `"skiplist"`. Overwrites any
    /// previous value for the same `(kind, cs)` key.
    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError>;

    /// Bytes previously stored under `(kind, cs)`, if any.
    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError>;

    /// A durable v1 validation certificate: `None` means uncertified;
    /// `Some(None)` certifies an empty tree. Certificates are separate from
    /// mutable derived data and attest to replay against certified parents,
    /// complete ancestry validity, and blob availability at validation time.
    fn validation_tree(&self, _cs: Id) -> Result<Option<Option<Id>>, StoreError> {
        Ok(None)
    }

    /// Trusted validator write: atomically store canonical changeset, immutable
    /// v1 certificate, optional Git mapping, and derived rows. All parents must
    /// already be certified. The caller MUST validate shape and blobs and replay
    /// against the first parent's certified tree without trusting derived caches
    /// (see Engine::validated_tree_for_new). Generic put/ingest/derived_put never
    /// grant certificates. Re-certifying with a different root is an error.
    /// Backends without certificate support must refuse this operation.
    fn put_validated_changeset(
        &self,
        _cs: &Changeset,
        _root_tree: Option<Id>,
        _derived: &[DerivedRow],
        _git_sha_hex: Option<&str>,
    ) -> Result<Id, StoreError> {
        Err(StoreError::Db(
            "validation certificates are unsupported by this metadata store".into(),
        ))
    }

    /// Atomically ingests one new changeset: the changeset row, its git-SHA
    /// mapping and any derived-data cache rows land in a single transaction,
    /// so a crash mid-ingest leaves the commit entirely absent (cleanly
    /// resumable) rather than partially recorded.
    ///
    /// `canonical` must be the changeset's canonical encoding (the id is
    /// recomputed from it). The same immutability and mapping-bijection
    /// rules as [`MetadataStore::put_changeset`] and
    /// [`MetadataStore::record_git_mapping`] apply. Derived rows are cache
    /// entries keyed by their own changeset id — normally the ingested
    /// changeset's id; rows for already-committed changesets (cache
    /// backfill) are equally valid here.
    ///
    /// The default implementation composes the individual operations (one
    /// transaction each); backends with real transactions override it.
    fn ingest_changeset(
        &self,
        canonical: &[u8],
        git_sha_hex: &str,
        derived: &[DerivedRow],
    ) -> Result<Id, StoreError> {
        use sha2::Digest;
        let id = Id::from_bytes(sha2::Sha256::digest(canonical).into());
        let cs = Changeset::decode(canonical)
            .map_err(|e| StoreError::Corrupt(format!("canonical bytes do not decode: {e}")))?;
        let stored = self.put_changeset(&cs)?;
        if stored != id {
            return Err(StoreError::Corrupt(format!(
                "changeset canonical bytes hash to {id}, stored as {stored}"
            )));
        }
        self.record_git_mapping(git_sha_hex, id)?;
        for row in derived {
            self.derived_put(&row.kind, row.cs, &row.bytes)?;
        }
        Ok(id)
    }
}

/// One derived-data cache row handed to [`MetadataStore::ingest_changeset`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedRow {
    /// Derived-data kind tag (e.g. `"tree"`).
    pub kind: String,
    /// The changeset the row belongs to.
    pub cs: Id,
    /// The encoded payload.
    pub bytes: Vec<u8>,
}

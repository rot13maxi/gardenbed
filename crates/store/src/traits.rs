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

    /// The verbatim bytes of the original git commit object for a
    /// changeset, if captured at import (v0.2 D21). Only commits whose
    /// bytes the model cannot reconstruct (signatures, unmodeled
    /// headers, non-UTF-8 identity bytes) carry a row; everything else
    /// is reconstructed. Rows land in the same ingest transaction as the
    /// changeset itself, so a raw row never names a missing changeset.
    /// Raw bytes are identity/export truth only — never parsed on the
    /// display path.
    fn raw_commit(&self, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        let _ = cs;
        Ok(None)
    }
    /// The verbatim bytes of the original git tree object keyed by its
    /// original git tree SHA (v0.2 raw-tree capture; the M18.5 successor
    /// to raw-commit capture). `None` when no row was captured (the tree
    /// reconstructs byte-exactly from the model, or the store predates
    /// raw-tree capture).
    ///
    /// The key is the **original git SHA-1** of the tree — never the
    /// canonical model tree id: multiple distinct git trees (differing
    /// pre-canonical modes, non-UTF-8 names) can normalize to one model
    /// tree, and keying by model id would silently conflate them. The
    /// model-tree association is carried by
    /// [`MetadataStore::raw_tree_model_id`].
    fn raw_tree(&self, _git_tree_sha: [u8; 20]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(None)
    }

    /// The canonical model tree id a captured raw tree (keyed by its
    /// original git tree SHA, see [`MetadataStore::raw_tree`]) normalizes
    /// to — the association the export uses to seed its render with
    /// original bytes. `None` when no row was captured, or when the
    /// normalized model tree is empty (an empty normalized tree has no
    /// model id; the export renders the canonical empty tree).
    fn raw_tree_model_id(&self, _git_tree_sha: [u8; 20]) -> Result<Option<Option<Id>>, StoreError> {
        Ok(None)
    }

    /// Records one verbatim git tree object, keyed by its **original git
    /// tree SHA** (see [`MetadataStore::raw_tree`] for why the git SHA is
    /// the key), together with the canonical model tree id the tree
    /// normalizes to (`None` for an empty normalized tree).
    ///
    /// Additive and content-addressed, like blob objects: rows may be
    /// written **before** the ordinary validated-changeset transaction
    /// that references them. A crash between the put and the ingest
    /// leaves an orphan row — harmless by construction, since the key is
    /// the git SHA of content the importer read from git, and a row is
    /// only ever consumed when a captured commit's `tree` header names
    /// its key.
    ///
    /// Immutability: re-putting the same git SHA with identical bytes and
    /// the same model association is an idempotent no-op; different bytes
    /// or a different association is a [`StoreError::Invalid`] naming the
    /// git SHA (the row is keyed by a 20-byte git tree SHA, not a model
    /// [`Id`](gardenbed_model::Id), so the violation is reported as
    /// `Invalid` rather than `ImmutabilityViolation`). The bytes must
    /// SHA-1 to `git_tree_sha` — anything else is also
    /// [`StoreError::Invalid`] (the row is content-addressed by its key).
    fn put_raw_tree(
        &self,
        _git_tree_sha: [u8; 20],
        _model_tree: Option<Id>,
        _bytes: &[u8],
    ) -> Result<(), StoreError> {
        Err(StoreError::Db(
            "raw-tree capture is unsupported by this metadata store".into(),
        ))
    }

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
    /// v1 certificate, optional Git mapping, raw commit bytes (v0.2 D21),
    /// and derived rows. All parents must already be certified. The caller
    /// MUST validate shape and blobs and replay against the first parent's
    /// certified tree without trusting derived caches (see
    /// Engine::validated_tree_for_new). Generic put/ingest/derived_put never
    /// grant certificates. Re-certifying with a different root is an error.
    /// Backends without certificate support must refuse this operation.
    ///
    /// `raw_commit` carries the verbatim git object bytes when the original
    /// commit is not reconstructable from the model (sparse capture);
    /// `None` stores no raw row (a previously captured row is kept —
    /// re-certification is idempotent, and dropping bytes on a re-put would
    /// silently regress fidelity).
    ///
    /// `verify` runs inside the store's write transaction, immediately
    /// before the rows land: it is the changeset's last-chance object
    /// re-check, and running it under the write lock makes it indivisible
    /// against a concurrent gc sweep (which holds the same lock for its
    /// generation-check-plus-unlink step). Production callers pass `Some`
    /// (the gc handshake); `None` skips the check and is reserved for
    /// test shims — a justified no-op (e.g. re-certifying already-live
    /// rows) passes an explicit `Some(&|| Ok(()))` so the decision is
    /// visible at the call site (round-2 review, P3).
    fn put_validated_changeset(
        &self,
        _cs: &Changeset,
        _root_tree: Option<Id>,
        _derived: &[DerivedRow],
        _git_sha_hex: Option<&str>,
        _raw_commit: Option<&[u8]>,
        _verify: Option<&dyn Fn() -> Result<(), StoreError>>,
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

//! Storage contracts shared by every backend.
//!
//! Two traits partition the repository state (see `DESIGN.md`):
//!
//! - [`ObjectStore`] — **loam**, the content-addressed object store: blobs
//!   and trees, immutable once written.
//! - [`MetadataStore`] — the **seedbed**: changesets, refs (compare-and-swap),
//!   the git-SHA mapping table, and the derived-data cache (compost).

use crate::StoreError;
use gardenbed_model::Changeset;
use gardenbed_model::Id;

/// Content-addressed object store (loam). Immutable once written.
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
/// "seedbed".
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

    /// Store bytes in the derived-data cache ("compost"). `kind` is a short
    /// tag naming the derived-data type, e.g. `"skiplist"`. Overwrites any
    /// previous value for the same `(kind, cs)` key.
    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError>;

    /// Bytes previously stored under `(kind, cs)`, if any.
    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError>;
}

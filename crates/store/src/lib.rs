//! Storage traits and backends: blob stores (fs, sqlite, s3) and metadata
//! stores (sqlite, postgres). Single-binary self-host mode is fs + sqlite.

use gardenbed_model::Id;

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    Io(String),
}

/// Content-addressed object storage. Immutable once written, cacheable
/// without coordination.
pub trait BlobStore {
    fn put(&mut self, id: Id, bytes: &[u8]) -> Result<(), StoreError>;
    fn get(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError>;
}

/// Changesets, refs (compare-and-swap), and the git SHA mapping tables.
pub trait MetadataStore {
    // TODO: put_changeset, get_changeset, cas_ref, resolve_ref, ...
}

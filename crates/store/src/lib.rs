//! gardenbed-store — storage backends for gardenbed.
//!
//! Two traits partition repository state (see `DESIGN.md`):
//!
//! - [`ObjectStore`] — **objects**, the content-addressed object store (blobs
//!   and trees). Immutable once written; every read is verified.
//!   Backends: [`FsObjectStore`] (filesystem), [`MemoryStore`].
//! - [`MetadataStore`] — the **metadata**: changesets, refs with
//!   compare-and-swap, the git-SHA mapping table, and the derived-data
//!   cache. Backends: [`SqliteMetadataStore`], [`MemoryStore`].
//!
//! The in-memory [`MemoryStore`] implements both traits and serves tests in
//! later milestones.

mod error;
mod fs;
mod memory;
mod sqlite;
mod traits;

pub use error::StoreError;
pub use fs::FsObjectStore;
pub use memory::MemoryStore;
pub use sqlite::SqliteMetadataStore;
pub use traits::DerivedRow;
pub use traits::MetadataStore;
pub use traits::ObjectStore;
pub use traits::RefCas;

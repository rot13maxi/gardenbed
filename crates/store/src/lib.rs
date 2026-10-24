//! gardenbed-store — storage backends for gardenbed.
//!
//! Two traits partition repository state (see `DESIGN.md`):
//!
//! - [`ObjectStore`] — **objects**, the content-addressed object store (blobs
//!   and trees). Immutable once written; every read is verified.
//!   Backends: [`FsObjectStore`] (filesystem), [`S3ObjectStore`] (S3 /
//!   MinIO, for scaled replicas), [`MemoryStore`].
//! - [`MetadataStore`] — **metadata**: changesets, refs with
//!   compare-and-swap, the git-SHA mapping table, and the derived-data
//!   cache. Backends: [`SqliteMetadataStore`], [`PostgresMetadataStore`]
//!   (for scaled replicas), [`MemoryStore`].
//!
//! The in-memory [`MemoryStore`] implements both traits and serves tests in
//! later milestones.
//!
//! Scale-out: [`StorageConfig`] parses one TOML file selecting a backend
//! per side and opens both (see `docs/scale-out-design.md`); the
//! Postgres and S3 backends bridge their async clients to the sync
//! traits through a private driver-thread runtime. All of it sits behind
//! the `scale` feature (on by default; `--no-default-features` builds a
//! pure-local store with only fs/sqlite/memory).

mod error;
mod fastcdc;
mod fs;
mod memory;
mod sqlite;
mod traits;

#[cfg(feature = "scale")]
mod config;
#[cfg(feature = "scale")]
mod driver;
#[cfg(feature = "scale")]
mod postgres;
#[cfg(feature = "scale")]
mod s3;

pub use error::StoreError;
pub use fs::FsObjectStore;
pub use memory::MemoryStore;
pub use sqlite::SqliteMetadataStore;
pub use traits::DerivedRow;
pub use traits::ExportMappingConflict;
pub use traits::ExportMappingReport;
pub use traits::MetadataStore;
pub use traits::ObjectStore;
pub use traits::RefCas;

#[cfg(feature = "scale")]
pub use config::MetadataConfig;
#[cfg(feature = "scale")]
pub use config::ObjectsConfig;
#[cfg(feature = "scale")]
pub use config::S3Config;
#[cfg(feature = "scale")]
pub use config::StorageConfig;
#[cfg(feature = "scale")]
pub use postgres::PostgresMetadataStore;
#[cfg(feature = "scale")]
pub use s3::S3ObjectStore;

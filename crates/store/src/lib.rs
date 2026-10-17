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
//! traits through a private driver-thread runtime.

mod config;
mod driver;
mod error;
mod fs;
mod memory;
mod postgres;
mod s3;
mod sqlite;
mod traits;

pub use config::MetadataConfig;
pub use config::ObjectsConfig;
pub use config::S3Config;
pub use config::StorageConfig;
pub use error::StoreError;
pub use fs::FsObjectStore;
pub use memory::MemoryStore;
pub use postgres::PostgresMetadataStore;
pub use s3::S3ObjectStore;
pub use sqlite::SqliteMetadataStore;
pub use traits::DerivedRow;
pub use traits::MetadataStore;
pub use traits::ObjectStore;
pub use traits::RefCas;

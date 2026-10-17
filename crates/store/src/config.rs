//! Storage configuration — one TOML file selecting the metadata and
//! object backends, shared by serve, import, and export (see
//! `docs/scale-out-design.md`).
//!
//! ```toml
//! [metadata]
//! backend = "postgres"            # or "sqlite"
//! url = "postgres://user:pass@host:5432/gardenbed"
//!
//! [objects]
//! backend = "s3"                  # or "fs"
//! endpoint = "http://minio:9000"
//! bucket = "gardenbed"
//! region = "us-east-1"
//! access_key = "..."
//! secret_key = "..."
//! path_style = true
//! ```
//!
//! The `sqlite`/`fs` branches keep the single-machine layout: the
//! metadata database at `<root>/metadata.sqlite3` and the object store
//! at `<root>/objects` — exactly what `--root` mode opens today, so a
//! storage config can point back at an existing local repository.

use crate::StoreError;
use crate::fs::FsObjectStore;
use crate::postgres::PostgresMetadataStore;
use crate::s3::S3ObjectStore;
use crate::sqlite::SqliteMetadataStore;
use crate::traits::MetadataStore;
use crate::traits::ObjectStore;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

/// Which metadata backend a [`StorageConfig`] opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataConfig {
    /// SQLite at `<root>/metadata.sqlite3` (the local default).
    Sqlite {
        /// Repository root directory.
        root: PathBuf,
    },
    /// PostgreSQL over the network (the scaled mode).
    Postgres {
        /// Connection URL, e.g.
        /// `postgres://user:pass@host:5432/gardenbed`.
        url: String,
    },
}

/// Which object backend a [`StorageConfig`] opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObjectsConfig {
    /// Filesystem at `<root>/objects` (the local default).
    Fs {
        /// Repository root directory.
        root: PathBuf,
    },
    /// S3-compatible object storage (the scaled mode).
    S3(S3Config),
}

/// Endpoint + credentials for one S3-compatible object store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Config {
    /// S3-compatible HTTPS/HTTP endpoint, e.g. `http://minio:9000`.
    pub endpoint: String,
    /// Bucket name.
    pub bucket: String,
    /// Region (MinIO conventionally accepts any, e.g. `us-east-1`).
    pub region: String,
    /// Static access key.
    pub access_key: String,
    /// Static secret key.
    pub secret_key: String,
    /// Path-style addressing (`http://host/bucket/key`); required by
    /// MinIO, optional on real S3.
    pub path_style: bool,
}

/// A parsed storage configuration: one metadata backend + one object
/// backend, opened atomically by [`StorageConfig::open`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StorageConfig {
    /// The metadata (`metadata`) backend.
    pub metadata: MetadataConfig,
    /// The objects (`objects`) backend.
    pub objects: ObjectsConfig,
}

/// One opened pair of repository stores: the object store plus the
/// metadata store, both behind their trait objects.
pub type Stores = (Arc<dyn ObjectStore>, Arc<dyn MetadataStore>);

impl StorageConfig {
    /// Parses a storage TOML file (schema in the module docs).
    pub fn from_path(p: &Path) -> Result<Self, StoreError> {
        let text = std::fs::read_to_string(p)
            .map_err(|e| StoreError::Io(format!("read {}: {e}", p.display())))?;
        Self::from_str(&text)
    }

    /// Parses storage TOML text.
    fn from_str(text: &str) -> Result<Self, StoreError> {
        let value: toml::Value = text
            .parse()
            .map_err(|e| StoreError::Invalid(format!("storage config is not valid TOML: {e}")))?;

        let metadata = value.get("metadata").ok_or_else(|| {
            StoreError::Invalid("storage config: [metadata] section is missing".into())
        })?;
        let metadata = match str_field(metadata, "backend", "metadata")? {
            "postgres" => MetadataConfig::Postgres {
                url: str_field(metadata, "url", "metadata")?.to_owned(),
            },
            "sqlite" => MetadataConfig::Sqlite {
                root: path_field(metadata, "root", "metadata")?,
            },
            other => {
                return Err(StoreError::Invalid(format!(
                    "metadata backend {other:?} is not one of \"sqlite\", \"postgres\""
                )));
            }
        };

        let objects = value.get("objects").ok_or_else(|| {
            StoreError::Invalid("storage config: [objects] section is missing".into())
        })?;
        let objects = match str_field(objects, "backend", "objects")? {
            "s3" => ObjectsConfig::S3(S3Config {
                endpoint: str_field(objects, "endpoint", "objects")?.to_owned(),
                bucket: str_field(objects, "bucket", "objects")?.to_owned(),
                region: str_field(objects, "region", "objects")?.to_owned(),
                access_key: str_field(objects, "access_key", "objects")?.to_owned(),
                secret_key: str_field(objects, "secret_key", "objects")?.to_owned(),
                path_style: match objects.get("path_style") {
                    None => false,
                    Some(toml::Value::Boolean(b)) => *b,
                    Some(_) => {
                        return Err(StoreError::Invalid(
                            "objects.path_style must be a boolean".into(),
                        ));
                    }
                },
            }),
            "fs" => ObjectsConfig::Fs {
                root: path_field(objects, "root", "objects")?,
            },
            other => {
                return Err(StoreError::Invalid(format!(
                    "objects backend {other:?} is not one of \"fs\", \"s3\""
                )));
            }
        };

        Ok(StorageConfig { metadata, objects })
    }

    /// Opens both backends. The object store opens first so a bad S3
    /// endpoint fails before any metadata schema is created.
    pub fn open(&self) -> Result<Stores, StoreError> {
        let objects: Arc<dyn ObjectStore> = match &self.objects {
            ObjectsConfig::Fs { root } => Arc::new(FsObjectStore::open(root)?),
            ObjectsConfig::S3(s3) => Arc::new(S3ObjectStore::open(
                &s3.endpoint,
                &s3.bucket,
                &s3.region,
                &s3.access_key,
                &s3.secret_key,
                s3.path_style,
            )?),
        };
        let metadata: Arc<dyn MetadataStore> = match &self.metadata {
            MetadataConfig::Sqlite { root } => {
                Arc::new(SqliteMetadataStore::open(&root.join("metadata.sqlite3"))?)
            }
            MetadataConfig::Postgres { url } => Arc::new(PostgresMetadataStore::open(url)?),
        };
        Ok((objects, metadata))
    }
}

/// Reads a required string field from a TOML table.
fn str_field<'a>(
    table: &'a toml::Value,
    field: &str,
    section: &str,
) -> Result<&'a str, StoreError> {
    match table.get(field) {
        Some(toml::Value::String(s)) => Ok(s),
        Some(_) => Err(StoreError::Invalid(format!(
            "{section}.{field} must be a string"
        ))),
        None => Err(StoreError::Invalid(format!("{section}.{field} is missing"))),
    }
}

/// Reads a required path field from a TOML table.
fn path_field(table: &toml::Value, field: &str, section: &str) -> Result<PathBuf, StoreError> {
    Ok(PathBuf::from(str_field(table, field, section)?))
}

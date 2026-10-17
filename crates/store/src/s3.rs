//! `objects` — the S3 (or MinIO) content-addressed object store, for
//! horizontally-scaled `gardenbed-server` replicas (see
//! `docs/scale-out-design.md`).
//!
//! Key layout is identical to [`crate::fs`]'s on-disk sharding —
//! `<first 2 hex>/<remaining 62 hex>` — and the payloads are
//! byte-identical to what `FsObjectStore` writes: blobs carry the §6
//! GBLO1 envelope (magic + `u64` LE raw length + one zstd frame), trees
//! are bare canonical bytes. A repository can therefore move between fs
//! and S3 wholesale.
//!
//! Immutability is enforced with conditional puts (`If-None-Match: *`,
//! via `object_store`'s `PutMode::Create`, supported by both S3 and
//! MinIO): when the precondition loses, the object is read back and
//! verified — same bytes is an idempotent no-op, different bytes is
//! [`StoreError::ImmutabilityViolation`]. Two replicas putting the same
//! id with the same bytes both succeed; different bytes never land.
//!
//! Every read is verified exactly like the fs backend: envelope
//! structure, decompressed length, and SHA-256 against the addressed
//! id — a torn or tampered object is never handed back as correct
//! bytes.
//!
//! Async bridging: the `object_store` client is async while the traits
//! are sync, so the store owns a dedicated driver thread running a
//! private single-threaded runtime (see [`crate::driver`]); every trait
//! method `block_on`s its handle.
//!
//! Durability: an acknowledged S3 put is durable by S3 semantics;
//! `GARDENBED_FSYNC` stays fs-only.

use crate::StoreError;
use crate::driver::Driver;
use crate::fs::decode_envelope;
use crate::fs::encode_envelope;
use gardenbed_model::Id;
use gardenbed_model::blob_id;
use object_store::ObjectStore as _;
use object_store::path::Path;
use object_store::PutMode;
use object_store::PutOptions;
use object_store::PutPayload;
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;

/// The S3 (objects) object store.
///
/// `endpoint` is an S3-compatible HTTPS/HTTP endpoint (AWS S3 or
/// MinIO); `path_style` selects path-style addressing
/// (`http://host/bucket/key`), which MinIO requires. Credentials are
/// static. Share via `Arc`.
#[derive(Debug)]
pub struct S3ObjectStore {
    inner: Arc<object_store::aws::AmazonS3>,
    driver: Driver,
}

impl S3ObjectStore {
    /// Connects to the bucket; fails fast on unreachable endpoints or
    /// missing buckets (one HEAD-equivalent round trip at open).
    pub fn open(
        endpoint: &str,
        bucket: &str,
        region: &str,
        access_key: &str,
        secret_key: &str,
        path_style: bool,
    ) -> Result<Self, StoreError> {
        let builder = object_store::aws::AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(bucket)
            .with_region(region)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_allow_http(true)
            .with_conditional_put(object_store::aws::S3ConditionalPut::ETagMatch);
        // `false` (the builder default) is path-style addressing.
        let builder = builder.with_virtual_hosted_style_request(!path_style);
        let inner = Arc::new(
            builder
                .build()
                .map_err(|e| StoreError::Invalid(format!("s3 client: {e}")))?,
        );
        let driver = Driver::new("gardenbed-s3-driver");
        // Fail fast: HEAD a probe key. A missing key is the healthy case
        // (NotFound); an unreachable endpoint or missing bucket surfaces
        // here, at open, not at the first put.
        let probe = Arc::clone(&inner);
        driver.block_on(async move {
            match probe.head(&Path::from("gardenbed-open-probe")).await {
                Ok(_) => {}
                Err(object_store::Error::NotFound { .. }) => {}
                Err(e) => {
                    return Err(StoreError::Db(format!("s3 bucket unreachable: {e}")))
                }
            }
            Ok::<(), StoreError>(())
        })?;
        let store = S3ObjectStore { inner, driver };
        Ok(store)
    }

    /// S3 key for an id: `<2 hex>/<62 hex>` — same sharding as the fs
    /// backend's directory layout, for operator familiarity.
    fn key_for(id: Id) -> Path {
        let hex = id.to_hex();
        Path::from(format!("{}/{}", &hex[..2], &hex[2..]))
    }

    /// Conditional put: writes `payload` under `key` only if absent.
    /// Returns `Ok(false)` when the key already existed (the caller then
    /// verifies what is there).
    fn put_if_absent(&self, key: &Path, payload: Vec<u8>) -> Result<bool, StoreError> {
        let inner = Arc::clone(&self.inner);
        let key = key.clone();
        self.driver.block_on(async move {
            let opts = PutOptions::from(PutMode::Create);
            let result = inner
                .put_opts(&key, PutPayload::from(payload), opts)
                .await;
            match result {
                Ok(_) => Ok(true),
                Err(object_store::Error::Precondition { .. })
                | Err(object_store::Error::AlreadyExists { .. }) => {
                    // If-None-Match lost: the object exists.
                    Ok(false)
                }
                Err(e) => Err(s3_err(e)),
            }
        })
    }

    /// Fetches the raw bytes at `key`; `None` when absent.
    fn get_raw(&self, key: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        let inner = Arc::clone(&self.inner);
        let key = key.clone();
        self.driver.block_on(async move {
            match inner.get(&key).await {
                Ok(get_result) => {
                    let bytes = get_result.bytes().await.map_err(s3_err)?;
                    Ok(Some(bytes.to_vec()))
                }
                Err(object_store::Error::NotFound { .. }) => Ok(None),
                Err(e) => Err(s3_err(e)),
            }
        })
    }

    /// Conditional put + verify-on-conflict — the shared immutability
    /// machinery of [`crate::traits::ObjectStore::put_blob`] and
    /// [`crate::traits::ObjectStore::put_tree`].
    ///
    /// `is_same` distinguishes "envelope of the same content" (no-op)
    /// from anything else (violation); for trees the raw bytes compare
    /// directly. A *structurally corrupt* blob envelope heals, exactly
    /// like the fs backend: the caller holds content that hashes to the
    /// id, so overwriting restores the invariant.
    fn conditional_put(
        &self,
        id: Id,
        payload: &[u8],
        same_content: impl Fn(&[u8]) -> bool,
        healable: bool,
    ) -> Result<(), StoreError> {
        let key = Self::key_for(id);
        match self.put_if_absent(&key, payload.to_vec())? {
            true => Ok(()),
            // Precondition lost: read back and verify.
            false => {
                let existing = self
                    .get_raw(&key)?
                    .ok_or_else(|| StoreError::Db("object vanished between put and read"
                        .into()))?;
                if same_content(&existing) {
                    return Ok(()); // idempotent no-op
                }
                if healable && crate::fs::parse_envelope(&existing).is_err() {
                    // Structurally corrupt envelope (external tampering):
                    // heal with the verified payload, unconditional put.
                    self.put_unconditional(&key, payload.to_vec())?;
                    return Ok(());
                }
                Err(StoreError::ImmutabilityViolation(id))
            }
        }
    }

    /// Unconditional put (heal path only — used with content that hashes
    /// to the addressed id, so overwriting is invariant-restoring).
    fn put_unconditional(&self, key: &Path, payload: Vec<u8>) -> Result<(), StoreError> {
        let inner = Arc::clone(&self.inner);
        let key = key.clone();
        self.driver.block_on(async move {
            inner
                .put_opts(&key, PutPayload::from(payload), PutOptions::default())
                .await
                .map_err(s3_err)?;
            Ok(())
        })
    }
}

impl crate::traits::ObjectStore for S3ObjectStore {
    fn put_blob(&self, content: &[u8]) -> Result<Id, StoreError> {
        let id = blob_id(content);
        let envelope = encode_envelope(content)?;
        // A *structurally decodable* envelope with different content is an
        // immutability violation — someone claims this id for other bytes.
        // A structurally corrupt envelope is healed, like fs.rs.
        self.conditional_put(
            id,
            &envelope,
            |existing| {
                crate::fs::parse_envelope(existing)
                    .is_ok_and(|decoded| decoded == content)
            },
            true,
        )?;
        Ok(id)
    }

    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        match self.get_raw(&Self::key_for(id))? {
            None => Ok(None),
            Some(raw) => Ok(Some(decode_envelope(&raw, id)?)),
        }
    }

    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), StoreError> {
        // A tree Id is defined as SHA-256(canonical tree bytes); enforce
        // the pairing so every stored object verifies under its address.
        let actual = Id::from_bytes(Sha256::digest(canonical_bytes).into());
        if actual != id {
            return Err(StoreError::Invalid(format!(
                "tree canonical bytes hash to {actual}, not {id}"
            )));
        }
        self.conditional_put(id, canonical_bytes, |existing| {
            existing == canonical_bytes
        }, false)
    }

    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        match self.get_raw(&Self::key_for(id))? {
            None => Ok(None),
            Some(bytes) => {
                let actual = Id::from_bytes(Sha256::digest(&bytes).into());
                if actual != id {
                    return Err(StoreError::Corrupt(format!(
                        "tree {id}: stored bytes hash to {actual}"
                    )));
                }
                Ok(Some(bytes))
            }
        }
    }

    fn has(&self, id: Id) -> Result<bool, StoreError> {
        let inner = Arc::clone(&self.inner);
        let key = Self::key_for(id);
        self.driver.block_on(async move {
            match inner.head(&key).await {
                Ok(_) => Ok(true),
                Err(object_store::Error::NotFound { .. }) => Ok(false),
                Err(e) => Err(s3_err(e)),
            }
        })
    }
}

fn s3_err(e: object_store::Error) -> StoreError {
    StoreError::Db(format!("s3 error: {e}"))
}

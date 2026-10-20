//! Counting proxies over the storage traits.
//!
//! The import wraps the caller's [`ObjectStore`] so that `objects_written`
//! in the [`ImportReport`](crate::ImportReport) counts **actual new loam
//! entries**: before every put the proxy asks `has()`; only a put that
//! created the object increments the counter. For the filesystem backend
//! this is exactly the delta of `<root>/objects` directory entries.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gardenbed_model::Id;
use gardenbed_store::{ObjectStore, StoreError};

/// Number of objects this proxy has observed being *created*.
#[derive(Debug)]
pub(crate) struct WriteCounter(AtomicU64);

impl WriteCounter {
    pub(crate) fn new() -> Self {
        WriteCounter(AtomicU64::new(0))
    }

    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn total(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Wraps an object store, counting creates (not idempotent re-puts).
pub(crate) struct CountingObjects {
    inner: Arc<dyn ObjectStore>,
    written: WriteCounter,
}

impl std::fmt::Debug for CountingObjects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingObjects")
            .field("written", &self.written.total())
            .finish_non_exhaustive()
    }
}

impl CountingObjects {
    pub(crate) fn new(inner: Arc<dyn ObjectStore>) -> Self {
        CountingObjects {
            inner,
            written: WriteCounter::new(),
        }
    }

    /// Objects created through this proxy so far.
    pub(crate) fn written(&self) -> u64 {
        self.written.total()
    }
}

impl ObjectStore for CountingObjects {
    fn put_blob(&self, content: &[u8]) -> Result<Id, StoreError> {
        let id = gardenbed_model::blob_id(content);
        let existed = self.inner.has(id)?;
        let out = self.inner.put_blob(content)?;
        if !existed {
            self.written.bump();
        }
        Ok(out)
    }

    fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_blob(id)
    }

    fn put_tree(&self, id: Id, canonical_bytes: &[u8]) -> Result<(), StoreError> {
        let existed = self.inner.has(id)?;
        self.inner.put_tree(id, canonical_bytes)?;
        if !existed {
            self.written.bump();
        }
        Ok(())
    }

    fn get_tree(&self, id: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_tree(id)
    }

    fn has(&self, id: Id) -> Result<bool, StoreError> {
        self.inner.has(id)
    }
}

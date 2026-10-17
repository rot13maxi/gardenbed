//! Error type shared by every storage backend.

use gardenbed_model::Id;

/// Errors surfaced by the storage layer (`objects` and `metadata`).
///
/// Library code never panics on bad input; every failure mode is one of
/// these variants.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A required row or object does not exist (e.g. a CAS that expected the
    /// ref to exist found it absent).
    #[error("not found")]
    NotFound,
    /// Stored bytes failed verification — bad envelope, wrong decompressed
    /// length, or SHA-256 mismatch against the addressed id. A store never
    /// hands back content it could not verify.
    #[error("corrupt object: {0}")]
    Corrupt(String),
    /// A compare-and-swap lost: the ref currently points at `current`.
    #[error("ref conflict: currently points at {current}")]
    Conflict {
        /// The ref's current value.
        current: Id,
    },
    /// An object with this Id already exists holding different bytes.
    /// Content-addressed storage is immutable once written.
    #[error("immutability violation: {0} already exists with different content")]
    ImmutabilityViolation(Id),
    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(String),
    /// Metadata database failure.
    #[error("database error: {0}")]
    Db(String),
    /// Caller-supplied input is invalid (bad ref name, bytes that do not
    /// hash to the claimed Id, malformed git SHA, ...).
    #[error("invalid input: {0}")]
    Invalid(String),
}

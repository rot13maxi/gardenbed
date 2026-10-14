//! Errors raised by rootstock imports.

use gardenbed_model::codec::CodecError;
use gardenbed_store::StoreError;

/// Everything that can go wrong while importing a git repository.
///
/// Per the milestone contract there are no panics on bad input: corrupt or
/// malformed git data surfaces as one of these errors with context naming
/// the offending object.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The git repository could not be opened (bad path, not a repo, …).
    #[error("git repository: {0}")]
    Git(String),
    /// A git object could not be read or is of an unexpected kind.
    #[error("git object {oid}: {detail}")]
    Object {
        /// Lowercase git SHA (or raw-ish display form) of the object.
        oid: String,
        /// What went wrong.
        detail: String,
    },
    /// A commit could not be parsed (malformed identity line, bad header, …).
    #[error("commit {sha}: {detail}")]
    Commit {
        /// Lowercase git SHA of the offending commit.
        sha: String,
        /// What could not be parsed.
        detail: String,
    },
    /// `git diff-tree` (rename detection, D8) failed for a commit.
    #[error("diff-tree for commit {sha}: {detail}")]
    Diff {
        /// Lowercase git SHA of the offending commit.
        sha: String,
        /// The failure (git stderr, parse error, …).
        detail: String,
    },
    /// A ref could not be moved (compare-and-swap lost to a concurrent
    /// writer) or a ref tip has no changeset mapping.
    #[error("ref {name}: {detail}")]
    Ref {
        /// The ref name.
        name: String,
        /// The failure.
        detail: String,
    },
    /// An underlying store failure (loam or seedbed).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A built changeset is not canonically encodable — an internal bug or
    /// an unrepresentable git construct, never silently ignored.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// A repository shape rootstock v0.1 deliberately does not support
    /// (documented limitation).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// Anything else (I/O, graph corruption, defensive checks).
    #[error("{0}")]
    Other(String),
}

//! Errors raised by git_bridge imports and exports.

use gardenbed_model::Id;
use gardenbed_model::codec::CodecError;
use gardenbed_store::StoreError;

/// I/O failures inside the export pipeline (stream writing, marks file
/// handling) surface as git errors: the stream's only consumer is git.
impl From<std::io::Error> for ExportError {
    fn from(e: std::io::Error) -> Self {
        ExportError::Git(format!("io: {e}"))
    }
}

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
    /// An underlying store failure (objects or metadata).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A built changeset is not canonically encodable — an internal bug or
    /// an unrepresentable git construct, never silently ignored.
    #[error(transparent)]
    Codec(#[from] CodecError),
    /// A repository shape git_bridge v0.1 deliberately does not support
    /// (documented limitation).
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// Anything else (I/O, graph corruption, defensive checks).
    #[error("{0}")]
    Other(String),
}

/// Everything that can go wrong while exporting changesets to git.
///
/// Like the import errors: no panics on bad input, and every failure
/// names the offending object.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// A git subprocess failed (repository init, `fast-import`,
    /// `rev-parse`, …) or the stream could not be written.
    #[error("git: {0}")]
    Git(String),
    /// A changeset is missing from the metadata (dangling ref or parent),
    /// is malformed, or cannot be rendered as a commit.
    #[error("changeset {id}: {detail}")]
    Changeset {
        /// Lowercase hex changeset id.
        id: String,
        /// What went wrong.
        detail: String,
    },
    /// A blob referenced by an exported changeset is absent from the objects.
    #[error("blob {id} is missing from the object store")]
    MissingBlob {
        /// The blob id.
        id: Id,
    },
    /// An identity cannot be rendered as a git ident line (it carries
    /// `<`, `>` or a newline, which fast-import could not re-create; the
    /// bed refuses such identities at birth, so this is defensive).
    #[error("changeset {cs}: the {field} cannot be exported: {detail}")]
    Identity {
        /// Hex changeset id carrying the identity.
        cs: String,
        /// Which identity field (`author name`, `committer email`, …).
        field: String,
        /// What is wrong with it.
        detail: String,
    },
    /// A ref name is not a valid git refname (or its tip is unmapped).
    #[error("ref {name}: {detail}")]
    Ref {
        /// The ref name.
        name: String,
        /// The failure.
        detail: String,
    },
    /// An underlying store failure (objects or metadata).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Anything else (I/O, defensive checks).
    #[error("{0}")]
    Other(String),
}

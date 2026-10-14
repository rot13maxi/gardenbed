//! Errors bed reports to its human. Every message is written to be
//! actionable in the garden vocabulary of the docs: say what failed, name
//! the file involved, and point at the command that fixes it.

use std::path::PathBuf;

use gardenbed_store::StoreError;

/// Everything `bed` can fail with in its local (offline) half.
#[derive(Debug, thiserror::Error)]
pub enum BedError {
    /// The directory does not look like a bed: no `config.toml`.
    #[error(
        "not a bed: {0} has no config.toml — plant one with `bed clone <server>`"
    )]
    NotABed(PathBuf),

    /// The config file exists but could not be read.
    #[error("cannot read the bed config at {path}: {source}")]
    ConfigRead {
        /// Config file path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config file is not valid TOML for the bed schema.
    #[error("cannot parse the bed config at {path}:\n  {message}")]
    ConfigParse {
        /// Config file path.
        path: PathBuf,
        /// What the parser choked on.
        message: String,
    },

    /// The config parsed but its values are unusable.
    #[error("invalid bed config: {0}")]
    ConfigInvalid(String),

    /// The config file could not be written.
    #[error("cannot write the bed config at {path}: {message}")]
    ConfigWrite {
        /// Config file path.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// A plot prefix does not name a place inside the garden.
    #[error("invalid plot prefix {raw:?}: {reason}")]
    PlotPrefix {
        /// The prefix as typed.
        raw: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A `<cs-or-ref>` argument is neither a changeset id nor a usable ref.
    #[error("invalid changeset id or ref {raw:?}: {reason}")]
    Revspec {
        /// The argument as typed.
        raw: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A path argument violates the repo's flat-path rules (spec §3.1).
    #[error("invalid path {raw:?}: {reason}")]
    Path {
        /// The path as typed.
        raw: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A line in `.bed/heads` is not a changeset id.
    #[error(".bed/heads line {line}: {reason}")]
    HeadsParse {
        /// 1-based line number of the offending line.
        line: usize,
        /// Why it was rejected.
        reason: String,
    },

    /// The treestate database could not be opened or updated.
    #[error("treestate at {path} is unusable: {message}")]
    Treestate {
        /// Database path.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// An I/O error outside the config and treestate files.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// An error surfaced by the local loam cache (`FsObjectStore`).
    #[error(transparent)]
    Store(#[from] StoreError),
}

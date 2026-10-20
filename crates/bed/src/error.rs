//! Errors bed reports to its human. Every message is written to be
//! actionable in the source-control terminology of the docs: say what failed, name
//! the file involved, and point at the command that fixes it.

use std::path::PathBuf;

use gardenbed_model::Id;
use gardenbed_store::StoreError;

/// Everything `bed` can fail with in its local (offline) half.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// A checkout would overwrite local work or cross an unsafe path.
    #[error("cannot materialize {path:?}: {reason}; preserve or commit local work before retrying")]
    CheckoutBlocked {
        /// Repository-relative path.
        path: String,
        /// Why materialization is unsafe.
        reason: String,
    },

    /// The directory does not look like a working copy: no `config.toml`.
    #[error("not a working copy: {0} has no config.toml — create one with `bed clone <server>`")]
    NotAWorkingCopy(PathBuf),

    /// The given bed directory looks like a working-copy root: a `.bed` lives
    /// inside it. `--state-dir` names the `.bed` directory itself.
    #[error(
        "no working copy at {0} — but {0}/.bed exists: --state-dir names the .bed \
         directory itself, so pass --state-dir {0}/.bed"
    )]
    StateDirIsWorkspace(PathBuf),

    /// The config file exists but could not be read.
    #[error("cannot read the working copy config at {path}: {source}")]
    ConfigRead {
        /// Config file path.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The config file is not valid TOML for the working copy schema.
    #[error("cannot parse the working copy config at {path}:\n  {message}")]
    ConfigParse {
        /// Config file path.
        path: PathBuf,
        /// What the parser choked on.
        message: String,
    },

    /// The config parsed but its values are unusable.
    #[error("invalid working copy config: {0}")]
    ConfigInvalid(String),

    /// The config file could not be written.
    #[error("cannot write the working copy config at {path}: {message}")]
    ConfigWrite {
        /// Config file path.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// A sparse prefix does not name a place inside the repository.
    #[error("invalid sparse prefix {raw:?}: {reason}")]
    SparsePrefix {
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
    #[error("working copy index at {path} is unusable: {message}")]
    Treestate {
        /// Database path.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },

    /// An I/O error outside the config and treestate files.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// An error surfaced by the local objects cache (`FsObjectStore`).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// `bed mount` cannot run on this host: no usable FUSE.
    #[error(
        "this host cannot mount a virtualized checkout — /dev/fuse or fusermount3 \
         is missing; install fuse3, or read files with `bed file` instead"
    )]
    MountUnavailable,

    /// The virtualized checkout could not be unmounted cleanly.
    #[error("unmounting the virtualized checkout failed: {source}")]
    MountUnmount {
        /// The underlying I/O error.
        source: std::io::Error,
    },

    // ------------------------------------------------- server (M4 read path)
    /// The server URL is malformed in a way the command refuses to guess
    /// around (e.g. a multi-segment http path, which cannot name a
    /// repository).
    #[error("bad server URL {server:?}: {reason}")]
    BadServerUrl {
        /// The URL as given.
        server: String,
        /// Why it is refused.
        reason: String,
    },

    /// The server could not be reached. Validation and connection both
    /// happen before any local state is written, so local state is unchanged.
    #[error(
        "cannot reach the server at {server}: {message}\n  \
         is it running? start the server with `gardenbed-server --addr <ip:port> --root <dir>`"
    )]
    Connect {
        /// The endpoint bed tried.
        server: String,
        /// Why the connection failed.
        message: String,
    },

    /// An RPC reached the server and was refused. The command is named so
    /// the human knows which step of the operation failed.
    #[error("bed {command}: the server refused ({code:?}): {message}")]
    Rpc {
        /// The command that issued the RPC (`file`, `switch`, …).
        command: &'static str,
        /// gRPC status code, e.g. `NotFound`.
        code: tonic::Code,
        /// The server's explanation.
        message: String,
    },

    /// A merge was refused for semantic conflicts: the server lists every
    /// conflicting path; resolution belongs in the working copy.
    #[error(
        "merge conflicts on: {paths}\n  \
         next step: edit the conflicting files, commit, and merge again"
    )]
    MergeConflict {
        /// The conflicting paths, comma-separated server message style.
        paths: String,
    },

    /// A merge was refused locally: the working copy has uncommitted
    /// work that the post-merge checkout would put at risk. Checked
    /// before the RPC so the server stores nothing on refusal.
    #[error(
        "merge refuses a dirty working copy — commit or preserve local work \
         first:\n{detail}"
    )]
    MergeDirtyWorkingCopy {
        /// The `bed status` rendering of what is dirty.
        detail: String,
    },
    /// A gitlink blob id that does not follow the canonical padding
    /// convention (upstream git SHA-1 left-padded with twelve zero bytes)
    /// was asked to render as a `Subproject commit <sha>` hunk. There is
    /// no honest pointer to name — refuse rather than render a SHA git
    /// would apply as a wrong target.
    #[error(
        "gitlink blob id {0} is not a zero-padded submodule SHA — cannot render a Subproject commit hunk"
    )]
    InvalidGitlinkId(Id),

    /// The command needs a current changeset and the working copy has none.
    #[error(
        "this working copy has no current changeset — create one with `bed clone` or \
         move with `bed switch`"
    )]
    NoCurrentCs,

    /// The bed is already initialized at this directory.
    #[error(
        "there is already a working copy at {0} — use it or choose \
         another --state-dir"
    )]
    AlreadyAWorkingCopy(PathBuf),

    /// The named ref does not exist on the server.
    #[error("the server has no ref {name:?}")]
    MissingRef {
        /// The ref name as requested.
        name: String,
    },

    /// `clone` without `--ref` resolved none of the standard starting
    /// refs. Names what was probed and — when the server's ref listing
    /// answered — what actually exists, so the fix is one flag away.
    #[error("no ref {primary} (tried {tried}); pass --ref{available}")]
    NoStartingRef {
        /// The default ref that did not resolve (`main`).
        primary: String,
        /// Every ref name the fallback probed, comma-separated in order.
        tried: String,
        /// Pre-rendered suffix: empty, or a newline naming the refs the
        /// server does have (its full stored names, exactly what
        /// `--ref` accepts).
        available: String,
    },

    /// The changeset id is not on the server (never created there, or the
    /// server is a different garden).
    #[error("changeset {0} is not on the server")]
    MissingCs(Id),

    /// The file the command wanted does not exist (or is a directory) at
    /// that changeset.
    #[error("there is no file at {path:?} in {cs}")]
    FileNotAFile {
        /// The path as requested.
        path: String,
        /// The changeset it was asked for (short form).
        cs: String,
    },

    /// `bed file` was pointed at a submodule pointer, which has no content
    /// of its own.
    #[error(
        "{path:?} is a submodule pointer in {cs} — `bed file` prints only \
             file and symlink content"
    )]
    FileIsGitlink {
        /// The path as requested.
        path: String,
        /// The changeset it was asked for (short form).
        cs: String,
    },

    /// The server streamed blob bytes that do not hash to the address
    /// they were asked for. A cache or tier is lying; refuse the bytes.
    #[error("the server sent blob {0} with content that does not match its address")]
    BlobHashMismatch(Id),

    /// The server streamed a malformed item (e.g. a blame range whose
    /// changeset address is not 32 bytes). Refuse the answer, not the working copy.
    #[error("the server sent a malformed response: {0}")]
    MalformedRpc(String),

    // ------------------------------------------------- write path (M5)
    /// An `--author` (or `GARDENBED_AUTHOR`) value is not a usable
    /// `Name <email>` identity, or carries characters git's ident format
    /// cannot carry (which would make the changeset unexportable).
    #[error(
        "bad author {raw:?}: {reason}\n  \
         commit with --author \"Name <email>\""
    )]
    Author {
        /// The value as given.
        raw: String,
        /// Why it was rejected.
        reason: String,
    },

    /// `commit` was called with an empty (or whitespace-only) message.
    #[error("the message is empty — commit with -m <message>")]
    EmptyMessage,

    /// The scan found nothing to commit: working copy is clean.
    #[error("working copy is clean — nothing modified, deleted, or untracked to commit")]
    NothingToCommit,

    /// A working file changed between the scan and the read that commits it.
    /// Harmless and rare; committing again simply re-scans.
    #[error("{0:?} keeps changing while committing — rerun `bed commit`")]
    FileChanged(String),

    /// `publish` was refused because the target ref is no longer where this
    /// working copy's changeset grew from — a fast-forward CAS lost to a
    /// moved ref.
    #[error(
        "cannot publish {cs}: it does not fast-forward `{ref}`{tip}\n  \
         rebase this changeset onto the new {ref} (`bed rebase {ref}`) and publish again"
    )]
    PublishNotFastForward {
        /// The changeset that was being published (short form).
        cs: String,
        /// The ref that was being published to.
        r#ref: String,
        /// The ref's current tip (short form), when the server said.
        tip: String,
    },

    /// A mutating ref/head command was refused because commits are still
    /// queued offline — syncing first keeps the server's view coherent.
    #[error(
        "cannot {command} with {pending} pending commit(s) not yet on the server — \
         run `bed sync` first"
    )]
    PendingCommits {
        /// The refused command's name.
        command: &'static str,
        /// How many commits wait for `bed sync`.
        pending: usize,
    },

    /// The requested changeset is queued offline: the object is local, but
    /// its parents may only exist on the server. Sync uploads it first.
    #[error(
        "changeset {cs} is pending — {pending} commit(s) not yet on the server; \
         run `bed sync` to upload it, or log a synced changeset"
    )]
    PendingCs {
        /// The pending changeset (short form).
        cs: String,
        /// How many commits wait for `bed sync`.
        pending: usize,
    },
}

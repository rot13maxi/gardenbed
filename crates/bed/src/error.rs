//! Errors bed reports to its human. Every message is written to be
//! actionable in the garden vocabulary of the docs: say what failed, name
//! the file involved, and point at the command that fixes it.

use std::path::PathBuf;

use gardenbed_model::Id;
use gardenbed_store::StoreError;

/// Everything `bed` can fail with in its local (offline) half.
#[derive(Debug, thiserror::Error)]
pub enum BedError {
    /// The directory does not look like a bed: no `config.toml`.
    #[error("not a bed: {0} has no config.toml — plant one with `bed clone <server>`")]
    NotABed(PathBuf),

    /// The given bed directory looks like a garden root: a `.bed` lives
    /// inside it. `--bed-dir` names the `.bed` directory itself.
    #[error(
        "no bed at {0} — but {0}/.bed exists: --bed-dir names the .bed \
         directory itself, so pass --bed-dir {0}/.bed"
    )]
    BedDirIsGardenRoot(PathBuf),

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

    // ------------------------------------------------- arboretum (M4 read path)
    /// The arboretum could not be reached. Validation and connection both
    /// happen before any local state is written, so nothing is half-planted.
    #[error(
        "cannot reach the arboretum at {server}: {message}\n  \
         is it running? plant the arboretum with `gardenbed-server --addr <ip:port> --root <dir>`"
    )]
    Connect {
        /// The endpoint bed tried.
        server: String,
        /// Why the connection failed.
        message: String,
    },

    /// An RPC reached the arboretum and was refused. The command is named so
    /// the human knows which step of the tending failed.
    #[error("bed {command}: the arboretum refused ({code:?}): {message}")]
    Rpc {
        /// The command that issued the RPC (`file`, `goto`, …).
        command: &'static str,
        /// gRPC status code, e.g. `NotFound`.
        code: tonic::Code,
        /// The server's explanation.
        message: String,
    },

    /// The command needs a current changeset and the bed has none.
    #[error(
        "this bed has no current changeset — plant one with `bed clone` or \
         move with `bed goto`"
    )]
    NoCurrentCs,

    /// The bed is already planted at this directory.
    #[error(
        "there is already a bed at {0} — tend it where it grows, or choose \
         another --bed-dir"
    )]
    AlreadyABed(PathBuf),

    /// The named ref does not exist on the arboretum.
    #[error("the arboretum has no ref {name:?}")]
    MissingRef {
        /// The ref name as requested.
        name: String,
    },

    /// `clone` without `--ref` resolved none of the standard starting
    /// refs. Names what was probed and — when the arboretum's ref listing
    /// answered — what actually exists, so the fix is one flag away.
    #[error("no ref {primary} (tried {tried}); pass --ref{available}")]
    NoStartingRef {
        /// The default ref that did not resolve (`main`).
        primary: String,
        /// Every ref name the fallback probed, comma-separated in order.
        tried: String,
        /// Pre-rendered suffix: empty, or a newline naming the refs the
        /// arboretum does have (its full stored names, exactly what
        /// `--ref` accepts).
        available: String,
    },

    /// The changeset id is not on the arboretum (never sown there, or the
    /// arboretum is a different garden).
    #[error("changeset {0} is not on the arboretum")]
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

    /// The arboretum streamed blob bytes that do not hash to the address
    /// they were asked for. A cache or tier is lying; refuse the bytes.
    #[error("the arboretum sent blob {0} with content that does not match its address")]
    BlobHashMismatch(Id),

    /// The arboretum streamed a malformed item (e.g. a blame range whose
    /// changeset address is not 32 bytes). Refuse the answer, not the bed.
    #[error("the arboretum sent a malformed response: {0}")]
    MalformedRpc(String),

    // ------------------------------------------------- write path (M5)
    /// An `--author` (or `GARDENBED_AUTHOR`) value is not a usable
    /// `Name <email>` identity, or carries characters git's ident format
    /// cannot carry (which would make the changeset unexportable).
    #[error(
        "bad author {raw:?}: {reason}\n  \
         sow with --author \"Name <email>\""
    )]
    Author {
        /// The value as given.
        raw: String,
        /// Why it was rejected.
        reason: String,
    },

    /// `sow` was called with an empty (or whitespace-only) message.
    #[error("the message is empty — sow with -m <message>")]
    EmptyMessage,

    /// The scan found nothing to sow: the plot is clean.
    #[error("the plot is clean — nothing modified, deleted, or untracked to sow")]
    NothingToSow,

    /// A working file changed between the scan and the read that sows it.
    /// Harmless and rare; sowing again simply re-scans.
    #[error("{0:?} keeps changing while sowing — rerun `bed sow`")]
    FileChanged(String),

    /// `harvest` was refused because `main` is no longer where this bed's
    /// changeset grew from — a fast-forward CAS lost to a moved ref.
    #[error(
        "`main` moved while harvesting — {cs} is not its tip anymore{tip}\n  \
         graft this changeset onto the new main (`bed graft main`) and harvest again"
    )]
    HarvestNotFastForward {
        /// The changeset that was being harvested (short form).
        cs: String,
        /// `main`'s current tip (short form), when the arboretum said.
        tip: String,
    },
}

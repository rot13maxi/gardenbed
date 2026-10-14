//! gardenbed-server — the arboretum: the authoritative gardenbed server
//! (lib part; the binary is `src/main.rs`).
//!
//! [`Repo`] wires one repository's stores (loam + seedbed) and the compost
//! [`Engine`] into the Task-B service layer: changeset creation with blob
//! validation and eager derivation, object access, the ancestry queries,
//! refs with compare-and-swap, server-side graft (linearized rebase), and
//! the harvest fast-forward. [`GardenbedService`] adapts [`Repo`] to the
//! generated tonic `Gardenbed` trait (v0.1.md §5).
//!
//! Error philosophy: a conforming client never sees an error for a *result*
//! (ref CAS conflicts come back in `CasResp`, empty answers as absent
//! optional fields); errors are reserved for genuinely absent objects
//! (`not_found`), bad input (`invalid_argument`), and semantic precondition
//! violations (`failed_precondition`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gardenbed_derive::{BlameRange, Engine};
use gardenbed_model::codec::validate_changeset;
use gardenbed_model::{Change, Changeset, Id, Mode, blob_id};
use gardenbed_store::{
    FsObjectStore, MetadataStore, ObjectStore, RefCas, SqliteMetadataStore, StoreError,
};

/// Largest blob accepted inline on `CreateChangeset` (1 MiB); larger bodies
/// must be uploaded with `PutBlob` first (v0.1.md §5).
pub const MAX_INLINE_BLOB_LEN: usize = 1 << 20;

/// Errors from [`Repo`] and the gRPC layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// An underlying store failure (missing rows, corruption, I/O, …).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A model value could not be canonically encoded/decoded.
    #[error(transparent)]
    Codec(#[from] gardenbed_model::codec::CodecError),
    /// Caller-supplied input is invalid (bad ids, missing blobs, …).
    #[error("invalid request: {0}")]
    Invalid(String),
    /// A referenced changeset does not exist.
    #[error("changeset {0} not found")]
    ChangesetNotFound(Id),
    /// A referenced object (blob or tree) does not exist.
    #[error("object {0} not found")]
    ObjectNotFound(Id),
    /// A referenced ref does not exist.
    #[error("ref {0:?} not found")]
    RefNotFound(String),
    /// Graft hit semantic conflicts; the paths are listed in the message.
    #[error("graft conflicts on: {}", .0.join(", "))]
    GraftConflict(Vec<String>),
    /// Harvest refused: ref "main" is not an ancestor of the target (or the
    /// CAS was lost to a concurrent harvest after the check passed).
    #[error("harvest refused: \"main\" is not an ancestor of the given changeset")]
    NotFastForward,
}

impl ServerError {
    /// Maps a store error raised while looking up changeset `cs` onto the
    /// request-shaped error (`NotFound` becomes [`ServerError::
    /// ChangesetNotFound`]).
    fn for_changeset(e: StoreError, cs: Id) -> Self {
        match e {
            StoreError::NotFound => ServerError::ChangesetNotFound(cs),
            other => ServerError::Store(other),
        }
    }
}

/// Per-repo request counters (debug/test infrastructure). Bumped by the gRPC
/// handlers for the four counted RPCs only — never by internal [`Repo`]
/// calls, so a test can prove an RPC really crossed the wire.
#[derive(Debug, Default)]
struct Stats {
    get_tree: AtomicU64,
    get_blob: AtomicU64,
    put_blob: AtomicU64,
    create_changeset: AtomicU64,
}

impl Stats {
    /// A sorted snapshot of the counters.
    fn snapshot(&self) -> BTreeMap<String, u64> {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        BTreeMap::from([
            ("create_changeset".to_owned(), get(&self.create_changeset)),
            ("get_blob".to_owned(), get(&self.get_blob)),
            ("get_tree".to_owned(), get(&self.get_tree)),
            ("put_blob".to_owned(), get(&self.put_blob)),
        ])
    }
}

/// The repository service: stores + compost engine + counters.
///
/// Cheap to share: every method takes `&self`, both stores live behind
/// `Arc`, and all counters are atomics. One `Repo` per process in v0.1
/// (single-repo servers).
pub struct Repo {
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    engine: Engine,
    stats: Stats,
}

impl std::fmt::Debug for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repo").finish_non_exhaustive()
    }
}

impl Repo {
    /// Opens (creating if needed) the repository rooted at `root`:
    /// `<root>/loam/` holds the content-addressed object store and
    /// `<root>/seedbed.sqlite3` the metadata database. Idempotent.
    pub fn open(root: &Path) -> Result<Self, ServerError> {
        let objects = Arc::new(FsObjectStore::open(&root.join("loam"))?);
        let metadata = Arc::new(SqliteMetadataStore::open(&root.join("seedbed.sqlite3"))?);
        Ok(Self::new(objects, metadata))
    }

    /// A repo over explicit stores (tests, embedded tiers).
    pub fn new(objects: Arc<dyn ObjectStore>, metadata: Arc<dyn MetadataStore>) -> Self {
        let engine = Engine::new(objects.clone(), metadata.clone());
        Repo {
            objects,
            metadata,
            engine,
            stats: Stats::default(),
        }
    }

    // -------------------------------------------------------- write path

    /// Sows a changeset: validates parents and blob references, stores any
    /// inline blobs, stores the changeset (canonical bytes are written by
    /// the seedbed), then eagerly materializes derived data (tree + skip
    /// list, D3). Returns the changeset's id.
    ///
    /// Blob resolution per change: a blob already in loam passes as-is;
    /// otherwise it must ride in `inline_blobs` (keyed by its 64-hex id),
    /// weigh ≤ [`MAX_INLINE_BLOB_LEN`], and hash to its id. Gitlink changes
    /// are exempt: their blob field is an opaque upstream pointer (the
    /// zero-padded git SHA-1), not a content address, and is never stored
    /// in loam.
    pub fn create_changeset(
        &self,
        cs: &Changeset,
        inline_blobs: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Id, ServerError> {
        // Semantic validation incl. canonical sortedness — the codec is the
        // single authority on changeset shape.
        validate_changeset(cs, true)?;
        for p in &cs.parents {
            if self.metadata.get_changeset(*p)?.is_none() {
                return Err(ServerError::ChangesetNotFound(*p));
            }
        }

        // Resolve blob references: loam hit, inline hit (verified, ≤ 1 MiB),
        // or gitlink pointer.
        let mut resolved: HashMap<Id, bool> = HashMap::new(); // id → stored now
        for c in &cs.changes {
            let Some(blob) = c.blob else { continue }; // DELETE carries none
            if c.mode == Mode::Gitlink {
                continue; // opaque pointer, never a loam object
            }
            if resolved.contains_key(&blob) {
                continue; // already resolved (or rejected) once
            }
            if self.objects.has(blob)? {
                resolved.insert(blob, false);
                continue;
            }
            let key = blob.to_hex();
            let Some(content) = inline_blobs.get(&key) else {
                return Err(ServerError::Invalid(format!(
                    "blob {key} is referenced but neither in loam nor inline"
                )));
            };
            if content.len() > MAX_INLINE_BLOB_LEN {
                return Err(ServerError::Invalid(format!(
                    "inline blob {key} is {} bytes; blobs over {} bytes must be \
                     uploaded via PutBlob first",
                    content.len(),
                    MAX_INLINE_BLOB_LEN
                )));
            }
            if blob_id(content) != blob {
                return Err(ServerError::Invalid(format!(
                    "inline blob content does not hash to its claimed id {key}"
                )));
            }
            self.objects.put_blob(content)?;
            resolved.insert(blob, true);
        }

        // The canonical encoding (and therefore the id) is written by the
        // seedbed; re-putting an identical changeset is idempotent.
        let cs_id = self.metadata.put_changeset(cs)?;

        // Eager materialization (D3). Stored first, derived second: a
        // failure here leaves the changeset present with recomputable
        // derived data, never a dangling reference.
        self.engine.on_changeset_created(cs_id)?;
        Ok(cs_id)
    }

    // --------------------------------------------------------- read path

    /// One changeset, decoded; [`ServerError::ChangesetNotFound`] when absent.
    pub fn get_changeset(&self, cs_id: Id) -> Result<Changeset, ServerError> {
        self.metadata
            .get_changeset(cs_id)?
            .map(|(cs, _)| cs)
            .ok_or(ServerError::ChangesetNotFound(cs_id))
    }

    /// The changeset's derived root tree id; `None` for an empty root (no
    /// files anywhere in its history).
    pub fn root_tree(&self, cs_id: Id) -> Result<Option<Id>, ServerError> {
        self.engine
            .root_tree(cs_id)
            .map_err(|e| ServerError::for_changeset(e, cs_id))
    }

    /// A stored tree by id; `None` when absent.
    pub fn get_tree(&self, id: Id) -> Result<Option<gardenbed_model::Tree>, ServerError> {
        Ok(self.engine.tree(id)?)
    }

    /// A blob's raw content; `None` when absent (gitlink pointers are never
    /// loam objects, so they read as absent here too).
    pub fn get_blob(&self, id: Id) -> Result<Option<Vec<u8>>, ServerError> {
        Ok(self.objects.get_blob(id)?)
    }

    /// Stores one blob under `SHA-256(content)`; returns the id.
    pub fn put_blob(&self, content: &[u8]) -> Result<Id, ServerError> {
        Ok(self.objects.put_blob(content)?)
    }

    /// The best common ancestor of `a` and `b` (`Some(a)` when `a` is an
    /// ancestor of `b`, and vice versa); `None` when they share none.
    pub fn merge_base(&self, a: Id, b: Id) -> Result<Option<Id>, ServerError> {
        self.engine.merge_base(a, b).map_err(|e| {
            // Either side may be the unknown one; name a on NotFound.
            ServerError::for_changeset(e, a)
        })
    }

    /// Is `a` an ancestor of `b`? (Every changeset is an ancestor of itself.)
    pub fn is_ancestor(&self, a: Id, b: Id) -> Result<bool, ServerError> {
        self.engine
            .is_ancestor(a, b)
            .map_err(|e| ServerError::for_changeset(e, a))
    }

    /// The tree diff between the trees of `a` and `b` (a → b), ops
    /// Add/Modify/Delete, rename detection off, sorted by path bytes.
    pub fn diff(&self, a: Id, b: Id) -> Result<Vec<Change>, ServerError> {
        self.engine
            .diff(a, b)
            .map_err(|e| ServerError::for_changeset(e, a))
    }

    /// The log of all ancestors of `cs_id` (inclusive), newest first, as
    /// `(changeset id, changeset)` pairs.
    ///
    /// Ordering is git's date-ordered walk: a max-heap keyed by
    /// `(committer time descending, insertion sequence ascending)` — among
    /// equal committer times the earliest-inserted entry pops first (git's
    /// `commit_list_insert_by_date` is FIFO on ties). Parents are inserted
    /// when their child pops, so a changeset is never emitted before at
    /// least one child that names it; that is the walk's topological
    /// guarantee (a strict topological sort of a DAG is not what `git log`
    /// computes either). `limit` = 0 means unlimited.
    pub fn log(&self, cs_id: Id, limit: u32) -> Result<Vec<(Id, Changeset)>, ServerError> {
        use std::cmp::Reverse;

        // Pop order: newest committer time first; on equal times, the
        // earliest-inserted id first. The id rides as raw bytes (`Id`
        // deliberately has no `Ord`).
        type HeapEntry = (u64, Reverse<u64>, [u8; 32]);
        let mut heap: std::collections::BinaryHeap<HeapEntry> = std::collections::BinaryHeap::new();
        let mut seen: std::collections::HashSet<Id> = std::collections::HashSet::new();
        let mut seq: u64 = 0;

        let head = self.get_changeset(cs_id)?;
        seen.insert(cs_id);
        heap.push((head.committer.time_secs, Reverse(seq), *cs_id.as_bytes()));
        seq += 1;

        let cap = if limit == 0 {
            usize::MAX
        } else {
            limit as usize
        };
        let mut out: Vec<(Id, Changeset)> = Vec::with_capacity(cap.min(64));
        while let Some((_, _, id_bytes)) = heap.pop() {
            let id = Id::from_bytes(id_bytes);
            // Insert the parents on pop (git inserts parents when a commit
            // is taken off its date-sorted list), each with a fresh seq.
            let cs = self.get_changeset(id)?;
            for p in &cs.parents {
                if seen.insert(*p) {
                    let parent = self.get_changeset(*p)?;
                    heap.push((parent.committer.time_secs, Reverse(seq), *p.as_bytes()));
                    seq += 1;
                }
            }
            out.push((id, cs));
            if out.len() >= cap {
                break;
            }
        }
        Ok(out)
    }

    /// First-parent lineage blame of `path` at changeset `cs_id` (v0.1.md
    /// D7): coalesced `(lo, hi, changeset)` ranges, 1-based inclusive,
    /// ascending, with rename following. Delegates to the compost
    /// [`Engine`] — the very implementation the M6 differential suite
    /// anchors to `git blame --first-parent`, so the gRPC surface and
    /// `bed blame` are semantically identical by construction.
    ///
    /// Errors with [`ServerError::ChangesetNotFound`] when the changeset
    /// (or the path at it) does not exist — the engine reports both as
    /// `NotFound`, and v0.1 carries a single `not_found` shape on the wire
    /// (clients that care about the distinction pre-check the path via
    /// `RootTree`/`GetTree`, as `bed blame` does). Gitlink paths and empty
    /// files answer with zero ranges (documented divergences, engine docs).
    pub fn blame(&self, cs_id: Id, path: &str) -> Result<Vec<BlameRange>, ServerError> {
        self.engine
            .blame(cs_id, path)
            .map_err(|e| ServerError::for_changeset(e, cs_id))
    }

    // -------------------------------------------------------------- refs

    /// All refs as `(name, id)`, sorted by name.
    pub fn list_refs(&self) -> Result<Vec<(String, Id)>, ServerError> {
        Ok(self.metadata.list_refs()?)
    }

    /// A ref's current value; `None` when it does not exist.
    ///
    /// Histories imported from git carry their full ref names
    /// (`refs/heads/main`), while v0.1 clients (bed, MCP, harvest) speak
    /// bare branch names (`main`). A bare name resolves against its
    /// `refs/heads/` spelling first, falling back to the name as given —
    /// so an imported arboretum and a natively grown one answer the same
    /// question identically.
    pub fn get_ref(&self, name: &str) -> Result<Option<Id>, ServerError> {
        let canonical = branch_ref_name(name);
        if let found @ Some(_) = self.metadata.resolve_ref(&canonical)? {
            return Ok(found);
        }
        if canonical == name {
            return Ok(None);
        }
        Ok(self.metadata.resolve_ref(name)?)
    }

    /// Compare-and-swap a ref. Conflicts are results ([`RefCas::Conflict`]),
    /// not errors; expecting `Some` on an absent ref is
    /// [`StoreError::NotFound`].
    ///
    /// Like [`Repo::get_ref`], bare branch names are canonicalized: the CAS
    /// targets the existing `refs/heads/<name>` row when one is there (an
    /// imported ref moves; a second ref is not forked), falls back to a row
    /// stored under the name as given, and otherwise creates the ref under
    /// its canonical `refs/heads/` spelling so exports stay valid git.
    pub fn cas_ref(
        &self,
        name: &str,
        expected: Option<Id>,
        new: Id,
    ) -> Result<RefCas, ServerError> {
        let canonical = branch_ref_name(name);
        let target = if self.metadata.resolve_ref(&canonical)?.is_some() {
            canonical
        } else if canonical != name && self.metadata.resolve_ref(name)?.is_some() {
            name.to_owned()
        } else {
            canonical
        };
        Ok(self.metadata.cas_ref(&target, expected, new)?)
    }

    // ------------------------------------------------- graft and harvest

    /// Server-side graft (linearized rebase): replays `cs_id`'s changes onto
    /// `new_parent`'s tree with **hard** conflict detection, then stores a
    /// new changeset `parents = [new_parent]` carrying the same changes,
    /// message, identities and signature, and eagerly materializes it.
    ///
    /// Conflict semantics (documented, deliberate):
    /// * a write (Add/Modify/rename destination) onto a path that exists
    ///   with the *same* blob and mode is a no-op; with a *different* blob
    ///   or mode it is a conflict;
    /// * a write whose path sits under an existing file path is a conflict;
    ///   a write that swallows existing paths under it replaces them (the
    ///   delta-apply's file↔directory semantics);
    /// * a Delete or rename source that does not exist is a conflict;
    /// * all conflicting paths are collected and reported together as one
    ///   [`ServerError::GraftConflict`].
    ///
    /// The replayed changes must apply cleanly: the stored changeset is
    /// derived by the engine exactly like any other (lenient apply), so the
    /// conflict checker only admits changes whose lenient application gives
    /// the same tree.
    pub fn graft(&self, cs_id: Id, new_parent: Id) -> Result<Id, ServerError> {
        let cs = self.get_changeset(cs_id)?;
        // The new parent must exist and its tree must be derivable (a
        // missing parent is `ChangesetNotFound`, mapped by `root_tree`).
        let base = self.root_tree(new_parent)?;

        // The flat working set of the new parent: path → (mode, blob).
        let mut files: BTreeMap<String, (Mode, Id)> = self
            .engine
            .flatten_tree(base)?
            .into_iter()
            .map(|(path, mode, blob)| (path, (mode, blob)))
            .collect();

        let mut conflicts: Vec<String> = Vec::new();
        for c in &cs.changes {
            match c.op {
                gardenbed_model::Op::Add | gardenbed_model::Op::Modify => {
                    check_write(&mut files, &c.path, c.blob, c.mode, &mut conflicts);
                }
                gardenbed_model::Op::Delete => {
                    if files.remove(&c.path).is_none() {
                        conflicts.push(format!("delete of missing path {:?}", c.path));
                    }
                }
                gardenbed_model::Op::Rename => {
                    let Some(src) = c.src_path.as_deref() else {
                        conflicts.push(format!("rename {:?} without a src_path", c.path));
                        continue;
                    };
                    if files.remove(src).is_none() {
                        conflicts.push(format!("rename of missing path {src:?}"));
                        continue;
                    }
                    check_write(&mut files, &c.path, c.blob, c.mode, &mut conflicts);
                }
            }
        }
        if !conflicts.is_empty() {
            return Err(ServerError::GraftConflict(conflicts));
        }

        // The replayed changeset: same changes and metadata, one parent.
        let grafted = Changeset {
            parents: vec![new_parent],
            changes: cs.changes.clone(),
            message: cs.message.clone(),
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            signature: cs.signature.clone(),
        };
        // validate_changeset(…, true) re-checks shape/sortedness; blob ids
        // are already loam objects (they shipped with the original), so the
        // inline map is empty.
        self.create_changeset(&grafted, &BTreeMap::new())
    }

    /// Fast-forwards ref `main` to `cs_id` (v0.1.md D5): asserts that the
    /// current `main` (when one exists) is an ancestor of `cs_id`, then
    /// compare-and-swaps `main` to it. Returns `cs_id`.
    ///
    /// With no `main` yet, the ancestry precondition holds trivially and the
    /// CAS creates the ref from nothing. A CAS lost to a concurrent harvest
    /// (between the check and the swap) surfaces as
    /// [`ServerError::NotFastForward`]; the caller retries against the new
    /// value.
    pub fn harvest(&self, cs_id: Id) -> Result<Id, ServerError> {
        // The harvested changeset must exist (a clear 404 beats a CAS that
        // would silently create a ref to nowhere).
        self.get_changeset(cs_id)?;
        let current = self.get_ref("main")?;
        if let Some(cur) = current {
            let ok = self
                .engine
                .is_ancestor(cur, cs_id)
                .map_err(|e| ServerError::for_changeset(e, cur))?;
            if !ok {
                return Err(ServerError::NotFastForward);
            }
        }
        match self.cas_ref("main", current, cs_id)? {
            RefCas::Won { .. } => Ok(cs_id),
            RefCas::Conflict { .. } => Err(ServerError::NotFastForward),
        }
    }

    /// A snapshot of the request counters, sorted by name.
    pub fn stats(&self) -> BTreeMap<String, u64> {
        self.stats.snapshot()
    }

    /// The stats counters (bumped by the gRPC handlers only).
    pub(crate) fn counter(&self, name: &str) -> Option<&AtomicU64> {
        match name {
            "get_tree" => Some(&self.stats.get_tree),
            "get_blob" => Some(&self.stats.get_blob),
            "put_blob" => Some(&self.stats.put_blob),
            "create_changeset" => Some(&self.stats.create_changeset),
            _ => None,
        }
    }
}

/// The canonical seedbed name for a client-facing ref: bare branch names
/// (`main`, `feature`) live under `refs/heads/` — the spelling git itself
/// requires and the one an import already stores. Full names pass through
/// untouched.
fn branch_ref_name(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_owned()
    } else {
        format!("refs/heads/{name}")
    }
}

/// One write (Add/Modify/rename destination) in the graft replay: `files` is
/// the flat working set, mutated only when the write is conflict-free.
fn check_write(
    files: &mut BTreeMap<String, (Mode, Id)>,
    path: &str,
    blob: Option<Id>,
    mode: Mode,
    conflicts: &mut Vec<String>,
) {
    let Some(blob) = blob else {
        conflicts.push(format!("write to {:?} without a blob", path));
        return;
    };
    match files.get(path) {
        // Same content and mode: the write is a no-op.
        Some(existing) if *existing == (mode, blob) => {}
        // Different content or mode: a hard conflict.
        Some(_) => conflicts.push(format!("{path:?} already exists with different content")),
        None => {
            // A path under an existing file cannot be created.
            let under_file = files.keys().any(|q| {
                path.len() > q.len()
                    && path.starts_with(q.as_str())
                    && path.as_bytes().get(q.len()) == Some(&b'/')
            });
            if under_file {
                conflicts.push(format!("{path:?} sits under an existing file"));
                return;
            }
            // The write replaces anything beneath it (file↔directory swap,
            // matching the delta-apply's replacement semantics).
            let swallowed: Vec<String> = files
                .keys()
                .filter(|q| q.starts_with(path) && q.as_bytes().get(path.len()) == Some(&b'/'))
                .cloned()
                .collect();
            for q in swallowed {
                files.remove(&q);
            }
            files.insert(path.to_owned(), (mode, blob));
        }
    }
}

// --------------------------------------------------------------- gRPC layer

use gardenbed_protocol::convert::{blob_key_to_id, id_from_bytes, id_to_bytes};
use gardenbed_protocol::gardenbed_server::Gardenbed;
use gardenbed_protocol::pb;
use std::pin::Pin;
use tonic::codegen::tokio_stream::{Stream, iter};
use tonic::{Request, Response, Status, Streaming};

/// Chunk size for `GetBlob` streams (~64 KiB).
const BLOB_CHUNK_LEN: usize = 1 << 16;

/// A boxed, Send stream of wire items — the shape of every generated
/// server-streaming response.
type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

impl From<ServerError> for Status {
    fn from(e: ServerError) -> Self {
        match &e {
            ServerError::Store(StoreError::NotFound) => Status::not_found(e.to_string()),
            ServerError::Store(StoreError::Invalid(_)) => Status::invalid_argument(e.to_string()),
            ServerError::Store(StoreError::Corrupt(_)) => Status::data_loss(e.to_string()),
            ServerError::Store(StoreError::ImmutabilityViolation(_)) => {
                Status::already_exists(e.to_string())
            }
            ServerError::Store(_) => Status::internal(e.to_string()),
            ServerError::Codec(_) => Status::invalid_argument(e.to_string()),
            ServerError::Invalid(_) => Status::invalid_argument(e.to_string()),
            ServerError::ChangesetNotFound(_)
            | ServerError::ObjectNotFound(_)
            | ServerError::RefNotFound(_) => Status::not_found(e.to_string()),
            ServerError::GraftConflict(_) | ServerError::NotFastForward => {
                Status::failed_precondition(e.to_string())
            }
        }
    }
}

/// The tonic adapter over one [`Repo`]: implements the generated
/// `Gardenbed` trait (all 17 RPCs of v0.1.md §5).
pub struct GardenbedService {
    repo: Arc<Repo>,
}

impl GardenbedService {
    /// A service over `repo` (shared behind an `Arc`).
    pub fn new(repo: Arc<Repo>) -> Self {
        GardenbedService { repo }
    }

    /// The request's `repo` field: validated non-empty and logged. Every
    /// request carries it (v0.1.md §5); v0.1 serves a single repo, so the
    /// name is accepted as-is once non-empty.
    fn repo_field<'a>(&self, repo: &'a str, rpc: &'static str) -> Result<&'a str, Status> {
        if repo.is_empty() {
            return Err(Status::invalid_argument(
                "request carries no repo name (the `repo` field is mandatory)",
            ));
        }
        log::debug!("request rpc={rpc} repo={repo}");
        Ok(repo)
    }

    /// Parses a 32-byte content address off the wire.
    fn id(bytes: &[u8], field: &'static str) -> Result<Id, Status> {
        id_from_bytes(bytes).map_err(|_| {
            Status::invalid_argument(format!(
                "{field} is not a 32-byte content address (got {} bytes)",
                bytes.len()
            ))
        })
    }

    /// Bumps one of the counted RPC counters (no-op for uncounted names).
    fn bump(&self, name: &str) {
        if let Some(c) = self.repo.counter(name) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[tonic::async_trait]
impl Gardenbed for GardenbedService {
    /// Server-streaming response type for `GetBlob`.
    type GetBlobStream = BoxStream<pb::BlobChunk>;
    /// Server-streaming response type for `Diff`.
    type DiffStream = BoxStream<pb::ChangeMsg>;
    /// Server-streaming response type for `Log`.
    type LogStream = BoxStream<pb::ChangesetSummary>;
    /// Server-streaming response type for `Blame` (first-parent lineage
    /// blame ranges, v0.1.md D7).
    type BlameStream = BoxStream<pb::BlameRange>;
    /// Server-streaming response type for `ListRefs`.
    type ListRefsStream = BoxStream<pb::RefEntry>;

    async fn root_tree(
        &self,
        request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::RootTreeResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "RootTree")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let tree = self.repo.root_tree(cs_id)?;
        Ok(Response::new(pb::RootTreeResp {
            tree_id: tree.map(id_to_bytes),
        }))
    }

    async fn get_tree(
        &self,
        request: Request<pb::GetTreeReq>,
    ) -> Result<Response<pb::TreeResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "GetTree")?;
        self.bump("get_tree");
        let id = Self::id(&req.id, "id")?;
        let Some(tree) = self.repo.get_tree(id)? else {
            return Err(Status::not_found(format!("tree {id} not found")));
        };
        let resp = pb::TreeResp::try_from(&tree)
            .map_err(|e| Status::internal(format!("stored tree {id} has no wire form: {e}")))?;
        Ok(Response::new(resp))
    }

    async fn get_blob(
        &self,
        request: Request<pb::GetBlobReq>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "GetBlob")?;
        self.bump("get_blob");
        let id = Self::id(&req.id, "id")?;
        let Some(content) = self.repo.get_blob(id)? else {
            return Err(Status::not_found(format!("blob {id} not found")));
        };
        // Own the chunks up front: the stream must be 'static and the blob
        // is already fully in memory anyway.
        let chunks: Vec<Vec<u8>> = content.chunks(BLOB_CHUNK_LEN).map(<[u8]>::to_vec).collect();
        let items = chunks.into_iter().map(|data| Ok(pb::BlobChunk { data }));
        Ok(Response::new(Box::pin(iter(items))))
    }

    async fn get_changeset(
        &self,
        request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::ChangesetMsg>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "GetChangeset")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let cs = self.repo.get_changeset(cs_id)?;
        let msg = pb::ChangesetMsg::try_from(&cs).map_err(|e| {
            Status::internal(format!("stored changeset {cs_id} has no wire form: {e}"))
        })?;
        Ok(Response::new(msg))
    }

    async fn put_blob(
        &self,
        request: Request<Streaming<pb::PutBlobChunk>>,
    ) -> Result<Response<pb::PutBlobResp>, Status> {
        let mut stream = request.into_inner();
        // The first frame MUST be the header naming the repo (client
        // streaming has no unary request message).
        let first = stream.message().await?.ok_or_else(|| {
            Status::invalid_argument("PutBlob stream ended before a header frame")
        })?;
        let repo_name = match first.frame {
            Some(pb::put_blob_chunk::Frame::Header(h)) => h.repo,
            Some(pb::put_blob_chunk::Frame::Data(_)) => {
                return Err(Status::invalid_argument(
                    "the first PutBlob frame must be the header, not a body chunk",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "the first PutBlob frame carries no header",
                ));
            }
        };
        self.repo_field(&repo_name, "PutBlob")?;

        let mut body = Vec::new();
        while let Some(chunk) = stream.message().await? {
            match chunk.frame {
                Some(pb::put_blob_chunk::Frame::Data(d)) => body.extend_from_slice(&d),
                Some(pb::put_blob_chunk::Frame::Header(_)) => {
                    return Err(Status::invalid_argument(
                        "duplicate header frame in a PutBlob stream",
                    ));
                }
                None => {
                    return Err(Status::invalid_argument(
                        "PutBlob frame carries neither header nor data",
                    ));
                }
            }
        }
        let id = self.repo.put_blob(&body)?;
        self.bump("put_blob");
        Ok(Response::new(pb::PutBlobResp {
            id: id_to_bytes(id),
        }))
    }

    async fn create_changeset(
        &self,
        request: Request<pb::CreateChangesetReq>,
    ) -> Result<Response<pb::CreateChangesetResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "CreateChangeset")?;
        let msg = req
            .cs
            .ok_or_else(|| Status::invalid_argument("CreateChangesetReq carries no changeset"))?;
        let cs = Changeset::try_from(msg)
            .map_err(|e| Status::invalid_argument(format!("changeset is not valid: {e}")))?;
        // Every inline key must be a 64-hex blob id; content size and hash
        // verification live in `Repo::create_changeset`.
        for key in req.inline_blobs.keys() {
            blob_key_to_id(key).map_err(|_| {
                Status::invalid_argument(format!(
                    "inline_blobs key {key:?} is not a 64-hex blob id"
                ))
            })?;
        }
        let inline: BTreeMap<String, Vec<u8>> = req
            .inline_blobs
            .into_iter()
            .map(|(key, content)| (key, content.to_vec()))
            .collect();
        let id = self.repo.create_changeset(&cs, &inline)?;
        self.bump("create_changeset");
        Ok(Response::new(pb::CreateChangesetResp {
            id: id_to_bytes(id),
        }))
    }

    async fn merge_base(
        &self,
        request: Request<pb::MergeBaseReq>,
    ) -> Result<Response<pb::MergeBaseResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "MergeBase")?;
        let a = Self::id(&req.a, "a")?;
        let b = Self::id(&req.b, "b")?;
        let base = self.repo.merge_base(a, b)?;
        Ok(Response::new(pb::MergeBaseResp {
            id: base.map(id_to_bytes),
        }))
    }

    async fn is_ancestor(
        &self,
        request: Request<pb::IsAncestorReq>,
    ) -> Result<Response<pb::IsAncestorResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "IsAncestor")?;
        let a = Self::id(&req.a, "a")?;
        let b = Self::id(&req.b, "b")?;
        let is_ancestor = self.repo.is_ancestor(a, b)?;
        Ok(Response::new(pb::IsAncestorResp { is_ancestor }))
    }

    async fn diff(
        &self,
        request: Request<pb::DiffReq>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Diff")?;
        let a = Self::id(&req.a, "a")?;
        let b = Self::id(&req.b, "b")?;
        let changes = self.repo.diff(a, b)?;
        let items = changes.into_iter().map(|c| {
            pb::ChangeMsg::try_from(&c)
                .map_err(|e| Status::internal(format!("change has no wire form: {e}")))
        });
        Ok(Response::new(Box::pin(iter(items))))
    }

    async fn log(&self, request: Request<pb::LogReq>) -> Result<Response<Self::LogStream>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Log")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let entries = self.repo.log(cs_id, req.limit)?;
        let items = entries.into_iter().map(|(id, cs)| {
            Ok(pb::ChangesetSummary {
                cs_id: id_to_bytes(id),
                parents: cs.parents.iter().map(|p| id_to_bytes(*p)).collect(),
                message: cs.message.clone(),
                author: Some(pb::IdentityMsg::from(&cs.author)),
                committer: Some(pb::IdentityMsg::from(&cs.committer)),
            })
        });
        Ok(Response::new(Box::pin(iter(items))))
    }

    async fn blame(
        &self,
        request: Request<pb::BlameReq>,
    ) -> Result<Response<Self::BlameStream>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Blame")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let ranges = self.repo.blame(cs_id, &req.path)?;
        let items = ranges.into_iter().map(|(lo, hi, cs)| {
            Ok(pb::BlameRange {
                lo,
                hi,
                cs_id: id_to_bytes(cs),
            })
        });
        Ok(Response::new(Box::pin(iter(items))))
    }

    async fn list_refs(
        &self,
        request: Request<pb::ListRefsReq>,
    ) -> Result<Response<Self::ListRefsStream>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "ListRefs")?;
        let refs = self.repo.list_refs()?;
        let items = refs.into_iter().map(|(name, cs_id)| {
            Ok(pb::RefEntry {
                name,
                cs_id: id_to_bytes(cs_id),
            })
        });
        Ok(Response::new(Box::pin(iter(items))))
    }

    async fn get_ref(
        &self,
        request: Request<pb::GetRefReq>,
    ) -> Result<Response<pb::GetRefResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "GetRef")?;
        if req.name.is_empty() {
            return Err(Status::invalid_argument("ref name must be non-empty"));
        }
        let cs_id = self.repo.get_ref(&req.name)?;
        Ok(Response::new(pb::GetRefResp {
            cs_id: cs_id.map(id_to_bytes),
        }))
    }

    async fn cas_ref(
        &self,
        request: Request<pb::CasRefReq>,
    ) -> Result<Response<pb::CasResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "CasRef")?;
        if req.name.is_empty() {
            return Err(Status::invalid_argument("ref name must be non-empty"));
        }
        let expected = match req.expected {
            Some(b) => Some(Self::id(&b, "expected")?),
            None => None,
        };
        let new = Self::id(&req.new, "new")?;
        let outcome = self.repo.cas_ref(&req.name, expected, new)?;
        let result = match outcome {
            RefCas::Won { .. } => pb::cas_resp::Result::Won(true),
            RefCas::Conflict { current } => {
                pb::cas_resp::Result::ConflictCurrent(id_to_bytes(current))
            }
        };
        Ok(Response::new(pb::CasResp {
            result: Some(result),
        }))
    }

    async fn graft(
        &self,
        request: Request<pb::GraftReq>,
    ) -> Result<Response<pb::GraftResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Graft")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let new_parent = Self::id(&req.new_parent, "new_parent")?;
        let id = self.repo.graft(cs_id, new_parent)?;
        Ok(Response::new(pb::GraftResp {
            id: id_to_bytes(id),
        }))
    }

    async fn harvest(
        &self,
        request: Request<pb::HarvestReq>,
    ) -> Result<Response<pb::HarvestResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Harvest")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let id = self.repo.harvest(cs_id)?;
        Ok(Response::new(pb::HarvestResp {
            id: id_to_bytes(id),
        }))
    }

    async fn get_stats(
        &self,
        request: Request<pb::GetStatsReq>,
    ) -> Result<Response<pb::StatsResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "GetStats")?;
        Ok(Response::new(pb::StatsResp {
            counters: self.repo.stats().into_iter().collect(),
        }))
    }
}

#[cfg(test)]
mod ref_normalization_tests {
    use super::*;

    fn id(tag: u8) -> Id {
        Id::from_bytes([tag; 32])
    }

    fn repo() -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo::open(dir.path()).expect("repo opens");
        (dir, repo)
    }

    /// A bare-name write lands under `refs/heads/` (git-valid on export)
    /// and reads back through both spellings.
    #[test]
    fn bare_ref_writes_land_under_refs_heads() {
        let (_dir, repo) = repo();
        assert!(matches!(
            repo.cas_ref("main", None, id(1)).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("refs/heads/main".to_owned(), id(1))]
        );
        assert_eq!(repo.get_ref("main").expect("get"), Some(id(1)));
        assert_eq!(repo.get_ref("refs/heads/main").expect("get"), Some(id(1)));
    }

    /// An imported history's full-name ref answers bare-name lookups, and a
    /// bare-name CAS moves *that* row instead of forking a second ref.
    #[test]
    fn bare_names_resolve_and_move_imported_refs() {
        let (_dir, repo) = repo();
        // The import path stores full names (rootstock does exactly this).
        repo.cas_ref("refs/heads/main", None, id(1))
            .expect("imported ref");
        assert_eq!(repo.get_ref("main").expect("fallback"), Some(id(1)));

        assert!(matches!(
            repo.cas_ref("main", Some(id(1)), id(2)).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("refs/heads/main".to_owned(), id(2))]
        );

        // CAS semantics are unchanged: a stale expectation still conflicts.
        assert!(matches!(
            repo.cas_ref("main", Some(id(1)), id(3)).expect("cas"),
            RefCas::Conflict { current } if current == id(2)
        ));
    }

    /// A row stored under the bare spelling (an older writer) still
    /// resolves and still receives bare-name CASes.
    #[test]
    fn legacy_bare_rows_still_resolve() {
        let (_dir, repo) = repo();
        repo.metadata
            .cas_ref("main", None, id(1))
            .expect("legacy bare row");
        assert_eq!(repo.get_ref("main").expect("fallback"), Some(id(1)));
        assert!(matches!(
            repo.cas_ref("main", Some(id(1)), id(2)).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("main".to_owned(), id(2))]
        );
    }
}

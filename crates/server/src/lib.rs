//! gardenbed-server — the server: the authoritative gardenbed server
//! (lib part; the binary is `src/main.rs`).
//!
//! [`Repo`] wires one repository's stores (objects + metadata) and the derived data
//! [`Engine`] into the Task-B service layer: changeset creation with blob
//! validation and eager derivation, object access, the ancestry queries,
//! refs with compare-and-swap, server-side rebase (linearized rebase), and
//! the publish fast-forward. [`GardenbedService`] adapts [`Repo`] to the
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
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, blob_id};
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
    /// A `<cs-or-ref>` revspec named neither an existing ref nor, read as
    /// a changeset id prefix, any stored changeset.
    #[error("no ref or changeset matching {0}")]
    RevspecNotFound(String),
    /// A changeset id prefix matched more than one changeset; the caller
    /// must send more hex characters.
    #[error("prefix {spec} is ambiguous ({matches} changesets)")]
    AmbiguousPrefix {
        /// The prefix as typed.
        spec: String,
        /// How many stored changesets share the prefix.
        matches: usize,
    },
    /// Rebase hit semantic conflicts; the paths are listed in the message.
    #[error("rebase conflicts on: {}", .0.join(", "))]
    RebaseConflict(Vec<String>),
    /// Publish refused: ref "main" is not an ancestor of the target (or the
    /// CAS was lost to a concurrent publish after the check passed).
    #[error("publish refused: \"main\" is not an ancestor of the given changeset")]
    NotFastForward,
    /// Merge hit semantic conflicts; the paths are listed in the message.
    #[error("merge conflicts on: {}", .0.join(", "))]
    MergeConflict(Vec<String>),

    /// Publish named "main" in the `ref` field: main publishes stay the
    /// plain (ref-less) request form, gated on protocol review.
    #[error(
        "publish refused: ref \"main\" must use the plain form (leave the ref field unset); main publishes are review-gated"
    )]
    MainRefReserved,
    /// Publish lost a lane-ref CAS (or hit a non-descendant target the
    /// rewound form already permitted); names the ref and its current tip.
    #[error(
        "publish refused: ref {ref_name:?} is not fast-forwarded by the given changeset; \
         the ref now points at {current}",
        current = current.to_hex()
    )]
    LaneNotFastForward {
        /// The ref whose CAS was lost.
        ref_name: String,
        /// The ref's current tip (full hex).
        current: Id,
    },
    #[error(
        "publish refused: ref {ref_name:?} would move backward from {old} to {new}, \
         losing {fell_off} changeset(s); re-send with force set to permit this move",
        old = old.to_hex(),
        new = new.to_hex()
    )]
    PublishRewind {
        /// The ref whose move was refused.
        ref_name: String,
        /// The ref's current tip (full hex).
        old: Id,
        /// The refused new tip (full hex).
        new: Id,
        /// Changesets reachable from the old tip but not the new one.
        fell_off: u64,
    },
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

/// The repository service: stores + derived data engine + counters.
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
    /// `<root>/objects/` holds the content-addressed object store and
    /// `<root>/metadata.sqlite3` the metadata database. Idempotent.
    pub fn open(root: &Path) -> Result<Self, ServerError> {
        Self::validate_layout(root)?;
        let objects = Arc::new(FsObjectStore::open(root)?);
        let metadata = Arc::new(SqliteMetadataStore::open(&root.join("metadata.sqlite3"))?);
        Ok(Self::new(objects, metadata))
    }

    /// Refuse obsolete layouts instead of silently opening an empty repository.
    pub fn validate_layout(root: &Path) -> Result<(), ServerError> {
        if root.join("seedbed.sqlite3").exists() && !root.join("metadata.sqlite3").exists() {
            return Err(ServerError::Invalid(
                "repository uses an obsolete storage layout; export it with the previous version and import into a new directory".into(),
            ));
        }
        Ok(())
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

    /// Opens a repository over explicit stores — the scale-out entry point
    /// (docs/scale-out-design.md): `gardenbed_store::StorageConfig::open`
    /// produces the `(objects, metadata)` pair and this builds the same
    /// service over them that [`Repo::open`] builds over a local root.
    /// Identical to [`Repo::new`]; the distinct name says "explicit stores
    /// are a first-class way to open, not a test hook".
    pub fn open_stores(objects: Arc<dyn ObjectStore>, metadata: Arc<dyn MetadataStore>) -> Self {
        Self::new(objects, metadata)
    }

    /// The object store this repo was opened over — for callers that drive
    /// store-level APIs alongside the repo (e.g. git import/export).
    pub fn objects_handle(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.objects)
    }

    /// The metadata store this repo was opened over.
    pub fn metadata_handle(&self) -> Arc<dyn MetadataStore> {
        Arc::clone(&self.metadata)
    }

    /// The derived-data engine this repo runs (git import takes it to keep
    /// its derivation on the same engine instance as the repo).
    pub fn engine_handle(&self) -> Engine {
        self.engine.clone()
    }

    // -------------------------------------------------------- write path

    /// Commits a changeset: validates parents and blob references, stores any
    /// inline blobs, validates tree application and computes derived data
    /// before storing the changeset. Returns the changeset's id.
    ///
    /// Blob resolution per change: a blob already in objects passes as-is;
    /// otherwise it must ride in `inline_blobs` (keyed by its 64-hex id),
    /// weigh ≤ [`MAX_INLINE_BLOB_LEN`], and hash to its id. Gitlink changes
    /// are exempt: their blob field is an opaque upstream pointer (the
    /// zero-padded git SHA-1), not a content address, and is never stored
    /// in objects.
    pub fn create_changeset(
        &self,
        cs: &Changeset,
        inline_blobs: &BTreeMap<String, Vec<u8>>,
    ) -> Result<Id, ServerError> {
        // Semantic validation incl. canonical sortedness — the codec is the
        // single authority on changeset shape.
        validate_changeset(cs, true)?;
        for p in &cs.parents {
            self.validate_history(*p)?;
        }

        // Resolve blob references: objects hit, inline hit (verified, ≤ 1 MiB),
        // or gitlink pointer.
        let mut resolved: HashMap<Id, bool> = HashMap::new(); // id → stored now
        for c in &cs.changes {
            let Some(blob) = c.blob else { continue }; // DELETE carries none
            if c.mode == Mode::Gitlink {
                continue; // opaque pointer, never a objects object
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
                    "blob {key} is referenced but neither in objects nor inline"
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

        // Derive against the parents before making the changeset visible.
        // Failed semantic validation may leave unreferenced immutable objects,
        // but never a changeset that a caller can subsequently publish.
        let cs_id = cs.id()?;
        let root = self.engine.validated_tree_for_new(cs)?;
        let rows = self
            .engine
            .derived_rows_for_validated_tree(cs_id, cs, root)?;
        self.metadata
            .put_validated_changeset(cs, root, &rows, None)?;
        Ok(cs_id)
    }

    /// Durable validation certificates bound publication work to new history.
    fn validate_history(&self, head: Id) -> Result<(), ServerError> {
        self.engine
            .certify_history(head)
            .map_err(|e| ServerError::for_changeset(e, head))
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
    /// objects objects, so they read as absent here too).
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
    /// ascending, with rename following. Delegates to the derived data
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

    /// The most recently *stored* changesets, newest arrival first, as
    /// `(changeset id, changeset)` pairs, capped at `limit` (`0` =
    /// unlimited — the whole arrival log).
    ///
    /// Unlike [`Repo::log`] this has no graph semantics: **unpublished
    /// changesets are included** (working copies commit directly to the
    /// server, so a changeset no ref can reach is the normal shape of
    /// agent work-in-progress, not corruption). Arrival order, not commit
    /// dates: see [`MetadataStore::recent_changesets`].
    pub fn activity(&self, limit: u32) -> Result<Vec<(Id, Changeset)>, ServerError> {
        let ids = self
            .metadata
            .recent_changesets(if limit == 0 { u32::MAX } else { limit })?;
        ids.into_iter()
            .map(|id| Ok((id, self.get_changeset(id)?)))
            .collect()
    }

    /// Resolves a client-facing `<cs-or-ref>` argument: a ref's current
    /// value, or — when no ref matches — the changeset a hex token names.
    /// `None` only when the token cannot name anything here.
    ///
    /// This is the shared revspec resolver: it is the one string-bearing
    /// lookup every id-taking surface funnels through (remote `bed`
    /// clients through the `GetRef` RPC, the MCP tools through [`Repo`]
    /// directly), so one fix here teaches `GetChangeset`, `Diff`,
    /// `Blame`, `file`, `switch` and friends truncated ids at once.
    /// Resolution order (precedence is deliberate and documented on the
    /// `GetRef` RPC):
    ///
    /// 1. an existing ref with that exact name wins over prefix
    ///    interpretation — the canonical `refs/heads/<name>` spelling
    ///    first, then the name as given (imported histories carry full
    ///    names; native writes land there too);
    /// 2. a hex token of 8..=63 chars is a **changeset id prefix**:
    ///    decoded case-insensitively to bytes and ranged over the
    ///    metadata's 32-byte ids with one indexed seek
    ///    (`id >= prefix‖0x00… AND id < prefix+1‖0x00…`). Exactly one
    ///    match resolves; several fail with
    ///    [`ServerError::AmbiguousPrefix`] (`invalid_argument`, naming
    ///    the match count); none with [`ServerError::RevspecNotFound`]
    ///    (`not_found`, saying the token is neither a ref nor a matching
    ///    prefix);
    /// 3. fewer than 8 hex chars is rejected with the minimum-length
    ///    hint — but only once no ref of that name exists (rule 1);
    /// 4. a full 64-hex token keeps today's behavior: every client parses
    ///    it as an id *before* any lookup, so here it is only ever a ref
    ///    name and stays `None` when no ref is so named.
    pub fn get_ref(&self, name: &str) -> Result<Option<Id>, ServerError> {
        let canonical = branch_ref_name(name);
        if let found @ Some(_) = self.metadata.resolve_ref(&canonical)? {
            return Ok(found);
        }
        if canonical != name
            && let found @ Some(_) = self.metadata.resolve_ref(name)?
        {
            return Ok(found);
        }
        // Not a ref under either spelling: a hex token may still name a
        // changeset id (see the doc comment for the precedence rules).
        match classify_id_hex(name) {
            HexToken::NotHex | HexToken::Full => Ok(None),
            HexToken::TooShort => Err(ServerError::Invalid(format!(
                "changeset id prefix must be at least 8 hex chars: {name:?}"
            ))),
            HexToken::Prefix { lo, hi } => prefix_outcome(
                name,
                &self
                    .metadata
                    .changeset_ids_in_id_range(&lo, hi.as_ref().map(|h| &h[..]))?,
            ),
        }
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
        self.validate_history(new)?;
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

    // ------------------------------------------------- rebase and publish

    /// Server-side rebase (linearized rebase): replays `cs_id`'s changes onto
    /// `new_parent`'s tree with **hard** conflict detection, then stores a
    /// new changeset `parents = [new_parent]` carrying the same changes,
    /// message, identities and signature, and eagerly materializes it.
    ///
    /// Writes replay when the target matches the pre-image or desired result;
    /// removals require an unchanged pre-image. Directory replacements also
    /// compare all descendants, preventing loss of concurrent subtree edits.
    /// Conflicts are checked against immutable trees before any application.
    pub fn rebase(&self, cs_id: Id, new_parent: Id) -> Result<Id, ServerError> {
        let cs = self.get_changeset(cs_id)?;
        // The new parent must exist and its tree must be derivable (a
        // missing parent is `ChangesetNotFound`, mapped by `root_tree`).
        let base = self.root_tree(new_parent)?;

        // The flat working set of the new parent: path → (mode, blob).
        let files: BTreeMap<String, (Mode, Id)> = self
            .engine
            .flatten_tree(base)?
            .into_iter()
            .map(|(path, mode, blob)| (path, (mode, blob)))
            .collect();

        // The pre-image: the flat tree of the changeset's own first parent.
        // A write may overwrite a path the new parent already has when that
        // copy is exactly the one the changeset was made against — the
        // text-merge-free three-way rule: untouched between parent and new
        // parent means the replay is as safe as it was against the parent.
        let pre: BTreeMap<String, (Mode, Id)> = match cs.parents.first() {
            Some(first_parent) => self
                .engine
                .flatten_tree(self.root_tree(*first_parent)?)?
                .into_iter()
                .map(|(path, mode, blob)| (path, (mode, blob)))
                .collect(),
            None => BTreeMap::new(),
        };

        let mut conflicts: Vec<String> = Vec::new();
        for c in &cs.changes {
            match c.op {
                gardenbed_model::Op::Add | gardenbed_model::Op::Modify => {
                    check_write(&files, &pre, &c.path, c.blob, c.mode, &mut conflicts);
                }
                gardenbed_model::Op::Delete => {
                    check_removal(&files, &pre, &c.path, &mut conflicts);
                }
                gardenbed_model::Op::Rename => {
                    let Some(src) = c.src_path.as_deref() else {
                        conflicts.push(format!("rename {:?} without a src_path", c.path));
                        continue;
                    };
                    check_removal(&files, &pre, src, &mut conflicts);
                    check_write(&files, &pre, &c.path, c.blob, c.mode, &mut conflicts);
                }
            }
        }
        if !conflicts.is_empty() {
            return Err(ServerError::RebaseConflict(conflicts));
        }

        // The replayed changeset: same changes and metadata, one parent.
        let rebased = Changeset {
            parents: vec![new_parent],
            changes: cs.changes.clone(),
            message: cs.message.clone(),
            author: cs.author.clone(),
            committer: cs.committer.clone(),
            signature: cs.signature.clone(),
        };
        // validate_changeset(…, true) re-checks shape/sortedness; blob ids
        // are already objects objects (they shipped with the original), so the
        // inline map is empty.
        self.create_changeset(&rebased, &BTreeMap::new())
    }

    /// Fast-forwards a ref to `cs_id` (v0.1.md D5): asserts that the ref's
    /// current value (when one exists) is an ancestor of `cs_id`, then
    /// compare-and-swaps the ref to it. Returns `cs_id`.
    ///
    /// `ref_name = None` is the classic main publish: with no value yet,
    /// the ancestry precondition holds trivially and the CAS creates the
    /// ref from nothing; a lost race surfaces as
    /// [`ServerError::NotFastForward`], the caller retries against the new
    /// value. A non-main ref gets the same CAS semantics plus the import
    /// rewind guard: a move to a non-descendant (`old` not an ancestor of
    /// `cs_id`) is refused with [`ServerError::PublishRewind`] naming the
    /// ref, both tips, and how many changesets fall off — unless `force`
    /// is set, the deliberate force-push escape hatch. Naming `"main"`
    /// (or `"refs/heads/main"`) explicitly is refused
    /// ([`ServerError::MainRefReserved`]): main publishes stay the plain
    /// form, gated on protocol review.
    pub fn publish(
        &self,
        cs_id: Id,
        ref_name: Option<&str>,
        force: bool,
    ) -> Result<Id, ServerError> {
        let name = ref_name.unwrap_or("main");
        if let Some(ref_name) = ref_name
            && matches!(ref_name, "main" | "refs/heads/main")
        {
            return Err(ServerError::MainRefReserved);
        }
        // The published changeset must exist (a clear 404 beats a CAS that
        // would silently create a ref to nowhere).
        self.get_changeset(cs_id)?;
        // Resolve the ref the same way `cas_ref` picks its target row: the
        // canonical `refs/heads/<name>` spelling first, then the name as
        // given (imported histories carry full names).
        let current = {
            let canonical = branch_ref_name(name);
            match self.metadata.resolve_ref(&canonical)? {
                found @ Some(_) => found,
                None if canonical != name => self.metadata.resolve_ref(name)?,
                None => None,
            }
        };
        if let Some(cur) = current
            && !self
                .engine
                .is_ancestor(cur, cs_id)
                .map_err(|e| ServerError::for_changeset(e, cur))?
        {
            if ref_name.is_none() {
                return Err(ServerError::NotFastForward);
            }
            if !force {
                return Err(ServerError::PublishRewind {
                    ref_name: name.to_owned(),
                    old: cur,
                    new: cs_id,
                    fell_off: self.count_fell_off(cur, cs_id)?,
                });
            }
        }
        match self.cas_ref(name, current, cs_id)? {
            RefCas::Won { .. } => Ok(cs_id),
            RefCas::Conflict { .. } => {
                if ref_name.is_none() {
                    Err(ServerError::NotFastForward)
                } else {
                    Err(ServerError::LaneNotFastForward {
                        ref_name: name.to_owned(),
                        current: self
                            .get_ref(name)?
                            .ok_or(ServerError::RefNotFound(name.to_owned()))?,
                    })
                }
            }
        }
    }

    /// How many changesets are reachable from `old` but not from `new` —
    /// the work that falls off a rewound ref. Two plain BFS walks over the
    /// metadata's parent edges (changeset rows only, each fetched once per
    /// walk), mirroring the import-side counter of the same name.
    fn count_fell_off(&self, old: Id, new: Id) -> Result<u64, ServerError> {
        fn reachable(
            metadata: &dyn gardenbed_store::MetadataStore,
            start: Id,
        ) -> Result<std::collections::HashSet<Id>, ServerError> {
            let mut seen = std::collections::HashSet::from([start]);
            let mut queue = vec![start];
            while let Some(x) = queue.pop() {
                let (cs, _) = metadata
                    .get_changeset(x)?
                    .ok_or(ServerError::ChangesetNotFound(x))?;
                for p in &cs.parents {
                    if seen.insert(*p) {
                        queue.push(*p);
                    }
                }
            }
            Ok(seen)
        }
        let from_old = reachable(self.metadata.as_ref(), old)?;
        let from_new = reachable(self.metadata.as_ref(), new)?;
        Ok(from_old.difference(&from_new).count() as u64)
    }

    /// Server-side two-parent merge (v0.2.md D9-D11): merges `theirs`'
    /// tree into `ours`' over their merge base and stores one changeset
    /// `parents = [ours, theirs]` whose changes are the diff
    /// `tree(ours) -> merged tree` - through the ordinary
    /// [`Repo::create_changeset`] path (zero new storage, zero new
    /// validation). Returns `(id, up_to_date)`; when `theirs` is already
    /// an ancestor of `ours`, answers `(ours, true)` and stores nothing.
    ///
    /// Conflicts are a hard error listing every conflicting path
    /// ([`ServerError::MergeConflict`], `FAILED_PRECONDITION`, rebase's
    /// shape) - nothing is stored beyond unreferenced merged blobs,
    /// harmless by construction. Unrelated histories (no merge base)
    /// fail naming the operation.
    pub fn merge(
        &self,
        ours: Id,
        theirs: Id,
        message: &str,
        author: &gardenbed_model::Identity,
        committer: &gardenbed_model::Identity,
    ) -> Result<(Id, bool), ServerError> {
        // Both sides must exist (clear 404s before any derivation).
        self.get_changeset(ours)?;
        self.get_changeset(theirs)?;
        // Up to date: theirs already an ancestor of ours (every
        // changeset is an ancestor of itself, so ours == theirs counts).
        if self.is_ancestor(theirs, ours)? {
            return Ok((ours, true));
        }
        let base = self.merge_base(ours, theirs)?.ok_or_else(|| {
            ServerError::Invalid(format!(
                "merge refused: {ours} and {theirs} share no common ancestor (unrelated histories cannot be merged)"
            ))
        })?;
        let changes = match self.engine.merge_tree_changes(Some(base), ours, theirs)? {
            Ok(changes) => changes,
            Err(conflicts) => return Err(ServerError::MergeConflict(conflicts)),
        };
        let merged = gardenbed_model::Changeset {
            parents: vec![ours, theirs],
            changes,
            message: message.to_owned(),
            author: author.clone(),
            committer: committer.clone(),
            signature: None,
        };
        // The ordinary create path: validation, blob resolution (merged
        // blobs are already objects objects), derived rows, one txn.
        let id = self.create_changeset(&merged, &BTreeMap::new())?;
        Ok((id, false))
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

/// The canonical metadata name for a client-facing ref: bare branch names
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

/// A hex token's id interpretation, applied once it has not matched a ref
/// (the classification behind [`Repo::get_ref`]'s rules 2-4).
enum HexToken {
    /// Not all-hex (empty, other characters, or longer than a full id):
    /// not an id shape at all.
    NotHex,
    /// Exactly 64 hex chars: a full id — never prefix-interpreted here,
    /// because clients resolve full ids directly against the changeset
    /// store before anything would treat the token as a ref name.
    Full,
    /// All-hex but shorter than 8 chars: too short to resolve as a prefix.
    TooShort,
    /// 8..=63 hex chars: the half-open 32-byte id range `[lo, hi)` the
    /// prefix selects. `hi` is the prefix incremented in its last nibble
    /// (with carry); `None` when every nibble is `0xF`, where the
    /// successor overflows and the range is open-ended.
    Prefix {
        /// Range start: prefix bytes right-padded with `0x00` to 32.
        lo: [u8; 32],
        /// Exclusive range end (the next prefix's start), if representable.
        hi: Option<[u8; 32]>,
    },
}

/// Classifies a token by id shape: all-hex (any case), with a full 64-hex
/// id kept distinct from resolvable prefixes (8..=63 hex chars).
fn classify_id_hex(token: &str) -> HexToken {
    let bytes = token.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
        return HexToken::NotHex;
    }
    if bytes.len() == 64 {
        return HexToken::Full;
    }
    if bytes.len() < 8 {
        return HexToken::TooShort;
    }
    let nibbles: Vec<u8> = bytes
        .iter()
        .map(|b| match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => b - b'A' + 10,
        })
        .collect();
    let lo = pack_nibbles(&nibbles);
    let hi = {
        // The exclusive upper bound: the prefix plus one, i.e. the first
        // nibble that is not 0xF incremented, everything after it zeroed
        // by the carry — or open-ended when the prefix is all 0xF.
        let mut inc = nibbles;
        let mut i = inc.len();
        loop {
            if i == 0 {
                break None;
            }
            i -= 1;
            if inc[i] == 0xF {
                inc[i] = 0;
            } else {
                inc[i] += 1;
                break Some(pack_nibbles(&inc));
            }
        }
    };
    HexToken::Prefix { lo, hi }
}

/// Packs big-endian nibbles (≤ 64) into exactly 32 zero-padded bytes.
fn pack_nibbles(nibbles: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, n) in nibbles.iter().enumerate() {
        if i % 2 == 0 {
            out[i / 2] = n << 4;
        } else {
            out[i / 2] |= n;
        }
    }
    out
}

/// The resolution outcome for one scanned id prefix: exactly one match
/// resolves; none says the token is neither a ref nor a matching prefix;
/// several name the match count so the caller can lengthen the prefix.
fn prefix_outcome(spec: &str, matches: &[Id]) -> Result<Option<Id>, ServerError> {
    match matches {
        [] => Err(ServerError::RevspecNotFound(spec.to_owned())),
        [only] => Ok(Some(*only)),
        many => Err(ServerError::AmbiguousPrefix {
            spec: spec.to_owned(),
            matches: many.len(),
        }),
    }
}

/// Compare immutable pre-images before applying any of the replay's changes.
/// This also covers explicit deletes swallowed by an earlier directory-to-file
/// replacement, and rename destinations independently of their sources.
fn check_removal(
    files: &BTreeMap<String, (Mode, Id)>,
    pre: &BTreeMap<String, (Mode, Id)>,
    path: &str,
    conflicts: &mut Vec<String>,
) {
    if files.get(path).is_none() || files.get(path) != pre.get(path) {
        conflicts.push(format!("removal of missing or changed path {path:?}"));
    }
}

fn check_write(
    files: &BTreeMap<String, (Mode, Id)>,
    pre: &BTreeMap<String, (Mode, Id)>,
    path: &str,
    blob: Option<Id>,
    mode: Mode,
    conflicts: &mut Vec<String>,
) {
    let Some(blob) = blob else {
        conflicts.push(format!("write to {path:?} without a blob"));
        return;
    };
    let existing = files.get(path);
    if existing == Some(&(mode, blob)) {
        return; // Exact desired result: no path or subtree is overwritten.
    }
    if existing != pre.get(path) {
        conflicts.push(format!("{path:?} was changed or deleted"));
    }
    let mut ancestor = path;
    while let Some((parent, _)) = ancestor.rsplit_once('/') {
        if let Some(value) = files.get(parent)
            && pre.get(parent) != Some(value)
        {
            conflicts.push(format!("{path:?} sits under a concurrently changed file"));
        }
        ancestor = parent;
    }
    // Replacing a directory must not erase concurrent additions, edits,
    // mode changes, or deletions anywhere in the swallowed subtree.
    let prefix = format!("{path}/");
    let descendants = |map: &BTreeMap<String, (Mode, Id)>| {
        map.range(prefix.clone()..)
            .take_while(|(p, _)| p.starts_with(&prefix))
            .map(|(p, value)| (p.clone(), *value))
            .collect::<BTreeMap<_, _>>()
    };
    if descendants(files) != descendants(pre) {
        conflicts.push(format!("directory {path:?} was changed"));
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
            ServerError::Invalid(_) | ServerError::AmbiguousPrefix { .. } => {
                Status::invalid_argument(e.to_string())
            }
            ServerError::ChangesetNotFound(_)
            | ServerError::ObjectNotFound(_)
            | ServerError::RefNotFound(_)
            | ServerError::RevspecNotFound(_) => Status::not_found(e.to_string()),
            ServerError::MergeConflict(_) => Status::failed_precondition(e.to_string()),
            ServerError::RebaseConflict(_)
            | ServerError::NotFastForward
            | ServerError::LaneNotFastForward { .. }
            | ServerError::PublishRewind { .. } => Status::failed_precondition(e.to_string()),
            ServerError::MainRefReserved => Status::invalid_argument(e.to_string()),
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
        // The shared revspec resolver (see `Repo::get_ref`): an existing ref
        // with the exact name wins over prefix interpretation; a hex token
        // of 8..=63 chars then resolves as a changeset id prefix (one match
        // → its id; several → `invalid_argument` naming the count; none →
        // `not_found` saying it matched neither a ref nor a prefix; shorter
        // → `invalid_argument` with the minimum-length hint). A full
        // 64-hex token stays a ref name here — clients parse those as ids
        // before any lookup.
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

    async fn rebase(
        &self,
        request: Request<pb::RebaseReq>,
    ) -> Result<Response<pb::RebaseResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Rebase")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let new_parent = Self::id(&req.new_parent, "new_parent")?;
        let id = self.repo.rebase(cs_id, new_parent)?;
        Ok(Response::new(pb::RebaseResp {
            id: id_to_bytes(id),
        }))
    }

    async fn publish(
        &self,
        request: Request<pb::PublishReq>,
    ) -> Result<Response<pb::PublishResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Publish")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        // An absent/empty ref field is the v0.1 plain form ("main");
        // anything else names a lane ref.
        let ref_name = match req.r#ref.as_str() {
            "" => None,
            name => Some(name),
        };
        let id = self.repo.publish(cs_id, ref_name, req.force)?;
        Ok(Response::new(pb::PublishResp {
            id: id_to_bytes(id),
        }))
    }

    async fn merge(
        &self,
        request: Request<pb::MergeReq>,
    ) -> Result<Response<pb::MergeResp>, Status> {
        let req = request.into_inner();
        self.repo_field(&req.repo, "Merge")?;
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let target = Self::id(&req.target, "target")?;
        let author = req
            .author
            .map(Identity::from)
            .ok_or_else(|| Status::invalid_argument("MergeReq carries no author"))?;
        let committer = req
            .committer
            .map(Identity::from)
            .ok_or_else(|| Status::invalid_argument("MergeReq carries no committer"))?;
        let (id, up_to_date) = self.repo.merge(cs_id, target, &req.message, &author, &committer)?;
        Ok(Response::new(pb::MergeResp {
            id: id_to_bytes(id),
            up_to_date,
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

/// A real published changeset for ref-writing fixtures: ref CAS validates
/// its target against the full DAG (`validate_history`), so refs in tests
/// must point at changesets that actually exist and derive cleanly — an
/// empty root, or a child of `parents`.
#[cfg(test)]
fn fixture_changeset(repo: &Repo, parents: Vec<Id>, message: &str) -> Id {
    let identity = gardenbed_model::Identity {
        name: "test".into(),
        email: "test@example.com".into(),
        time_secs: 1,
        tz_min: 0,
    };
    let cs = Changeset {
        parents,
        changes: vec![],
        message: message.into(),
        author: identity.clone(),
        committer: identity,
        signature: None,
    };
    repo.create_changeset(&cs, &BTreeMap::new())
        .expect("fixture changeset publishes")
}

#[cfg(test)]
mod revspec_tests {
    use super::*;
    use gardenbed_store::MemoryStore;

    fn id(tag: u8) -> Id {
        Id::from_bytes([tag; 32])
    }

    /// A `MemoryStore` whose id-prefix scan answers from a canned list:
    /// the resolver's decisions do not need real SHA-256 ids.
    struct CannedScan {
        inner: MemoryStore,
        matches: std::sync::Mutex<Vec<Id>>,
    }

    impl MetadataStore for CannedScan {
        fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError> {
            self.inner.put_changeset(cs)
        }
        fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError> {
            self.inner.get_changeset(id)
        }
        fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError> {
            self.inner.resolve_ref(name)
        }
        fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
            self.inner.list_refs()
        }
        fn cas_ref(&self, name: &str, expected: Option<Id>, new: Id) -> Result<RefCas, StoreError> {
            self.inner.cas_ref(name, expected, new)
        }
        fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError> {
            self.inner.record_git_mapping(git_sha_hex, cs)
        }
        fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError> {
            self.inner.cs_for_git(git_sha_hex)
        }
        fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
            self.inner.git_for_cs(cs)
        }
        fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
            self.inner.derived_put(kind, cs, bytes)
        }
        fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
            self.inner.derived_get(kind, cs)
        }
        fn validation_tree(&self, cs: Id) -> Result<Option<Option<Id>>, StoreError> {
            self.inner.validation_tree(cs)
        }
        fn put_validated_changeset(
            &self,
            cs: &Changeset,
            root: Option<Id>,
            rows: &[gardenbed_store::DerivedRow],
            git: Option<&str>,
        ) -> Result<Id, StoreError> {
            self.inner.put_validated_changeset(cs, root, rows, git)
        }
        fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError> {
            self.inner.recent_changesets(limit)
        }
        fn changeset_ids_in_id_range(
            &self,
            _lo: &[u8],
            _hi_exclusive: Option<&[u8]>,
        ) -> Result<Vec<Id>, StoreError> {
            Ok(self.matches.lock().expect("scan lock").clone())
        }
    }

    fn canned_repo(matches: Vec<Id>) -> Repo {
        Repo::new(
            Arc::new(MemoryStore::new()),
            Arc::new(CannedScan {
                inner: MemoryStore::new(),
                matches: std::sync::Mutex::new(matches),
            }),
        )
    }

    /// Exactly one match resolves (rule 2).
    #[test]
    fn single_prefix_match_resolves() {
        let repo = canned_repo(vec![id(7)]);
        assert_eq!(
            repo.get_ref("1d050f87").expect("resolve"),
            Some(id(7)),
            "an 8-hex prefix resolves when the scan finds one changeset"
        );
    }

    /// Several matches refuse, naming the count (rule 2).
    #[test]
    fn ambiguous_prefix_names_the_match_count() {
        let repo = canned_repo(vec![id(1), id(2), id(3)]);
        let err = repo.get_ref("1d050f87").expect_err("ambiguous");
        assert_eq!(
            err.to_string(),
            "prefix 1d050f87 is ambiguous (3 changesets)"
        );
    }

    /// No ref and no matching changeset: the error names both readings
    /// of the token.
    #[test]
    fn unmatched_prefix_says_no_ref_or_changeset() {
        let repo = canned_repo(vec![]);
        let err = repo.get_ref("1d050f877").expect_err("unmatched");
        assert_eq!(err.to_string(), "no ref or changeset matching 1d050f877");
    }

    /// Fewer than 8 hex chars is rejected with the minimum-length hint —
    /// but only once no ref of that name exists (rule 1 runs first).
    #[test]
    fn too_short_prefix_is_rejected_with_the_hint() {
        let repo = canned_repo(vec![id(1)]);
        let err = repo.get_ref("1d05").expect_err("too short");
        assert!(
            err.to_string()
                .contains("changeset id prefix must be at least 8 hex chars"),
            "hint present: {err}"
        );

        // A ref with exactly that name still wins — and, like every ref,
        // must name a published changeset.
        let repo = canned_repo(vec![id(1)]);
        let head = fixture_changeset(&repo, vec![], "ref target");
        repo.cas_ref("1d05", None, head).expect("cas");
        assert_eq!(repo.get_ref("1d05").expect("ref wins"), Some(head));
    }

    /// Rule 1: an existing ref with that exact name wins over prefix
    /// interpretation — even when the prefix scan would be ambiguous.
    #[test]
    fn existing_ref_wins_over_a_matching_prefix() {
        let repo = canned_repo(vec![id(1), id(2), id(3)]);
        let head = fixture_changeset(&repo, vec![], "ref target");
        repo.cas_ref("deadbeef01", None, head).expect("cas");
        assert_eq!(
            repo.get_ref("deadbeef01").expect("ref wins"),
            Some(head),
            "the ref answers although the scan would find 3 changesets"
        );
    }

    /// A full 64-hex token is never prefix-interpreted here (rule 4:
    /// clients resolve those as ids before any ref lookup), and a
    /// non-hex token stays a plain ref name.
    #[test]
    fn full_hex_tokens_are_never_prefix_interpreted() {
        let full = id(5).to_hex();
        let repo = canned_repo(vec![id(5)]);
        assert_eq!(
            repo.get_ref(&full).expect("full id"),
            None,
            "a 64-hex token is only ever a ref name in get_ref"
        );
        assert_eq!(repo.get_ref("main").expect("plain name"), None);
    }

    /// The prefix byte-range: case-insensitive decode, odd trailing
    /// nibble shifted low, exclusive bound = prefix + 1 in nibble space
    /// (with carry), open-ended when every nibble is 0xF.
    #[test]
    fn prefix_bounds_follow_the_bytes() {
        // Pads a leading byte slice with zeros to a full 32-byte id.
        let pad = |head: &[u8]| {
            let mut out = [0u8; 32];
            out[..head.len()].copy_from_slice(head);
            out
        };
        let bounds = |token: &str| match classify_id_hex(token) {
            HexToken::Prefix { lo, hi } => Some((lo, hi)),
            _ => None,
        };
        // Uppercase decodes like lowercase; even prefix → hi = prefix + 1.
        assert_eq!(
            bounds("1D050F87"),
            Some((
                pad(&[0x1d, 0x05, 0x0f, 0x87]),
                Some(pad(&[0x1d, 0x05, 0x0f, 0x88]))
            ))
        );
        // Odd prefix: the trailing nibble rides low in `lo`, and `hi`
        // increments that nibble (0x70…0x7f → 0x80).
        assert_eq!(
            bounds("1d050f877"),
            Some((
                pad(&[0x1d, 0x05, 0x0f, 0x87, 0x70]),
                Some(pad(&[0x1d, 0x05, 0x0f, 0x87, 0x80]))
            ))
        );
        // Carry through trailing F nibbles.
        assert_eq!(
            bounds("1d050fff"),
            Some((
                pad(&[0x1d, 0x05, 0x0f, 0xff]),
                Some(pad(&[0x1d, 0x05, 0x10, 0x00]))
            ))
        );
        // All-F prefix: the successor overflows, the range is open-ended.
        assert_eq!(
            bounds("ffffffff"),
            Some((pad(&[0xff, 0xff, 0xff, 0xff]), None))
        );
        // The minimal prefix and a long odd one classify the same way.
        assert!(matches!(
            bounds("00000000"),
            Some((lo, _)) if lo == pad(&[0x00])
        ));
        assert!(matches!(bounds(&"a".repeat(63)), Some((_, Some(_)))));

        // Shape classification: 64-hex is Full (never a prefix), shorter
        // than 8 is TooShort, anything non-hex (or over-long) is NotHex.
        assert!(matches!(classify_id_hex(&"a".repeat(64)), HexToken::Full));
        assert!(matches!(
            classify_id_hex(&"a".repeat(63)),
            HexToken::Prefix { .. }
        ));
        assert!(matches!(
            classify_id_hex(&"a".repeat(7)),
            HexToken::TooShort
        ));
        assert!(matches!(classify_id_hex(&"a".repeat(65)), HexToken::NotHex));
        assert!(matches!(classify_id_hex("main"), HexToken::NotHex));
        assert!(matches!(classify_id_hex(""), HexToken::NotHex));
        assert!(matches!(classify_id_hex(&"z".repeat(64)), HexToken::NotHex));
    }
}

#[cfg(test)]
mod ref_normalization_tests {
    use super::*;

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
        let head = fixture_changeset(&repo, vec![], "root");
        assert!(matches!(
            repo.cas_ref("main", None, head).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("refs/heads/main".to_owned(), head)]
        );
        assert_eq!(repo.get_ref("main").expect("get"), Some(head));
        assert_eq!(repo.get_ref("refs/heads/main").expect("get"), Some(head));
    }

    /// An imported history's full-name ref answers bare-name lookups, and a
    /// bare-name CAS moves *that* row instead of forking a second ref.
    #[test]
    fn bare_names_resolve_and_move_imported_refs() {
        let (_dir, repo) = repo();
        // The import path stores full names (git_bridge does exactly this),
        // and every ref target is a published changeset.
        let r1 = fixture_changeset(&repo, vec![], "root");
        repo.cas_ref("refs/heads/main", None, r1)
            .expect("imported ref");
        assert_eq!(repo.get_ref("main").expect("fallback"), Some(r1));

        let r2 = fixture_changeset(&repo, vec![r1], "second");
        assert!(matches!(
            repo.cas_ref("main", Some(r1), r2).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("refs/heads/main".to_owned(), r2)]
        );

        // CAS semantics are unchanged: a stale expectation still conflicts.
        let r3 = fixture_changeset(&repo, vec![r2], "third");
        assert!(matches!(
            repo.cas_ref("main", Some(r1), r3).expect("cas"),
            RefCas::Conflict { current } if current == r2
        ));
    }

    /// A row stored under the bare spelling (an older writer) still
    /// resolves and still receives bare-name CASes.
    #[test]
    fn legacy_bare_rows_still_resolve() {
        let (_dir, repo) = repo();
        // The legacy row goes straight into the metadata store (bypassing
        // the Repo-level validation a pre-validation writer would have
        // skipped) but still names a real published changeset.
        let r1 = fixture_changeset(&repo, vec![], "root");
        repo.metadata
            .cas_ref("main", None, r1)
            .expect("legacy bare row");
        assert_eq!(repo.get_ref("main").expect("fallback"), Some(r1));
        let r2 = fixture_changeset(&repo, vec![r1], "second");
        assert!(matches!(
            repo.cas_ref("main", Some(r1), r2).expect("cas"),
            RefCas::Won { .. }
        ));
        assert_eq!(
            repo.list_refs().expect("refs"),
            vec![("main".to_owned(), r2)]
        );
    }
}

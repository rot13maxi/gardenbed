//! bed's read path to the arboretum (v0.1.md §2): connect, resolve
//! `<cs-or-ref>` arguments, and fetch objects **want-list**.
//!
//! Everything here is content-addressed: every request names an id bed is
//! missing, and every fetch is **cache-first** — `.bed/objects` (loam's
//! `FsObjectStore`, reused as-is) is consulted before any RPC. That is what
//! makes interrupted sessions resume exactly: a rerun requests only what
//! the first run never received, and the server's request counters prove
//! nothing crosses the wire twice.
//!
//! The want-list plan ([`plan_plot`]) resolves `RootTree(cs)` server-side,
//! then walks the subtrees under each plot prefix, fetching missing trees
//! with bounded parallelism and collecting the file entries to materialize.
//! Blobs are fetched afterwards, deduplicated by id, with the same bound.
//! Tree payloads are cached locally as canonical bytes, so a rerun's walk
//! runs from cache.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

use gardenbed_model::{Id, Mode, Tree, TreeKind};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_store::ObjectStore as _;
use tokio::sync::Semaphore;
use tokio_stream::StreamExt;

use crate::bed::Bed;
use crate::error::BedError;
use crate::revspec::CsOrRef;
use crate::treestate::MODE_EXEC;
use crate::treestate::MODE_GITLINK;
use crate::treestate::MODE_REGULAR;
use crate::treestate::MODE_SYMLINK;

/// Default bound on concurrent object fetches (`--concurrency`).
pub const DEFAULT_CONCURRENCY: usize = 8;

/// One materializable file of the plot at a changeset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanFile {
    /// Repo-relative, forward-slash path.
    pub path: String,
    /// §4 mode (regular / exec / symlink / gitlink).
    pub mode: Mode,
    /// Content address of the blob (for gitlinks: the upstream pointer).
    pub blob: Id,
}

impl PlanFile {
    /// The §4 mode code as stored in the treestate.
    pub fn mode_code(&self) -> i64 {
        match self.mode {
            Mode::Regular => MODE_REGULAR,
            Mode::Exec => MODE_EXEC,
            Mode::Symlink => MODE_SYMLINK,
            Mode::Gitlink => MODE_GITLINK,
        }
    }
}

/// The result of a want-list fetch: the plot's files plus how many objects
/// this run actually pulled over the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Trees fetched from the arboretum this run (cache misses).
    pub trees_fetched: u64,
    /// Blobs fetched from the arboretum this run (cache misses).
    pub blobs_fetched: u64,
    /// File entries collected under the plot prefixes.
    pub files: usize,
}

/// A connected session with one arboretum over one repo.
///
/// Cheap to clone; every object fetch goes through the shared [`Fetcher`].
pub struct Session {
    /// The typed gRPC client.
    pub client: GardenbedClient,
    repo: String,
    fetcher: Arc<Fetcher>,
}

impl Session {
    /// Opens a bed and connects to its arboretum, with the default fetch
    /// concurrency.
    ///
    /// The connection is real (`Endpoint::connect`), so a server that is
    /// down fails here — before any local state is touched.
    pub async fn open(bed: &Bed) -> Result<Self, BedError> {
        Self::open_with(bed, DEFAULT_CONCURRENCY).await
    }

    /// [`Session::open`] with an explicit bound on concurrent object
    /// fetches (`--concurrency`).
    pub async fn open_with(bed: &Bed, concurrency: usize) -> Result<Self, BedError> {
        Self::connect(
            bed.config().repo.as_str(),
            bed.objects()?,
            bed.config().server.as_str(),
            concurrency,
        )
        .await
    }

    /// Connects to `endpoint`, caching objects into `objects` on behalf of
    /// `repo`, with at most `concurrency` fetches in flight.
    pub async fn connect(
        repo: &str,
        objects: gardenbed_store::FsObjectStore,
        endpoint: &str,
        concurrency: usize,
    ) -> Result<Self, BedError> {
        let client = GardenbedClient::connect(endpoint).await.map_err(|e| {
            BedError::Connect {
                server: endpoint.to_owned(),
                message: e.to_string(),
            }
        })?;
        let fetcher = Arc::new(Fetcher::new(client.clone(), repo, objects, concurrency));
        Ok(Session {
            client,
            repo: repo.to_owned(),
            fetcher,
        })
    }

    /// The cache-first fetcher (shared counter state).
    pub fn fetcher(&self) -> &Arc<Fetcher> {
        &self.fetcher
    }

    /// Resolves a `<cs-or-ref>` argument to a changeset id that exists on
    /// the arboretum: a 64-hex token is looked up as an id, anything else
    /// as a ref name. Both paths verify existence, so every caller gets a
    /// clean [`BedError`] instead of a raw gRPC status later.
    pub async fn resolve(&self, spec: &CsOrRef, command: &'static str) -> Result<Id, BedError> {
        let id = match spec {
            CsOrRef::Id(id) => *id,
            CsOrRef::Ref(name) => {
                let value = rpc(&self.client.get_ref(&self.repo, name).await, command)?;
                value.ok_or_else(|| BedError::MissingRef {
                    name: name.to_owned(),
                })?
            }
        };
        // Existence check: a wrong id should say "not on the arboretum",
        // not surface as a mysterious missing-object error later.
        self.get_changeset(id, command).await?;
        Ok(id)
    }

    /// The bed's current changeset (config `current_cs`), verified on the
    /// arboretum.
    pub async fn resolve_current_cs(
        &self,
        bed: &Bed,
        command: &'static str,
    ) -> Result<Id, BedError> {
        let hex = bed
            .config()
            .current_cs
            .as_deref()
            .ok_or(BedError::NoCurrentCs)?;
        let id = Id::from_str(hex).map_err(|_| BedError::NoCurrentCs)?;
        self.get_changeset(id, command).await?;
        Ok(id)
    }

    /// One changeset, with server errors mapped to bed's voice.
    pub async fn get_changeset(
        &self,
        id: Id,
        command: &'static str,
    ) -> Result<gardenbed_model::Changeset, BedError> {
        match self.client.get_changeset(&self.repo, id).await {
            Ok(cs) => Ok(cs),
            Err(status) if status.code() == tonic::Code::NotFound => Err(BedError::MissingCs(id)),
            Err(status) => Err(BedError::Rpc {
                command,
                code: status.code(),
                message: status.message().to_owned(),
            }),
        }
    }

    /// The changeset's root tree id (`None` = the empty tree).
    pub async fn root_tree(
        &self,
        cs: Id,
        command: &'static str,
    ) -> Result<Option<Id>, BedError> {
        match self.client.root_tree(&self.repo, cs).await {
            Ok(tree) => Ok(tree),
            Err(status) if status.code() == tonic::Code::NotFound => Err(BedError::MissingCs(cs)),
            Err(status) => Err(BedError::Rpc {
                command,
                code: status.code(),
                message: status.message().to_owned(),
            }),
        }
    }

    /// The first parent of `cs`, or `None` for a root changeset.
    pub async fn first_parent(
        &self,
        cs: Id,
        command: &'static str,
    ) -> Result<Option<Id>, BedError> {
        let cs_obj = self.get_changeset(cs, command).await?;
        Ok(cs_obj.parents.first().copied())
    }

    /// The log from `cs`, newest first. A missing head changeset maps to
    /// [`BedError::MissingCs`]; stream failures to [`BedError::Rpc`].
    pub async fn log(
        &self,
        cs: Id,
        limit: u32,
        command: &'static str,
    ) -> Result<Vec<gardenbed_protocol::pb::ChangesetSummary>, BedError> {
        let stream = match self.client.log(&self.repo, cs, limit).await {
            Ok(stream) => stream,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(BedError::MissingCs(cs));
            }
            Err(status) => {
                return Err(BedError::Rpc {
                    command,
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        };
        drain(stream, command).await
    }

    /// The tree diff `a → b` (rename detection off, server-side).
    pub async fn diff(
        &self,
        a: Id,
        b: Id,
        command: &'static str,
    ) -> Result<Vec<gardenbed_protocol::pb::ChangeMsg>, BedError> {
        let stream = match self.client.diff(&self.repo, a, b).await {
            Ok(stream) => stream,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(BedError::MissingCs(a));
            }
            Err(status) => {
                return Err(BedError::Rpc {
                    command,
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        };
        drain(stream, command).await
    }

    /// The tree entry at `path` under `cs`'s root tree, resolving
    /// `RootTree` and walking down (caching trees along the way). `None`
    /// when the path names nothing (or sits under a file).
    pub async fn locate_entry(
        &self,
        cs: Id,
        path: &str,
        command: &'static str,
    ) -> Result<Option<gardenbed_model::TreeEntry>, BedError> {
        let Some(root) = self.root_tree(cs, command).await? else {
            return Ok(None);
        };
        let mut cur = root;
        let comps: Vec<&str> = path.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = self.fetcher.tree(cur).await?;
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                return Ok(None);
            };
            if i + 1 == comps.len() {
                return Ok(Some(entry.clone()));
            }
            match entry.kind {
                TreeKind::Dir => {
                    cur = entry.tree.ok_or_else(|| BedError::BlobHashMismatch(cur))?;
                }
                // A file cannot contain anything below it.
                TreeKind::File => return Ok(None),
            }
        }
        unreachable!("the loop returns on the final component")
    }
}

/// Drains a server stream into a Vec, mapping mid-stream failures.
async fn drain<T>(
    mut stream: tonic::Streaming<T>,
    command: &'static str,
) -> Result<Vec<T>, BedError> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.map_err(|status| BedError::Rpc {
            command,
            code: status.code(),
            message: status.message().to_owned(),
        })?);
    }
    Ok(out)
}

/// Maps a ready `Result<_, tonic::Status>` onto bed's error voice.
fn rpc<T: Clone>(result: &Result<T, tonic::Status>, command: &'static str) -> Result<T, BedError> {
    match result {
        Ok(t) => Ok(t.clone()),
        Err(status) => Err(BedError::Rpc {
            command,
            code: status.code(),
            message: status.message().to_owned(),
        }),
    }
}

/// Cache-first object fetcher: the local CAS is consulted before every RPC;
/// trees are stored as canonical bytes, blobs as raw content verified
/// against their address on arrival (a lying tier can never poison the
/// cache — the store re-verifies the hash at write time too).
pub struct Fetcher {
    client: GardenbedClient,
    repo: String,
    objects: gardenbed_store::FsObjectStore,
    bound: Arc<Semaphore>,
    /// Per-id locks: concurrent requests for the same tree id collapse into
    /// one fetch (the second waiter re-checks the cache and finds it).
    tree_locks: tokio::sync::Mutex<std::collections::HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
    /// Trees fetched from the arboretum this run (cache misses).
    trees_fetched: AtomicU64,
    /// Blobs fetched from the arboretum this run (cache misses).
    blobs_fetched: AtomicU64,
}

impl Fetcher {
    /// A fetcher over `objects` for `repo`, with at most `concurrency`
    /// fetches in flight.
    fn new(
        client: GardenbedClient,
        repo: &str,
        objects: gardenbed_store::FsObjectStore,
        concurrency: usize,
    ) -> Self {
        Fetcher {
            client,
            repo: repo.to_owned(),
            objects,
            bound: Arc::new(Semaphore::new(concurrency.max(1))),
            tree_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            trees_fetched: AtomicU64::new(0),
            blobs_fetched: AtomicU64::new(0),
        }
    }

    /// How many objects this run pulled over the wire so far: `(trees,
    /// blobs)`.
    pub fn fetched(&self) -> (u64, u64) {
        (self.trees_fetched.load(Relaxed), self.blobs_fetched.load(Relaxed))
    }

    /// A tree by id — local cache first, then one `GetTree` RPC, stored
    /// locally as canonical bytes (the store verifies the bytes hash to the
    /// id before publishing them).
    ///
    /// Concurrent requests for the same id collapse into a single fetch:
    /// latecomers take the id's lock, re-find the tree in the cache, and
    /// return without an RPC. Every tree is therefore fetched at most once
    /// per run, whatever the walk shape.
    pub async fn tree(&self, id: Id) -> Result<Tree, BedError> {
        if let Some(bytes) = self.objects.get_tree(id)? {
            return Tree::decode(&bytes)
                .map_err(|_| BedError::BlobHashMismatch(id));
        }
        let lock = {
            let mut locks = self.tree_locks.lock().await;
            locks.entry(id).or_default().clone()
        };
        let _gate = lock.lock().await;
        // Someone may have fetched it while we waited for the lock.
        if let Some(bytes) = self.objects.get_tree(id)? {
            return Tree::decode(&bytes)
                .map_err(|_| BedError::BlobHashMismatch(id));
        }
        let _permit = self.bound.acquire().await.expect("semaphore is never closed");
        let tree = self
            .client
            .get_tree(&self.repo, id)
            .await
            .map_err(|status| BedError::Rpc {
                command: "fetch",
                code: status.code(),
                message: status.message().to_owned(),
            })?;
        let bytes = tree
            .canonical_bytes()
            .map_err(|_| BedError::BlobHashMismatch(id))?;
        self.objects.put_tree(id, &bytes)?;
        self.trees_fetched.fetch_add(1, Relaxed);
        Ok(tree)
    }

    /// Raw blob content by id — local cache first, then a streamed
    /// `GetBlob`, reassembled and hash-verified before it is cached.
    pub async fn blob(&self, id: Id) -> Result<Vec<u8>, BedError> {
        if let Some(content) = self.objects.get_blob(id)? {
            return Ok(content);
        }
        let _permit = self.bound.acquire().await.expect("semaphore is never closed");
        let mut stream = self
            .client
            .get_blob(&self.repo, id)
            .await
            .map_err(|status| BedError::Rpc {
                command: "fetch",
                code: status.code(),
                message: status.message().to_owned(),
            })?;
        let mut content = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|status| BedError::Rpc {
                command: "fetch",
                code: status.code(),
                message: status.message().to_owned(),
            })?;
            content.extend_from_slice(&chunk.data);
        }
        if gardenbed_model::blob_id(&content) != id {
            return Err(BedError::BlobHashMismatch(id));
        }
        self.objects.put_blob(&content)?;
        self.blobs_fetched.fetch_add(1, Relaxed);
        Ok(content)
    }

    /// Fetches every missing blob in `ids` (deduplicated by content
    /// address, bounded parallelism). Cached blobs cost nothing.
    pub async fn blobs(self: &Arc<Self>, ids: &HashSet<Id>) -> Result<(), BedError> {
        let mut missing = Vec::new();
        for id in ids {
            if !self.objects.has(*id)? {
                missing.push(*id);
            }
        }
        let mut tasks = tokio::task::JoinSet::new();
        for id in missing {
            let fetcher = self.clone();
            tasks.spawn(async move { fetcher.blob(id).await.map(|_| id) });
        }
        while let Some(res) = tasks.join_next().await {
            res.expect("blob fetch task does not panic")?;
        }
        Ok(())
    }
}

/// Walks the subtrees under each plot prefix at the root tree `root`,
/// fetching every missing tree it passes and collecting the file entries
/// under the prefixes.
///
/// Prefixes match at component boundaries exactly like
/// [`crate::plot::in_plot`]: `"src"` selects the `src` subtree, a bare file
/// prefix (`"Cargo.toml"`) selects that one entry, and `"."` selects the
/// whole tree. A prefix that names nothing in this tree simply yields no
/// entries (planting ahead of the garden is allowed).
///
/// Returns the plan — file entries deduplicated by path, sorted by path
/// bytes — plus fetch statistics. Every blob under the plan is fetched
/// (cache-first) before the plan is returned, so materialization never
/// touches the network.
pub async fn plan_plot(
    session: &Session,
    root: Option<Id>,
    prefixes: &[String],
) -> Result<(Vec<PlanFile>, FetchStats), BedError> {
    let mut files: Vec<PlanFile> = Vec::new();
    let mut seen_paths: BTreeSet<String> = BTreeSet::new();
    let mut blob_ids: HashSet<Id> = HashSet::new();

    let Some(root) = root else {
        // The empty tree: nothing to plant under any prefix.
        return Ok((files, FetchStats::default()));
    };

    // Subtrees still to walk: (tree id, dir path the tree hangs under).
    let mut queue: Vec<(Id, String)> = Vec::new();
    let mut queued: HashSet<(Id, String)> = HashSet::new();
    let push = |tree: Id, dir: String, queue: &mut Vec<_>, queued: &mut HashSet<_>| {
        if queued.insert((tree, dir.clone())) {
            queue.push((tree, dir));
        }
    };

    // Resolve each prefix to a starting subtree, fetching the trees along
    // the way (cache-first inside `Fetcher::tree`).
    for prefix in prefixes {
        if prefix == "." {
            push(root, String::new(), &mut queue, &mut queued);
            continue;
        }
        let mut cur = root;
        let comps: Vec<&str> = prefix.split('/').collect();
        for (i, comp) in comps.iter().enumerate() {
            let tree = session.fetcher.tree(cur).await?;
            let Some(entry) = tree.entries.iter().find(|e| e.name == *comp) else {
                break; // names nothing in this tree — nothing to plant
            };
            let last = i + 1 == comps.len();
            match entry.kind {
                TreeKind::Dir => {
                    let id = entry.tree.ok_or_else(|| BedError::BlobHashMismatch(cur))?;
                    if last {
                        push(id, prefix.clone(), &mut queue, &mut queued);
                    }
                    cur = id;
                }
                TreeKind::File => {
                    if last {
                        // An exact-file prefix: one entry, no subtree.
                        let blob = entry.blob.ok_or_else(|| BedError::BlobHashMismatch(cur))?;
                        if seen_paths.insert(prefix.clone()) {
                            files.push(PlanFile {
                                path: prefix.clone(),
                                mode: entry.mode,
                                blob,
                            });
                        }
                        // Gitlink pointers are never loam objects.
                        if entry.mode != Mode::Gitlink {
                            blob_ids.insert(blob);
                        }
                    }
                    break;
                }
            }
        }
    }

    // Breadth-first walk of the subtrees, one level at a time; each tree is
    // visited once (the `queued` set de-duplicates across overlapping
    // prefixes and repeated ids).
    while !queue.is_empty() {
        let batch: Vec<(Id, String)> = std::mem::take(&mut queue);
        let fetcher = session.fetcher().clone();
        let mut tasks = tokio::task::JoinSet::new();
        for (tree_id, dir) in batch {
            let fetcher = fetcher.clone();
            tasks.spawn(async move { fetcher.tree(tree_id).await.map(|t| (tree_id, dir, t)) });
        }
        while let Some(joined) = tasks.join_next().await {
            let (tree_id, dir, tree) = joined.expect("plan walk task does not panic")?;
            for entry in &tree.entries {
                let path = if dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{dir}/{}", entry.name)
                };
                match entry.kind {
                    TreeKind::Dir => {
                        let id = entry
                            .tree
                            .ok_or_else(|| BedError::BlobHashMismatch(tree_id))?;
                        push(id, path.clone(), &mut queue, &mut queued);
                    }
                    TreeKind::File => {
                        let blob = entry
                            .blob
                            .ok_or_else(|| BedError::BlobHashMismatch(tree_id))?;
                        if seen_paths.insert(path.clone()) {
                            files.push(PlanFile {
                                path,
                                mode: entry.mode,
                                blob,
                            });
                        }
                        // A gitlink's blob field is an opaque upstream
                        // pointer, not a content address — it is never a
                        // loam object and must not be fetched.
                        if entry.mode != Mode::Gitlink {
                            blob_ids.insert(blob);
                        }
                    }
                }
            }
        }
    }

    // Fetch every missing blob now, deduplicated by content address.
    session.fetcher().blobs(&blob_ids).await?;
    let (trees_fetched, blobs_fetched) = session.fetcher().fetched();
    let file_count = files.len();
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok((
        files,
        FetchStats {
            trees_fetched,
            blobs_fetched,
            files: file_count,
        },
    ))
}


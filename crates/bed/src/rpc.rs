//! working copy's read path to the server (v0.1.md §2): connect, resolve
//! `<cs-or-ref>` arguments, and fetch objects **want-list**.
//!
//! Everything here is content-addressed: every request names an id bed is
//! missing, and every fetch is **cache-first** — `.bed/objects` (the object store's
//! `FsObjectStore`, reused as-is) is consulted before any RPC. That is what
//! makes interrupted sessions resume exactly: a rerun requests only what
//! the first run never received, and the server's request counters prove
//! nothing crosses the wire twice.
//!
//! The want-list plan ([`plan_sparse`]) resolves `RootTree(cs)` server-side,
//! then walks the subtrees under each sparse prefix, fetching missing trees
//! with bounded parallelism and collecting the file entries to materialize.
//! Blobs are fetched afterwards, deduplicated by id, with the same bound.
//! Tree payloads are cached locally as canonical bytes, so a rerun's walk
//! runs from cache.

use std::collections::BTreeSet;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

use gardenbed_model::{Id, Mode, Tree, TreeKind};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_store::ObjectStore as _;
use tokio::sync::Semaphore;
use tokio_stream::StreamExt;

use crate::error::ClientError;
use crate::revspec::CsOrRef;
use crate::treestate::MODE_EXEC;
use crate::treestate::MODE_GITLINK;
use crate::treestate::MODE_REGULAR;
use crate::treestate::MODE_SYMLINK;
use crate::workspace::WorkingCopy;

/// Default bound on concurrent object fetches (`--concurrency`).
pub const DEFAULT_CONCURRENCY: usize = 8;

/// One materializable file of the sparse selection at a changeset.
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

/// The result of a want-list fetch: the selected paths' files plus how many objects
/// this run actually pulled over the wire.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchStats {
    /// Trees fetched from the server this run (cache misses).
    pub trees_fetched: u64,
    /// Blobs fetched from the server this run (cache misses).
    pub blobs_fetched: u64,
    /// File entries collected under the selected paths prefixes.
    pub files: usize,
}

/// A connected session with one server over one repo.
///
/// Cheap to clone; every object fetch goes through the shared [`Fetcher`].
pub struct Session {
    /// The typed gRPC client.
    pub client: GardenbedClient,
    repo: String,
    fetcher: Arc<Fetcher>,
}

impl Session {
    /// Opens a working copy and connects to its server, with the default fetch
    /// concurrency.
    ///
    /// The connection is real (`Endpoint::connect`), so a server that is
    /// down fails here — before any local state is touched.
    pub async fn open(bed: &WorkingCopy) -> Result<Self, ClientError> {
        Self::open_with(bed, DEFAULT_CONCURRENCY).await
    }

    /// [`Session::open`] with an explicit bound on concurrent object
    /// fetches (`--concurrency`).
    pub async fn open_with(bed: &WorkingCopy, concurrency: usize) -> Result<Self, ClientError> {
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
    ///
    /// The TCP+HTTP/2 handshake is bounded (`Endpoint::connect_timeout`,
    /// 5s): a hung server (SIGSTOPped, or a black-holed address) fails
    /// as a `ClientError::Connect` within the deadline instead of
    /// blocking indefinitely — which is what lets the offline queue
    /// treat unreachable as queueable.
    pub async fn connect(
        repo: &str,
        objects: gardenbed_store::FsObjectStore,
        endpoint: &str,
        concurrency: usize,
    ) -> Result<Self, ClientError> {
        Self::connect_via_daemon(repo, objects, endpoint, concurrency).await
    }

    /// [`Session::connect`] with the daemon-first probe spelled out:
    /// connect to the daemon socket when one is listening (the embedded
    /// endpoint is kept as the fetch-phase fallback), else connect to
    /// `endpoint` directly (v0.2.md D19).
    ///
    /// An absent or dead daemon is not an error — it is a normal state of
    /// an optional tier. When the daemon is primary, fetch RPCs retry
    /// exactly once against the embedded client on daemon death, so a
    /// daemon crash mid-`sparse add` is invisible to the command's
    /// outcome; write RPCs are never blindly retried.
    pub async fn connect_via_daemon(
        repo: &str,
        objects: gardenbed_store::FsObjectStore,
        endpoint: &str,
        concurrency: usize,
    ) -> Result<Self, ClientError> {
        let connect_embedded = || async {
            let server = endpoint.to_owned();
            let endpoint =
                tonic::transport::Endpoint::from_shared(server.clone()).map_err(|e| {
                    ClientError::Connect {
                        server: server.clone(),
                        message: format!("invalid endpoint: {e}"),
                    }
                })?;
            let channel = endpoint
                .connect_timeout(std::time::Duration::from_secs(5))
                .connect()
                .await
                .map_err(|e| ClientError::Connect {
                    server,
                    message: e.to_string(),
                })?;
            let mut client = GardenbedClient::from_channel(channel);
            // GARDENBED_TOKEN, when set in the environment, authenticates
            // every request (the single knob bed exposes — see
            // `GardenbedClient::connect`). The daemon-path client above
            // already picked it up through `connect`; the embedded fallback
            // builds its channel by hand, so apply it here too.
            if let Ok(token) = std::env::var("GARDENBED_TOKEN")
                && !token.is_empty()
            {
                client = client.with_token(&token);
            }
            Ok::<_, ClientError>(client)
        };
        let (client, fallback) = match crate::daemon::socket_path() {
            Some(path) => {
                let daemon_endpoint = format!("unix://{}", path.display());
                match GardenbedClient::connect(&daemon_endpoint).await {
                    Ok(daemon_client) => {
                        let embedded_client = connect_embedded().await?;
                        (daemon_client, Some(embedded_client))
                    }
                    Err(_) => (connect_embedded().await?, None),
                }
            }
            None => (connect_embedded().await?, None),
        };
        let fetcher = Arc::new(Fetcher::new(
            client.clone(),
            fallback,
            repo,
            objects,
            concurrency,
        ));
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
    /// the server: a 64-hex token is looked up as an id, anything else
    /// as a ref name. Both paths verify existence, so every caller gets a
    /// clean [`ClientError`] instead of a raw gRPC status later.
    pub async fn resolve(&self, spec: &CsOrRef, command: &'static str) -> Result<Id, ClientError> {
        let id = match spec {
            CsOrRef::Id(id) => *id,
            CsOrRef::Ref(name) => {
                let value = rpc(&self.client.get_ref(&self.repo, name).await, command)?;
                value.ok_or_else(|| ClientError::MissingRef {
                    name: name.to_owned(),
                })?
            }
        };
        // Existence check: a wrong id should say "not on the server",
        // not surface as a mysterious missing-object error later.
        self.get_changeset(id, command).await?;
        Ok(id)
    }

    /// The working copy's current changeset (config `current_cs`), verified on the
    /// server.
    pub async fn resolve_current_cs(
        &self,
        bed: &WorkingCopy,
        command: &'static str,
    ) -> Result<Id, ClientError> {
        let hex = bed
            .config()
            .current_cs
            .as_deref()
            .ok_or(ClientError::NoCurrentCs)?;
        let id = Id::from_str(hex).map_err(|_| ClientError::NoCurrentCs)?;
        self.get_changeset(id, command).await?;
        Ok(id)
    }

    /// One changeset, with server errors mapped to working copy's voice.
    pub async fn get_changeset(
        &self,
        id: Id,
        command: &'static str,
    ) -> Result<gardenbed_model::Changeset, ClientError> {
        match self.client.get_changeset(&self.repo, id).await {
            Ok(cs) => Ok(cs),
            Err(status) if status.code() == tonic::Code::NotFound => {
                Err(ClientError::MissingCs(id))
            }
            Err(status) => Err(ClientError::Rpc {
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
    ) -> Result<Option<Id>, ClientError> {
        match self.client.root_tree(&self.repo, cs).await {
            Ok(tree) => Ok(tree),
            Err(status) if status.code() == tonic::Code::NotFound => {
                Err(ClientError::MissingCs(cs))
            }
            Err(status) => Err(ClientError::Rpc {
                command,
                code: status.code(),
                message: status.message().to_owned(),
            }),
        }
    }

    /// Server-side two-parent merge of `target` into `cs` (v0.2.md
    /// D9-D11): one changeset `parents = [cs, target]` whose changes are
    /// the diff `tree(cs) -> merged tree`. Conflicts map to
    /// [`ClientError::Rpc`] (FAILED_PRECONDITION, every conflicting path
    /// in the message); an already-merged target answers
    /// `(cs, true)` with nothing stored.
    pub async fn merge(
        &self,
        cs: Id,
        target: Id,
        message: &str,
        author: &gardenbed_model::Identity,
        committer: &gardenbed_model::Identity,
        command: &'static str,
    ) -> Result<(Id, bool), ClientError> {
        match self
            .client
            .merge(&self.repo, cs, target, message, author, committer)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(status) if status.code() == tonic::Code::NotFound => {
                Err(ClientError::MissingCs(cs))
            }
            // The conflict contract (v0.2.md D11): FAILED_PRECONDITION
            // whose message is "merge conflicts on: <paths>" — mapped to
            // the error that carries the resolution hint.
            Err(status)
                if status.code() == tonic::Code::FailedPrecondition
                    && status.message().starts_with("merge conflicts on:") =>
            {
                Err(ClientError::MergeConflict {
                    paths: status
                        .message()
                        .trim_start_matches("merge conflicts on:")
                        .trim()
                        .to_owned(),
                })
            }
            Err(status) => Err(ClientError::Rpc {
                command,
                code: status.code(),
                message: status.message().to_owned(),
            }),
        }
    }

    /// Server-side octopus merge of every `targets` changeset into
    /// `cs`, folded sequentially (git-merge-octopus semantics). ANY
    /// conflict in ANY round refuses — the error carries the round
    /// and every conflicting path; nothing is stored. Targets already
    /// reachable from the accumulated head are skipped. When every
    /// target is already reachable, answers `(cs, true)`.
    pub async fn merge_octopus(
        &self,
        cs: Id,
        targets: &[Id],
        message: &str,
        author: &gardenbed_model::Identity,
        committer: &gardenbed_model::Identity,
        command: &'static str,
    ) -> Result<(Id, bool), ClientError> {
        match self
            .client
            .merge_octopus(&self.repo, cs, targets, message, author, committer)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(status) if status.code() == tonic::Code::NotFound => {
                Err(ClientError::MissingCs(cs))
            }
            // The octopus conflict contract mirrors the two-way one
            // (D11): FAILED_PRECONDITION whose message starts with the
            // octopus refusal and lists the paths.
            Err(status)
                if status.code() == tonic::Code::FailedPrecondition
                    && status.message().starts_with("merge conflicts on:") =>
            {
                Err(ClientError::MergeConflict {
                    paths: status
                        .message()
                        .trim_start_matches("merge conflicts on:")
                        .trim()
                        .to_owned(),
                })
            }
            Err(status) => Err(ClientError::Rpc {
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
    ) -> Result<Option<Id>, ClientError> {
        let cs_obj = self.get_changeset(cs, command).await?;
        Ok(cs_obj.parents.first().copied())
    }

    /// The log from `cs`, newest first. A missing head changeset maps to
    /// [`ClientError::MissingCs`]; stream failures to [`ClientError::Rpc`].
    pub async fn log(
        &self,
        cs: Id,
        limit: u32,
        command: &'static str,
    ) -> Result<Vec<gardenbed_protocol::pb::ChangesetSummary>, ClientError> {
        let stream = match self.client.log(&self.repo, cs, limit).await {
            Ok(stream) => stream,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(ClientError::MissingCs(cs));
            }
            Err(status) => {
                return Err(ClientError::Rpc {
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
    ) -> Result<Vec<gardenbed_protocol::pb::ChangeMsg>, ClientError> {
        let stream = match self.client.diff(&self.repo, a, b).await {
            Ok(stream) => stream,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(ClientError::MissingCs(a));
            }
            Err(status) => {
                return Err(ClientError::Rpc {
                    command,
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        };
        drain(stream, command).await
    }

    /// First-parent lineage blame ranges of `path` at `cs` (v0.1.md D7),
    /// collected from the streamed RPC. Callers pre-validate the path
    /// (`locate_entry`), so a `not_found` here means the changeset is not
    /// on the server — mapped to [`ClientError::MissingCs`]; stream
    /// failures map to [`ClientError::Rpc`].
    pub async fn blame(
        &self,
        cs: Id,
        path: &str,
        command: &'static str,
    ) -> Result<Vec<gardenbed_protocol::pb::BlameRange>, ClientError> {
        let stream = match self.client.blame(&self.repo, cs, path).await {
            Ok(stream) => stream,
            Err(status) if status.code() == tonic::Code::NotFound => {
                return Err(ClientError::MissingCs(cs));
            }
            Err(status) => {
                return Err(ClientError::Rpc {
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
    ) -> Result<Option<gardenbed_model::TreeEntry>, ClientError> {
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
                    cur = entry.tree.ok_or(ClientError::BlobHashMismatch(cur))?;
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
) -> Result<Vec<T>, ClientError> {
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.map_err(|status| ClientError::Rpc {
            command,
            code: status.code(),
            message: status.message().to_owned(),
        })?);
    }
    Ok(out)
}

/// Maps a ready `Result<_, tonic::Status>` onto working copy's error voice.
fn rpc<T: Clone>(
    result: &Result<T, tonic::Status>,
    command: &'static str,
) -> Result<T, ClientError> {
    match result {
        Ok(t) => Ok(t.clone()),
        Err(status) => Err(ClientError::Rpc {
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
    /// The embedded (config.server) client, present only when `client`
    /// talks to the daemon: fetch RPCs retry exactly once against it on
    /// daemon transport death (v0.2.md D19). Write RPCs never touch it.
    fallback: Option<GardenbedClient>,
    repo: String,
    objects: gardenbed_store::FsObjectStore,
    bound: Arc<Semaphore>,
    /// Per-id locks: concurrent requests for the same tree id collapse into
    /// one fetch (the second waiter re-checks the cache and finds it).
    tree_locks: tokio::sync::Mutex<std::collections::HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
    /// Trees fetched from the server this run (cache misses).
    trees_fetched: AtomicU64,
    /// Blobs fetched from the server this run (cache misses).
    blobs_fetched: AtomicU64,
}

impl Fetcher {
    /// A fetcher over `objects` for `repo`, with at most `concurrency`
    /// fetches in flight. `fallback` is the embedded endpoint, set only
    /// when `client` is the daemon.
    fn new(
        client: GardenbedClient,
        fallback: Option<GardenbedClient>,
        repo: &str,
        objects: gardenbed_store::FsObjectStore,
        concurrency: usize,
    ) -> Self {
        Fetcher {
            client,
            fallback,
            repo: repo.to_owned(),
            objects,
            bound: Arc::new(Semaphore::new(concurrency.max(1))),
            tree_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            trees_fetched: AtomicU64::new(0),
            blobs_fetched: AtomicU64::new(0),
        }
    }

    /// True when a status means the daemon's socket went away mid-RPC
    /// (transport death) — the one fetch-phase failure worth one retry
    /// against the embedded endpoint. Anything else (auth, refusal,
    /// not-found) is a real answer, not a transport event.
    fn daemon_transport_dead(status: &tonic::Status) -> bool {
        matches!(
            status.code(),
            tonic::Code::Unavailable | tonic::Code::Internal
        )
    }

    /// Runs `call` against the primary client, retrying exactly once
    /// against the embedded endpoint when the primary is the daemon and
    /// it died mid-RPC (fetch RPCs only — writes are never blindly
    /// retried). Maps both failures onto working copy's error voice.
    async fn fetch_with_fallback<T, F, Fut>(&self, call: F) -> Result<T, ClientError>
    where
        F: Fn(GardenbedClient) -> Fut,
        Fut: Future<Output = Result<T, tonic::Status>>,
    {
        let command: &'static str = "fetch";
        let to_error = |status: tonic::Status| ClientError::Rpc {
            command,
            code: status.code(),
            message: status.message().to_owned(),
        };
        match call(self.client.clone()).await {
            Ok(value) => Ok(value),
            Err(status) => {
                let retry = self
                    .fallback
                    .as_ref()
                    .filter(|_| Self::daemon_transport_dead(&status));
                match retry {
                    Some(embedded) => call(embedded.clone()).await.map_err(to_error),
                    None => Err(to_error(status)),
                }
            }
        }
    }
    /// How many objects this run pulled over the wire so far: `(trees,
    /// blobs)`.
    pub fn fetched(&self) -> (u64, u64) {
        (
            self.trees_fetched.load(Relaxed),
            self.blobs_fetched.load(Relaxed),
        )
    }

    /// A tree by id — local cache first, then one `GetTree` RPC, stored
    /// locally as canonical bytes (the store verifies the bytes hash to the
    /// id before publishing them).
    ///
    /// Concurrent requests for the same id collapse into a single fetch:
    /// latecomers take the id's lock, re-find the tree in the cache, and
    /// return without an RPC. Every tree is therefore fetched at most once
    /// per run, whatever the walk shape.
    pub async fn tree(&self, id: Id) -> Result<Tree, ClientError> {
        if let Some(bytes) = self.objects.get_tree(id)? {
            return Tree::decode(&bytes).map_err(|_| ClientError::BlobHashMismatch(id));
        }
        let lock = {
            let mut locks = self.tree_locks.lock().await;
            locks.entry(id).or_default().clone()
        };
        let _gate = lock.lock().await;
        // Someone may have fetched it while we waited for the lock.
        if let Some(bytes) = self.objects.get_tree(id)? {
            return Tree::decode(&bytes).map_err(|_| ClientError::BlobHashMismatch(id));
        }
        let _permit = self
            .bound
            .acquire()
            .await
            .expect("semaphore is never closed");
        let repo = self.repo.clone();
        let tree = self
            .fetch_with_fallback(move |client| {
                let repo = repo.clone();
                async move { client.get_tree(&repo, id).await }
            })
            .await?;

        let bytes = tree
            .canonical_bytes()
            .map_err(|_| ClientError::BlobHashMismatch(id))?;
        self.objects.put_tree(id, &bytes)?;
        self.trees_fetched.fetch_add(1, Relaxed);
        Ok(tree)
    }

    /// Raw blob content by id — local cache first, then a streamed
    /// `GetBlob`, reassembled and hash-verified before it is cached.
    pub async fn blob(&self, id: Id) -> Result<Vec<u8>, ClientError> {
        if let Some(content) = self.objects.get_blob(id)? {
            return Ok(content);
        }
        let _permit = self
            .bound
            .acquire()
            .await
            .expect("semaphore is never closed");
        let repo = self.repo.clone();
        let mut stream = self
            .fetch_with_fallback(move |client| {
                let repo = repo.clone();
                async move { client.get_blob(&repo, id).await }
            })
            .await?;
        let mut content = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|status| ClientError::Rpc {
                command: "fetch",
                code: status.code(),
                message: status.message().to_owned(),
            })?;
            content.extend_from_slice(&chunk.data);
        }
        if gardenbed_model::blob_id(&content) != id {
            return Err(ClientError::BlobHashMismatch(id));
        }
        self.objects.put_blob(&content)?;
        self.blobs_fetched.fetch_add(1, Relaxed);
        Ok(content)
    }

    /// Fetches every missing blob in `ids` (deduplicated by content
    /// address, bounded parallelism). Cached blobs cost nothing.
    pub async fn blobs(self: &Arc<Self>, ids: &HashSet<Id>) -> Result<(), ClientError> {
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

/// Walks the subtrees under each sparse prefix at the root tree `root`,
/// fetching every missing tree it passes and collecting the file entries
/// under the prefixes.
///
/// Prefixes match at component boundaries exactly like
/// [`crate::sparse::in_sparse`]: `"src"` selects the `src` subtree, a bare file
/// prefix (`"Cargo.toml"`) selects that one entry, and `"."` selects the
/// whole tree. A prefix that names nothing in this tree simply yields no
/// entries (creating ahead of the working copy is allowed).
///
/// Returns the plan — file entries deduplicated by path, sorted by path
/// bytes — plus fetch statistics. Every blob under the plan is fetched
/// (cache-first) before the plan is returned, so materialization never
/// touches the network.
pub async fn plan_sparse(
    session: &Session,
    root: Option<Id>,
    prefixes: &[String],
) -> Result<(Vec<PlanFile>, FetchStats), ClientError> {
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
                    let id = entry.tree.ok_or(ClientError::BlobHashMismatch(cur))?;
                    if last {
                        push(id, prefix.clone(), &mut queue, &mut queued);
                    }
                    cur = id;
                }
                TreeKind::File => {
                    if last {
                        // An exact-file prefix: one entry, no subtree.
                        let blob = entry.blob.ok_or(ClientError::BlobHashMismatch(cur))?;
                        if seen_paths.insert(prefix.clone()) {
                            files.push(PlanFile {
                                path: prefix.clone(),
                                mode: entry.mode,
                                blob,
                            });
                        }
                        // Gitlink pointers are never objects objects.
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
                        let id = entry.tree.ok_or(ClientError::BlobHashMismatch(tree_id))?;
                        push(id, path.clone(), &mut queue, &mut queued);
                    }
                    TreeKind::File => {
                        let blob = entry.blob.ok_or(ClientError::BlobHashMismatch(tree_id))?;
                        if seen_paths.insert(path.clone()) {
                            files.push(PlanFile {
                                path,
                                mode: entry.mode,
                                blob,
                            });
                        }
                        // A gitlink's blob field is an opaque upstream
                        // pointer, not a content address — it is never a
                        // objects object and must not be fetched.
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

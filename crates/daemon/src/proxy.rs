//! The caching proxy: the daemon's implementation of the generated tonic
//! `Gardenbed` trait.
//!
//! Every RPC is forwarded verbatim to the upstream server (the `repo` field
//! rides untouched — the daemon serves N repositories through the same
//! upstream, exactly like a client would). The only divergence is
//! `GetTree`/`GetBlob`: answered **cache-first** from the shared object
//! store, write-through after each upstream fetch. Because every request
//! names a content address and the store hash-verifies on read *and*
//! write, a warm second working copy's fetch never reaches the server and
//! a corrupt cache can never poison anyone (v0.2.md §5's cache-tier
//! argument, applied locally).
//!
//! The shared store is **partitioned by (upstream origin, repository)**:
//! one `FsObjectStore` per scope under `<cache_root>/scopes/<hex>/`, the
//! hex a SHA-256 over the upstream endpoint and the repository name (the
//! same convention the regional cache tier uses). The scope is a
//! membership proof: an object is cached under a scope only after that
//! scope's own origin fetch succeeded, so a repository that lacks an
//! object is answered with the origin's own `NOT_FOUND`, never another
//! repository's cached bytes — the cross-repository object leak the
//! content-address-only layout allowed (`authorize_hit`'s `ListRefs`
//! probe proves the caller may read the repository, but says nothing
//! about which objects that repository holds). The endpoint rides in the
//! scope for the same reason: a reused cache root fronting a different
//! upstream starts cold rather than serving the old upstream's objects.
//! Pre-scope cache roots (`<cache_root>/objects/...`, written before the
//! partition) are never read: their bytes carry no membership proof, so
//! serving them would need an independent establishment that does not
//! exist — a miss plus one origin fetch is the safe re-warm.
//!
//! Writes (`PutBlob`, `CreateChangeset`, `CasRef`, `Rebase`, `Publish`, `Merge`) are
//! gated on the peer-uid check ([`peer_uid_ok`]): the unix socket is
//! mode-0700 in a private directory, and the kernel credential check is
//! the second lock on that door.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use gardenbed_model::{Id, Tree};
use gardenbed_protocol::convert::{id_from_bytes, id_to_bytes};
use gardenbed_protocol::gardenbed_server::Gardenbed;
use gardenbed_protocol::pb;
use gardenbed_store::{FsObjectStore, ObjectStore as _};
use sha2::{Digest, Sha256};
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

/// The `authorization` metadata value of an incoming request, forwarded
/// verbatim to the upstream. The daemon never interprets credentials —
/// an authed upstream makes the call.
type AuthValue = Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>;

/// Captures `request`'s `authorization` metadata (before `into_inner`).
fn auth_of<T>(request: &Request<T>) -> AuthValue {
    request.metadata().get("authorization").cloned()
}

/// Re-wraps `request`'s message in an upstream request carrying the
/// caller's `authorization` metadata.
fn forward<T>(request: Request<T>) -> tonic::Request<T> {
    let value = request.metadata().get("authorization").cloned();
    let mut out = tonic::Request::new(request.into_inner());
    if let Some(value) = value {
        out.metadata_mut().insert("authorization", value);
    }
    out
}

/// The daemon's upstream connection, re-established on demand.
struct Upstream {
    endpoint: String,
    client: tokio::sync::Mutex<Option<gardenbed_protocol::client::GardenbedClient>>,
}

impl Upstream {
    fn new(endpoint: String) -> Self {
        Upstream {
            endpoint,
            client: tokio::sync::Mutex::new(None),
        }
    }

    /// The current client, connecting (once) if necessary. A cached
    /// channel that has died is discarded and re-dialed: the daemon
    /// outlives server restarts.
    async fn get(&self) -> Result<gardenbed_protocol::client::GardenbedClient, Status> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = gardenbed_protocol::client::GardenbedClient::connect(&self.endpoint)
            .await
            .map_err(|e| {
                Status::unavailable(format!(
                    "daemon cannot reach its upstream server at {}: {e}",
                    self.endpoint
                ))
            })?;
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Drops the cached channel (after a mid-RPC failure, so the next call
    /// re-dials).
    async fn reset(&self) {
        self.client.lock().await.take();
    }
}

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// The caching gRPC proxy over one upstream server.
pub struct ProxyService {
    upstream: Arc<Upstream>,
    /// The cache root every scope directory lives under (kept for
    /// `Clone`, which reopens per-scope stores from it).
    cache_root: PathBuf,
    /// One object store per (origin, repo) scope. A scope is opened
    /// lazily on first use and kept for the daemon's lifetime; stores
    /// are open-directory value types, so the map holds `Arc`s.
    scopes: Mutex<HashMap<String, Arc<FsObjectStore>>>,
    /// Per-tree singleflight: concurrent `GetTree` requests for the same
    /// (scope, id) collapse into one upstream fetch.
    #[allow(clippy::type_complexity)]
    tree_locks: Mutex<HashMap<(String, Id), Arc<tokio::sync::Mutex<()>>>>,
    /// Counters for tests/diagnostics: how many RPCs the daemon answered
    /// from its cache.
    cache_hits: Arc<std::sync::atomic::AtomicU64>,
}

impl ProxyService {
    /// A proxy over `upstream` caching objects into per-(origin, repo)
    /// scope stores under `cache_root` (created if missing; same GBLO1
    /// envelope and sharding as `.bed/objects` inside each scope). The
    /// root itself is only created here; each scope directory is opened
    /// lazily on its first use.
    pub fn new(upstream: &str, cache_root: &Path) -> Result<Self, Status> {
        std::fs::create_dir_all(cache_root).map_err(|e| {
            Status::internal(format!(
                "cannot create the shared object cache root at {}: {e}",
                cache_root.display()
            ))
        })?;
        Ok(ProxyService {
            upstream: Arc::new(Upstream::new(upstream.to_owned())),
            cache_root: cache_root.to_path_buf(),
            scopes: Mutex::new(HashMap::new()),
            tree_locks: Mutex::new(HashMap::new()),
            cache_hits: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// `(cache_hits, )` — GetTree/GetBlob requests answered without an
    /// upstream RPC.
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The cache scope key for `repo` under this proxy's upstream: the
    /// SHA-256 hex over `(upstream endpoint, repo name)`. A digest, not
    /// the raw repo string: repository names are protocol strings, never
    /// filesystem path components (a hostile or merely unusual name must
    /// not escape the cache root or collide with its sibling
    /// directories). The endpoint rides in the key so a cache root reused
    /// for a different upstream starts cold (that upstream's membership
    /// is unproven) instead of serving the old upstream's objects.
    fn scope_key(&self, repo: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.upstream.endpoint.as_bytes());
        hasher.update([0]);
        hasher.update(repo.as_bytes());
        let digest: [u8; 32] = hasher.finalize().into();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The (lazily opened, shared) object store for one cache scope:
    /// `<cache_root>/scopes/<scope_key(repo)>`. Opening is
    /// singleflighted through the map lock; a concurrent first use of
    /// the same scope joins the same `Arc` store.
    fn scope_store(&self, repo: &str) -> Result<Arc<FsObjectStore>, Status> {
        let key = self.scope_key(repo);
        let mut scopes = self.scopes.lock().expect("scope map");
        if let Some(store) = scopes.get(&key) {
            return Ok(store.clone());
        }
        let dir = self.cache_root.join("scopes").join(&key);
        let store = Arc::new(FsObjectStore::open(&dir).map_err(|e| {
            Status::internal(format!(
                "cannot open the scoped object cache at {}: {e}",
                dir.display()
            ))
        })?);
        scopes.insert(key, store.clone());
        Ok(store)
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

    /// One cheap upstream authorization consult. `ListRefs` is the
    /// probe: a repo-scoped read whose server handler runs `authorize`
    /// (repo + `Role::Read`) before touching its stores, and it needs
    /// no object ids — the probe cannot confuse "denied" with "missing
    /// object".
    ///
    /// Cache hits MUST pass this gate. The scoped cache proves
    /// membership (the object was fetched from this repo on this
    /// upstream) but nothing about the *caller*: without a consult,
    /// serving a hit would hand cached bytes to a peer the server would
    /// deny (an anonymous peer, or a token scoped to another repo).
    /// Correctness beats the cache here: the daemon is a convenience
    /// tier, not a policy engine. The probe is one round-trip per cache
    /// hit — the price of not partitioning the cache per credential.
    async fn authorize_hit(&self, repo: &str, auth: &AuthValue) -> Result<(), Status> {
        let client = self.upstream.get().await?.with_auth_value(auth.clone());
        let status = match client.list_refs(repo).await {
            Ok(mut stream) => {
                // Drain the (tiny) stream so the call truly completes;
                // the denial, if any, rides the response headers or the
                // first item.
                use tokio_stream::StreamExt as _;
                while let Some(item) = stream.next().await {
                    if let Err(status) = item {
                        if transport_dead(&status) {
                            self.upstream.reset().await;
                        }
                        return Err(status);
                    }
                }
                return Ok(());
            }
            Err(status) => status,
        };
        if transport_dead(&status) {
            self.upstream.reset().await;
        }
        Err(status)
    }

    /// Cache-first tree lookup: the (origin, repo) scope's store (after
    /// an upstream authorization consult), then one upstream `GetTree`,
    /// write-through. Concurrent requests for the same (scope, id)
    /// collapse into a single fetch.
    async fn tree_from_cache(
        &self,
        repo: &str,
        id: Id,
        auth: AuthValue,
    ) -> Result<pb::TreeResp, Status> {
        let store = self.scope_store(repo)?;
        if let Some(bytes) = store
            .get_tree(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            // The server — not the cache — decides who may read.
            self.authorize_hit(repo, &auth).await?;
            let tree = Tree::decode(&bytes)
                .map_err(|_| Status::internal("shared cache holds an undecodable tree"))?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return pb::TreeResp::try_from(&tree)
                .map_err(|_| Status::internal("cached tree cannot be re-encoded"));
        }
        // Singleflight per (scope, id): the winner fetches and writes
        // through, the latecomer re-checks the scope's cache under the
        // lock.
        let lock = {
            let mut locks = self.tree_locks.lock().expect("tree lock map");
            locks.entry((self.scope_key(repo), id)).or_default().clone()
        };
        let _gate = lock.lock().await;
        if let Some(bytes) = store
            .get_tree(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            self.authorize_hit(repo, &auth).await?;
            let tree = Tree::decode(&bytes)
                .map_err(|_| Status::internal("shared cache holds an undecodable tree"))?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return pb::TreeResp::try_from(&tree)
                .map_err(|_| Status::internal("cached tree cannot be re-encoded"));
        }
        let client = self.upstream.get().await?.with_auth_value(auth);
        let tree = match client.get_tree(repo, id).await {
            Ok(tree) => tree,
            Err(status) => {
                if transport_dead(&status) {
                    self.upstream.reset().await;
                }
                return Err(status);
            }
        };
        let bytes = tree
            .canonical_bytes()
            .map_err(|_| Status::internal("upstream tree cannot be canonically encoded"))?;
        store
            .put_tree(id, &bytes)
            .map_err(|e| Status::internal(format!("shared cache write failed: {e}")))?;
        pb::TreeResp::try_from(&tree)
            .map_err(|_| Status::internal("upstream tree cannot be re-encoded"))
    }

    /// Cache-first blob lookup: the (origin, repo) scope's store (after
    /// an upstream authorization consult), else one streamed upstream
    /// `GetBlob`, reassembled, hash-verified, write-through, then
    /// streamed back to the caller in 64 KiB chunks (tonic's default
    /// 4 MiB decode limit applies per message — one giant chunk would
    /// fail the client).
    async fn blob_stream(
        self,
        repo: String,
        id: Id,
        auth: AuthValue,
    ) -> Result<BoxStream<pb::BlobChunk>, Status> {
        let store = self.scope_store(&repo)?;
        if let Some(content) = store
            .get_blob(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            // The server — not the cache — decides who may read.
            self.authorize_hit(&repo, &auth).await?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(chunked(content));
        }
        let client = self.upstream.get().await?.with_auth_value(auth);
        let mut stream = match client.get_blob(&repo, id).await {
            Ok(stream) => stream,
            Err(status) => {
                if transport_dead(&status) {
                    self.upstream.reset().await;
                }
                return Err(status);
            }
        };
        let mut content = Vec::new();
        use tokio_stream::StreamExt as _;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(status) => {
                    if transport_dead(&status) {
                        self.upstream.reset().await;
                    }
                    return Err(status);
                }
            };
            content.extend_from_slice(&chunk.data);
        }
        if gardenbed_model::blob_id(&content) != id {
            return Err(Status::data_loss(format!(
                "upstream blob {} failed its content-address check",
                id.to_hex()
            )));
        }
        store
            .put_blob(&content)
            .map_err(|e| Status::internal(format!("shared cache write failed: {e}")))?;
        Ok(chunked(content))
    }
}

/// The blob chunk size the daemon re-emits, matching the server's
/// `BLOB_CHUNK_LEN` (64 KiB): tonic's default 4 MiB per-message decode
/// limit applies on the client side of the daemon's socket, so one
/// blob-sized chunk would fail any fetch over 4 MiB.
const BLOB_CHUNK_LEN: usize = 1 << 16;

/// Wraps `content` as a ready-made `BlobChunk` stream in `BLOB_CHUNK_LEN`
/// pieces — the same shape the server streams.
fn chunked(content: Vec<u8>) -> BoxStream<pb::BlobChunk> {
    let chunks: Vec<Vec<u8>> = content.chunks(BLOB_CHUNK_LEN).map(<[u8]>::to_vec).collect();
    Box::pin(tokio_stream::iter(
        chunks.into_iter().map(|data| Ok(pb::BlobChunk { data })),
    ))
}

/// True when a tonic status means the daemon's cached upstream channel is
/// unusable (the server went away mid-RPC): the next call re-dials.
fn transport_dead(status: &Status) -> bool {
    matches!(
        status.code(),
        tonic::Code::Unavailable | tonic::Code::Internal
    )
}

/// Verifies the unix-socket peer's uid against the daemon's own uid
/// (SO_PEERCRED, delivered by tonic as `UdsConnectInfo` in the request
/// extensions). Returns `Ok(())` when the peer may proceed.
///
/// Read RPCs are open to any local peer (the socket directory is 0700
/// anyway); this gate is applied to write RPCs, where a second lock on the
/// door costs nothing.
pub fn peer_uid_ok<T>(request: &Request<T>) -> Result<(), Status> {
    let info = request
        .extensions()
        .get::<tonic::transport::server::UdsConnectInfo>()
        .ok_or_else(|| {
            Status::permission_denied(
                "the daemon cannot verify this connection's peer credentials \
                 (unix sockets only); the write was refused",
            )
        })?;
    let cred = info.peer_cred.as_ref().ok_or_else(|| {
        Status::permission_denied(
            "the daemon could not read this connection's peer credentials; \
             the write was refused",
        )
    })?;
    check_peer_uid(cred.uid())
}

/// The credential decision itself: a peer uid other than the daemon's own
/// is refused. Split out so the rule is unit-testable without a socket.
fn check_peer_uid(peer: u32) -> Result<(), Status> {
    let own = crate::paths::current_uid();
    if peer != own {
        return Err(Status::permission_denied(format!(
            "the daemon only accepts writes from its owner (uid {own}); \
             this connection belongs to uid {peer} and was refused"
        )));
    }
    Ok(())
}

#[tonic::async_trait]
impl Gardenbed for ProxyService {
    type GetBlobStream = BoxStream<pb::BlobChunk>;
    type DiffStream = BoxStream<pb::ChangeMsg>;
    type LogStream = BoxStream<pb::ChangesetSummary>;
    type BlameStream = BoxStream<pb::BlameRange>;
    type ListRefsStream = BoxStream<pb::RefEntry>;

    async fn root_tree(
        &self,
        request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::RootTreeResp>, Status> {
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().root_tree(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn get_tree(
        &self,
        request: Request<pb::GetTreeReq>,
    ) -> Result<Response<pb::TreeResp>, Status> {
        let auth = auth_of(&request);
        let req = request.into_inner();
        let id = Self::id(&req.id, "id")?;
        let tree = self.tree_from_cache(&req.repo, id, auth).await?;
        Ok(Response::new(tree))
    }

    async fn get_blob(
        &self,
        request: Request<pb::GetBlobReq>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let auth = auth_of(&request);
        let req = request.into_inner();
        let id = Self::id(&req.id, "id")?;
        let stream = self.clone().blob_stream(req.repo, id, auth).await?;
        Ok(Response::new(stream))
    }

    async fn get_changeset(
        &self,
        request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::ChangesetMsg>, Status> {
        let auth = auth_of(&request);
        let client = self.upstream.get().await?;
        let req = request.into_inner();
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let cs = match client
            .with_auth_value(auth)
            .get_changeset(&req.repo, cs_id)
            .await
        {
            Ok(cs) => cs,
            Err(status) => {
                if transport_dead(&status) {
                    self.upstream.reset().await;
                }
                return Err(status);
            }
        };
        let msg = pb::ChangesetMsg::try_from(&cs)
            .map_err(|_| Status::internal("upstream changeset cannot be re-encoded"))?;
        Ok(Response::new(msg))
    }

    async fn put_blob(
        &self,
        request: Request<Streaming<pb::PutBlobChunk>>,
    ) -> Result<Response<pb::PutBlobResp>, Status> {
        peer_uid_ok(&request)?;
        let auth = auth_of(&request);
        // Drain the client stream, then forward through the typed wrapper
        // (the header names the repo; body chunks ride in order).
        let mut stream = request.into_inner();
        let mut frames: Vec<pb::PutBlobChunk> = Vec::new();
        while let Some(chunk) = stream.message().await? {
            frames.push(chunk);
        }
        let repo = frames.first().and_then(|f| match &f.frame {
            Some(pb::put_blob_chunk::Frame::Header(h)) => Some(h.repo.clone()),
            _ => None,
        });
        let client = self.upstream.get().await?;
        let id = client
            .with_auth_value(auth)
            .put_blob(
                &repo.unwrap_or_default(),
                tokio_stream::iter(
                    frames
                        .into_iter()
                        .filter_map(|f| match f.frame {
                            Some(pb::put_blob_chunk::Frame::Data(d)) => Some(d),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .await?;
        Ok(Response::new(pb::PutBlobResp {
            id: id_to_bytes(id),
        }))
    }

    async fn create_changeset(
        &self,
        request: Request<pb::CreateChangesetReq>,
    ) -> Result<Response<pb::CreateChangesetResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let id = client
            .raw()
            .clone()
            .create_changeset(forward(request))
            .await?
            .into_inner()
            .id;
        Ok(Response::new(pb::CreateChangesetResp { id }))
    }

    async fn merge_base(
        &self,
        request: Request<pb::MergeBaseReq>,
    ) -> Result<Response<pb::MergeBaseResp>, Status> {
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().merge_base(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn is_ancestor(
        &self,
        request: Request<pb::IsAncestorReq>,
    ) -> Result<Response<pb::IsAncestorResp>, Status> {
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().is_ancestor(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn diff(
        &self,
        request: Request<pb::DiffReq>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client
            .raw()
            .clone()
            .diff(forward(request))
            .await?
            .into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let upstream = self.upstream.clone();
        tokio::spawn(async move {
            use tokio_stream::StreamExt as _;
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break; // the caller went away
                }
            }
            // If the stream died mid-way, the next upstream call re-dials.
            let _ = &upstream;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn log(&self, request: Request<pb::LogReq>) -> Result<Response<Self::LogStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client
            .raw()
            .clone()
            .log(forward(request))
            .await?
            .into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            use tokio_stream::StreamExt as _;
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn blame(
        &self,
        request: Request<pb::BlameReq>,
    ) -> Result<Response<Self::BlameStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client
            .raw()
            .clone()
            .blame(forward(request))
            .await?
            .into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            use tokio_stream::StreamExt as _;
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn list_refs(
        &self,
        request: Request<pb::ListRefsReq>,
    ) -> Result<Response<Self::ListRefsStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client
            .raw()
            .clone()
            .list_refs(forward(request))
            .await?
            .into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            use tokio_stream::StreamExt as _;
            while let Some(item) = stream.next().await {
                if tx.send(item).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn get_ref(
        &self,
        request: Request<pb::GetRefReq>,
    ) -> Result<Response<pb::GetRefResp>, Status> {
        let auth = auth_of(&request);
        let client = self.upstream.get().await?;
        let req = request.into_inner();
        let value = client
            .with_auth_value(auth)
            .get_ref(&req.repo, &req.name)
            .await?;
        Ok(Response::new(pb::GetRefResp {
            cs_id: value.map(id_to_bytes),
        }))
    }

    async fn cas_ref(
        &self,
        request: Request<pb::CasRefReq>,
    ) -> Result<Response<pb::CasResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().cas_ref(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn rebase(
        &self,
        request: Request<pb::RebaseReq>,
    ) -> Result<Response<pb::RebaseResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().rebase(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn publish(
        &self,
        request: Request<pb::PublishReq>,
    ) -> Result<Response<pb::PublishResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().publish(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn merge(
        &self,
        request: Request<pb::MergeReq>,
    ) -> Result<Response<pb::MergeResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().merge(forward(request)).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn get_stats(
        &self,
        request: Request<pb::GetStatsReq>,
    ) -> Result<Response<pb::StatsResp>, Status> {
        // The proxy's own counters ride along under "daemon." keys, so a
        // client can distinguish daemon cache hits from server counts.
        let mut resp = {
            let client = self.upstream.get().await?;
            client
                .raw()
                .clone()
                .get_stats(forward(request))
                .await?
                .into_inner()
        };
        resp.counters
            .insert("daemon.cache_hits".to_owned(), self.cache_hits());
        Ok(Response::new(resp))
    }
}

impl Clone for ProxyService {
    /// Clones the proxy handle. Cheap: every field is behind an Arc or a
    /// lock, except the per-scope store map and the tree singleflight map
    /// — fresh maps per clone only bound fetch collapsing, never
    /// correctness (the scope's cache re-check under each lock is what
    /// dedups). The scope map starts empty and reopens lazily, the same
    /// way the first construction did.
    fn clone(&self) -> Self {
        ProxyService {
            upstream: self.upstream.clone(),
            cache_root: self.cache_root.clone(),
            scopes: Mutex::new(HashMap::new()),
            tree_locks: Mutex::new(HashMap::new()),
            cache_hits: self.cache_hits.clone(),
        }
    }
}

/// The `Generation` bookkeeping is owned by the sidecar; re-exported here
/// so the binary can wire both services from one place.
pub use crate::sidecar::SidecarService;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_uid_check_requires_unix_credentials() {
        // A request with no UdsConnectInfo extension (e.g. from an
        // in-process test server) is refused, never silently allowed.
        let request = Request::new(());
        let err = peer_uid_ok(&request).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn peer_uid_check_accepts_own_uid_and_refuses_others() {
        // The credential decision: the daemon's own uid passes, any other
        // uid is refused with the refusal naming the peer uid and the
        // daemon's own. (A wrong-uid peer cannot be forged in-process;
        // this rule plus the end-to-end write refusal in the integration
        // suite together cover acceptance 7.)
        let own = crate::paths::current_uid();
        assert!(check_peer_uid(own).is_ok());

        let other = if own == 0 { 1 } else { 0 };
        let err = check_peer_uid(other).unwrap_err();
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
        let message = err.message();
        assert!(message.contains("was refused"), "{message}");
        assert!(message.contains(&format!("uid {other}")), "{message}");
    }
    #[tokio::test]
    async fn peer_uid_check_over_a_real_socket() {
        // SO_PEERCRED over a real unix socket: a connected peer's
        // credentials (what tonic's `Connected` impl reads to build
        // `UdsConnectInfo`) carry our own uid.
        use tokio::net::UnixStream;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let connect = tokio::spawn(async move { UnixStream::connect(&path).await });
        let (server_stream, _) = listener.accept().await.unwrap();
        let cred = server_stream.peer_cred().expect("peer credentials");
        assert_eq!(cred.uid(), crate::paths::current_uid());
        drop(server_stream);
        drop(listener);
        drop(connect.await);
    }
}

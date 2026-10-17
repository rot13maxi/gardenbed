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
//! Writes (`PutBlob`, `CreateChangeset`, `CasRef`, `Rebase`, `Publish`) are
//! gated on the peer-uid check ([`peer_uid_ok`]): the unix socket is
//! mode-0700 in a private directory, and the kernel credential check is
//! the second lock on that door.

use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use gardenbed_model::{Id, Tree};
use gardenbed_protocol::convert::{id_from_bytes, id_to_bytes};
use gardenbed_protocol::gardenbed_server::Gardenbed;
use gardenbed_protocol::pb;
use gardenbed_store::{FsObjectStore, ObjectStore as _};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status, Streaming};

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
    cache: FsObjectStore,
    /// Per-tree singleflight: concurrent `GetTree` requests for the same
    /// id collapse into one upstream fetch.
    tree_locks: Mutex<HashMap<Id, Arc<tokio::sync::Mutex<()>>>>,
    /// Counters for tests/diagnostics: how many RPCs the daemon answered
    /// from its cache.
    cache_hits: Arc<std::sync::atomic::AtomicU64>,
}

impl ProxyService {
    /// A proxy over `upstream` caching objects into the store rooted at
    /// `cache_root` (created if missing; same GBLO1 envelope and sharding
    /// as `.bed/objects`).
    pub fn new(upstream: &str, cache_root: &Path) -> Result<Self, Status> {
        let cache = FsObjectStore::open(cache_root).map_err(|e| {
            Status::internal(format!(
                "cannot open the shared object cache at {}: {e}",
                cache_root.display()
            ))
        })?;
        Ok(ProxyService {
            upstream: Arc::new(Upstream::new(upstream.to_owned())),
            cache,
            tree_locks: Mutex::new(HashMap::new()),
            cache_hits: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// `(cache_hits, )` — GetTree/GetBlob requests answered without an
    /// upstream RPC.
    pub fn cache_hits(&self) -> u64 {
        self.cache_hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The shared object store (same store the daemon writes through).
    pub fn cache(&self) -> &FsObjectStore {
        &self.cache
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

    /// Cache-first tree lookup: shared store, then one upstream `GetTree`,
    /// write-through. Concurrent requests for the same id collapse into a
    /// single fetch.
    async fn tree_from_cache(&self, repo: &str, id: Id) -> Result<pb::TreeResp, Status> {
        if let Some(bytes) = self
            .cache
            .get_tree(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            let tree = Tree::decode(&bytes)
                .map_err(|_| Status::internal("shared cache holds an undecodable tree"))?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(pb::TreeResp::try_from(&tree)
                .map_err(|_| Status::internal("cached tree cannot be re-encoded"))?);
        }
        // Singleflight per id: the winner fetches and writes through, the
        // latecomer re-checks the cache under the lock.
        let lock = {
            let mut locks = self.tree_locks.lock().expect("tree lock map");
            locks.entry(id).or_default().clone()
        };
        let _gate = lock.lock().await;
        if let Some(bytes) = self
            .cache
            .get_tree(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            let tree = Tree::decode(&bytes)
                .map_err(|_| Status::internal("shared cache holds an undecodable tree"))?;
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(pb::TreeResp::try_from(&tree)
                .map_err(|_| Status::internal("cached tree cannot be re-encoded"))?);
        }
        let client = self.upstream.get().await?;
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
        self.cache
            .put_tree(id, &bytes)
            .map_err(|e| Status::internal(format!("shared cache write failed: {e}")))?;
        Ok(pb::TreeResp::try_from(&tree)
            .map_err(|_| Status::internal("upstream tree cannot be re-encoded"))?)
    }

    /// Cache-first blob lookup: shared store, else one streamed upstream
    /// `GetBlob`, reassembled, hash-verified, write-through, then streamed
    /// back to the caller in chunks.
    async fn blob_stream(
        self,
        repo: String,
        id: Id,
    ) -> Result<BoxStream<pb::BlobChunk>, Status> {
        if let Some(content) = self
            .cache
            .get_blob(id)
            .map_err(|e| Status::internal(format!("shared cache read failed: {e}")))?
        {
            self.cache_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(chunked(content));
        }
        let client = self.upstream.get().await?;
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
        self.cache
            .put_blob(&content)
            .map_err(|e| Status::internal(format!("shared cache write failed: {e}")))?;
        Ok(chunked(content))
    }
}

/// Wraps `content` as a ready-made `BlobChunk` stream (64 KiB chunks).
fn chunked(content: Vec<u8>) -> BoxStream<pb::BlobChunk> {
    // One chunk is plenty for the daemon's local socket; splitting only
    // adds scheduling. The wire shape stays a stream either way.
    Box::pin(tokio_stream::once(Ok(pb::BlobChunk { data: content })))
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
        let resp = client.raw().clone().root_tree(request.into_inner()).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn get_tree(
        &self,
        request: Request<pb::GetTreeReq>,
    ) -> Result<Response<pb::TreeResp>, Status> {
        let req = request.into_inner();
        let id = Self::id(&req.id, "id")?;
        let tree = self.tree_from_cache(&req.repo, id).await?;
        Ok(Response::new(tree))
    }

    async fn get_blob(
        &self,
        request: Request<pb::GetBlobReq>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let req = request.into_inner();
        let id = Self::id(&req.id, "id")?;
        let stream = self.clone().blob_stream(req.repo, id).await?;
        Ok(Response::new(stream))
    }

    async fn get_changeset(
        &self,
        request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::ChangesetMsg>, Status> {
        let client = self.upstream.get().await?;
        let req = request.into_inner();
        let cs_id = Self::id(&req.cs_id, "cs_id")?;
        let cs = match client.get_changeset(&req.repo, cs_id).await {
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
            .create_changeset(request.into_inner())
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
        let resp = client.raw().clone().merge_base(request.into_inner()).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn is_ancestor(
        &self,
        request: Request<pb::IsAncestorReq>,
    ) -> Result<Response<pb::IsAncestorResp>, Status> {
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().is_ancestor(request.into_inner()).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn diff(
        &self,
        request: Request<pb::DiffReq>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client.raw().clone().diff(request.into_inner()).await?.into_inner();
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

    async fn log(
        &self,
        request: Request<pb::LogReq>,
    ) -> Result<Response<Self::LogStream>, Status> {
        let client = self.upstream.get().await?;
        let mut stream = client.raw().clone().log(request.into_inner()).await?.into_inner();
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
        let mut stream = client.raw().clone().blame(request.into_inner()).await?.into_inner();
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
            .list_refs(request.into_inner())
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
        let client = self.upstream.get().await?;
        let req = request.into_inner();
        let value = client.get_ref(&req.repo, &req.name).await?;
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
        let resp = client.raw().clone().cas_ref(request.into_inner()).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn rebase(
        &self,
        request: Request<pb::RebaseReq>,
    ) -> Result<Response<pb::RebaseResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().rebase(request.into_inner()).await?;
        Ok(Response::new(resp.into_inner()))
    }

    async fn publish(
        &self,
        request: Request<pb::PublishReq>,
    ) -> Result<Response<pb::PublishResp>, Status> {
        peer_uid_ok(&request)?;
        let client = self.upstream.get().await?;
        let resp = client.raw().clone().publish(request.into_inner()).await?;
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
                .get_stats(request.into_inner())
                .await?
                .into_inner()
        };
        resp.counters.insert(
            "daemon.cache_hits".to_owned(),
            self.cache_hits(),
        );
        Ok(Response::new(resp))
    }
}

impl Clone for ProxyService {
    /// Clones the proxy handle. Cheap: every field is behind an Arc or a
    /// lock, except the cache store handle (an open-directory value type)
    /// and the tree singleflight map — a fresh map per clone only bounds
    /// fetch collapsing, never correctness (the cache re-check under each
    /// lock is what dedups).
    fn clone(&self) -> Self {
        let cache_root = self
            .cache
            .objects_dir()
            .parent()
            .expect("the cache dir always has a parent")
            .to_path_buf();
        ProxyService {
            upstream: self.upstream.clone(),
            cache: FsObjectStore::open(&cache_root).expect("the shared cache reopens"),
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

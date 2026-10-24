//! The ABA cross-principal cache-poisoning regression: a same-URL
//! forwarding layer (load balancer, reverse proxy) may answer the
//! daemon's identity probe (`RepoInfo`) from backend A while routing
//! the object fetch (`GetBlob`/`GetTree`) to backend B during a
//! failover/restore window — B holding a *replaced* repository under
//! the same name. Without binding the fetched bytes to the
//! incarnation that authorized the scope, B's objects land in A's
//! cache scope, and an A-only principal later reads them through A's
//! identity while the restored A answers NotFound.
//!
//! The topology here is all real: two `gardenbed-server` processes
//! with distinct histories under the repo name `alpha`, real token
//! policies (A: wildcard `admin` + A-only read `victim`; B: `admin`
//! only), and a typed forwarding proxy implementing the wire service
//! on ONE endpoint. The proxy's `RepoInfo` always answers from A; its
//! object fetches route by a shared window flag (open → B, closed →
//! A). Every reply is a real server's reply — the proxy only chooses
//! whom to ask, exactly what a misrouting load balancer does.
//!
//! The contract under test: the daemon's cache write-through requires
//! the serving repository's incarnation proof, so B's bytes (served to
//! the authorized `admin` during the window) never enter A's scope.
//! After the window closes and the daemon restarts over the same URL
//! and cache root, the A-only `victim` gets NotFound for the B-only
//! blob AND tree — matching the restored origin's own answer.

mod common;

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{Daemon, Server};
use gardenbed_harness::generator::{GenOptions, generate};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_protocol::gardenbed_client::GardenbedClient as RawClient;
use gardenbed_protocol::gardenbed_server::{Gardenbed, GardenbedServer};
use gardenbed_protocol::pb;
use tokio_stream::StreamExt as _;
use tonic::transport::Server as TonicServer;
use tonic::{Request, Response, Status};

/// The forwarding proxy: one wire endpoint in front of two backends.
/// `RepoInfo` (and the ListRefs authorization consult) always answers
/// from A; `GetBlob`/`GetTree` route by the shared window flag.
struct SplittingProxy {
    a: RawClient<tonic::transport::Channel>,
    b: RawClient<tonic::transport::Channel>,
    /// Open: object content comes from B (the privileged window).
    route_content_to_b: Arc<AtomicBool>,
}

/// Forwards the caller's credentials (the daemon's per-request
/// `authorization` value) so the REAL backend policy decides.
fn forward_auth<T: Clone>(src: &Request<T>) -> Request<T> {
    let mut out = Request::new(src.get_ref().clone());
    if let Some(value) = src.metadata().get("authorization") {
        let value = String::from_utf8(value.as_bytes().to_vec())
            .ok()
            .and_then(|s| tonic::metadata::MetadataValue::try_from(s.as_str()).ok());
        if let Some(value) = value {
            out.metadata_mut().insert("authorization", value);
        }
    }
    out
}

#[tonic::async_trait]
impl Gardenbed for SplittingProxy {
    async fn repo_info(
        &self,
        request: Request<pb::RepoInfoReq>,
    ) -> Result<Response<pb::RepoInfoResp>, Status> {
        // The identity probe ALWAYS answers from A: the daemon keys its
        // scope on this answer, and the window never touches it.
        let mut a = self.a.clone();
        Ok(a.repo_info(forward_auth(&request)).await?)
    }

    async fn get_tree(
        &self,
        request: Request<pb::GetTreeReq>,
    ) -> Result<Response<pb::TreeResp>, Status> {
        let mut backend = if self.route_content_to_b.load(Ordering::Relaxed) {
            self.b.clone()
        } else {
            self.a.clone()
        };
        Ok(backend.get_tree(forward_auth(&request)).await?)
    }

    type GetBlobStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::BlobChunk, Status>> + Send>>;

    async fn get_blob(
        &self,
        request: Request<pb::GetBlobReq>,
    ) -> Result<Response<Self::GetBlobStream>, Status> {
        let mut backend = if self.route_content_to_b.load(Ordering::Relaxed) {
            self.b.clone()
        } else {
            self.a.clone()
        };
        let stream = backend.get_blob(forward_auth(&request)).await?.into_inner();
        // `Streaming` is already a sendable stream: forward it as-is.
        Ok(Response::new(Box::pin(stream)))
    }

    type ListRefsStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::RefEntry, Status>> + Send>>;

    async fn list_refs(
        &self,
        request: Request<pb::ListRefsReq>,
    ) -> Result<Response<Self::ListRefsStream>, Status> {
        // The authorization consult always answers from A: the victim IS
        // read-authorized on A's repo — that is what makes any leak
        // cross-principal rather than an auth failure.
        let mut a = self.a.clone();
        let stream = a.list_refs(forward_auth(&request)).await?.into_inner();
        // `Streaming` is already a sendable stream: forward it as-is.
        Ok(Response::new(Box::pin(stream)))
    }

    /// The daemon only ever calls the four methods above against its
    /// upstream (identity, trees, blobs, refs); the rest of the
    /// surface fails closed.
    async fn root_tree(
        &self,
        _request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::RootTreeResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn get_changeset(
        &self,
        _request: Request<pb::GetChangesetReq>,
    ) -> Result<Response<pb::ChangesetMsg>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn put_blob(
        &self,
        _request: Request<tonic::Streaming<pb::PutBlobChunk>>,
    ) -> Result<Response<pb::PutBlobResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn create_changeset(
        &self,
        _request: Request<pb::CreateChangesetReq>,
    ) -> Result<Response<pb::CreateChangesetResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn merge_base(
        &self,
        _request: Request<pb::MergeBaseReq>,
    ) -> Result<Response<pb::MergeBaseResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn is_ancestor(
        &self,
        _request: Request<pb::IsAncestorReq>,
    ) -> Result<Response<pb::IsAncestorResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    type DiffStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::ChangeMsg, Status>> + Send>>;

    async fn diff(
        &self,
        _request: Request<pb::DiffReq>,
    ) -> Result<Response<Self::DiffStream>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    type LogStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = Result<pb::ChangesetSummary, Status>> + Send>,
    >;

    async fn log(
        &self,
        _request: Request<pb::LogReq>,
    ) -> Result<Response<Self::LogStream>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    type BlameStream =
        std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<pb::BlameRange, Status>> + Send>>;

    async fn blame(
        &self,
        _request: Request<pb::BlameReq>,
    ) -> Result<Response<Self::BlameStream>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn get_ref(
        &self,
        _request: Request<pb::GetRefReq>,
    ) -> Result<Response<pb::GetRefResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn cas_ref(
        &self,
        _request: Request<pb::CasRefReq>,
    ) -> Result<Response<pb::CasResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn rebase(
        &self,
        _request: Request<pb::RebaseReq>,
    ) -> Result<Response<pb::RebaseResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn publish(
        &self,
        _request: Request<pb::PublishReq>,
    ) -> Result<Response<pb::PublishResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn merge(
        &self,
        _request: Request<pb::MergeReq>,
    ) -> Result<Response<pb::MergeResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }

    async fn get_stats(
        &self,
        _request: Request<pb::GetStatsReq>,
    ) -> Result<Response<pb::StatsResp>, Status> {
        Err(Status::unimplemented("not routed by this proxy"))
    }
}

#[tokio::test]
async fn daemon_aba_same_url_content_poisoning_is_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");

    // REAL auth policies: A accepts the wildcard admin and the A-only
    // read victim; B accepts the admin only. All reads the daemon
    // performs run under one of these real policies.
    let auth_a = dir.path().join("auth-a.toml");
    fs::write(
        &auth_a,
        r#"
            [tokens."admin"]
            repos = ["*"]
            role = "admin"

            [tokens."victim"]
            repos = ["alpha"]
            role = "read"
        "#,
    )
    .expect("auth-a written");
    let auth_b = dir.path().join("auth-b.toml");
    fs::write(
        &auth_b,
        r#"
            [tokens."admin"]
            repos = ["*"]
            role = "admin"
        "#,
    )
    .expect("auth-b written");

    // REAL repositories with distinct histories under one name.
    let history_a = generate(31, &GenOptions::default());
    common::ingest(&history_a, &root.join("alpha"));
    let root_b = dir.path().join("root-b");
    fs::create_dir_all(&root_b).expect("root-b dir");
    let history_b = generate(77, &GenOptions::default());
    common::ingest(&history_b, &root_b.join("alpha"));

    let mut server_a = Server::spawn_with_auth(&root, &auth_a).await;
    let mut server_b = Server::spawn_with_auth(&root_b, &auth_b).await;

    // The B-only objects: present in B's history, absent from A's
    // (the epochs' contents differ, so their content addresses differ).
    let head_b = history_b.changesets.len() - 1;
    let files_b = gardenbed_harness::generator::files_at(&history_b, head_b);
    let (b_only_blob, _) = *files_b
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("B's head has a regular file");
    let repo_b = gardenbed_server::Repo::open(&root_b.join("alpha")).expect("B's alpha opens");
    let b_only_tree = repo_b
        .root_tree(history_b.head)
        .expect("B head tree")
        .expect("B head has files");

    // The typed forwarding proxy on ONE endpoint, in front of both.
    let (proxy_ready_tx, proxy_ready_rx) = tokio::sync::oneshot::channel::<String>();
    let route_content_to_b = Arc::new(AtomicBool::new(false));
    let window = Arc::clone(&route_content_to_b);
    let a_url = server_a.http();
    let b_url = server_b.http();
    let proxy_task = tokio::spawn(async move {
        let proxy = SplittingProxy {
            a: RawClient::connect(a_url).await.expect("dial A"),
            b: RawClient::connect(b_url).await.expect("dial B"),
            route_content_to_b: window,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy binds");
        let addr = listener.local_addr().expect("proxy local addr");
        let _ = proxy_ready_tx.send(format!("http://{addr}"));
        TonicServer::builder()
            .add_service(GardenbedServer::new(proxy))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("proxy serves");
    });

    // Daemon over the ONE proxy URL.
    let proxy_url = proxy_ready_rx.await.expect("proxy endpoint");
    let mut daemon = Daemon::spawn(&daemon_root, &proxy_url).await;

    // The privileged window: identity still answers from A (the proxy
    // is unchanged), but object content routes to B. The admin —
    // authorized by BOTH real policies — fetches B-only objects; the
    // daemon serves them (authorized) but must NOT write them into
    // A's scope: the responses carry B's incarnation, not A's.
    route_content_to_b.store(true, Ordering::Relaxed);
    let admin = GardenbedClient::connect(&format!("unix://{}", daemon.socket.display()))
        .await
        .expect("admin connects")
        .with_token("admin");
    let mut stream = admin
        .get_blob("alpha", b_only_blob)
        .await
        .expect("B's blob serves to the admin during the window");
    let mut served = Vec::new();
    while let Some(chunk) = stream.next().await {
        served.extend_from_slice(&chunk.expect("B blob chunk").data);
    }
    assert_eq!(
        served,
        history_b.blob(&b_only_blob).expect("B blob content"),
        "B's real bytes serve to the authorized admin"
    );
    admin
        .get_tree("alpha", b_only_tree)
        .await
        .expect("B's tree serves to the admin during the window");

    // Window closed: content routes back to A. The daemon RESTARTS
    // over the same proxy URL and the same cache root — if B's bytes
    // had been written through under A's scope, they would persist.
    route_content_to_b.store(false, Ordering::Relaxed);
    daemon.kill().await;
    let mut daemon2 = Daemon::spawn(&daemon_root, &proxy_url).await;

    // Ground truth, direct: the restored A answers NotFound for the
    // B-only objects to BOTH principals.
    let direct = GardenbedClient::connect(&server_a.http())
        .await
        .expect("direct A client");
    for token in ["admin", "victim"] {
        let direct = direct.clone().with_token(token);
        let err = direct
            .get_blob("alpha", b_only_blob)
            .await
            .expect_err("A answers NotFound for B's blob");
        assert_eq!(err.code(), tonic::Code::NotFound);
        let err = direct
            .get_tree("alpha", b_only_tree)
            .await
            .expect_err("A answers NotFound for B's tree");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // THE assertion: the A-only victim (read-authorized by A's real
    // policy) asks through the restarted daemon for the B-only blob
    // and tree. NotFound — never the poisoned bytes.
    let victim = GardenbedClient::connect(&format!("unix://{}", daemon2.socket.display()))
        .await
        .expect("victim connects")
        .with_token("victim");
    let err = victim
        .get_blob("alpha", b_only_blob)
        .await
        .expect_err("the B-only blob must not be readable through A's scope");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "victim gets the origin's NotFound for B's blob: {err}"
    );
    let err = victim
        .get_tree("alpha", b_only_tree)
        .await
        .expect_err("the B-only tree must not be readable through A's scope");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "victim gets the origin's NotFound for B's tree: {err}"
    );

    // Control: A's OWN content still serves through the same daemon
    // (the scope warms normally on proven fetches).
    let head_a = history_a.changesets.len() - 1;
    let files_a = gardenbed_harness::generator::files_at(&history_a, head_a);
    let (a_blob, _) = *files_a
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("A's head has a regular file");
    let mut stream = victim
        .get_blob("alpha", a_blob)
        .await
        .expect("A's own blob serves through the daemon");
    let mut served = Vec::new();
    while let Some(chunk) = stream.next().await {
        served.extend_from_slice(&chunk.expect("A blob chunk").data);
    }
    assert_eq!(served, history_a.blob(&a_blob).expect("A blob content"));

    daemon2.kill().await;
    server_a.shutdown().await;
    server_b.shutdown().await;
    proxy_task.abort();
}

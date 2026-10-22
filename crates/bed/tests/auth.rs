//! Token auth, end to end through the real `bed` binary: with
//! `GARDENBED_TOKEN` set, the full clone → sparse → status → commit →
//! publish flow works against an authed server; without it (or with a
//! wrong token) the first RPC fails with the actionable denial, exit 1.
//! The daemon path forwards the token unchanged (bed → daemon → server).

mod common;

use std::fs;

use gardenbed_harness::generator::{GenOptions, generate};
use tempfile::TempDir;

use common::{
    Daemon, Server, bed_direct, bed_via_daemon, bed_via_daemon_with_token, bed_with_token, ingest,
};

/// Writes an auth.toml granting `token` write on every repo (`*` is the
/// wildcard), returning its path.
fn auth_file(dir: &TempDir, token: &str) -> std::path::PathBuf {
    let path = dir.path().join("auth.toml");
    fs::write(
        &path,
        format!("[tokens.\"{token}\"]\nrepos = [\"*\"]\nrole = \"write\"\n"),
    )
    .expect("auth.toml written");
    path
}

/// bed with GARDENBED_TOKEN set runs the full flow against an authed
/// server: clone, sparse, status, commit, publish — all exit 0.
#[tokio::test]
async fn bed_with_token_full_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let auth_path = auth_file(&dir, "integration-token");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let token = "integration-token";

    let (code, out, err) = bed_with_token(&garden, &["clone", &server.http()], token);
    assert_eq!(code, 0, "clone: {err}\n{out}");
    let (code, out, err) = bed_with_token(&garden, &["sparse", "add", "."], token);
    assert_eq!(code, 0, "sparse: {err}\n{out}");
    let (code, out, err) = bed_with_token(&garden, &["status"], token);
    assert_eq!(code, 0, "status: {err}\n{out}");

    fs::write(garden.join("auth-note.md"), b"token auth works\n").expect("write");
    let (code, out, err) = bed_with_token(
        &garden,
        &[
            "commit",
            "-m",
            "document token auth",
            "--author",
            "A. Gardener <a@bed.example>",
        ],
        token,
    );
    assert_eq!(code, 0, "commit: {err}\n{out}");
    let (code, out, err) = bed_with_token(&garden, &["publish"], token);
    assert_eq!(code, 0, "publish: {err}\n{out}");

    server.shutdown().await;
}

/// bed without GARDENBED_TOKEN against the same authed server fails on
/// the first RPC with the actionable error and exit code 1.
#[tokio::test]
async fn bed_without_token_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let auth_path = auth_file(&dir, "integration-token");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");

    // The daemon is not involved in this path; keep it off so the denial
    // comes from the server itself.
    let (code, out, err) = bed_direct(&garden, &["clone", &server.http()]);
    assert_eq!(code, 1, "clone exits 1: {err}\n{out}");
    assert!(
        err.contains("requires a token"),
        "stderr carries the actionable hint: {err}"
    );
    assert!(
        err.contains("GARDENBED_TOKEN"),
        "stderr names the next step: {err}"
    );

    server.shutdown().await;
}

/// A wrong token is denied with the token-role message, exit 1.
#[tokio::test]
async fn bed_with_wrong_token_denied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let auth_path = auth_file(&dir, "integration-token");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");

    let (code, out, err) = bed_with_token(&garden, &["clone", &server.http()], "not-the-token");
    assert_eq!(code, 1, "clone exits 1: {err}\n{out}");
    assert!(
        err.contains("denied for this token"),
        "stderr names the failure: {err}"
    );

    server.shutdown().await;
}

/// The token survives the daemon hop: bed → daemon → authed server.
#[tokio::test]
async fn token_works_through_daemon() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let auth_path = auth_file(&dir, "integration-token");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let token = "integration-token";

    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &garden, &["clone", &server.http()], token);
    assert_eq!(code, 0, "clone through daemon: {err}\n{out}");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &garden, &["sparse", "add", "."], token);
    assert_eq!(code, 0, "sparse through daemon: {err}\n{out}");

    // Without the token, the same daemon path is denied end to end: the
    // daemon forwards the (absent) credentials and the server refuses.
    let other = dir.path().join("other");
    fs::create_dir_all(&other).expect("other dir");
    let (code, out, err) = bed_via_daemon(&daemon, &other, &["clone", &server.http()]);
    assert_eq!(code, 1, "clone without token exits 1: {err}\n{out}");
    assert!(
        err.contains("requires a token"),
        "stderr carries the server's anonymous denial: {err}"
    );

    daemon.kill().await;
    server.shutdown().await;
}

/// The cache must not become an auth bypass: once a tokened clone warms
/// the daemon's shared cache, an anonymous reader through the same
/// daemon is still denied (the daemon consults the server before
/// serving a hit), while the tokened reader keeps working.
#[tokio::test]
async fn daemon_cache_hit_still_requires_token() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let auth_path = auth_file(&dir, "integration-token");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    // Warm the cache: a full tokened clone pulls trees and blobs
    // through the daemon into its shared store.
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let token = "integration-token";
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &garden, &["clone", &server.http()], token);
    assert_eq!(code, 0, "warm clone: {err}\n{out}");

    // The reviewer's scenario: anonymous GetBlob/GetTree on the warm
    // cache must be denied — direct to the server it is, so through the
    // daemon it must be too. `sparse add .` materializes the full tree,
    // i.e. exercises GetTree + GetBlob for every object, all cache hits.
    let anon = dir.path().join("anon");
    fs::create_dir_all(&anon).expect("anon dir");
    let (code, out, err) = bed_via_daemon(&daemon, &anon, &["clone", &server.http()]);
    assert_eq!(code, 1, "anonymous clone exits 1: {err}\n{out}");
    assert!(
        err.contains("requires a token") || err.contains("denied"),
        "stderr carries the server's denial, not cached bytes: {err}"
    );

    // The tokened reader keeps working on the same warm cache.
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &garden, &["sparse", "add", "."], token);
    assert_eq!(code, 0, "tokened sparse after warm: {err}\n{out}");

    daemon.kill().await;
    server.shutdown().await;
}

/// The cache must not cross repo scopes: a token scoped to repo
/// `elsewhere` cannot read repo `default`'s objects through the daemon
/// even when they are already cached (fetched by a wildcard token).
#[tokio::test]
async fn daemon_cache_hit_respects_repo_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    // Two tokens: one wildcard reader (warms the cache), one scoped to
    // a repo that does not exist on this server.
    let auth_path = dir.path().join("auth.toml");
    fs::write(
        &auth_path,
        "[tokens.\"warm-token\"]\nrepos = [\"*\"]\nrole = \"write\"\n\
         [tokens.\"scoped-token\"]\nrepos = [\"elsewhere\"]\nrole = \"read\"\n",
    )
    .expect("auth.toml written");
    let mut server = Server::spawn_with_auth(&server_root, &auth_path).await;
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    // Warm the cache with the wildcard token.
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &garden, &["clone", &server.http()], "warm-token");
    assert_eq!(code, 0, "warm clone: {err}\n{out}");

    // The scoped token, denied by the server on repo `default`, is
    // denied through the warm daemon too — the cache serves nothing.
    let scoped = dir.path().join("scoped");
    fs::create_dir_all(&scoped).expect("scoped dir");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &scoped, &["clone", &server.http()], "scoped-token");
    assert_eq!(code, 1, "scoped clone exits 1: {err}\n{out}");
    assert!(
        err.contains("denied for this token"),
        "stderr names the repo-scope denial: {err}"
    );

    daemon.kill().await;
    server.shutdown().await;
}

/// A blob larger than tonic's 4 MiB per-message decode limit flows
/// through the daemon: the proxy re-chunks at 64 KiB like the server.
#[tokio::test]
async fn daemon_streams_blob_over_4mib() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server_root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");
    let history = generate(7, &GenOptions::default());
    ingest(&history, &server_root);
    let mut server = Server::spawn(&server_root).await;
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    // Clone, then add a 5 MiB file and publish it so the server holds
    // a blob the daemon cannot emit as a single message.
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let (code, out, err) = bed_via_daemon(&daemon, &garden, &["clone", &server.http()]);
    assert_eq!(code, 0, "clone: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon, &garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse: {err}\n{out}");
    let mut big = vec![0u8; 5 * 1024 * 1024];
    // A pattern that survives compression and position checks.
    for (i, b) in big.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    fs::write(garden.join("big.bin"), &big).expect("big file written");
    let (code, out, err) = bed_via_daemon(
        &daemon,
        &garden,
        &[
            "commit",
            "-m",
            "big blob",
            "--author",
            "A. Gardener <a@bed.example>",
        ],
    );
    assert_eq!(code, 0, "commit big: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon, &garden, &["publish"]);
    assert_eq!(code, 0, "publish big: {err}\n{out}");

    // A fresh working copy materializes it through the daemon — the
    // 5 MiB blob is fetched, cached, and re-emitted in 64 KiB chunks.
    let fresh = dir.path().join("fresh");
    fs::create_dir_all(&fresh).expect("fresh dir");
    let (code, out, err) = bed_via_daemon(&daemon, &fresh, &["clone", &server.http()]);
    assert_eq!(code, 0, "clone with big blob: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon, &fresh, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse materializes big.bin: {err}\n{out}");
    let got = fs::read(fresh.join("big.bin")).expect("big.bin materialized");
    assert_eq!(got.len(), big.len(), "big.bin size round-trips");
    assert_eq!(got, big, "big.bin content round-trips");

    daemon.kill().await;
    server.shutdown().await;
}

/// The daemon's cache must not leak objects across repositories. This is
/// the cross-repository half of [`daemon_cache_hit_respects_repo_scope`]:
/// that test only exercises a token denied on the *same* repo (a
/// repo-scope denial), which the daemon's `ListRefs` probe already
/// catches. Here both repos exist on the same server and the token is
/// authorized for the asked repo — only the *object* is missing from it:
///
/// 1. repo `alpha` holds object X; repo `beta` (same server, different
///    content) does not.
/// 2. a wildcard token warms the daemon's shared cache with alpha's
///    objects (trees and blobs).
/// 3. a beta-scoped token asks `GetBlob`/`GetTree` for alpha's X with
///    `repo=beta`: `ListRefs(beta)` authorizes (the token is valid for
///    beta), but the origin read is NotFound — beta does not have X.
///
/// The daemon must return NotFound, not alpha's cached bytes: the cache
/// is keyed by content id alone, so without a repo membership record it
/// would hand beta a blob it never stored (a cross-repo object leak).
/// Asserted per hit type (blob, then tree) through a raw client on the
/// daemon's socket, so the exact RPC — not a whole `bed clone` — is the
/// unit under test.
#[tokio::test]
async fn daemon_cache_does_not_leak_objects_across_repos() {
    use gardenbed_protocol::client::GardenbedClient;
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");

    // Two repositories with disjoint content: alpha's blob bytes (and
    // therefore tree ids) are nothing like beta's, so "beta has alpha's
    // object" can only ever be true through the daemon's shared cache.
    let alpha_history = generate(7, &GenOptions::default());
    let beta_history = generate(9, &GenOptions::default());
    ingest(&alpha_history, &root.join("alpha"));
    ingest(&beta_history, &root.join("beta"));

    // Warm token: wildcard (may fetch alpha). Beta token: read on beta
    // only — authorized for the *asked* repo, denied on alpha.
    let auth_path = dir.path().join("auth.toml");
    fs::write(
        &auth_path,
        "[tokens.\"warm-token\"]\nrepos = [\"*\"]\nrole = \"write\"\n\
         [tokens.\"beta-token\"]\nrepos = [\"beta\"]\nrole = \"read\"\n",
    )
    .expect("auth.toml written");
    let mut server = Server::spawn_with_auth(&root, &auth_path).await;
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    // Warm the shared cache with alpha's trees and blobs via a real
    // tokened clone + sparse materialization through the daemon.
    let alpha_wc = dir.path().join("alpha-wc");
    fs::create_dir_all(&alpha_wc).expect("alpha working-copy dir");
    let (code, out, err) = bed_via_daemon_with_token(
        &daemon,
        &alpha_wc,
        &["clone", &format!("{}/alpha", server.http())],
        "warm-token",
    );
    assert_eq!(code, 0, "alpha clone warms the cache: {err}\n{out}");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &alpha_wc, &["sparse", "add", "."], "warm-token");
    assert_eq!(code, 0, "alpha sparse materializes: {err}\n{out}");

    // Pick one of alpha's blobs and one of its trees from the head
    // changeset — both are now in the daemon's shared cache.
    let head = alpha_history.changesets.len() - 1;
    let files = gardenbed_harness::generator::files_at(&alpha_history, head);
    let (blob_id, _) = *files
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("alpha's head has a regular file");

    let alpha_repo = gardenbed_server::Repo::open(&root.join("alpha")).expect("alpha repo opens");
    let root_tree = alpha_repo
        .root_tree(alpha_history.head)
        .expect("alpha head tree")
        .expect("alpha head has files");

    // The beta-token client, straight onto the daemon's socket: asking
    // for alpha's blob under repo `beta` must NOT return alpha's bytes.
    let daemon_client = GardenbedClient::connect(&format!("unix://{}", daemon.socket.display()))
        .await
        .expect("client connects to the daemon socket")
        .with_token("beta-token");

    // Blob leak check: alpha's blob id, repo beta.
    let err = daemon_client
        .get_blob("beta", blob_id)
        .await
        .expect_err("alpha's blob is not beta's; the daemon must not serve it");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the daemon answers NotFound, not cached cross-repo bytes: {err}"
    );

    // Tree leak check: alpha's root tree id, repo beta.
    let err = daemon_client
        .get_tree("beta", root_tree)
        .await
        .expect_err("alpha's tree is not beta's; the daemon must not serve it");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the daemon answers NotFound, not cached cross-repo bytes: {err}"
    );

    // Control: the same ids ARE servable for repo alpha through the same
    // daemon (a beta denial must not have poisoned the alpha entries).
    let warm_client = GardenbedClient::connect(&format!("unix://{}", daemon.socket.display()))
        .await
        .expect("client reconnects to the daemon socket")
        .with_token("warm-token");
    let mut blob_stream = warm_client
        .get_blob("alpha", blob_id)
        .await
        .expect("alpha's own blob still serves through the daemon");
    let mut served = Vec::new();
    while let Some(chunk) = blob_stream.next().await {
        served.extend_from_slice(&chunk.expect("alpha blob chunk").data);
    }
    assert_eq!(
        served,
        alpha_history.blob(&blob_id).expect("alpha blob content"),
        "alpha's blob round-trips for alpha"
    );
    warm_client
        .get_tree("alpha", root_tree)
        .await
        .expect("alpha's own tree still serves through the daemon");

    daemon.kill().await;
    server.shutdown().await;
}

/// The scoped cache survives a daemon restart: killing the daemon and
/// starting a fresh one over the same cache root (and the same upstream
/// endpoint — here a fixed bind address, standing in for the same server
/// coming back) serves the previously warmed objects with **zero** new
/// server-side object fetches. The scope key is a digest over
/// (upstream endpoint, repo), so the same pair lands in the same scope.
#[tokio::test]
async fn daemon_scoped_cache_persists_across_restart() {
    // One history, one repo; a fixed server address so the restarted
    // daemon computes the same scope key.
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");
    let cache_home = daemon_root.join("xdg-cache");
    let history = generate(5, &GenOptions::default());
    ingest(&history, &root.join("alpha"));
    let mut server = Server::spawn_at_addr(&root, "127.0.0.1:39711").await;

    // Warm through daemon #1.
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;
    let wc = dir.path().join("wc");
    fs::create_dir_all(&wc).expect("wc dir");
    let (code, out, err) = bed_via_daemon(
        &daemon,
        &wc,
        &["clone", &format!("{}/alpha", server.http())],
    );
    assert_eq!(code, 0, "clone through daemon #1: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon, &wc, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse through daemon #1: {err}\n{out}");

    let client = server.client().await;
    let (trees_before, blobs_before) = server.counted_for_repo(&client, "alpha").await;

    // Kill daemon #1, restart over the same cache root + same endpoint.
    daemon.kill().await;
    let mut daemon2 = Daemon::spawn_with_socket(
        &daemon_root.join("daemon2.sock"),
        &cache_home,
        &server.http(),
    )
    .await;

    // The same content re-read through daemon #2 is served from the
    // persisted scope store: the server counters must not move.
    let wc2 = dir.path().join("wc2");
    fs::create_dir_all(&wc2).expect("wc2 dir");
    let (code, out, err) = bed_via_daemon(
        &daemon2,
        &wc2,
        &["clone", &format!("{}/alpha", server.http())],
    );
    assert_eq!(code, 0, "clone through daemon #2: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon2, &wc2, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse through daemon #2: {err}\n{out}");

    let (trees_after, blobs_after) = server.counted_for_repo(&client, "alpha").await;
    assert_eq!(
        (trees_before, blobs_before),
        (trees_after, blobs_after),
        "zero new server fetches after restart \
         (before {trees_before}/{blobs_before}, after {trees_after}/{blobs_after})"
    );

    daemon2.kill().await;
    server.shutdown().await;
}

/// A cache root reused for a *different* upstream endpoint starts cold:
/// the scope key includes the endpoint, so objects warmed under
/// upstream A are never served to a daemon fronting upstream B — even
/// for the same repo name and a token that is authorized there. The
/// asking client gets upstream B's own answer for content it lacks.
#[tokio::test]
async fn daemon_scoped_cache_isolates_a_reused_cache_root_from_other_upstreams() {
    use gardenbed_protocol::client::GardenbedClient;
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let daemon_root = dir.path().join("daemon");
    let cache_home = daemon_root.join("xdg-cache");

    // Upstream A (fixed addr) with repo `alpha` holding object X.
    let root_a = dir.path().join("server-a");
    let history_a = generate(5, &GenOptions::default());
    ingest(&history_a, &root_a.join("alpha"));
    let mut server_a = Server::spawn_at_addr(&root_a, "127.0.0.1:39713").await;

    // Upstream B with a repo of the *same name* `alpha` and totally
    // different content (it must not contain A's object X).
    let root_b = dir.path().join("server-b");
    let history_b = generate(9, &GenOptions::default());
    ingest(&history_b, &root_b.join("alpha"));
    let mut server_b = Server::spawn(&root_b).await;

    // Warm the shared cache root through a daemon on upstream A.
    let mut daemon_a = Daemon::spawn(&daemon_root, &server_a.http()).await;
    let wc = dir.path().join("wc");
    fs::create_dir_all(&wc).expect("wc dir");
    let (code, out, err) = bed_via_daemon(
        &daemon_a,
        &wc,
        &["clone", &format!("{}/alpha", server_a.http())],
    );
    assert_eq!(code, 0, "clone through the upstream-A daemon: {err}\n{out}");
    let (code, out, err) = bed_via_daemon(&daemon_a, &wc, &["sparse", "add", "."]);
    assert_eq!(
        code, 0,
        "sparse through the upstream-A daemon: {err}\n{out}"
    );
    daemon_a.kill().await;

    // Pick one of A's blobs — now warm in the cache root.
    let head = history_a.changesets.len() - 1;
    let files = gardenbed_harness::generator::files_at(&history_a, head);
    let (blob_id, _) = *files
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("A's head has a regular file");

    // A new daemon over the SAME cache root but fronting upstream B:
    // same repo name, so only the endpoint differs.
    let mut daemon_b = Daemon::spawn_with_socket(
        &daemon_root.join("daemon-b.sock"),
        &cache_home,
        &server_b.http(),
    )
    .await;
    let client_b = GardenbedClient::connect(&format!("unix://{}", daemon_b.socket.display()))
        .await
        .expect("client connects to the upstream-B daemon socket");

    // B's repo lacks A's object: the daemon must NOT serve the warm
    // cross-origin bytes; it answers with B's own NotFound.
    let err = client_b
        .get_blob("alpha", blob_id)
        .await
        .expect_err("A's blob is not in B's repo; the daemon must not serve it");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the daemon answers B's NotFound, not A's cached bytes: {err}"
    );

    // And B's own content still serves through that daemon.
    let head_b = history_b.changesets.len() - 1;
    let files_b = gardenbed_harness::generator::files_at(&history_b, head_b);
    let (blob_b, _) = *files_b
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("B's head has a regular file");
    let mut stream = client_b
        .get_blob("alpha", blob_b)
        .await
        .expect("B's own blob serves through the upstream-B daemon");
    let mut served = Vec::new();
    while let Some(chunk) = stream.next().await {
        served.extend_from_slice(&chunk.expect("B blob chunk").data);
    }
    assert_eq!(
        served,
        history_b.blob(&blob_b).expect("B blob content"),
        "B's blob round-trips for B"
    );

    daemon_b.kill().await;
    server_b.shutdown().await;
    server_a.shutdown().await;
}

/// Same-origin, same-name repository **replacement** must not inherit
/// the predecessor's cached objects. Supported operations can put a
/// different history under the same `(endpoint, repo)` name — delete
/// the repo directory and ingest a new history under the same name. The
/// replacement repository has a **new metadata database**, so the
/// server mints it a fresh incarnation identity, and the daemon's scope
/// key (endpoint, repo, incarnation) lands in a cold scope: a caller
/// authorized only for the replacement repository gets the replacement
/// origin's own NotFound for the predecessor's objects — never the
/// predecessor's warm bytes (unlike a working copy's own local cache,
/// the daemon's caller never possessed those bytes; serving them would
/// be a cross-principal leak).
#[tokio::test]
async fn daemon_scoped_cache_denies_replacement_repo_the_prior_incarnation_objects() {
    use gardenbed_protocol::client::GardenbedClient;
    use tokio_stream::StreamExt as _;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");

    // Tokened fixture: `first-token` is authorized for the FIRST
    // incarnation's content; `replacement-token` for the replacement's
    // (same repo name — different principals is the leak scenario).
    // A raw string keeps the TOML literal: no escape-decoding surprises.
    let auth_path = dir.path().join("auth.toml");
    fs::write(
        &auth_path,
        r#"
            [tokens."first-token"]
            repos = ["*"]
            role = "write"

            [tokens."replacement-token"]
            repos = ["*"]
            role = "read"
        "#,
    )
    .expect("auth.toml written");

    let history_first = generate(5, &GenOptions::default());
    ingest(&history_first, &root.join("alpha"));
    // First server: ephemeral port; the ACTUAL bound address is reused
    // for the replacement so the daemon sees one stable endpoint.
    let mut server = Server::spawn_with_auth(&root, &auth_path).await;
    let addr = server.addr.clone();
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    // Warm the daemon's cache with the FIRST incarnation's objects,
    // fetched by the first principal.
    let wc = dir.path().join("wc");
    fs::create_dir_all(&wc).expect("wc dir");
    let (code, out, err) = bed_via_daemon_with_token(
        &daemon,
        &wc,
        &["clone", &format!("{}/alpha", server.http())],
        "first-token",
    );
    assert_eq!(code, 0, "clone of the first history: {err}\n{out}");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &wc, &["sparse", "add", "."], "first-token");
    assert_eq!(code, 0, "sparse of the first history: {err}\n{out}");

    let head = history_first.changesets.len() - 1;
    let files = gardenbed_harness::generator::files_at(&history_first, head);
    let (blob_id, _) = *files
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("the first history's head has a regular file");
    let first_repo = gardenbed_server::Repo::open(&root.join("alpha")).expect("alpha repo opens");
    let root_tree = first_repo
        .root_tree(history_first.head)
        .expect("alpha head tree")
        .expect("alpha head has files");

    // Replace the repository BEHIND THE LIVE DAEMON: stop only the
    // server, delete the repo dir, ingest a different history under the
    // same name, and bring the replacement origin back on the SAME
    // endpoint (the saved address). The daemon — with its warm in-memory
    // scope map and cached upstream channel — must resolve a FRESH
    // identity on the next request (per-request RepoInfo), reconnect,
    // and answer from the cold replacement scope. (The restarted-daemon
    // variant of this scenario is the restart-persistence family;
    // keeping the daemon alive is the harder case: stale in-memory
    // state must not serve the prior incarnation.)
    server.shutdown().await;
    fs::remove_dir_all(root.join("alpha")).expect("the old repo dir is removed");
    let history_second = generate(11, &GenOptions::default());
    ingest(&history_second, &root.join("alpha"));
    let mut server2 = Server::spawn_with_auth_at_addr(&root, &auth_path, &addr).await;

    // Ground truth, directly against the replacement origin: the first
    // history's blob is gone (NotFound).
    let direct = GardenbedClient::connect(&server2.http())
        .await
        .expect("client connects to the replacement origin")
        .with_token("replacement-token");
    let err = direct
        .get_blob("alpha", blob_id)
        .await
        .expect_err("the replacement repo lacks the first history's blob");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the origin answers NotFound for the replaced-away blob: {err}"
    );

    // The LIVE daemon (same process, warm in-memory scope map, cached
    // upstream channel that just went dead): a caller with a token
    // authorized for the replacement repository asks for the FIRST
    // incarnation's blob and tree. The warm cache holds them — fetched
    // by the first principal — but the replacement's fresh incarnation
    // scopes to a cold namespace: the daemon must answer the
    // replacement origin's NotFound, never the warm bytes. (The
    // upstream channel reset mid-test is the transport-dead retry
    // path: the request is retried after a fresh dial, not answered
    // from cache.)
    let client = GardenbedClient::connect(&format!("unix://{}", daemon.socket.display()))
        .await
        .expect("client connects to the live daemon")
        .with_token("replacement-token");

    // Blob: denied (NotFound via the replacement origin).
    let err = client
        .get_blob("alpha", blob_id)
        .await
        .expect_err("the prior incarnation's blob must not reach the replacement's callers");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the daemon answers the replacement's NotFound, not the prior incarnation's bytes: {err}"
    );

    // Tree: same denial.
    let err = client
        .get_tree("alpha", root_tree)
        .await
        .expect_err("the prior incarnation's tree must not reach the replacement's callers");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the daemon answers the replacement's NotFound, not the prior incarnation's bytes: {err}"
    );

    // Control: the replacement's OWN content serves through the same
    // daemon (the fresh scope warms normally).
    let head2 = history_second.changesets.len() - 1;
    let files2 = gardenbed_harness::generator::files_at(&history_second, head2);
    let (blob2, _) = *files2
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("the second history's head has a regular file");
    let mut stream = client
        .get_blob("alpha", blob2)
        .await
        .expect("the replacement's own blob serves through the daemon");
    let mut served = Vec::new();
    while let Some(chunk) = stream.next().await {
        served.extend_from_slice(&chunk.expect("replacement blob chunk").data);
    }
    assert_eq!(
        served,
        history_second
            .blob(&blob2)
            .expect("replacement blob content"),
        "the replacement's blob round-trips"
    );

    daemon.kill().await;
    server2.shutdown().await;
}

/// The restarted-daemon variant of the replacement denial: the daemon
/// is killed and a fresh process starts over the same cache root. The
/// persistent scope directory of the prior incarnation still holds the
/// warm bytes, but the replacement's fresh identity keys a different
/// scope: the denial holds across restarts too, not only across the
/// in-memory scope map.
#[tokio::test]
async fn daemon_restarted_over_replaced_repo_still_denies_prior_incarnation() {
    use gardenbed_protocol::client::GardenbedClient;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");
    let daemon_root = dir.path().join("daemon");

    let auth_path = dir.path().join("auth.toml");
    fs::write(
        &auth_path,
        r#"
            [tokens."first-token"]
            repos = ["*"]
            role = "write"

            [tokens."replacement-token"]
            repos = ["*"]
            role = "read"
        "#,
    )
    .expect("auth.toml written");

    let history_first = generate(5, &GenOptions::default());
    ingest(&history_first, &root.join("alpha"));
    let mut server = Server::spawn_with_auth(&root, &auth_path).await;
    let addr = server.addr.clone();
    let mut daemon = Daemon::spawn(&daemon_root, &server.http()).await;

    let wc = dir.path().join("wc");
    fs::create_dir_all(&wc).expect("wc dir");
    let (code, out, err) = bed_via_daemon_with_token(
        &daemon,
        &wc,
        &["clone", &format!("{}/alpha", server.http())],
        "first-token",
    );
    assert_eq!(code, 0, "clone of the first history: {err}\n{out}");
    let (code, out, err) =
        bed_via_daemon_with_token(&daemon, &wc, &["sparse", "add", "."], "first-token");
    assert_eq!(code, 0, "sparse of the first history: {err}\n{out}");

    let head = history_first.changesets.len() - 1;
    let files = gardenbed_harness::generator::files_at(&history_first, head);
    let (blob_id, _) = *files
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("the first history's head has a regular file");

    // Replace the repository (new metadata database, fresh identity)
    // and kill the daemon: the next daemon is a fresh process over the
    // same cache root.
    daemon.kill().await;
    server.shutdown().await;
    fs::remove_dir_all(root.join("alpha")).expect("the old repo dir is removed");
    let history_second = generate(11, &GenOptions::default());
    ingest(&history_second, &root.join("alpha"));
    let mut server2 = Server::spawn_with_auth_at_addr(&root, &auth_path, &addr).await;
    let mut daemon2 = Daemon::spawn(&daemon_root, &server2.http()).await;

    let client = GardenbedClient::connect(&format!("unix://{}", daemon2.socket.display()))
        .await
        .expect("client connects to the restarted daemon")
        .with_token("replacement-token");
    let err = client
        .get_blob("alpha", blob_id)
        .await
        .expect_err("the prior incarnation's blob must not survive a daemon restart");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "the restarted daemon answers the replacement's NotFound: {err}"
    );

    daemon2.kill().await;
    server2.shutdown().await;
}

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
    let (code, out, err) = bed_via_daemon_with_token(
        &daemon,
        &alpha_wc,
        &["sparse", "add", "."],
        "warm-token",
    );
    assert_eq!(code, 0, "alpha sparse materializes: {err}\n{out}");

    // Pick one of alpha's blobs and one of its trees from the head
    // changeset — both are now in the daemon's shared cache.
    let head = alpha_history.changesets.len() - 1;
    let files = gardenbed_harness::generator::files_at(&alpha_history, head);
    let (blob_id, _) = files
        .values()
        .find(|(_, mode)| *mode == gardenbed_model::Mode::Regular)
        .expect("alpha's head has a regular file")
        .clone();

    let alpha_repo = gardenbed_server::Repo::open(&root.join("alpha")).expect("alpha repo opens");
    let root_tree = alpha_repo
        .root_tree(alpha_history.head)
        .expect("alpha head tree")
        .expect("alpha head has files");

    // The beta-token client, straight onto the daemon's socket: asking
    // for alpha's blob under repo `beta` must NOT return alpha's bytes.
    let daemon_client =
        GardenbedClient::connect(&format!("unix://{}", daemon.socket.display()))
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

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

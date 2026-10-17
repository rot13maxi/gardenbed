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

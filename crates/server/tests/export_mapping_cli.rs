//! CLI wiring for `gardenbed-server export --record-mappings`
//! (v0.2, dogfood #26): the flag must reach the library option and the
//! mapping rows must land in the served root's metadata store.
//!
//! Part of integrity.rs because the export path opens the same stores
//! the server would (layout resolution), so this is a subprocess-level
//! end-to-end check: build the server binary, seed a repository through
//! the server CLI, export with the flag, inspect the sqlite.

use std::path::Path;
use std::process::Command;

/// Runs the just-built server binary with `args`, asserting success and
/// returning stdout.
fn server_out(args: &[&str]) -> String {
    let exe = env!("CARGO_BIN_EXE_gardenbed-server");
    let out = Command::new(exe)
        .args(args)
        .env("GARDENBED_AUTHOR", "Test Author <t@example.com>")
        .env_remove("GARDENBED_STORAGE")
        .output()
        .expect("server binary runs");
    assert!(
        out.status.success(),
        "server {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sqlite_count(root: &Path, sql: &str) -> u64 {
    let db = root.join("default/metadata.sqlite3");
    let conn = rusqlite::Connection::open(&db).expect("sqlite opens");
    conn.query_row(sql, [], |r| r.get::<_, u64>(0))
        .expect("query")
}

/// Exports a seeded repository with `--record-mappings` and verifies the
/// rows landed; a second run is idempotent (no new rows) and the plain
/// (flagless) export records nothing.
#[test]
fn export_record_mappings_cli_lands_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Import a tiny git repo: the import itself materializes
    // `<root>/default` and writes its own mapping row — the export
    // should then find that row unchanged, never double it.
    let src = dir.path().join("src.git");
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "--bare", "src.git"])
        .output()
        .expect("git init");
    assert!(out.status.success());
    let blob = Command::new("git")
        .current_dir(&src)
        .args(["hash-object", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin.as_mut().unwrap().write_all(b"cli\n").unwrap();
            c.wait_with_output()
        })
        .expect("hash-object");
    let blob_sha = String::from_utf8_lossy(&blob.stdout).trim().to_owned();
    let tree = Command::new("git")
        .current_dir(&src)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(format!("100644 blob {blob_sha}\tf\n").as_bytes())
                .unwrap();
            c.wait_with_output()
        })
        .expect("mktree");
    let tree_sha = String::from_utf8_lossy(&tree.stdout).trim().to_owned();
    let commit_raw = format!(
        "tree {tree_sha}\nauthor A <a@x> 1700000000 +0000\n\
         committer A <a@x> 1700000000 +0000\n\ncli seed\n"
    );
    let commit = Command::new("git")
        .current_dir(&src)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write as _;
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(commit_raw.as_bytes())
                .unwrap();
            c.wait_with_output()
        })
        .expect("commit");
    let commit_sha = String::from_utf8_lossy(&commit.stdout).trim().to_owned();
    let out = Command::new("git")
        .current_dir(&src)
        .args(["update-ref", "refs/heads/main", &commit_sha])
        .output()
        .expect("update-ref");
    assert!(out.status.success());
    let root = dir.path().join("root");
    let root_str = root.to_str().unwrap().to_owned();
    server_out(&[
        "import",
        "--root",
        &root_str,
        "--repo",
        "default",
        "--from",
        src.to_str().unwrap(),
    ]);

    let rows_after_import = sqlite_count(&root, "SELECT COUNT(*) FROM git_map");
    assert_eq!(rows_after_import, 1, "import wrote the mapping row");

    // Plain export (no flag): read-only, rows unchanged.
    let out_bare_plain = dir.path().join("plain.git");
    server_out(&[
        "export",
        "--root",
        &root_str,
        "--repo",
        "default",
        "--into",
        out_bare_plain.to_str().unwrap(),
    ]);
    assert_eq!(
        sqlite_count(&root, "SELECT COUNT(*) FROM git_map"),
        rows_after_import,
        "flagless export records nothing"
    );

    // Flagged export: the imported pair is found unchanged (the importer
    // wrote it first), nothing new, no conflicts.
    let out_bare = dir.path().join("flagged.git");
    let stdout = server_out(&[
        "export",
        "--root",
        &root_str,
        "--repo",
        "default",
        "--into",
        out_bare.to_str().unwrap(),
        "--record-mappings",
    ]);
    assert!(
        stdout.contains("mappings: 0 recorded, 1 unchanged, 0 conflict(s)"),
        "report line names the mapping outcome: {stdout}"
    );
    assert_eq!(
        sqlite_count(&root, "SELECT COUNT(*) FROM git_map"),
        rows_after_import,
        "the already-recorded pair is not doubled"
    );
}

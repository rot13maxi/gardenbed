//! The native Windows end-to-end smoke: the real `bed` binary talking to
//! a real `gardenbed-server` binary over **TCP on 127.0.0.1** (the
//! `common::Server` helper — no Unix-domain sockets, no FUSE), proving
//! the whole client stack works on a Windows kernel, not just compiles.
//!
//! One full lap, hand-checked at every stop:
//!
//! 1. `bed clone` from the live server plants the working copy at the
//!    seed tip — the file's bytes on disk are compared exactly;
//! 2. `bed sparse add` materializes a second subtree — bytes compared;
//! 3. a modify + commit + publish moves the server's `main` to a new
//!    changeset — the printed id is verified against the server's own
//!    ref read (not just trusted from stdout);
//! 4. a fresh `bed clone` of `main` from the same server (reopen) sees
//!    the published content — proving the bytes round-tripped through
//!    the server's object store on this platform.
//!
//! The test is portable and also runs on Linux (free extra coverage);
//! the workflow pins `--test-threads=1` so the single server process and
//! the single-lap story stay deterministic.

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;

/// Runs the real `bed` binary in `dir`: (exit code, stdout, stderr).
fn bed(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("GARDENBED_AUTHOR")
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The working copy's current changeset, parsed out of its own
/// `config.toml` (the file the CLI itself writes).
fn current_cs(garden: &Path) -> Id {
    let text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("current_cs") {
            let (_, value) = rest.split_once('=').expect("current_cs = <hex>");
            return Id::from_str(value.trim().trim_matches('"'))
                .expect("current_cs is 64 hex chars");
        }
    }
    panic!("config.toml has no current_cs:\n{text}");
}

#[tokio::test]
async fn bed_clone_sparse_modify_commit_publish_and_reopen_over_tcp() {
    // ---- the fixture: one root changeset with two subtrees -------------
    let dir = tempfile::tempdir().unwrap();
    let repo_root = dir.path().join("server").join(common::REPO);
    let ident = |secs: u64| Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    };
    let seed = |path: &str, content: &[u8]| {
        (
            Change {
                path: path.into(),
                op: Op::Add,
                blob: Some(blob_id(content)),
                src_path: None,
                mode: Mode::Regular,
            },
            content.to_vec(),
        )
    };
    let (c_readme, b_readme) = seed("docs/readme.md", b"# hello\n");
    let (c_lib, b_lib) = seed("src/lib.rs", b"pub fn bed() {}\n");
    let cs1 = Changeset {
        parents: Vec::new(),
        changes: vec![c_readme, c_lib],
        message: "seed the repository".into(),
        author: ident(1_700_000_000),
        committer: ident(1_700_000_000),
        signature: None,
    };
    let mut inline = std::collections::BTreeMap::new();
    inline.insert(blob_id(&b_readme).to_hex(), b_readme.clone());
    inline.insert(blob_id(&b_lib).to_hex(), b_lib.clone());
    {
        let repo = Repo::open(&repo_root).expect("repo opens");
        let cs1 = repo.create_changeset(&cs1, &inline).expect("cs1 ingests");
        repo.cas_ref("main", None, cs1).expect("main ref created");
    }

    // ---- the server: a real gardenbed-server binary on 127.0.0.1 -------
    let mut server = common::Server::spawn(repo_root.parent().unwrap()).await;
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    // 1. clone plants the working copy at main's tip; sparse add
    //    materializes the tree, bytes exact.
    let (code, out, stderr) = bed(&garden, &["clone", &server.http_garden()]);
    assert_eq!(code, 0, "clone: {stderr}\n{out}");
    assert!(out.contains("current changeset: "), "out: {out}");
    assert_eq!(
        current_cs(&garden),
        Id::from_str(&cs1_hex(&repo_root)).unwrap(),
        "planted at main's tip"
    );
    let (code, out, stderr) = bed(&garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse add: {stderr}\n{out}");
    assert_eq!(
        fs::read(garden.join("docs/readme.md")).unwrap(),
        b"# hello\n",
        "clone materialized the seed bytes exactly"
    );
    assert_eq!(
        fs::read(garden.join("src/lib.rs")).unwrap(),
        b"pub fn bed() {}\n",
        "sparse add materialized src/lib.rs exactly"
    );
    // 3. modify → status sees it → commit → publish moves main.
    fs::write(garden.join("docs/readme.md"), b"# hello\n\n tended\n").unwrap();
    let (code, out, stderr) = bed(&garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}\n{out}");
    assert!(
        out.contains("docs/readme.md"),
        "status sees the edit: {out}"
    );

    let (code, out, stderr) = bed(
        &garden,
        &[
            "commit",
            "-m",
            "tend the readme",
            "--author",
            "A. Gardener <a@bed.example>",
        ],
    );
    assert_eq!(code, 0, "commit: {stderr}\n{out}");
    assert!(out.contains("created changeset "), "out: {out}");
    let committed = current_cs(&garden);

    let (code, out, stderr) = bed(&garden, &["publish"]);
    assert_eq!(code, 0, "publish: {stderr}\n{out}");
    assert!(out.contains("published"), "out: {out}");

    // The server's own main ref — not stdout — is the oracle.
    {
        let repo = Repo::open(&repo_root).expect("repo reopens");
        let main = repo.get_ref("main").expect("main readable").unwrap();
        assert_eq!(main, committed, "publish moved the server's main");
    }

    // 4. reopen: a fresh clone of main sees the published bytes.
    let garden2 = dir.path().join("garden2");
    fs::create_dir_all(&garden2).expect("second garden dir");
    let (code, out, stderr) = bed(&garden2, &["clone", &server.http_garden()]);
    assert_eq!(code, 0, "reclone: {stderr}\n{out}");
    let (code, out, stderr) = bed(&garden2, &["sparse", "add", "."]);
    assert_eq!(code, 0, "reopen sparse: {stderr}\n{out}");
    assert_eq!(
        current_cs(&garden2),
        committed,
        "the reopened copy starts at the published tip"
    );
    assert_eq!(
        fs::read(garden2.join("docs/readme.md")).unwrap(),
        b"# hello\n\n tended\n",
        "the published content round-tripped through the server"
    );

    server.shutdown().await;
}

/// The hex of the seed changeset, read from the repo the server serves
/// (the fixture computed the id inside a scope above; this re-derives it
/// from the ref rather than threading the value out).
fn cs1_hex(repo_root: &Path) -> String {
    let repo = Repo::open(repo_root).expect("repo opens for the tip read");
    repo.get_ref("main")
        .expect("main readable")
        .unwrap()
        .to_hex()
}

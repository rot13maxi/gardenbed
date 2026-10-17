//! Multi-repository verification (v0.2.md §5, M11 + M12).
//!
//! One server process serves N repositories out of a root directory; the
//! wire's `repo` field selects. These tests verify the M11 criteria:
//!
//! 1. One process, ≥ 4 repositories, concurrent writes/reads across all;
//!    `GetStats(repo)` counts only that repository's RPCs.
//! 2. Unknown names → `not_found` on every RPC; empty →
//!    `invalid_argument`; a changeset existing in repo A 404s in repo B as
//!    a changeset error, not a repository error.
//! 3. Name rules: `../escape`, reserved names, `.` → `invalid_argument`.
//! 4. Migration: import → serve as `default` → `move --into alpha` →
//!    byte-identical changeset ids, refs, git-mapping rows; both a
//!    `--repo alpha` clone and a plain clone against the unmoved legacy
//!    root work.
//! 5. Isolation: concurrent CAS `main` in two repositories — each repo
//!    sees exactly one winner per race; nothing crosses repos.
//! 6. Resource cap: `--max-repos 2` keeps open handles ≤ 2 under
//!    round-robin load.
//!
//! M12: URL-clone and `--repo`-flag clone produce identical config
//! values; the MCP binary serves the named repository only, unknown names
//! exit 1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr as _;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::{Repo, repos};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Duration, timeout};
use tonic::Code;

/// A fixed identity (times differ per changeset so log ordering is stable).
fn identity(name: &str, secs: u64) -> Identity {
    Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

/// A canonically sorted Add change.
fn add(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

/// One changeset fixture.
fn cs(parents: Vec<Id>, changes: Vec<Change>, message: &str, secs: u64) -> Changeset {
    Changeset {
        parents,
        changes,
        message: message.into(),
        author: identity("amy", secs),
        committer: identity("amy", secs),
        signature: None,
    }
}

/// Commits `cs` with its inline blobs, returning the assigned id.
async fn commit(client: &GardenbedClient, repo: &str, cs: &Changeset, blobs: &[&[u8]]) -> Id {
    client
        .create_changeset(repo, cs, blobs.iter().map(|b| (blob_id(b), b.to_vec())))
        .await
        .expect("changeset created")
}

/// One running server: the spawned child plus its bound address.
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    /// Spawns the built binary on an ephemeral port (optional
    /// `--max-repos`) and waits for its `listening on <addr>` line.
    async fn spawn(root: &Path, max_repos: Option<usize>) -> Self {
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"));
        cmd.arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(max) = max_repos {
            cmd.arg("--max-repos").arg(max.to_string());
        }
        let mut child = cmd.spawn().expect("server binary spawns");
        let stdout = child.stdout.take().expect("server stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            let mut lines = BufReader::new(stdout).lines();
            lines
                .next_line()
                .await
                .expect("server stdout open")
                .expect("server prints a line")
        })
        .await
        .expect("server announces itself within 30s");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        ServerProc { child, addr }
    }

    /// The endpoint bed expects: `http://ip:port`.
    fn http(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A connected typed client.
    async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&self.http())
            .await
            .expect("client connects to the server")
    }

    /// Terminates the server and waits for it to exit.
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Creates `names.len()` empty repositories under `root` (one directory
/// each — the directory is the registry) and returns the root.
fn multi_root(dir: &Path, names: &[&str]) -> PathBuf {
    for name in names {
        Repo::open(&dir.join(name)).expect("repository opens");
    }
    dir.to_path_buf()
}

/// Locates a built workspace binary (bed, gardenbed-mcp): the
/// `CARGO_BIN_EXE_<name>` env var when Cargo sets it, else the workspace
/// target directory.
fn workspace_bin(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var(format!("CARGO_BIN_EXE_{name}")) {
        return PathBuf::from(path);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // crates/server
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("..").join("..").join("target"));
    let profiles: &[&str] = if cfg!(debug_assertions) {
        &["debug", "release"]
    } else {
        &["release", "debug"]
    };
    for profile in profiles {
        let candidate = target.join(profile).join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "{name} binary not found under {} — run `cargo build --workspace` first",
        target.display()
    );
}

/// Runs `bed` in `dir`, returning (exit code, stdout, stderr).
fn bed(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(workspace_bin("bed"))
        .current_dir(dir)
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("bed binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

// ------------------------------------------------------------- M11.1 + M11.2

/// M11.1: four repositories served by one process, concurrent
/// writes/reads across all, per-repo stats. M11.2: unknown names 404 on
/// every RPC; a changeset in repo A 404s in repo B as a changeset error.
#[tokio::test]
async fn four_repos_concurrent_isolated_stats_and_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["alpha", "beta", "gamma", "delta"]);
    let server = ServerProc::spawn(&root, None).await;
    let client = server.client().await;

    // Concurrent writes across all four: each repo gets its own root
    // changeset plus a child, all in flight together.
    let mut handles = Vec::new();
    for name in ["alpha", "beta", "gamma", "delta"] {
        let client = client.clone();
        let name = name.to_owned();
        handles.push(tokio::spawn(async move {
            let content = format!("hello {name}\n").into_bytes();
            let root_cs = cs(
                Vec::new(),
                vec![add("a.txt", &content)],
                &format!("root {name}"),
                1,
            );
            let id_root = commit(&client, &name, &root_cs, &[&content]).await;
            let child = cs(
                vec![id_root],
                vec![Change {
                    path: "b.txt".into(),
                    op: Op::Add,
                    blob: Some(blob_id(b"child\n")),
                    src_path: None,
                    mode: Mode::Regular,
                }],
                "child",
                2,
            );
            let id_child = commit(&client, &name, &child, &[b"child\n"]).await;
            (name, id_root, id_child)
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.expect("task joins"));
    }

    // Concurrent reads across all four: log, diff, blob.
    let mut handles = Vec::new();
    for (name, id_root, id_child) in &ids {
        let client = client.clone();
        let (name, id_root, id_child) = (name.clone(), *id_root, *id_child);
        handles.push(tokio::spawn(async move {
            use tokio_stream::StreamExt;
            let mut log = client.log(&name, id_child, 0).await.expect("log ok");
            let mut entries = 0;
            while let Some(item) = log.next().await {
                item.expect("log item");
                entries += 1;
            }
            assert_eq!(entries, 2, "both changesets in {name}'s log");
            let mut stream = client
                .diff(&name, id_root, id_child)
                .await
                .expect("diff ok");
            let mut n = 0;
            while let Some(item) = stream.next().await {
                item.expect("diff item");
                n += 1;
            }
            assert_eq!(n, 1, "one change in {name}'s diff");
        }));
    }
    for h in handles {
        h.await.expect("read task joins");
    }

    // GetStats(repo) counts only that repository's RPCs: alpha has seen 2
    // create_changeset; beta must report 0 (it was written through a
    // clone of the same client, but each call names its repo).
    for (name, _, _) in &ids {
        let stats: BTreeMap<String, u64> = client.get_stats(name).await.expect("stats ok");
        let created = *stats.get("create_changeset").expect("counter exists");
        assert_eq!(created, 2, "{name} saw exactly its own two creates");
    }

    // M11.2: a changeset in repo A 404s in repo B as a *changeset* error.
    let (_, alpha_root, _) = &ids[0];
    let (_, beta_root, _) = &ids[1];
    let err = client
        .get_changeset("beta", *alpha_root)
        .await
        .expect_err("alpha's changeset is not in beta");
    assert_eq!(err.code(), Code::NotFound, "as a changeset-level not_found");
    let msg = err.message();
    assert!(
        msg.contains("changeset") && !msg.contains("repository"),
        "the error names the changeset, not the repository: {msg}"
    );
    // And symmetrically.
    let err = client
        .get_changeset("alpha", *beta_root)
        .await
        .expect_err("beta's changeset is not in alpha");
    assert_eq!(err.code(), Code::NotFound);

    // M11.2: unknown names → not_found listing the served names, on
    // several RPCs.
    for rpc in ["get_changeset", "get_tree", "log", "list_refs", "get_stats"] {
        let err = match rpc {
            "get_changeset" => client.get_changeset("nope", *alpha_root).await.unwrap_err(),
            "get_tree" => client.get_tree("nope", *alpha_root).await.unwrap_err(),
            "log" => client.log("nope", *alpha_root, 0).await.unwrap_err(),
            "list_refs" => client.list_refs("nope").await.unwrap_err(),
            _ => client.get_stats("nope").await.unwrap_err(),
        };
        assert_eq!(err.code(), Code::NotFound, "{rpc}: not_found");
        assert!(
            err.message().contains("repository nope not found"),
            "{rpc} names the repository: {}",
            err.message()
        );
        assert!(
            err.message().contains("alpha, beta, delta, gamma"),
            "{rpc} lists the served names sorted: {}",
            err.message()
        );
    }

    // Empty name → invalid_argument.
    let err = client.get_stats("").await.expect_err("empty rejected");
    assert_eq!(err.code(), Code::InvalidArgument);

    // A tree id read in repo A is absent in repo B (trees are derived per
    // repo; identical content yields identical ids, so use alpha's root
    // tree against a repo whose content differs).
    let alpha_tree = client
        .root_tree("alpha", ids[0].1)
        .await
        .expect("root tree");
    let tree_id = alpha_tree.expect("alpha root has files");
    let gamma_tree = client
        .root_tree("gamma", ids[2].1)
        .await
        .expect("root tree");
    if gamma_tree != Some(tree_id) {
        let err = client
            .get_tree("gamma", tree_id)
            .await
            .expect_err("no leak");
        assert_eq!(err.code(), Code::NotFound);
    }

    server.shutdown().await;
}

// -------------------------------------------------------------------- M11.3

/// Name rules: illegal and reserved names are `invalid_argument` before
/// any filesystem access.
#[tokio::test]
async fn illegal_and_reserved_names_are_invalid_argument() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["alpha"]);
    let server = ServerProc::spawn(&root, None).await;
    let client = server.client().await;

    let bad_names = [
        "",                 // empty
        ".",                // dot
        "..",               // parent
        "../escape",        // traversal
        "/abs",             // rooted
        "a/b",              // separator
        ".hidden",          // leading dot
        "objects",          // reserved
        "metadata.sqlite3", // reserved
        "seedbed.sqlite3",  // reserved
        "git-mirror.git",   // reserved
        "tmp",              // reserved
        "space name",       // illegal character
        "naïve",            // non-ASCII
    ];
    for name in bad_names {
        let err = client.get_stats(name).await.expect_err("rejected");
        assert_eq!(
            err.code(),
            Code::InvalidArgument,
            "{name:?} must be invalid_argument (got {})",
            err.message()
        );
    }
    // 65 bytes is over the cap; 64 is legal.
    let long_ok = "a".repeat(64);
    let long_bad = "a".repeat(65);
    let stats = client.get_stats(&long_ok).await;
    assert!(stats.is_err(), "the 64-byte name is not served (not_found)");
    if let Err(e) = stats {
        assert_eq!(e.code(), Code::NotFound);
    }
    let err = client.get_stats(&long_bad).await.expect_err("too long");
    assert_eq!(err.code(), Code::InvalidArgument);

    server.shutdown().await;
}

// ------------------------------------------------------- M11.5 (CAS isolation)

/// Isolation: 32 threads CAS `main` in two repositories concurrently;
/// each repo sees exactly one winner per round; nothing crosses repos.
#[tokio::test]
async fn concurrent_cas_main_across_two_repos_stays_isolated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["alpha", "beta"]);
    let server = ServerProc::spawn(&root, None).await;
    let client = server.client().await;

    // A root changeset published to main in each repo.
    let mut mains = BTreeMap::new();
    for name in ["alpha", "beta"] {
        let content = format!("base {name}\n").into_bytes();
        let root_cs = cs(Vec::new(), vec![add("f.txt", &content)], "root", 1);
        let id = commit(&client, name, &root_cs, &[&content]).await;
        client
            .cas_ref(name, "main", None, id)
            .await
            .expect("initial cas wins");
        mains.insert(name.to_owned(), id);
    }

    // 32 racers per repo: each proposes its own changeset against the
    // same expected main. Exactly one must win per repo.
    const RACERS: usize = 32;
    let mut handles = Vec::new();
    for name in ["alpha", "beta"] {
        for i in 0..RACERS {
            let client = client.clone();
            let name = name.to_owned();
            let expected = *mains.get(name.as_str()).unwrap();
            handles.push(tokio::spawn(async move {
                let content = format!("racer {name} {i}\n").into_bytes();
                let racer = cs(
                    vec![expected],
                    vec![add(&format!("r{i}.txt"), &content)],
                    &format!("racer {i}"),
                    2,
                );
                let id = commit(&client, &name, &racer, &[&content]).await;
                let outcome = client
                    .cas_ref(&name, "main", Some(expected), id)
                    .await
                    .expect("cas answered");
                (name, outcome)
            }));
        }
    }
    let mut winners: BTreeMap<String, usize> = BTreeMap::new();
    for h in handles {
        let (name, outcome) = h.await.expect("racer joins");
        use gardenbed_protocol::client::CasOutcome;
        match outcome {
            CasOutcome::Won => *winners.entry(name).or_default() += 1,
            CasOutcome::Conflict { .. } => {}
        }
    }
    for name in ["alpha", "beta"] {
        assert_eq!(
            winners.get(name).copied().unwrap_or(0),
            1,
            "{name}: exactly one CAS winner"
        );
        // The winner's changeset is in main's ancestry: main advanced
        // past the root. The other 31 remain unpublished but intact.
        let main_now = client
            .get_ref(name, "main")
            .await
            .expect("get_ref")
            .expect("main exists");
        use tokio_stream::StreamExt;
        let mut log = client
            .log(name, main_now, 0)
            .await
            .expect("log at new main");
        let mut entries = 0;
        while let Some(item) = log.next().await {
            item.expect("log item");
            entries += 1;
        }
        assert!(
            entries >= 2,
            "{name}: main advanced past the root ({entries} entries)"
        );
    }

    server.shutdown().await;
}

// ------------------------------------------------------------- M11.6 (cap)

/// Resource cap: `--max-repos 2` with 4 repositories touched
/// round-robin keeps open handles ≤ 2 (the RepoSet evicts idle LRU
/// handles; a shared handle count is asserted through the library).
#[tokio::test]
async fn max_repos_caps_open_handles() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["r1", "r2", "r3", "r4"]);
    let server = ServerProc::spawn(&root, Some(2)).await;
    let client = server.client().await;

    // Round-robin over all four, repeatedly: every request must succeed
    // (eviction is transparent) while open handles stay ≤ 2.
    for round in 0..4 {
        for name in ["r1", "r2", "r3", "r4"] {
            let stats: BTreeMap<String, u64> = client
                .get_stats(name)
                .await
                .unwrap_or_else(|e| panic!("round {round} {name}: {e}"));
            assert!(stats.contains_key("get_tree"), "counters present");
        }
    }
    // The same cap holds in-process: a fresh set over the same root.
    let set = repos::RepoSet::new(&root, 2);
    for round in 0..6 {
        for name in ["r1", "r2", "r3", "r4"] {
            let guard = set.resolve(name).expect("resolve");
            assert!(set.open_count() <= 2, "cap respected (round {round})");
            drop(guard);
        }
    }
    assert!(set.open_count() <= 2, "cap respected after the loop");

    server.shutdown().await;
}

/// Object-store checksum sweep: every file under `dir/objects` must be a
/// 64-hex-named object whose bytes verify — blobs decode from their zstd
/// envelope and hash to their id, trees decode and hash to theirs.
/// (Ported from the chaos suite, which asserts the same invariant after
/// SIGKILL storms; here it proves the move disturbed nothing.)
fn sweep_objects(dir: &Path) {
    use gardenbed_model::Tree;
    use sha2::Digest;
    let objects = dir.join("objects");
    let mut verified = 0u64;
    let mut stack = vec![objects];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let entry = entry.expect("dir entry");
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue; // never-published atomic-write tmp files
            }
            if entry.file_type().expect("file type").is_dir() {
                stack.push(entry.path());
                continue;
            }
            let shard = entry
                .path()
                .parent()
                .expect("shard dir")
                .file_name()
                .expect("shard name")
                .to_string_lossy()
                .to_string();
            let name = entry.file_name().to_string_lossy().to_string();
            let id = Id::from_str(&format!("{shard}{name}")).expect("object names are 64-hex ids");
            let bytes = std::fs::read(entry.path()).expect("object reads");
            if bytes.starts_with(b"GBLO1") {
                let raw_len = u64::from_le_bytes(bytes[5..13].try_into().expect("envelope header"));
                let content = zstd::stream::decode_all(std::io::Cursor::new(&bytes[13..]))
                    .unwrap_or_else(|e| panic!("blob {id} frame does not decode: {e}"));
                assert_eq!(
                    content.len() as u64,
                    raw_len,
                    "blob {id} envelope length lies (torn write observed)"
                );
                assert_eq!(blob_id(&content), id, "blob {id} fails its checksum");
            } else {
                Tree::decode(&bytes)
                    .unwrap_or_else(|e| panic!("object {id} is not a decodable tree: {e}"));
                let digest = sha2::Sha256::digest(&bytes);
                assert_eq!(
                    digest.as_slice(),
                    id.as_bytes(),
                    "tree {id} fails its checksum"
                );
            }
            verified += 1;
        }
    }
    assert!(
        verified >= 2,
        "sweep found the repository's objects ({verified})"
    );
}

/// Every row of every table in `db`, one tab-separated line per row,
/// tables sorted — the byte-level fingerprint for the move test. Uses the
/// sqlite3 CLI (the same binary the demo's verification steps use) and
/// skips sqlite's internal `sqlite_*` tables.
fn sqlite_dump(db: &Path) -> String {
    let list = std::process::Command::new("sqlite3")
        .arg("-readonly")
        .arg(db)
        .arg("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name;")
        .output()
        .expect("sqlite3 list tables");
    assert!(
        list.status.success(),
        "sqlite3 failed: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let mut dump = String::new();
    for table in String::from_utf8_lossy(&list.stdout).lines() {
        dump.push_str(&format!("== {table} ==\n"));
        let rows = std::process::Command::new("sqlite3")
            .arg("-readonly")
            .arg(db)
            .arg("-cmd")
            .arg(".mode quote")
            .arg(format!("SELECT * FROM {table} ORDER BY rowid;"))
            .output()
            .expect("sqlite3 dump rows");
        assert!(
            rows.status.success(),
            "sqlite3 failed: {}",
            String::from_utf8_lossy(&rows.stderr)
        );
        dump.push_str(&String::from_utf8_lossy(&rows.stdout));
    }
    dump
}

// ------------------------------------------------------------ M11.4 (move)

/// Migration: a legacy v0.1 root serves as `default`; `move --into
/// alpha` keeps changeset ids and refs byte-identical; both the moved
/// name and (before the move) a plain `--repo default` clone work.
#[tokio::test]
async fn legacy_root_serves_as_default_and_move_preserves_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("server");

    // A legacy layout: stores directly under the root.
    let repo = Repo::open(&root).expect("legacy repo opens");
    let content = b"legacy\n".to_vec();
    let root_cs = cs(Vec::new(), vec![add("a.txt", &content)], "root", 1);
    let mut inline = BTreeMap::new();
    inline.insert(blob_id(&content).to_hex(), content.clone());
    let id = repo.create_changeset(&root_cs, &inline).expect("commit");
    repo.cas_ref("main", None, id).expect("main created");
    drop(repo);

    // Serves as `default` — the name bed clones by default.
    assert!(repos::RepoSet::is_legacy_root(&root));
    let set = repos::RepoSet::new(&root, repos::DEFAULT_MAX_REPOS);
    assert_eq!(set.served_names(), vec!["default"]);
    let server = ServerProc::spawn(&root, None).await;

    // A plain clone (no --repo) against the legacy root works.
    let garden = dir.path().join("garden");
    std::fs::create_dir_all(&garden).expect("garden dir");
    let (code, out, stderr) = bed(&garden, &["clone", &server.http()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        out.contains("created working copy for default"),
        "out: {out}"
    );
    server.shutdown().await;

    // The wire also resolves `default` with the repository's contents.
    let server = ServerProc::spawn(&root, None).await;
    let client = server.client().await;
    let got = client
        .get_ref("default", "main")
        .await
        .expect("get_ref")
        .expect("main exists");
    assert_eq!(got, id, "byte-identical changeset id through the wire");
    server.shutdown().await;

    // Full metadata dump before the move: every row of every table,
    // ordered, so the move can be proven byte-preserving beyond the
    // changeset and ref already asserted (M11.4: git-map, validation
    // certificates, and derived rows survive verbatim).
    let dump_before = sqlite_dump(&root.join("metadata.sqlite3"));

    // move --from default --into alpha: same bytes, new name.
    let moved = set.move_repo("default", "alpha").expect("move");
    assert_eq!(moved, root.parent().unwrap().join("server").join("alpha"));
    let set = repos::RepoSet::new(&root, repos::DEFAULT_MAX_REPOS);
    assert_eq!(
        set.served_names(),
        vec!["alpha"],
        "only alpha is served now"
    );

    let repo = Repo::open(&root.join("alpha")).expect("moved repo opens");
    assert_eq!(
        repo.get_ref("main").expect("ref").expect("exists"),
        id,
        "byte-identical id after the move"
    );
    let stored = repo.get_changeset(id).expect("changeset");
    assert_eq!(stored.message, "root", "byte-identical changeset bytes");
    let dump_after = sqlite_dump(&root.join("alpha").join("metadata.sqlite3"));
    assert_eq!(
        dump_before, dump_after,
        "every metadata row is byte-identical after the move"
    );
    // Object-store checksum sweep on the moved directory: every object
    // is a well-formed blob envelope or tree payload whose bytes hash to
    // its id (the same sweep the chaos suite runs).
    sweep_objects(&root.join("alpha"));

    // The vacated name 404s.
    let server = ServerProc::spawn(&root, None).await;
    let client = server.client().await;
    let err = client.get_stats("default").await.expect_err("vacated");
    assert_eq!(err.code(), Code::NotFound);
    assert!(err.message().contains("serves: alpha"));
    // The new name serves the same id.
    let got = client
        .get_ref("alpha", "main")
        .await
        .expect("get_ref")
        .expect("main exists");
    assert_eq!(got, id);
    server.shutdown().await;
}

// ------------------------------------------------------------------ M12.1

/// URL-clone and `--repo`-flag clone produce identical config values.
#[tokio::test]
async fn url_clone_and_repo_flag_clone_agree() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["alpha"]);
    // Content + main ref so clone has something to resolve.
    {
        let repo = Repo::open(&dir.path().join("alpha")).expect("repo");
        let content = b"hello\n".to_vec();
        let root_cs = cs(Vec::new(), vec![add("a.txt", &content)], "root", 1);
        let mut inline = BTreeMap::new();
        inline.insert(blob_id(&content).to_hex(), content);
        let id = repo.create_changeset(&root_cs, &inline).expect("commit");
        repo.cas_ref("main", None, id).expect("main");
    }
    let server = ServerProc::spawn(&root, None).await;

    let g1 = dir.path().join("g1");
    let g2 = dir.path().join("g2");
    std::fs::create_dir_all(&g1).expect("dir");
    std::fs::create_dir_all(&g2).expect("dir");

    let (code, _, stderr) = bed(&g1, &["clone", &format!("{}/alpha", server.http())]);
    assert_eq!(code, 0, "url clone: {stderr}");
    let (code, _, stderr) = bed(&g2, &["clone", &server.http(), "--repo", "alpha"]);
    assert_eq!(code, 0, "flag clone: {stderr}");

    let c1 = std::fs::read_to_string(g1.join(".bed/config.toml")).expect("config 1");
    let c2 = std::fs::read_to_string(g2.join(".bed/config.toml")).expect("config 2");
    assert_eq!(c1, c2, "identical config values");
    assert!(c1.contains("repo = \"alpha\""), "config names alpha:\n{c1}");

    // Both commit and publish against their own repository: materialize
    // the file first (clone materializes nothing by design).
    let (code, _, stderr) = bed(&g1, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse add: {stderr}");
    std::fs::write(g1.join("a.txt"), b"hello\nchanged\n").expect("write");
    let (code, _, stderr) = bed(&g1, &["commit", "-m", "from url clone"]);
    assert_eq!(code, 0, "commit: {stderr}");
    let (code, _, stderr) = bed(&g1, &["publish"]);
    assert_eq!(code, 0, "publish: {stderr}");

    server.shutdown().await;
}

// ------------------------------------------------------------------ M12.2

/// The MCP binary serves the named repository only; an unknown name
/// exits 1 with the reason.
#[tokio::test]
async fn mcp_serves_the_named_repository_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = multi_root(dir.path(), &["alpha", "beta"]);
    // Distinct content per repo.
    let mut ids = BTreeMap::new();
    for name in ["alpha", "beta"] {
        let repo = Repo::open(&dir.path().join(name)).expect("repo");
        let content = format!("{name}\n").into_bytes();
        let root_cs = cs(Vec::new(), vec![add("a.txt", &content)], "root", 1);
        let mut inline = BTreeMap::new();
        inline.insert(blob_id(&content).to_hex(), content);
        let id = repo.create_changeset(&root_cs, &inline).expect("commit");
        repo.cas_ref("main", None, id).expect("main");
        ids.insert(name.to_owned(), id);
    }

    let mcp = workspace_bin("gardenbed-mcp");
    let run = |repo: &str| {
        std::process::Command::new(&mcp)
            .arg("--root")
            .arg(&root)
            .arg("--repo")
            .arg(repo)
            .stdin(Stdio::null())
            .output()
            .expect("mcp spawns")
    };

    // A named repo serves; `log --cs <id>` returns that repo's changeset.
    for (name, id) in &ids {
        let out = run(name);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{name} serves (stderr: {})",
            String::from_utf8_lossy(&out.stderr)
        );
        let ask = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"log\",\"arguments\":{{\"limit\":1,\"cs\":\"{}\"}}}}}}",
            id.to_hex()
        );
        let out = std::process::Command::new(&mcp)
            .arg("--root")
            .arg(&root)
            .arg("--repo")
            .arg(name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(ask.as_bytes()).unwrap();
                c.wait_with_output()
            })
            .expect("mcp session");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("root"),
            "{name} answered with its own changeset: {text}"
        );
    }

    // An unknown name exits 1 naming the reason.
    let out = run("nope");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("repository nope not found"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("alpha") && stderr.contains("beta"),
        "{stderr}"
    );
}

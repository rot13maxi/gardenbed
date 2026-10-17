//! M15 offline-queue integration tests (v0.2.md §6, decision D17):
//! `bed commit` while the server is down, `bed sync` draining the queue,
//! refusals while queued, offline status, recovery, and publish-after-sync.
//!
//! Oracles are independent of the code under test:
//!
//! * **server-side changesets** are read back through the server library
//!   (`Repo::get_changeset`), never through counted RPCs;
//! * **the queue** is inspected through `bed status` and the server's
//!   GetStats counters;
//! * **idempotence** is asserted on the server's repository state (what
//!   changesets exist), not on counters (the server counts every request
//!   including idempotent re-creates).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;
use tempfile::TempDir;

use common::{REPO, Server, bed, bed_async};

/// The same small hand-built history `write_path.rs` uses: one seed
/// changeset, `main` pointing at it.
struct World {
    _dir: TempDir,
    /// The server root (persists on disk; a second server can spawn over
    /// it after the first is killed).
    _root: PathBuf,
    garden: PathBuf,
    server: Server,
    #[expect(dead_code)]
    repo: Repo,
    head: Id,
}

async fn setup() -> World {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("store").join(REPO);
    let repo = Repo::open(&root).expect("repo opens");

    let ident = |secs: u64| Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    };
    let add = |path: &str, content: &[u8]| Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    };
    let c1 = add("docs/readme.md", b"# hello\n");
    let c2 = add("src/main.rs", b"fn main() {}\n");
    let c3 = add("src/lib.rs", b"pub fn bed() {}\n");
    let c4 = add("notes/todo.txt", b"buy seeds\n");
    let cs1 = Changeset {
        parents: Vec::new(),
        changes: vec![c1.clone(), c4, c3, c2],
        message: "seed the repository".into(),
        author: ident(1_700_000_000),
        committer: ident(1_700_000_000),
        signature: None,
    };
    let mut inline1 = std::collections::BTreeMap::new();
    for content in [
        &b"# hello\n"[..],
        b"fn main() {}\n",
        b"pub fn bed() {}\n",
        b"buy seeds\n",
    ] {
        inline1.insert(blob_id(content).to_hex(), content.to_vec());
    }
    let id1 = repo.create_changeset(&cs1, &inline1).expect("cs1 ingests");
    repo.cas_ref("main", None, id1).expect("main created");

    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let server = Server::spawn(root.parent().unwrap()).await;
    World {
        _dir: dir,
        _root: root,
        garden,
        server,
        repo,
        head: id1,
    }
}

/// Clones a working copy and sparses everything, via the real CLI.
fn plant(w: &World) {
    let (code, _, stderr) = bed(
        &w.garden,
        &["clone", &format!("{}/{}", w.server.http(), REPO)],
    );
    assert_eq!(code, 0, "clone: {stderr}");
    let (code, out, stderr) = bed(&w.garden, &["sparse", "add", "."]);
    assert_eq!(code, 0, "sparse add: {stderr}\n{out}");
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("working copy is clean"), "status: {out}");
}

/// The working copy's current changeset, read from its config.
fn current_cs(garden: &Path) -> Id {
    let text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    for line in text.lines() {
        if line.trim().starts_with("current_cs") {
            let (_, value) = line.split_once('=').expect("current_cs = <hex>");
            let hex = value.trim().trim_matches('"');
            return Id::from_str(hex).expect("current_cs is 64 hex chars");
        }
    }
    panic!("config.toml has no current_cs:\n{text}");
}

/// Points the working copy's config at `endpoint` — used after a server
/// restart, which binds a fresh port.
fn repoint_server(garden: &Path, endpoint: &str) {
    let mut text = fs::read_to_string(garden.join(".bed/config.toml")).expect("config.toml");
    let start = text.find("server = \"").expect("server key");
    let rest = &text[start + 10..];
    let end = rest.find('\"').expect("closing quote");
    text.replace_range(start + 10..start + 10 + end, endpoint);
    fs::write(garden.join(".bed/config.toml"), text).expect("config.toml rewritten");
}

/// M15 airplane test: with the server down, three chained offline commits
/// queue locally (output names the queue first, ids computed locally,
/// each chaining onto the previous pending tip); with the server back,
/// `bed sync` drains oldest-first, the server's ids equal the locally
/// computed ones, the queue is empty, and the server counted exactly one
/// create-changeset call per commit.
#[tokio::test]
async fn offline_commits_sync_when_server_returns() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    w.server.shutdown().await;

    let mut expected = Vec::new();
    for i in 0..3 {
        let content = format!("offline note {i}\n");
        fs::write(w.garden.join("notes/todo.txt"), &content).unwrap();
        let (code, out, stderr) = bed(
            &w.garden,
            &[
                "commit",
                "-m",
                &format!("offline change {i}"),
                "--author",
                "Offline Gardener <offline@bed.example>",
            ],
        );
        assert_eq!(code, 0, "offline commit {i}: {stderr}\n{out}");
        let id = current_cs(&w.garden);
        assert_eq!(
            out.lines().next(),
            Some(
                format!(
                    "queued as {} ({} pending — run bed sync when online)",
                    id.short(),
                    i + 1
                )
                .as_str()
            ),
            "commit {i} names the queue first: {out}"
        );
        // Chained: the id changes every commit, and the previous commit
        // is no longer the current one.
        assert!(
            !expected.contains(&id),
            "each offline commit has its own id"
        );
        expected.push(id);
    }

    // Server returns (its state persisted on disk under the same root).
    let server = Server::spawn(w._root.parent().unwrap()).await;
    repoint_server(&w.garden, &server.http());
    w.server = server;

    let (code, out, stderr) = bed(&w.garden, &["sync"]);
    assert_eq!(code, 0, "sync: {stderr}\n{out}");
    for id in &expected {
        assert!(
            out.contains(&format!("synced {}", id.short())),
            "sync: {out}"
        );
    }
    assert!(out.contains("queue empty"), "sync summary: {out}");

    // Server-side ids equal the locally computed ones, parents chained
    // onto the recorded base. Read back through the server library.
    let repo = Repo::open(&w._root).expect("repo reopens");
    let mut parent = base;
    for (i, id) in expected.iter().enumerate() {
        let cs = repo
            .get_changeset(*id)
            .expect("the synced changeset exists");
        assert_eq!(cs.parents.first().copied(), Some(parent), "chained parent");
        assert_eq!(cs.message, format!("offline change {i}"));
        parent = *id;
    }
    assert_eq!(current_cs(&w.garden), *expected.last().unwrap());

    // GetStats: exactly one create-changeset call per commit (this server
    // process served only the sync).
    let client = w.server.client().await;
    let stats = client.get_stats(REPO).await.expect("get_stats");
    assert_eq!(
        stats.get("create_changeset").copied().unwrap_or(0),
        expected.len() as u64,
        "exactly {} create-changeset calls: {stats:?}",
        expected.len()
    );

    // Queue empty, status silent about pending, working copy clean.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(!out.contains("pending"), "status after sync: {out}");
    assert!(
        out.contains("working copy is clean"),
        "status after sync: {out}"
    );
    let (code, out, stderr) = bed(&w.garden, &["sync"]);
    assert_eq!(code, 0, "second sync: {stderr}");
    assert!(out.contains("nothing to sync"), "second sync: {out}");

    w.server.shutdown().await;
}

/// M15 kill-mid-sync idempotence: a sync killed after the first
/// CreateChangeset landed, restarted, ends with an empty queue, the
/// pending tip on the server, and the working copy's current changeset
/// unchanged by the kill (sync never repoints).
#[tokio::test]
async fn killed_sync_restarts_without_duplicates() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    w.server.shutdown().await;

    // Four offline commits (several, so the drain has room to die early).
    let mut expected = Vec::new();
    for i in 0..4 {
        let content = format!("offline row {i}\n");
        fs::write(w.garden.join("notes/todo.txt"), &content).unwrap();
        let (code, out, stderr) = bed(
            &w.garden,
            &[
                "commit",
                "-m",
                &format!("offline {i}"),
                "--author",
                "Offline Gardener <offline@bed.example>",
            ],
        );
        assert_eq!(code, 0, "offline commit {i}: {stderr}\n{out}");
        expected.push(current_cs(&w.garden));
    }
    let tip = *expected.last().unwrap();

    let server = Server::spawn(w._root.parent().unwrap()).await;
    repoint_server(&w.garden, &server.http());
    w.server = server;

    let mut child = bed_async(&w.garden, &["sync"]);
    let client = w.server.client().await;

    // Kill the sync the moment the first changeset landed on the server.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let stats = client.get_stats(REPO).await.expect("get_stats");
        if stats.get("create_changeset").copied().unwrap_or(0) >= 1 {
            break;
        }
        if let Ok(Some(_)) = child.try_wait() {
            panic!("the sync exited before any changeset landed");
        }
        assert!(Instant::now() < deadline, "create_changeset never bumped");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    child.kill().await.expect("SIGKILL the syncing bed");
    let _ = child.wait().await;

    // The working copy's current never moved (sync is upload-only).
    assert_eq!(current_cs(&w.garden), tip);

    // Restart the sync: it completes.
    let (code, out, stderr) = bed(&w.garden, &["sync"]);
    assert_eq!(code, 0, "restarted sync: {stderr}\n{out}");
    assert!(out.contains("queue empty"), "restarted sync: {out}");

    // No duplicates on the server: every queued changeset exists (the
    // re-create is an idempotent re-put), the chain is intact, and the
    // working copy is still at the tip. Read through the server library.
    let repo = Repo::open(&w._root).expect("repo reopens");
    let mut parent = base;
    for (i, id) in expected.iter().enumerate() {
        let cs = repo
            .get_changeset(*id)
            .expect("the synced changeset exists");
        assert_eq!(cs.parents.first().copied(), Some(parent), "chained parent");
        assert_eq!(cs.message, format!("offline {i}"));
        parent = *id;
    }
    assert_eq!(current_cs(&w.garden), tip, "sync never repoints");

    // Queue ends empty.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(
        !out.contains("pending"),
        "status after restarted sync: {out}"
    );

    w.server.shutdown().await;
}

/// Plants one more changeset on top of `base` through the server
/// library (uncounted) and fast-forwards main to it: the returned id is
/// strictly newer than base.
fn repo_step_ahead(root: &Path, base: Id) -> Id {
    use gardenbed_model::{Change, Changeset, Identity, Op};
    let repo = Repo::open(root).expect("repo reopens");
    let ident = Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: 1_700_000_200,
        tz_min: 0,
    };
    let content = b"stepped ahead\n";
    let change = Change {
        path: "notes/todo.txt".into(),
        op: Op::Modify,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    };
    let cs = Changeset {
        parents: vec![base],
        changes: vec![change],
        message: "step ahead".into(),
        author: ident.clone(),
        committer: ident,
        signature: None,
    };
    let mut inline = std::collections::BTreeMap::new();
    inline.insert(blob_id(content).to_hex(), content.to_vec());
    let id = repo.create_changeset(&cs, &inline).expect("step ingests");
    repo.cas_ref("main", Some(base), id).expect("main moves");
    id
}

/// M15 refusals: switch, rebase, and publish refuse with the pending
/// count while commits are queued (offline — the queue is local state),
/// status prints the pending line, log/diff at the pending tip fail
/// queue-aware, and everything works after `bed sync`.
#[tokio::test]
async fn switch_rebase_publish_refuse_while_pending_then_work() {
    let mut w = setup().await;
    plant(&w);

    w.server.shutdown().await;

    fs::write(w.garden.join("notes/todo.txt"), b"buy seeds offline\n").unwrap();
    let (code, out, stderr) = bed(
        &w.garden,
        &[
            "commit",
            "-m",
            "offline one",
            "--author",
            "Offline Gardener <offline@bed.example>",
        ],
    );
    assert_eq!(code, 0, "offline commit: {stderr}\n{out}");

    // All three refuse, naming bed sync and the pending count.
    for command in ["switch", "rebase"] {
        let (code, _, stderr) = bed(&w.garden, &[command, "main"]);
        assert_eq!(code, 1, "{command} while queued must fail");
        assert!(
            stderr.contains("1 pending commit(s) not yet on the server"),
            "{command} refusal names the count: {stderr}"
        );
        assert!(
            stderr.contains("bed sync"),
            "{command} refusal names sync: {stderr}"
        );
    }
    let (code, _, stderr) = bed(&w.garden, &["publish"]);
    assert_eq!(code, 1, "publish while queued must fail");
    assert!(
        stderr.contains("1 pending commit(s) not yet on the server"),
        "publish refusal names the count: {stderr}"
    );
    assert!(
        stderr.contains("bed sync"),
        "publish refusal names sync: {stderr}"
    );

    // Status prints the pending line while offline.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "offline status: {stderr}");
    assert!(
        out.contains("1 pending commit(s) not yet on the server — run bed sync"),
        "offline status names the queue: {out}"
    );

    // log/diff at the pending tip refuse queue-aware.
    let (code, _, stderr) = bed(&w.garden, &["log"]);
    assert_eq!(code, 1, "log at pending tip must fail");
    assert!(stderr.contains("is pending"), "log refusal: {stderr}");
    assert!(
        stderr.contains("bed sync"),
        "log refusal names sync: {stderr}"
    );
    let (code, _, stderr) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 1, "diff at pending tip must fail");
    assert!(stderr.contains("is pending"), "diff refusal: {stderr}");

    let server = Server::spawn(w._root.parent().unwrap()).await;
    repoint_server(&w.garden, &server.http());
    w.server = server;

    let (code, out, stderr) = bed(&w.garden, &["sync"]);
    assert_eq!(code, 0, "sync: {stderr}\n{out}");
    assert!(out.contains("queue empty"), "sync: {out}");

    // After sync the commands work again.
    let (code, out, stderr) = bed(&w.garden, &["log"]);
    assert_eq!(code, 0, "log after sync: {stderr}");
    assert!(
        out.contains("offline one"),
        "log shows the synced commit: {out}"
    );
    let (code, _out, stderr) = bed(&w.garden, &["diff"]);
    assert_eq!(code, 0, "diff after sync: {stderr}");
    let (code, out, stderr) = bed(&w.garden, &["switch", "main"]);
    assert_eq!(code, 0, "switch after sync: {stderr}\n{out}");
    let (code, out, stderr) = bed(&w.garden, &["rebase", "main"]);
    assert_eq!(code, 0, "rebase after sync: {stderr}\n{out}");
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(!out.contains("pending"), "status after sync: {out}");

    w.server.shutdown().await;
}

/// M15 offline status vs online status: the pending line appears while
/// queued (offline) and is absent when the queue is empty (online).
#[tokio::test]
async fn status_pending_line_only_while_queued() {
    let mut w = setup().await;
    plant(&w);

    // Online with an empty queue: no pending line.
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "online status: {stderr}");
    assert!(!out.contains("pending"), "online status is silent: {out}");

    w.server.shutdown().await;

    fs::write(w.garden.join("notes/todo.txt"), b"buy mulch\n").unwrap();
    let (code, out, stderr) = bed(
        &w.garden,
        &[
            "commit",
            "-m",
            "offline for status",
            "--author",
            "Offline Gardener <offline@bed.example>",
        ],
    );
    assert_eq!(code, 0, "offline commit: {stderr}\n{out}");

    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "offline status: {stderr}");
    assert!(
        out.contains("1 pending commit(s) not yet on the server — run bed sync"),
        "offline status shows the queue line: {out}"
    );
    assert!(
        out.lines().next().unwrap().starts_with("1 pending"),
        "the pending line comes first: {out}"
    );

    w.server.shutdown().await;
}

/// M15 publish CAS unchanged after sync: publishing the synced tip
/// fast-forwards `main` successfully.
#[tokio::test]
async fn publish_after_sync_fast_forwards() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    w.server.shutdown().await;

    fs::write(w.garden.join("notes/todo.txt"), b"buy seeds and publish\n").unwrap();
    let (code, out, stderr) = bed(
        &w.garden,
        &[
            "commit",
            "-m",
            "offline then publish",
            "--author",
            "Offline Gardener <offline@bed.example>",
        ],
    );
    assert_eq!(code, 0, "offline commit: {stderr}\n{out}");
    let tip = current_cs(&w.garden);

    let server = Server::spawn(w._root.parent().unwrap()).await;
    repoint_server(&w.garden, &server.http());
    w.server = server;

    let (code, out, stderr) = bed(&w.garden, &["sync"]);
    assert_eq!(code, 0, "sync: {stderr}\n{out}");

    // The publish CAS behaves as it always does: fast-forward main.
    let (code, out, stderr) = bed(&w.garden, &["publish"]);
    assert_eq!(code, 0, "publish after sync: {stderr}\n{out}");
    assert!(out.contains("published"), "publish output: {out}");

    // main really points at the synced tip.
    let repo = Repo::open(&w._root).expect("repo reopens");
    let main = repo.get_ref("main").expect("get_ref").expect("main exists");
    assert_eq!(main, tip);
    assert_ne!(main, base);

    w.server.shutdown().await;
}

/// M15 connectivity nuance: while the server is reachable, its errors are
/// surfaced, never queued. A reachable-but-refusing server (publish's CAS
/// refusal at main's tip) must fail loudly and move no queue state. The
/// classification itself (only connect-level failures queue) is
/// unit-tested in `queue.rs`'s `only_connect_errors_queue`.
#[tokio::test]
async fn reachable_server_errors_surface_not_queue() {
    let mut w = setup().await;
    plant(&w);
    let base = w.head;

    // Move main ahead through the server library (uncounted, no working
    // copy involvement): publishing the working copy's now-older changeset
    // is a server-level CAS refusal — not a connect failure — and must
    // reach the user, never the queue.
    let ahead = repo_step_ahead(&w._root, base);
    let (code, _, stderr) = bed(&w.garden, &["publish"]);
    assert_eq!(code, 1, "publish behind main refuses: {stderr}");
    assert!(
        stderr.contains("cannot publish"),
        "the server's refusal reached the user: {stderr}"
    );
    assert_eq!(current_cs(&w.garden), base, "no state moved");
    let (code, out, stderr) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(!out.contains("pending"), "nothing was queued: {out}");
    let _ = ahead;

    w.server.shutdown().await;
}

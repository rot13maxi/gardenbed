//! Verification M2.5 (server smoke test): spawns the built
//! `gardenbed-server` binary on an ephemeral port with a temp repo root and
//! drives it through the typed client — object access, the change graph,
//! refs, graft/harvest, blame ranges, the empty-`repo`
//! rejection, and the request counters.

use std::collections::BTreeMap;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::{CasOutcome, GardenbedClient};
use tonic::Status;

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

/// One change with explicit op, mode, and rename source — the graft
/// edge-case fixtures need more than `add`'s plain adds.
fn chg(path: &str, op: Op, content: Option<&[u8]>, mode: Mode, src: Option<&str>) -> Change {
    Change {
        path: path.into(),
        op,
        blob: content.map(blob_id),
        src_path: src.map(str::to_owned),
        mode,
    }
}

/// One changeset fixture: message + fixed author/committer identity.
fn cs(
    parents: Vec<gardenbed_model::Id>,
    changes: Vec<Change>,
    message: &str,
    secs: u64,
) -> Changeset {
    Changeset {
        parents,
        changes,
        message: message.into(),
        author: identity("amy", secs),
        committer: identity("amy", secs),
        signature: None,
    }
}

/// Sows `cs` with its inline blobs, returning the assigned id.
async fn sow(
    client: &GardenbedClient,
    repo: &str,
    cs: &Changeset,
    blobs: &[&[u8]],
) -> gardenbed_model::Id {
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
    /// Spawns the built binary on an ephemeral port and waits for its
    /// `listening on <addr>` stdout line.
    async fn spawn(root: &std::path::Path) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::{Duration, timeout};

        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("server binary spawns");
        let stdout = child.stdout.take().expect("stdout piped");
        let mut lines = BufReader::new(stdout).lines();
        let line = timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("server announces itself within 30s")
            .expect("server stdout open")
            .expect("server prints a line");
        let addr = line
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("expected a listening line, got {line:?}"))
            .to_owned();
        ServerProc { child, addr }
    }

    /// A connected client.
    async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&format!("http://{}", self.addr))
            .await
            .expect("client connects")
    }

    /// Terminates the server and waits for it to exit.
    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// A blob's content streamed back through GetBlob, reassembled.
async fn read_blob(client: &GardenbedClient, repo: &str, id: gardenbed_model::Id) -> Vec<u8> {
    use tokio_stream::StreamExt;
    let mut stream = client.get_blob(repo, id).await.expect("GetBlob ok");
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.expect("chunk ok").data);
    }
    out
}

/// Collects a client-streaming response into a Vec.
async fn drain<T>(stream: Result<tonic::Streaming<T>, Status>) -> Vec<T>
where
    T: Send + 'static,
{
    use tokio_stream::StreamExt;
    let mut stream = stream.expect("stream opens");
    let mut out = Vec::new();
    while let Some(item) = stream.next().await {
        out.push(item.expect("item ok"));
    }
    out
}

#[tokio::test]
async fn server_smoke_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";

    // ------------------------------------------------------ write path
    let a = b"hello garden\n".to_vec();
    let b = b"world\n".to_vec();
    let cs_root = Changeset {
        parents: vec![],
        changes: vec![add("a.txt", &a), add("dir/b.txt", &b)],
        message: "root".into(),
        author: identity("amy", 10),
        committer: identity("amy", 10),
        signature: None,
    };
    let client = server.client().await;

    // An empty repo name must be rejected before anything else happens.
    let err = client
        .get_stats("")
        .await
        .expect_err("empty repo name rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    let id_root = client
        .create_changeset(
            repo,
            &cs_root,
            [(blob_id(&a), a.clone()), (blob_id(&b), b.clone())],
        )
        .await
        .expect("root changeset created");

    // ------------------------------------------------- object access
    // The wire round-trip of a changeset.
    let got = client
        .get_changeset(repo, id_root)
        .await
        .expect("cs readable");
    assert_eq!(got, cs_root, "GetChangeset round-trips the canonical model");

    // The root tree and one level of its entries (two GetTree calls).
    let tree_root = client
        .root_tree(repo, id_root)
        .await
        .expect("RootTree ok")
        .expect("non-empty root tree");
    let tree = client.get_tree(repo, tree_root).await.expect("GetTree ok");
    let names: Vec<&str> = tree.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["a.txt", "dir"], "canonical order");
    let child_tree = tree
        .entries
        .iter()
        .find(|e| e.name == "dir")
        .and_then(|e| e.tree)
        .expect("dir entry carries a tree id");
    let subtree = client.get_tree(repo, child_tree).await.expect("GetTree ok");
    assert_eq!(subtree.entries.len(), 1);
    assert_eq!(subtree.entries[0].name, "b.txt");

    // Blob content comes back byte-identical through the stream.
    let got_a = read_blob(&client, repo, blob_id(&a)).await;
    assert_eq!(got_a, a, "GetBlob round-trips the content");

    // ------------------------------------------------------ log/ancestry
    // A child changeset modifying a.txt.
    let a2 = b"hello garden v2\n".to_vec();
    let cs_child = Changeset {
        parents: vec![id_root],
        changes: vec![Change {
            path: "a.txt".into(),
            op: Op::Modify,
            blob: Some(blob_id(&a2)),
            src_path: None,
            mode: Mode::Regular,
        }],
        message: "update a".into(),
        author: identity("bob", 20),
        committer: identity("bob", 20),
        signature: None,
    };
    let id_child = client
        .create_changeset(repo, &cs_child, [(blob_id(&a2), a2.clone())])
        .await
        .expect("child changeset created");

    assert_eq!(
        client
            .merge_base(repo, id_root, id_child)
            .await
            .expect("base"),
        Some(id_root)
    );
    assert!(
        client
            .is_ancestor(repo, id_root, id_child)
            .await
            .expect("ancestor")
    );
    assert!(
        !client
            .is_ancestor(repo, id_child, id_root)
            .await
            .expect("not ancestor")
    );

    // Diff root → child: exactly the a.txt modify.
    let diff = drain(client.diff(repo, id_root, id_child).await).await;
    assert_eq!(diff.len(), 1, "one changed path");
    assert_eq!(diff[0].path, "a.txt");
    assert_eq!(diff[0].op as i32, 1, "modify");

    // Log walks all ancestry, newest first.
    let log = drain(client.log(repo, id_child, 0).await).await;
    assert_eq!(log.len(), 2, "head + root");
    assert_eq!(
        log[0].cs_id,
        gardenbed_protocol::convert::id_to_bytes(id_child)
    );
    assert_eq!(
        log[1].cs_id,
        gardenbed_protocol::convert::id_to_bytes(id_root)
    );
    let limited = drain(client.log(repo, id_child, 1).await).await;
    assert_eq!(limited.len(), 1, "limit respected");

    // ----------------------------------------------------------- refs
    let won = client
        .cas_ref(repo, "feature", None, id_child)
        .await
        .expect("cas");
    assert_eq!(won, CasOutcome::Won);
    // A stale CAS comes back as a conflict result, never an error.
    let stale = client
        .cas_ref(repo, "feature", Some(id_root), id_child)
        .await
        .expect("cas");
    assert!(matches!(stale, CasOutcome::Conflict { current } if current == id_child));
    assert_eq!(
        client.get_ref(repo, "feature").await.expect("ref"),
        Some(id_child)
    );
    let refs = drain(client.list_refs(repo).await).await;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "refs/heads/feature");

    // Harvest fast-forwards "main"; a non-descendant is refused.
    let harvested = client.harvest(repo, id_child).await.expect("harvest");
    assert_eq!(harvested, id_child);
    assert_eq!(
        client.get_ref(repo, "main").await.expect("main"),
        Some(id_child)
    );

    // ----------------------------------------------------------- graft
    // An unrelated branch root; grafting the child onto it replays its
    // changes as a fresh single-parent changeset.
    let c = b"unrelated\n".to_vec();
    let cs_branch = Changeset {
        parents: vec![],
        changes: vec![add("other.txt", &c)],
        message: "branch".into(),
        author: identity("cat", 30),
        committer: identity("cat", 30),
        signature: None,
    };
    let id_branch = client
        .create_changeset(repo, &cs_branch, [(blob_id(&c), c.clone())])
        .await
        .expect("branch changeset created");
    let grafted = client
        .graft(repo, id_child, id_branch)
        .await
        .expect("graft succeeds (no conflicts)");
    let got_graft = client
        .get_changeset(repo, grafted)
        .await
        .expect("graft readable");
    assert_eq!(got_graft.parents, vec![id_branch], "graft has one parent");
    assert_eq!(
        got_graft.changes, cs_child.changes,
        "changes replayed verbatim"
    );
    let graft_tree = client.root_tree(repo, grafted).await.expect("graft tree");
    let grafted_files = client
        .get_tree(repo, graft_tree.expect("non-empty"))
        .await
        .expect("tree");
    let mut graft_names: Vec<&str> = grafted_files
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    graft_names.sort_unstable();
    // The graft replays the delta only: the branch's other.txt plus the
    // replayed a.txt modify.
    assert_eq!(
        graft_names,
        vec!["a.txt", "other.txt"],
        "replayed tree merges"
    );

    // A conflicting graft fails with FAILED_PRECONDITION listing the path:
    // this changeset deletes "other.txt", which exists on its own parent
    // (so it stores fine) but not on the graft target's tree.
    let cs_bad = Changeset {
        parents: vec![id_branch],
        changes: vec![Change {
            path: "other.txt".into(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        }],
        message: "bad".into(),
        author: identity("eve", 40),
        committer: identity("eve", 40),
        signature: None,
    };
    let id_bad = client
        .create_changeset(repo, &cs_bad, [])
        .await
        .expect("valid against its own parent");
    let err = client
        .graft(repo, id_bad, id_root)
        .await
        .expect_err("graft conflicts on the missing path");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("other.txt"),
        "conflict names the path: {err}"
    );

    // A modify replays onto a new parent whose copy is the pre-image: the
    // text-merge-free three-way rule. This branch root carries a.txt exactly
    // as id_root had it, so the child's a.txt update replays cleanly over it.
    let cs_branch_pre = Changeset {
        parents: vec![],
        changes: vec![add("a.txt", &a), add("other.txt", &c)],
        message: "branch with the pre-image a.txt".into(),
        author: identity("cat", 30),
        committer: identity("cat", 30),
        signature: None,
    };
    let id_branch_pre = client
        .create_changeset(
            repo,
            &cs_branch_pre,
            [(blob_id(&a), a.clone()), (blob_id(&c), c.clone())],
        )
        .await
        .expect("pre-image branch created");
    let grafted_pre = client
        .graft(repo, id_child, id_branch_pre)
        .await
        .expect("graft over an untouched pre-image succeeds");
    let pre_tree = client
        .root_tree(repo, grafted_pre)
        .await
        .expect("graft tree")
        .expect("non-empty");
    let mut pre_files: Vec<String> = client
        .get_tree(repo, pre_tree)
        .await
        .expect("tree")
        .entries
        .iter()
        .map(|e| e.name.clone())
        .collect();
    pre_files.sort_unstable();
    let pre_names: Vec<&str> = pre_files.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        pre_names,
        vec!["a.txt", "other.txt"],
        "replayed modify overwrote the pre-image copy"
    );

    // But a copy that diverged from the pre-image is a hard conflict: the
    // replay would clobber a concurrent change.
    let diverged = b"divergent\n".to_vec();
    let cs_branch_div = Changeset {
        parents: vec![],
        changes: vec![add("a.txt", &diverged), add("other.txt", &c)],
        message: "branch with a diverged a.txt".into(),
        author: identity("cat", 30),
        committer: identity("cat", 30),
        signature: None,
    };
    let id_branch_div = client
        .create_changeset(
            repo,
            &cs_branch_div,
            [(blob_id(&diverged), diverged), (blob_id(&c), c)],
        )
        .await
        .expect("diverged branch created");
    let err = client
        .graft(repo, id_child, id_branch_div)
        .await
        .expect_err("graft onto a diverged copy conflicts");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("a.txt"),
        "conflict names the path: {err}"
    );

    // ---------------------------------------------------------- blame
    // First-parent lineage blame streams coalesced ranges: the child
    // rewrote a.txt's only line, so it attributes to the child changeset.
    let ranges = drain(client.blame(repo, id_child, "a.txt").await).await;
    assert_eq!(ranges.len(), 1, "one coalesced range: {ranges:?}");
    assert_eq!(ranges[0].lo, 1);
    assert_eq!(ranges[0].hi, 1);
    assert_eq!(
        ranges[0].cs_id,
        gardenbed_protocol::convert::id_to_bytes(id_child)
    );
    // A path that does not exist at that changeset is a clean `not_found`.
    let err = client
        .blame(repo, id_child, "no-such-path.txt")
        .await
        .expect_err("blame of a missing path");
    assert_eq!(err.code(), tonic::Code::NotFound);

    // --------------------------------------------------------- counters
    // Exactly the four counted RPCs were exercised, in these amounts:
    // create_changeset ×6 (root, child, branch, bad, pre-image branch,
    // diverged branch); GetTree ×4 (root, child subtree, graft tree,
    // pre-image graft tree); GetBlob ×1; PutBlob ×0 so far.
    let stats: BTreeMap<String, u64> = client.get_stats(repo).await.expect("stats");
    assert_eq!(stats.get("create_changeset"), Some(&6), "stats: {stats:?}");
    assert_eq!(stats.get("get_tree"), Some(&4), "stats: {stats:?}");
    assert_eq!(stats.get("get_blob"), Some(&1), "stats: {stats:?}");
    assert_eq!(stats.get("put_blob"), Some(&0), "stats: {stats:?}");

    // PutBlob (> 1 MiB bodies) streams a large blob and counts it.
    let big: Vec<u8> = (0..(1 << 20) + 7).map(|i| (i % 251) as u8).collect();
    let big_id = client
        .put_blob(repo, tokio_stream::once(big.clone()))
        .await
        .expect("PutBlob ok");
    assert_eq!(
        big_id,
        blob_id(&big),
        "PutBlob replies with the content address"
    );
    let got_big = read_blob(&client, repo, big_id).await;
    assert_eq!(got_big, big, "the big blob round-trips");

    let stats = client.get_stats(repo).await.expect("stats");
    assert_eq!(stats.get("put_blob"), Some(&1), "stats: {stats:?}");
    assert_eq!(stats.get("get_blob"), Some(&2), "stats: {stats:?}");
    assert_eq!(stats.get("create_changeset"), Some(&6));
    assert_eq!(stats.get("get_tree"), Some(&4));

    server.shutdown().await;
}

/// Truncated changeset ids resolve through the shared revspec resolver
/// (ROUGH-EDGE-4.1): hex prefixes of ≥ 8 chars work on every id-taking
/// surface via `GetRef`'s prefix fallback, with ref-wins precedence and
/// the three error shapes pinned end to end. The seedbed is real SQLite
/// here, so the ambiguous case plants a twin-id row beside a genuine
/// changeset (the ids are hash-driven and cannot be chosen).
#[tokio::test]
async fn revspec_prefix_resolution_end_to_end() {
    use tonic::Code;

    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;

    // A two-commit history, `main` harvested onto the child.
    let a = b"hello garden\n".to_vec();
    let a2 = b"hello garden v2\n".to_vec();
    let cs_root = cs(
        vec![],
        vec![add("a.txt", &a), add("dir/b.txt", b"world\n")],
        "root",
        10,
    );
    let id_root = sow(&client, repo, &cs_root, &[&a, b"world\n"]).await;
    let cs_child = cs(
        vec![id_root],
        vec![chg("a.txt", Op::Modify, Some(&a2), Mode::Regular, None)],
        "update a",
        20,
    );
    let id_child = sow(&client, repo, &cs_child, &[&a2]).await;
    client.harvest(repo, id_child).await.expect("harvest");

    let hex_root = id_root.to_hex();

    // An 8-hex prefix resolves; so do bed's 16-hex `short()` form and an
    // UPPERCASE spelling (canonical spec §1: parsers accept any case).
    let p8 = &hex_root[..8];
    assert_eq!(
        client.get_ref(repo, p8).await.expect("prefix"),
        Some(id_root),
        "8-hex prefix resolves"
    );
    assert_eq!(
        client.get_ref(repo, &hex_root[..16]).await.expect("short"),
        Some(id_root),
        "the 16-hex short form resolves"
    );
    assert_eq!(
        client
            .get_ref(repo, &p8.to_uppercase())
            .await
            .expect("upper"),
        Some(id_root),
        "prefixes are case-insensitive"
    );

    // GetChangeset through a prefix: resolve, then fetch the changeset.
    let resolved = client
        .get_ref(repo, p8)
        .await
        .expect("prefix")
        .expect("resolved");
    let got = client
        .get_changeset(repo, resolved)
        .await
        .expect("readable");
    assert_eq!(got, cs_root, "the prefix resolved to the root changeset");

    // Diff takes prefixes on both sides (a → b is still the a.txt modify).
    let p_child = &id_child.to_hex()[..8];
    let a_id = client
        .get_ref(repo, p8)
        .await
        .expect("a")
        .expect("resolved");
    let b_id = client
        .get_ref(repo, p_child)
        .await
        .expect("b")
        .expect("resolved");
    let diff = drain(client.diff(repo, a_id, b_id).await).await;
    assert_eq!(diff.len(), 1, "one changed path");
    assert_eq!(diff[0].path, "a.txt");

    // Blame through a prefix-resolved changeset.
    let ranges = drain(client.blame(repo, b_id, "a.txt").await).await;
    assert_eq!(ranges.len(), 1, "one range: {ranges:?}");
    assert_eq!(
        gardenbed_protocol::convert::id_from_bytes(&ranges[0].cs_id).expect("id"),
        id_child,
        "the modified line blames to the child"
    );

    // A full 64-hex token is never prefix-interpreted here: clients parse
    // those as ids before any lookup, so GetRef only ever sees ref names.
    assert_eq!(
        client.get_ref(repo, &hex_root).await.expect("full id"),
        None,
        "64-hex keeps today's ref-only behavior in GetRef"
    );

    // Precedence: an existing ref with an exactly hex-shaped name wins over
    // prefix interpretation — this one matches no changeset, so without
    // the ref-first rule it would be a `no ref or changeset` error.
    client
        .cas_ref(repo, "deadbeef01", None, id_child)
        .await
        .expect("cas");
    assert_eq!(
        client.get_ref(repo, "deadbeef01").await.expect("ref wins"),
        Some(id_child),
        "an existing ref with that exact name wins over prefix interpretation"
    );

    // Fewer than 8 hex chars: rejected with the minimum-length hint
    // (invalid_argument) once no ref of that name exists.
    let err = client
        .get_ref(repo, &hex_root[..6])
        .await
        .expect_err("too short");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message()
            .contains("changeset id prefix must be at least 8 hex chars"),
        "hint present: {err}"
    );

    // A well-formed prefix that matches neither a ref nor a changeset:
    // not_found, naming both failures.
    let err = client
        .get_ref(repo, "1d050f877")
        .await
        .expect_err("no match anywhere");
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), "no ref or changeset matching 1d050f877");

    // Ambiguity: plant a second changeset row whose id shares the root's
    // first 8 hex chars (hash-prefixed ids cannot be engineered, so the
    // twin is written beside a real row in the test's own seedbed — the
    // same offline surgery the chaos suite practices).
    let seedbed =
        rusqlite::Connection::open(dir.path().join("seedbed.sqlite3")).expect("seedbed opens");
    seedbed.busy_timeout(std::time::Duration::from_secs(5)).ok();
    let canonical: Vec<u8> = seedbed
        .query_row(
            "SELECT canonical FROM changesets WHERE id = ?1",
            [id_root.as_bytes()],
            |r| r.get(0),
        )
        .expect("root row present");
    let mut twin = *id_root.as_bytes();
    twin[31] ^= 0x01; // same 8-hex prefix, different id
    seedbed
        .execute(
            "INSERT INTO changesets(id, canonical) VALUES(?1, ?2)",
            rusqlite::params![&twin[..], &canonical],
        )
        .expect("twin row planted");
    let err = client.get_ref(repo, p8).await.expect_err("ambiguous now");
    assert_eq!(err.code(), Code::InvalidArgument);
    assert_eq!(
        err.message(),
        format!("prefix {p8} is ambiguous (2 changesets)")
    );

    server.shutdown().await;
}

/// The graft three-way rule's edge cases (FIX-A follow-up): writes are
/// admitted when the new parent's copy is a no-op or exactly the
/// pre-image — including mode-only changes, renames over a modified
/// source, delete/re-add interplay, symlink retargets and empty files —
/// while a genuinely diverged modify still fails and leaves zero partial
/// state.
#[tokio::test]
async fn graft_three_way_edge_cases() {
    use tonic::Code;

    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;

    // The shared root: one file per edge case.
    let cs_root = cs(
        vec![],
        vec![
            add("conflict.txt", b"orig\n"),
            add("gone.txt", b"gone\n"),
            chg("link", Op::Add, Some(b"old-target"), Mode::Symlink, None),
            add("mode.txt", b"mode\n"),
            add("ren.txt", b"ren\n"),
        ],
        "root",
        10,
    );
    let id_root = sow(
        &client,
        repo,
        &cs_root,
        &[b"orig\n", b"gone\n", b"old-target", b"mode\n", b"ren\n"],
    )
    .await;

    // (a) Mode-only change (regular → exec) over an identical pre-image:
    // the content is unchanged, the mode is the change, and the new
    // parent's copy is exactly the pre-image — admitted.
    let cs_mode = cs(
        vec![id_root],
        vec![chg(
            "mode.txt",
            Op::Modify,
            Some(b"mode\n"),
            Mode::Exec,
            None,
        )],
        "flip mode.txt to exec",
        20,
    );
    let id_mode = sow(&client, repo, &cs_mode, &[]).await;
    let cs_target_a = cs(
        vec![],
        vec![add("mode.txt", b"mode\n"), add("ta.txt", b"ta\n")],
        "branch with the pre-image mode.txt",
        30,
    );
    let id_target_a = sow(&client, repo, &cs_target_a, &[b"ta\n"]).await;
    let grafted_a = client
        .graft(repo, id_mode, id_target_a)
        .await
        .expect("mode-only graft over the pre-image replays");
    let tree_a = client
        .get_tree(
            repo,
            client
                .root_tree(repo, grafted_a)
                .await
                .expect("tree")
                .expect("non-empty"),
        )
        .await
        .expect("entries");
    assert_eq!(tree_entry_mode(&tree_a, "mode.txt"), Mode::Exec);
    assert_eq!(
        tree_entry_blob(&tree_a, "mode.txt"),
        Some(blob_id(b"mode\n"))
    );
    assert_eq!(tree_entry_mode(&tree_a, "ta.txt"), Mode::Regular);
    // The grafted changeset is exactly the deterministic replay.
    assert_eq!(grafted_a, expected_graft_id(&cs_mode, id_target_a));
    let got_a = client
        .get_changeset(repo, grafted_a)
        .await
        .expect("readable");
    assert_eq!(got_a.parents, vec![id_target_a], "one parent: the target");

    // (b) Rename interplay: the renamed source was ALSO modified on the
    // new main. The rename's source must merely exist (v0.1 semantics:
    // no text merge, the replayed rename carries its own blob), so this
    // graft succeeds and the modification rides out with the rename.
    let cs_rename = cs(
        vec![id_root],
        vec![chg(
            "renamed.txt",
            Op::Rename,
            Some(b"ren\n"),
            Mode::Regular,
            Some("ren.txt"),
        )],
        "rename ren.txt",
        40,
    );
    let id_rename = sow(&client, repo, &cs_rename, &[]).await;
    let cs_main_mod = cs(
        vec![id_root],
        vec![chg(
            "ren.txt",
            Op::Modify,
            Some(b"ren v2\n"),
            Mode::Regular,
            None,
        )],
        "modify the future rename source on main",
        50,
    );
    let id_main_mod = sow(&client, repo, &cs_main_mod, &[b"ren v2\n"]).await;
    let grafted_b = client
        .graft(repo, id_rename, id_main_mod)
        .await
        .expect("rename replays over a modified source");
    let tree_b = client
        .get_tree(
            repo,
            client
                .root_tree(repo, grafted_b)
                .await
                .expect("tree")
                .expect("non-empty"),
        )
        .await
        .expect("entries");
    assert!(
        tree_entry_blob(&tree_b, "ren.txt").is_none(),
        "the source path is gone"
    );
    assert_eq!(
        tree_entry_blob(&tree_b, "renamed.txt"),
        Some(blob_id(b"ren\n")),
        "the replayed rename carries its own (pre-main) blob"
    );

    // (c) Delete-then-re-add the same content, both directions. Re-add:
    // the changeset re-adds a path its own parent deleted, onto a target
    // that kept the original — an exact no-op write (same blob and mode).
    let cs_del = cs(
        vec![id_root],
        vec![chg("gone.txt", Op::Delete, None, Mode::Regular, None)],
        "delete gone.txt",
        50,
    );
    let id_del = sow(&client, repo, &cs_del, &[]).await;
    let cs_readd = cs(
        vec![id_del],
        vec![add("gone.txt", b"gone\n")],
        "re-add gone.txt",
        50,
    );
    let id_readd = sow(&client, repo, &cs_readd, &[]).await;
    let grafted_c1 = client
        .graft(repo, id_readd, id_root)
        .await
        .expect("re-add over the kept copy is a no-op");
    assert_eq!(
        client.root_tree(repo, grafted_c1).await.expect("tree"),
        client.root_tree(repo, id_root).await.expect("tree"),
        "the replayed re-add changed nothing"
    );
    // Delete: the target deleted and re-added the same content, so its
    // copy is again the pre-image and the delete replays cleanly.
    let cs_del_again = cs(
        vec![id_root],
        vec![chg("gone.txt", Op::Delete, None, Mode::Regular, None)],
        "delete gone.txt (again)",
        60,
    );
    let id_del_again = sow(&client, repo, &cs_del_again, &[]).await;
    let grafted_c2 = client
        .graft(repo, id_del_again, id_readd)
        .await
        .expect("delete over a re-added identical copy replays");
    assert_eq!(
        client.root_tree(repo, grafted_c2).await.expect("tree"),
        client.root_tree(repo, id_del).await.expect("tree"),
        "the replayed delete lands where the first delete did"
    );

    // (d) Symlink target change onto an identical pre-image.
    let cs_link = cs(
        vec![id_root],
        vec![chg(
            "link",
            Op::Modify,
            Some(b"new-target"),
            Mode::Symlink,
            None,
        )],
        "retarget the symlink",
        60,
    );
    let id_link = sow(&client, repo, &cs_link, &[b"new-target"]).await;
    let cs_target_d = cs(
        vec![],
        vec![
            chg("link", Op::Add, Some(b"old-target"), Mode::Symlink, None),
            add("td.txt", b"td\n"),
        ],
        "branch with the pre-image symlink",
        70,
    );
    let id_target_d = sow(&client, repo, &cs_target_d, &[b"td\n"]).await;
    let grafted_d = client
        .graft(repo, id_link, id_target_d)
        .await
        .expect("symlink retarget over the pre-image replays");
    let tree_d = client
        .get_tree(
            repo,
            client
                .root_tree(repo, grafted_d)
                .await
                .expect("tree")
                .expect("non-empty"),
        )
        .await
        .expect("entries");
    assert_eq!(tree_entry_mode(&tree_d, "link"), Mode::Symlink);
    assert_eq!(
        tree_entry_blob(&tree_d, "link"),
        Some(blob_id(b"new-target"))
    );

    // (e) Empty-file creation onto an existing empty file: an exact no-op.
    let cs_empty = cs(
        vec![id_root],
        vec![add("newempty.txt", b"")],
        "add an empty file",
        70,
    );
    let id_empty = sow(&client, repo, &cs_empty, &[b""]).await;
    let cs_target_e = cs(
        vec![],
        vec![add("newempty.txt", b"")],
        "empty file here",
        80,
    );
    let id_target_e = sow(&client, repo, &cs_target_e, &[b""]).await;
    let grafted_e = client
        .graft(repo, id_empty, id_target_e)
        .await
        .expect("empty-file creation over an existing empty file");
    assert_eq!(
        client.root_tree(repo, grafted_e).await.expect("tree"),
        client.root_tree(repo, id_target_e).await.expect("tree"),
        "the replayed creation changed nothing"
    );

    // (f) A genuinely conflicting modify still FAILS_PRECONDITION naming
    // the path — and a failed graft leaves zero partial state: the refs
    // are untouched and the would-be grafted changeset does not exist.
    let cs_conflict = cs(
        vec![id_root],
        vec![chg(
            "conflict.txt",
            Op::Modify,
            Some(b"new\n"),
            Mode::Regular,
            None,
        )],
        "rewrite conflict.txt",
        90,
    );
    let id_conflict = sow(&client, repo, &cs_conflict, &[b"new\n"]).await;
    let cs_target_f = cs(
        vec![],
        vec![add("conflict.txt", b"divergent\n")],
        "diverged conflict.txt",
        90,
    );
    let id_target_f = sow(&client, repo, &cs_target_f, &[b"divergent\n"]).await;

    let refs_before = drain(client.list_refs(repo).await).await;
    let graft_err = client
        .graft(repo, id_conflict, id_target_f)
        .await
        .expect_err("conflicting graft refused");
    assert_eq!(graft_err.code(), Code::FailedPrecondition);
    assert!(
        graft_err.message().contains("conflict.txt"),
        "conflict names the path: {graft_err}"
    );
    // Zero partial state: refs identical, and the replayed changeset
    // (computable client-side — graft is deterministic) is absent.
    assert_eq!(
        drain(client.list_refs(repo).await).await,
        refs_before,
        "a failed graft leaves no ref behind"
    );
    let would_be = expected_graft_id(&cs_conflict, id_target_f);
    let missing = client
        .get_changeset(repo, would_be)
        .await
        .expect_err("no changeset was stored by the failed graft");
    assert_eq!(
        missing.code(),
        Code::NotFound,
        "no partial state: {missing}"
    );
    // Retrying fails identically — nothing half-written the first time.
    let retry = client
        .graft(repo, id_conflict, id_target_f)
        .await
        .expect_err("retry refuses identically");
    assert_eq!(retry.message(), graft_err.message());

    server.shutdown().await;
}

/// A tree entry's mode by name (`Mode::Regular` default when absent —
/// callers only read entries the fixture knows exist).
fn tree_entry_mode(tree: &gardenbed_model::Tree, name: &str) -> Mode {
    tree.entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("entry {name:?} present"))
        .mode
}

/// A tree entry's blob id, or `None` when the path is absent.
fn tree_entry_blob(tree: &gardenbed_model::Tree, name: &str) -> Option<gardenbed_model::Id> {
    tree.entries
        .iter()
        .find(|e| e.name == name)
        .and_then(|e| e.blob)
}

/// The id a graft of `cs` onto `new_parent` will deterministically get:
/// the replayed changeset is fully derived client-side, so a failed graft
/// can prove zero partial state by showing its id is absent.
fn expected_graft_id(cs: &Changeset, new_parent: gardenbed_model::Id) -> gardenbed_model::Id {
    use sha2::{Digest, Sha256};
    let replayed = Changeset {
        parents: vec![new_parent],
        changes: cs.changes.clone(),
        message: cs.message.clone(),
        author: cs.author.clone(),
        committer: cs.committer.clone(),
        signature: cs.signature.clone(),
    };
    let canonical = replayed.canonical_bytes().expect("canonical encodes");
    Id::from_bytes(Sha256::digest(&canonical).into())
}

//! Verification M2.5 (server smoke test): spawns the built
//! `gardenbed-server` binary on an ephemeral port with a temp repo root and
//! drives it through the typed client — object access, the change graph,
//! refs, graft/harvest, blame ranges, the empty-`repo`
//! rejection, and the request counters.

use std::collections::BTreeMap;

use gardenbed_model::{blob_id, Change, Changeset, Identity, Mode, Op};
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
        .create_changeset(repo, &cs_root, [(blob_id(&a), a.clone()), (blob_id(&b), b.clone())])
        .await
        .expect("root changeset created");

    // ------------------------------------------------- object access
    // The wire round-trip of a changeset.
    let got = client.get_changeset(repo, id_root).await.expect("cs readable");
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
        client.merge_base(repo, id_root, id_child).await.expect("base"),
        Some(id_root)
    );
    assert!(client.is_ancestor(repo, id_root, id_child).await.expect("ancestor"));
    assert!(!client
        .is_ancestor(repo, id_child, id_root)
        .await
        .expect("not ancestor"));

    // Diff root → child: exactly the a.txt modify.
    let diff = drain(client.diff(repo, id_root, id_child).await).await;
    assert_eq!(diff.len(), 1, "one changed path");
    assert_eq!(diff[0].path, "a.txt");
    assert_eq!(diff[0].op as i32, 1, "modify");

    // Log walks all ancestry, newest first.
    let log = drain(client.log(repo, id_child, 0).await).await;
    assert_eq!(log.len(), 2, "head + root");
    assert_eq!(log[0].cs_id, gardenbed_protocol::convert::id_to_bytes(id_child));
    assert_eq!(log[1].cs_id, gardenbed_protocol::convert::id_to_bytes(id_root));
    let limited = drain(client.log(repo, id_child, 1).await).await;
    assert_eq!(limited.len(), 1, "limit respected");

    // ----------------------------------------------------------- refs
    let won = client.cas_ref(repo, "feature", None, id_child).await.expect("cas");
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
    assert_eq!(refs[0].name, "feature");

    // Harvest fast-forwards "main"; a non-descendant is refused.
    let harvested = client.harvest(repo, id_child).await.expect("harvest");
    assert_eq!(harvested, id_child);
    assert_eq!(client.get_ref(repo, "main").await.expect("main"), Some(id_child));

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
    let got_graft = client.get_changeset(repo, grafted).await.expect("graft readable");
    assert_eq!(got_graft.parents, vec![id_branch], "graft has one parent");
    assert_eq!(got_graft.changes, cs_child.changes, "changes replayed verbatim");
    let graft_tree = client.root_tree(repo, grafted).await.expect("graft tree");
    let grafted_files = client
        .get_tree(repo, graft_tree.expect("non-empty"))
        .await
        .expect("tree");
    let mut graft_names: Vec<&str> = grafted_files.entries.iter().map(|e| e.name.as_str()).collect();
    graft_names.sort_unstable();
    // The graft replays the delta only: the branch's other.txt plus the
    // replayed a.txt modify.
    assert_eq!(graft_names, vec!["a.txt", "other.txt"], "replayed tree merges");

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
    assert!(err.message().contains("other.txt"), "conflict names the path: {err}");

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
    // create_changeset ×4 (root, child, branch, bad); GetTree ×3 (root,
    // child subtree, graft tree); GetBlob ×1; PutBlob ×0 so far.
    let stats: BTreeMap<String, u64> = client.get_stats(repo).await.expect("stats");
    assert_eq!(stats.get("create_changeset"), Some(&4), "stats: {stats:?}");
    assert_eq!(stats.get("get_tree"), Some(&3), "stats: {stats:?}");
    assert_eq!(stats.get("get_blob"), Some(&1), "stats: {stats:?}");
    assert_eq!(stats.get("put_blob"), Some(&0), "stats: {stats:?}");

    // PutBlob (> 1 MiB bodies) streams a large blob and counts it.
    let big: Vec<u8> = (0..(1 << 20) + 7).map(|i| (i % 251) as u8).collect();
    let big_id = client
        .put_blob(repo, tokio_stream::once(big.clone()))
        .await
        .expect("PutBlob ok");
    assert_eq!(big_id, blob_id(&big), "PutBlob replies with the content address");
    let got_big = read_blob(&client, repo, big_id).await;
    assert_eq!(got_big, big, "the big blob round-trips");

    let stats = client.get_stats(repo).await.expect("stats");
    assert_eq!(stats.get("put_blob"), Some(&1), "stats: {stats:?}");
    assert_eq!(stats.get("get_blob"), Some(&2), "stats: {stats:?}");
    assert_eq!(stats.get("create_changeset"), Some(&4));
    assert_eq!(stats.get("get_tree"), Some(&3));

    server.shutdown().await;
}

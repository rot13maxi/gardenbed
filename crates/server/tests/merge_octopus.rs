//! Octopus merge verification at the server boundary (the merges
//! lane): git-merge-octopus parity, verified against git 2.43 on this
//! box.
//!
//! 1. Fold shape: `bed merge main t1 t2 t3` folds sequentially; the
//!    merged tree carries every target's file; parents are
//!    (ours, t1, t2, t3).
//! 2. Up-to-date targets are SKIPPED: not parents, no tree change
//!    (git: "Already up to date with ...").
//! 3. First-round fast-forward: ours is an ancestor of the first
//!    target and nothing has merged — the target REPLACES ours as the
//!    first parent; ours is not a parent (git: "Fast-forwarding
//!    to:").
//! 4. Conflict refusal parity: ANY conflict in ANY round refuses with
//!    FAILED_PRECONDITION naming the round, the target, and the
//!    paths — no changeset is stored (git: exit 2 "Should not be
//!    doing an octopus" for non-last rounds, exit 1 for the last —
//!    both refusals, no commit).
//! 5. Concurrency: 16 racing identical octopus merges converge on one
//!    changeset id.
//! 6. Differential parity: a real git repository given the same
//!    history shape produces the same merged tree (path set + file
//!    contents) for the clean fold, and refuses for the conflicting
//!    fold.

use std::collections::BTreeMap;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::Repo;
use tokio_stream::StreamExt;

fn identity(name: &str, secs: u64) -> Identity {
    Identity {
        name: name.into(),
        email: format!("{name}@garden.example"),
        time_secs: 1_700_000_000 + secs,
        tz_min: 0,
    }
}

fn add(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

fn blobs(items: &[&[u8]]) -> Vec<(Id, Vec<u8>)> {
    items.iter().map(|b| (blob_id(b), b.to_vec())).collect()
}

/// A running `gardenbed-server` binary (same contract as merge.rs).
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    async fn spawn(root: &std::path::Path) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::{Duration, timeout};

        Repo::open(&root.join("garden")).expect("fixture repository opens");

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

    async fn client(&self) -> GardenbedClient {
        GardenbedClient::connect(&format!("http://{}", self.addr))
            .await
            .expect("client connects")
    }

    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// The plain fold fixture: root with one file; three targets each
/// adding their OWN file (disjoint — the fold is clean). Returns
/// (root, ours_child, [t1, t2, t3]).
async fn octo_targets(client: &GardenbedClient, repo: &str) -> (Id, Id, Vec<Id>) {
    let root = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![],
                changes: vec![add("base", b"base\n")],
                message: "root".into(),
                author: identity("amy", 0),
                committer: identity("amy", 0),
                signature: None,
            },
            blobs(&[b"base\n"]),
        )
        .await
        .expect("root created");
    let ours = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![add("ours", b"ours\n")],
                message: "ours".into(),
                author: identity("amy", 1),
                committer: identity("amy", 1),
                signature: None,
            },
            blobs(&[b"ours\n"]),
        )
        .await
        .expect("ours created");
    let mut targets = Vec::new();
    for (k, who) in [("t1", "bob"), ("t2", "cat"), ("t3", "dan")] {
        let t = client
            .create_changeset(
                repo,
                &Changeset {
                    parents: vec![root],
                    changes: vec![add(&format!("target_{k}"), format!("{k}\n").as_bytes())],
                    message: k.into(),
                    author: identity(who, 2),
                    committer: identity(who, 2),
                    signature: None,
                },
                blobs(&[format!("{k}\n").as_bytes()]),
            )
            .await
            .expect("target created");
        targets.push(t);
    }
    (root, ours, targets)
}

#[tokio::test]
async fn octopus_folds_sequentially_with_octopus_parents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, targets) = octo_targets(&client, repo).await;

    let (merged, up_to_date) = client
        .merge_octopus(
            repo,
            ours,
            &targets,
            "octopus",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect("octopus merges");
    assert!(!up_to_date);

    let cs = client.get_changeset(repo, merged).await.expect("readable");
    let mut expected_parents = vec![ours];
    expected_parents.extend(targets.iter().copied());
    assert_eq!(cs.parents, expected_parents, "octopus parents (ours, t1..t3)");

    // The merged tree carries every side's file.
    let root_tree = client
        .root_tree(repo, merged)
        .await
        .expect("root tree")
        .expect("non-empty tree");
    let tree = client.get_tree(repo, root_tree).await.expect("tree");
    let mut names: Vec<&str> = tree
        .entries
        .iter()
        .map(|e| e.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["base", "ours", "target_t1", "target_t2", "target_t3"],
        "every target's file is in the merged tree"
    );
    let _ = root;
    server.shutdown().await;
}

#[tokio::test]
async fn octopus_skips_up_to_date_targets() {
    // git: "Already up to date with <name>" — the target is NOT a
    // parent and does not change the tree.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, targets) = octo_targets(&client, repo).await;

    // t3 is an ancestor of t2's child? No — build the up-to-date case:
    // merge t1 into ours FIRST, then octopus-merge (t1, t2): t1 is
    // already an ancestor of ours.
    let (pre, _) = client
        .merge(
            repo,
            ours,
            targets[0],
            "pre",
            &identity("amy", 5),
            &identity("amy", 5),
        )
        .await
        .expect("pre merge");
    let (merged, _) = client
        .merge_octopus(
            repo,
            pre,
            &[targets[0], targets[1]],
            "octopus",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect("octopus merges");
    let cs = client.get_changeset(repo, merged).await.expect("readable");
    // t1 skipped: parents are (pre, t2) — NOT (pre, t1, t2).
    assert_eq!(cs.parents, vec![pre, targets[1]], "up-to-date target skipped from parents");

    // Every target up to date: up_to_date answer, nothing stored.
    // (Two up-to-date targets: t0 merged into pre above, and pre
    // itself — an ancestor of itself.)
    let (id, up) = client
        .merge_octopus(
            repo,
            pre,
            &[targets[0], pre],
            "noop",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect("all-up-to-date octopus answers");
    assert!(up, "all-up-to-date targets answer up_to_date");
    assert_eq!(id, pre);
    let _ = root;
    server.shutdown().await;
}

#[tokio::test]
async fn octopus_first_round_fast_forward_replaces_ours() {
    // git: "Fast-forwarding to: <name>" — ours is an ancestor of the
    // first target, so the target replaces ours as the first parent
    // and ours is NOT a parent.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, targets) = octo_targets(&client, repo).await;

    // ours is a child of root; make a target that DESCENDS from ours
    // so ours is its ancestor.
    let ff = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![ours],
                changes: vec![add("ff", b"ff\n")],
                message: "ff".into(),
                author: identity("eve", 3),
                committer: identity("eve", 3),
                signature: None,
            },
            blobs(&[b"ff\n"]),
        )
        .await
        .expect("ff target created");

    let (merged, _) = client
        .merge_octopus(
            repo,
            ours,
            &[ff, targets[1]],
            "octopus ff",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect("octopus merges");
    let cs = client.get_changeset(repo, merged).await.expect("readable");
    // FF round: parents = (ff, t2) — ours is NOT among them.
    assert_eq!(
        cs.parents,
        vec![ff, targets[1]],
        "first-round FF replaces ours as first parent; ours is not a parent"
    );
    let _ = root;
    server.shutdown().await;
}

#[tokio::test]
async fn octopus_refuses_on_any_conflict_with_round_and_paths() {
    // git: ANY conflict refuses the whole octopus — exit 2 ("Should
    // not be doing an octopus") when the conflict is not in the last
    // round, exit 1 when it is; NO commit either way. The refusal
    // names the round and the conflicting paths.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, targets) = octo_targets(&client, repo).await;

    // Two conflicting targets: both add the SAME path differently.
    let c1 = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![add("clash", b"one\n")],
                message: "c1".into(),
                author: identity("bob", 2),
                committer: identity("bob", 2),
                signature: None,
            },
            blobs(&[b"one\n"]),
        )
        .await
        .expect("c1 created");
    let c2 = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![add("clash", b"two\n")],
                message: "c2".into(),
                author: identity("cat", 2),
                committer: identity("cat", 2),
                signature: None,
            },
            blobs(&[b"two\n"]),
        )
        .await
        .expect("c2 created");

    // Non-last-round conflict: (t1, c1, c2, t2) — the c1/c2 clash
    // surfaces in round 2 of 4.
    let err = client
        .merge_octopus(
            repo,
            ours,
            &[targets[0], c1, c2, targets[1]],
            "bad octopus",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect_err("non-last-round conflict refuses");
    assert_eq!(
        err.code(),
        tonic::Code::FailedPrecondition,
        "octopus conflict is FAILED_PRECONDITION: {err}"
    );
    let msg = err.message();
    assert!(msg.contains("octopus"), "refusal names the octopus: {msg}");
    assert!(msg.contains("round"), "refusal names the round: {msg}");
    assert!(msg.contains("clash"), "refusal names the path: {msg}");

    // Last-round conflict: (t1, c1) with c1 clashing with t1? Use
    // (t1, c1, c2) — the clash is in the LAST round.
    let err2 = client
        .merge_octopus(
            repo,
            ours,
            &[targets[0], c1, c2],
            "last-round bad",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect_err("last-round conflict refuses");
    assert_eq!(err2.code(), tonic::Code::FailedPrecondition);

    // Nothing was stored: ours' history is unchanged — no octopus
    // changeset is reachable from ours.
    let mut stream = client
        .log(repo, ours, 0)
        .await
        .expect("log opens");
    let mut saw_octopus = false;
    while let Some(item) = stream.next().await {
        let summary = item.expect("log item ok");
        if summary.message.contains("octopus") {
            saw_octopus = true;
        }
    }
    assert!(
        !saw_octopus,
        "no octopus changeset was stored on refusal"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn octopus_concurrent_identical_merges_converge_on_one_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, targets) = octo_targets(&client, repo).await;

    let mut handles = Vec::new();
    for _ in 0..16 {
        let c = server.client().await;
        let ts = targets.clone();
        handles.push(tokio::spawn(async move {
            c.merge_octopus(
                repo,
                ours,
                &ts,
                "octopus",
                &identity("amy", 9),
                &identity("amy", 9),
            )
            .await
            .expect("merge ok")
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.expect("task joined").0);
    }
    let first = ids[0];
    assert!(
        ids.iter().all(|id| *id == first),
        "all 16 concurrent octopus merges converge on one id, got {ids:?}"
    );
    let _ = root;
    server.shutdown().await;
}

#[tokio::test]
async fn octopus_matches_git_tree_on_clean_fold() {
    // Differential parity: the same history shape in a real git
    // repository, merged by git's own octopus, produces the same
    // path set and file contents.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (_root, ours, targets) = octo_targets(&client, repo).await;

    let (merged, _) = client
        .merge_octopus(
            repo,
            ours,
            &targets,
            "octopus",
            &identity("amy", 9),
            &identity("amy", 9),
        )
        .await
        .expect("octopus merges");

    // The gardenbed merged tree, flattened (this fixture is one level).
    let root_tree = client
        .root_tree(repo, merged)
        .await
        .expect("root tree")
        .expect("non-empty tree");
    let tree = client.get_tree(repo, root_tree).await.expect("tree");
    let mut garden: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for e in &tree.entries {
        assert!(e.tree.is_none(), "fixture is flat: {}", e.name);
        let blob = e.blob.expect("file entry carries a blob");
        let mut stream = client.get_blob(repo, blob).await.expect("blob stream");
        let mut content = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("chunk ok");
            content.extend_from_slice(&chunk.data);
        }
        garden.insert(e.name.clone(), content);
    }

    // git's answer for the same shape.
    let gdir = tempfile::tempdir().expect("git tempdir");
    let g = |args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(gdir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .env("LANG", "C");
        for (k, _) in std::env::vars() {
            if k.starts_with("GIT_") {
                cmd.env_remove(&k);
            }
        }
        let out = cmd.args(args).output().expect("git runs");
        assert!(
            out.status.success(),
            "git {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
    };
    g(&["init", "-q", "-b", "main", "."]);
    g(&["config", "user.email", "t@g.example"]);
    g(&["config", "user.name", "T"]);
    std::fs::write(gdir.path().join("base"), b"base\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-qm", "root"]);
    // ours
    std::fs::write(gdir.path().join("ours"), b"ours\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-qm", "ours"]);
    // t1..t3 off the root
    for (name, content) in [("target_t1", "t1\n"), ("target_t2", "t2\n"), ("target_t3", "t3\n")] {
        g(&["checkout", "-q", "HEAD~1"]);
        std::fs::write(gdir.path().join(name), content).unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", name]);
        g(&["branch", "-f", name]);
    }
    g(&["checkout", "-q", "main"]);
    g(&["merge", "-m", "octopus", "target_t1", "target_t2", "target_t3"]);

    let listing = {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(gdir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .args(["ls-tree", "-r", "HEAD"])
            .output()
            .expect("ls-tree runs")
    };
    let text = String::from_utf8_lossy(&listing.stdout).into_owned();
    let mut git: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for line in text.lines() {
        let (head, path) = line.split_once('\t').expect("tab");
        let sha = head.split_whitespace().nth(2).expect("sha");
        let blob = {
            let mut cmd = std::process::Command::new("git");
            cmd.current_dir(gdir.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("LC_ALL", "C")
                .env("LANG", "C")
                .args(["cat-file", "blob", sha])
                .output()
                .expect("cat-file runs")
                .stdout
        };
        git.insert(path.to_owned(), blob);
    }
    assert_eq!(garden, git, "gardenbed octopus tree equals git's");
    server.shutdown().await;
}

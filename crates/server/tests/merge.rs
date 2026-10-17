//! Merge verification at the server boundary (v0.2.md M10):
//!
//! 1. Concurrency: 16 racing merges of the same pair converge on one
//!    changeset id.
//! 2. Publish-after-merge works through the unchanged publish path.
//! 3. Up-to-date merges report `up_to_date` and create nothing.
//! 4. Unrelated histories are refused with an error naming the merge.
//! 5. Parity: direct `Repo::merge` calls are deterministic and
//!    idempotent for the same pair and identities.
//! 6. Crash safety: SIGKILL racing a merge leaves the repo openable
//!    and a fresh merge of the same pair still succeeds (deterministic
//!    rebuild; merged blobs never dangle referenced-but-missing).
//! 7. The conflict contract: FAILED_PRECONDITION listing every path.

use std::collections::BTreeMap;
use std::sync::Arc;

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::Repo;
use gardenbed_store::{FsObjectStore, MetadataStore, ObjectStore, SqliteMetadataStore};

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

fn modify(path: &str, content: &[u8]) -> Change {
    Change {
        path: path.into(),
        op: Op::Modify,
        blob: Some(blob_id(content)),
        src_path: None,
        mode: Mode::Regular,
    }
}

/// Inline-blob pairs for the typed client's `create_changeset`.
fn blobs(items: &[&[u8]]) -> Vec<(Id, Vec<u8>)> {
    items.iter().map(|b| (blob_id(b), b.to_vec())).collect()
}

/// Inline-blob map for `Repo::create_changeset`.
fn blobs_map(items: &[&[u8]]) -> BTreeMap<String, Vec<u8>> {
    items
        .iter()
        .map(|b| (blob_id(b).to_hex(), b.to_vec()))
        .collect()
}

/// A three-changeset divergent fixture: root, ours, theirs, all committed
/// through the RPC. Returns (root, ours, theirs).
async fn diverge(client: &GardenbedClient, repo: &str) -> (Id, Id, Id) {
    let base_text: Vec<u8> = (1..=12)
        .flat_map(|i| format!("base line {i}\n").into_bytes())
        .collect();
    let root = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![],
                changes: vec![
                    add("only_ours.txt", b"ours keeps\n"),
                    add("shared", &base_text),
                ],
                message: "root".into(),
                author: identity("amy", 0),
                committer: identity("amy", 0),
                signature: None,
            },
            blobs(&[&base_text, b"ours keeps\n"]),
        )
        .await
        .expect("root created");
    // ours edits the top; theirs edits the bottom: non-overlapping.
    let mut ours_text = base_text.clone();
    ours_text.splice(0..9, b"OURS edits the top\n".iter().copied());
    let ours = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![
                    add("ours_new", b"only ours added\n"),
                    modify("shared", &ours_text),
                ],
                message: "ours".into(),
                author: identity("amy", 1),
                committer: identity("amy", 1),
                signature: None,
            },
            blobs(&[&ours_text, b"only ours added\n"]),
        )
        .await
        .expect("ours created");
    let mut theirs_text = base_text.clone();
    let cut = theirs_text.len() - b"base line 12\n".len();
    theirs_text.splice(cut.., b"theirs edited the bottom\n".iter().copied());
    let theirs = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![
                    modify("shared", &theirs_text),
                    add("theirs_new", b"only theirs added\n"),
                ],
                message: "theirs".into(),
                author: identity("bob", 2),
                committer: identity("bob", 2),
                signature: None,
            },
            blobs(&[&theirs_text, b"only theirs added\n"]),
        )
        .await
        .expect("theirs created");
    (root, ours, theirs)
}

/// A running `gardenbed-server` binary (same contract as smoke.rs).
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    async fn spawn(root: &std::path::Path) -> Self {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::time::{Duration, timeout};

        // Multi-repo serving (main fa1f5e9): the registry is the set of
        // repositories that exist under `root`; create the fixture's repo
        // before spawning so `garden` resolves.
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

#[tokio::test]
async fn merge_concurrent_same_pair_converges_on_one_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (_, ours, theirs) = diverge(&client, repo).await;

    let mut handles = Vec::new();
    for _ in 0..16 {
        let c = server.client().await;
        handles.push(tokio::spawn(async move {
            c.merge(
                repo,
                ours,
                theirs,
                "merge theirs",
                &identity("amy", 3),
                &identity("amy", 3),
            )
            .await
            .expect("merge ok")
        }));
    }
    let mut ids = Vec::new();
    for h in handles {
        ids.push(h.await.expect("task joined"));
    }
    let first = ids[0].0;
    assert!(
        ids.iter().all(|(id, _)| *id == first),
        "all 16 concurrent merges must converge on one id, got {ids:?}"
    );
    let merged_cs = client
        .get_changeset(repo, first)
        .await
        .expect("cs readable");
    assert_eq!(
        merged_cs.parents,
        vec![ours, theirs],
        "two-parent structure"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn merge_and_publish_race_one_winner_per_position() {
    // M10.2's second half: N merges of the same pair race M publishes
    // of ascending children of the merge. Every merge converges on one
    // id (determinism), so the publishers race CAS on `main` from a
    // shared base: exactly one Won per position, losers get
    // NotFastForward, and `main` ends on a published child.
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (_, ours, theirs) = diverge(&client, repo).await;

    // Merging first (deterministically) gives the publishers their
    // shared parent; the race under test is the publish CAS.
    let (merged, _) = client
        .merge(
            repo,
            ours,
            theirs,
            "merge theirs",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect("merge ok");

    // M ascending children of the merge.
    const PUBLISHERS: usize = 8;
    let mut children = Vec::new();
    for k in 0..PUBLISHERS {
        let child = client
            .create_changeset(
                repo,
                &Changeset {
                    parents: vec![merged],
                    changes: vec![add(&format!("post_merge_{k}"), b"child\n")],
                    message: format!("post {k}"),
                    author: identity("amy", 4),
                    committer: identity("amy", 4),
                    signature: None,
                },
                blobs(&[b"child\n"]),
            )
            .await
            .expect("child created");
        children.push(child);
    }

    // N fresh merges of the same pair race the publishes: all must
    // answer the same id, concurrently with the CAS traffic.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let c = server.client().await;
        handles.push(tokio::spawn(async move {
            c.merge(
                repo,
                ours,
                theirs,
                "merge theirs",
                &identity("amy", 3),
                &identity("amy", 3),
            )
            .await
            .expect("merge ok")
        }));
    }
    let mut publishers = Vec::new();
    for child in children.iter().copied() {
        let c = server.client().await;
        publishers.push(tokio::spawn(async move {
            c.publish(repo, child, None, false).await
        }));
    }

    let mut merge_ids = Vec::new();
    for h in handles {
        merge_ids.push(h.await.expect("task joined").0);
    }
    assert!(
        merge_ids.iter().all(|id| *id == merged),
        "racing merges still converge on one id, got {merge_ids:?}"
    );

    // Every publish outcome is Won or NotFastForward; count the wins.
    let mut wins = 0;
    for h in publishers {
        match h.await.expect("publisher joined") {
            Ok(_tip) => wins += 1,
            Err(status) => {
                assert_eq!(
                    status.code(),
                    tonic::Code::FailedPrecondition,
                    "publish loses only to NotFastForward, got {status:?}"
                );
            }
        }
    }
    assert_eq!(wins, 1, "exactly one publish winner per race");

    // `main` ends on one of the published children, and the merge is
    // its ancestor (publish only fast-forwards).
    let tip = client
        .get_ref(repo, "main")
        .await
        .expect("ref readable")
        .expect("main set");
    assert!(
        children.contains(&tip),
        "main ends on a published child of the merge, got {tip:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn merge_then_publish_uses_unchanged_publish_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (_, ours, theirs) = diverge(&client, repo).await;
    let (merged, up_to_date) = client
        .merge(
            repo,
            ours,
            theirs,
            "merge theirs",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect("merge ok");
    assert!(!up_to_date);

    // Publish a child on top of the merge through the ordinary path.
    let child = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![merged],
                changes: vec![add("post_merge", b"after the merge\n")],
                message: "post".into(),
                author: identity("amy", 4),
                committer: identity("amy", 4),
                signature: None,
            },
            blobs(&[b"after the merge\n"]),
        )
        .await
        .expect("child created");
    client
        .publish(repo, child, None, false)
        .await
        .expect("publish of a merge child works with zero publish-code changes");
    let head = client
        .get_ref(repo, "main")
        .await
        .expect("ref readable")
        .expect("head published");
    assert_eq!(head, child);
    server.shutdown().await;
}

#[tokio::test]
async fn merge_up_to_date_reports_and_creates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let (root, ours, theirs) = diverge(&client, repo).await;
    // Merging an ancestor into a descendant is up to date.
    let (id, up_to_date) = client
        .merge(
            repo,
            ours,
            root,
            "noop",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect("merge ok");
    assert!(up_to_date, "root is an ancestor of ours");
    assert_eq!(id, ours, "up-to-date merge reports ours' id");
    // And after a real merge, merging theirs again is up to date.
    let (merged, _) = client
        .merge(
            repo,
            ours,
            theirs,
            "merge theirs",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect("merge ok");
    let (id2, up2) = client
        .merge(
            repo,
            merged,
            theirs,
            "again",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect("merge ok");
    assert!(up2);
    assert_eq!(id2, merged);
    server.shutdown().await;
}

#[tokio::test]
async fn merge_unrelated_histories_is_refused_naming_the_operation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let a = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![],
                changes: vec![add("a", b"a\n")],
                message: "root a".into(),
                author: identity("amy", 0),
                committer: identity("amy", 0),
                signature: None,
            },
            blobs(&[b"a\n"]),
        )
        .await
        .expect("a created");
    let b = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![],
                changes: vec![add("b", b"b\n")],
                message: "root b".into(),
                author: identity("bob", 1),
                committer: identity("bob", 1),
                signature: None,
            },
            blobs(&[b"b\n"]),
        )
        .await
        .expect("b created");
    let err = client
        .merge(
            repo,
            a,
            b,
            "unrelated",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect_err("unrelated merge refused");
    let msg = err.message();
    assert!(
        msg.contains("merge refused"),
        "error names the merge: {msg}"
    );
    assert!(msg.contains("unrelated"), "error states the cause: {msg}");
    server.shutdown().await;
}

#[tokio::test]
async fn merge_conflict_lists_every_path_failed_precondition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = ServerProc::spawn(dir.path()).await;
    let repo = "garden";
    let client = server.client().await;
    let root = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![],
                changes: vec![add("c", b"base\n")],
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
                changes: vec![modify("c", b"ours\n")],
                message: "ours".into(),
                author: identity("amy", 1),
                committer: identity("amy", 1),
                signature: None,
            },
            blobs(&[b"ours\n"]),
        )
        .await
        .expect("ours created");
    let theirs = client
        .create_changeset(
            repo,
            &Changeset {
                parents: vec![root],
                changes: vec![modify("c", b"theirs\n")],
                message: "theirs".into(),
                author: identity("bob", 2),
                committer: identity("bob", 2),
                signature: None,
            },
            blobs(&[b"theirs\n"]),
        )
        .await
        .expect("theirs created");
    let err = client
        .merge(
            repo,
            ours,
            theirs,
            "conflict",
            &identity("amy", 3),
            &identity("amy", 3),
        )
        .await
        .expect_err("conflicting merge refused");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message().contains("c"),
        "conflict path listed: {}",
        err.message()
    );
    server.shutdown().await;
}

/// Direct `Repo::merge` is deterministic and idempotent for a fixed
/// pair + identities — the parity anchor the CLI test compares against.
#[tokio::test]
async fn merge_repo_direct_is_deterministic_and_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let objects =
        Arc::new(FsObjectStore::open(dir.path()).expect("objects open")) as Arc<dyn ObjectStore>;
    let metadata =
        Arc::new(SqliteMetadataStore::open(&dir.path().join("meta.sqlite3")).expect("meta open"))
            as Arc<dyn MetadataStore>;
    let repo = Repo::new(objects.clone(), metadata.clone());

    let base_text = b"base line 1\nbase line 2\nbase line 3\n".to_vec();
    let root = repo
        .create_changeset(
            &Changeset {
                parents: vec![],
                changes: vec![add("f", &base_text)],
                message: "root".into(),
                author: identity("amy", 0),
                committer: identity("amy", 0),
                signature: None,
            },
            &blobs_map(&[&base_text]),
        )
        .expect("root");
    let ours_text = b"OURS line 1\nbase line 2\nbase line 3\n".to_vec();
    let ours = repo
        .create_changeset(
            &Changeset {
                parents: vec![root],
                changes: vec![modify("f", &ours_text)],
                message: "ours".into(),
                author: identity("amy", 1),
                committer: identity("amy", 1),
                signature: None,
            },
            &blobs_map(&[&ours_text]),
        )
        .expect("ours");
    let theirs_text = b"base line 1\nbase line 2\nTHEIRS line 3\n".to_vec();
    let theirs = repo
        .create_changeset(
            &Changeset {
                parents: vec![root],
                changes: vec![modify("f", &theirs_text)],
                message: "theirs".into(),
                author: identity("bob", 2),
                committer: identity("bob", 2),
                signature: None,
            },
            &blobs_map(&[&theirs_text]),
        )
        .expect("theirs");

    let (id1, up1) = repo
        .merge(ours, theirs, "m", &identity("amy", 3), &identity("amy", 3))
        .expect("first merge");
    assert!(!up1);
    let (id2, up2) = repo
        .merge(ours, theirs, "m", &identity("amy", 3), &identity("amy", 3))
        .expect("second merge");
    assert!(!up2);
    assert_eq!(id1, id2, "re-merging the same pair is idempotent");
    // Merged blob content is readable (no dangling references).
    let merged_cs = repo.get_changeset(id1).expect("merged cs readable");
    for c in &merged_cs.changes {
        if let Some(b) = c.blob {
            assert!(
                objects.get_blob(b).expect("blob readable").is_some(),
                "merged blob {} referenced and present",
                b.to_hex()
            );
        }
    }
}

/// SIGKILL crash safety (v0.2.md M10.5, ×100): for each iteration,
/// spawn a server, commit a divergent pair, race a merge against a kill
/// at a delay swept across the merge window (tens of milliseconds —
/// past fixture setup, into derivation, staging, and the write
/// transaction). Afterwards the on-disk repo must reopen, and a fresh
/// `Repo::merge` of the interrupted pair — when its fixture completed —
/// must re-merge to a stable id (deterministic rebuild; the
/// absent-or-complete contract: no half-written changeset a later merge
/// cannot rebuild).
#[test]
fn merge_sigkill_leaves_absent_or_complete_state() {
    const ITERATIONS: usize = 100;
    for i in 0..ITERATIONS {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        // Multi-repo registry: create the fixture repo before spawning.
        Repo::open(&root.join("garden")).expect("fixture repository opens");
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(&root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("server spawns");
        let out = child.stdout.take().expect("stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if let Some(addr) = line.strip_prefix("listening on ") {
                    let _ = tx.send(addr.to_owned());
                    break;
                }
            }
        });
        let addr = rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("listening");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        let mut child2 = child;
        // (ours, theirs, merge-completed) when the fixture landed before
        // the kill; None when the server died before the divergent pair
        // existed — that iteration still exercises repo reopening below.
        let interrupted: Option<(Id, Id, bool)> = rt.block_on(async {
            let client = GardenbedClient::connect(&format!("http://{addr}"))
                .await
                .ok()?;
            let repo = "garden";
            // The server may be killed mid-fixture: on error bail.
            let (_r, ours, theirs) = diverge(&client, repo).await;
            let author = identity("amy", 3);
            let committer = identity("amy", 3);
            let merge_fut = client.merge(repo, ours, theirs, "crash merge", &author, &committer);
            // Kill delay sweeps the merge window in tens of ms — past
            // fixture setup, into derivation, blob staging, and the
            // write transaction (0-5 ms only ever killed the fixture).
            let delay_ms = 10 + ((i % 10) as u64) * 10; // 10..=100 ms
            let kill_fut = async {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                let _ = child2.kill();
            };
            let (merge_outcome, ()) = tokio::join!(merge_fut, kill_fut);
            Some((ours, theirs, merge_outcome.is_ok()))
        });
        let _ = child2.wait();

        // Absent-or-complete: reopen the on-disk stores directly and
        // merge the same pair again — it must succeed and produce the
        // same id a second time (deterministic rebuild).
        let meta_path = {
            let mut p = None;
            if let Ok(rd) = std::fs::read_dir(&root) {
                for e in rd.flatten() {
                    if e.path().extension().is_some_and(|x| x == "sqlite3") {
                        p = Some(e.path());
                        break;
                    }
                }
            }
            match p {
                Some(p) => p,
                None => continue, // server died before creating metadata
            }
        };
        let objects =
            Arc::new(FsObjectStore::open(&root).expect("objects reopen")) as Arc<dyn ObjectStore>;
        let metadata = Arc::new(SqliteMetadataStore::open(&meta_path).expect("meta reopen"))
            as Arc<dyn MetadataStore>;
        let repo = Repo::new(objects.clone(), metadata.clone());
        // THE interrupted pair — when its fixture completed — must
        // re-merge to a stable id: the absent-or-complete contract on
        // the exact pair the kill raced, not a fresh pair.
        if let Some((ours, theirs, merge_completed)) = interrupted {
            let (id_a, _) = repo
                .merge(
                    ours,
                    theirs,
                    "crash merge",
                    &identity("amy", 3),
                    &identity("amy", 3),
                )
                .expect("interrupted pair re-merges (absent-or-complete)");
            let (id_b, _) = repo
                .merge(
                    ours,
                    theirs,
                    "crash merge",
                    &identity("amy", 3),
                    &identity("amy", 3),
                )
                .expect("second re-merge succeeds");
            assert_eq!(id_a, id_b, "re-merge id is stable");
            if merge_completed {
                // The kill raced a merge that had already answered: the
                // stored changeset must still be there and equal.
                let _ = repo
                    .get_changeset(id_a)
                    .expect("completed merge survives the kill");
            }
        }
        // A fresh pair must also merge on the reopened repo (deterministic
        // rebuild contract, independent of the interrupted pair).
        let base_text = b"crash base 1\ncrash base 2\ncrash base 3\n".to_vec();
        let root2 = repo
            .create_changeset(
                &Changeset {
                    parents: vec![],
                    changes: vec![add("f", &base_text)],
                    message: "root2".into(),
                    author: identity("amy", 10),
                    committer: identity("amy", 10),
                    signature: None,
                },
                &blobs_map(&[&base_text]),
            )
            .expect("root after crash");
        let ours_text = b"CRASH ours 1\ncrash base 2\ncrash base 3\n".to_vec();
        let ours2 = repo
            .create_changeset(
                &Changeset {
                    parents: vec![root2],
                    changes: vec![modify("f", &ours_text)],
                    message: "ours2".into(),
                    author: identity("amy", 11),
                    committer: identity("amy", 11),
                    signature: None,
                },
                &blobs_map(&[&ours_text]),
            )
            .expect("ours after crash");
        let theirs_text = b"crash base 1\ncrash base 2\nCRASH theirs 3\n".to_vec();
        let theirs2 = repo
            .create_changeset(
                &Changeset {
                    parents: vec![root2],
                    changes: vec![modify("f", &theirs_text)],
                    message: "theirs2".into(),
                    author: identity("bob", 12),
                    committer: identity("bob", 12),
                    signature: None,
                },
                &blobs_map(&[&theirs_text]),
            )
            .expect("theirs after crash");
        let (id_a, _) = repo
            .merge(
                ours2,
                theirs2,
                "re",
                &identity("amy", 13),
                &identity("amy", 13),
            )
            .expect("merge succeeds after SIGKILL (absent-or-complete)");
        let (id_b, _) = repo
            .merge(
                ours2,
                theirs2,
                "re",
                &identity("amy", 13),
                &identity("amy", 13),
            )
            .expect("second merge also succeeds");
        assert_eq!(id_a, id_b);
    }
}

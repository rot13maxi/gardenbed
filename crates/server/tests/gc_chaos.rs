//! The M13 gc chaos suite (v0.2.md §M13): SIGKILL the `gc` subprocess at
//! swept delays covering both the mark and the sweep, then prove the
//! repository is sound; and `--force` gc under concurrent server load.
//!
//! * **V3 crash safety**: each iteration spawns `gardenbed-server gc
//!   --min-age 0` over a fixture seeded with old orphans, SIGKILLs it at
//!   a swept delay (mark phase, sweep phase, post-completion), then:
//!   1. every object on disk passes its content-address checksum
//!      (`sweep_objects`, the M8 verifier),
//!   2. `PRAGMA integrity_check` is ok,
//!   3. acknowledged pre-GC answers still read byte-exact,
//!   4. a follow-up gc completes and converges (subsequent gc deletes
//!      zero, zero).
//! * **V4 live gc**: with the server serving concurrent GetBlob /
//!   CreateChangeset traffic, `gc --force` never fails an acknowledged
//!   read, and a changeset created mid-sweep either survives whole or the
//!   sweep reports `aborted` (an ingest landed).
//!
//! Both classes use the *binary* (never the in-process API) so the kill
//! lands in real process state.

use std::collections::{BTreeMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr as _;
use std::time::Duration;

use gardenbed_harness::{GenOptions, generate};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_server::Repo;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{sleep, timeout};
use tokio_stream::StreamExt as _;

/// Iterations of the kill loop. The M13 gate says 100; the default keeps
/// a full run within a few minutes. `GARDENBED_GC_CHAOS_ITERS` raises it.
fn kill_iters() -> usize {
    std::env::var("GARDENBED_GC_CHAOS_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

/// Backdate distance: older than every min-age gate used here.
const OLD_SECS: u64 = 100_000;

// ---------------------------------------------------------------- helpers

/// Spawns `gardenbed-server gc ...` (no wait). Returns the child.
fn spawn_gc(root: &Path, args: &[&str]) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("gc")
        .arg("--root")
        .arg(root)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("gc subprocess spawns")
}

/// One running server process (fresh ephemeral port per spawn).
struct ServerProc {
    child: tokio::process::Child,
    addr: String,
}

impl ServerProc {
    async fn spawn(root: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
            .arg("--addr")
            .arg("127.0.0.1:0")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("gardenbed-server binary spawns");
        let stdout = child.stdout.take().expect("server stdout piped");
        let line = timeout(Duration::from_secs(30), async {
            BufReader::new(stdout).lines().next_line().await
        })
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

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Runs gc to completion; returns (exit code, combined stdout+stderr).
fn run_gc(root: &Path, args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_gardenbed-server"))
        .arg("gc")
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("gc subprocess runs");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), text)
}

/// Backdates a path's mtime.
fn backdate(path: &Path) {
    let old = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(OLD_SECS))
        .expect("clock");
    let f = std::fs::File::open(path).expect("backdate target opens");
    f.set_modified(old).expect("mtime set");
}

/// Content-address checksum sweep (the M8 verifier, gc edition): every
/// non-dot file under `objects/` must decode and hash to its id.
fn sweep_objects(root: &Path) -> u64 {
    let objects = root.join("objects");
    let mut verified = 0u64;
    let mut stack = vec![objects];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let entry = entry.expect("dir entry");
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
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
            let id = Id::from_str(&format!("{shard}{name}")).expect("64-hex object name");
            let bytes = std::fs::read(entry.path()).expect("object reads");
            if bytes.starts_with(b"GBLO1") {
                let raw_len = u64::from_le_bytes(bytes[5..13].try_into().expect("envelope header"));
                let content = zstd::stream::decode_all(Cursor::new(&bytes[13..]))
                    .unwrap_or_else(|e| panic!("blob {id} frame does not decode: {e}"));
                assert_eq!(content.len() as u64, raw_len, "blob {id} envelope lies");
                assert_eq!(blob_id(&content), id, "blob {id} fails its checksum");
            } else {
                assert_eq!(
                    bytes.first().copied(),
                    Some(0x02),
                    "object {id} is neither envelope nor tree"
                );
                gardenbed_model::Tree::decode(&bytes)
                    .unwrap_or_else(|e| panic!("object {id} is not a decodable tree: {e}"));
                let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
                assert_eq!(digest.as_slice(), id.as_bytes(), "tree {id} checksum");
            }
            verified += 1;
        }
    }
    verified
}

/// `PRAGMA integrity_check` on the metadata.
fn integrity_check(root: &Path) {
    let conn = rusqlite::Connection::open(root.join("metadata.sqlite3")).expect("metadata opens");
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity_check runs");
    assert_eq!(verdict, "ok", "metadata failed its integrity check");
}

/// The fixture: an ingested merge-dense history with `main` set, plus a
/// population of old orphan objects (tree-shaped junk) and stale tmp
/// files for the sweep to chew on, plus the acknowledged-read baseline.
struct Fixture {
    root: PathBuf,
    main: Id,
    /// A sampled (blob id, content) acknowledged before any gc.
    baseline: Vec<(Id, Vec<u8>)>,
    orphans: Vec<Id>,
    /// Content of each orphan blob (index-aligned with `orphans`).
    orphan_contents: Vec<Vec<u8>>,
}
fn build_fixture(root: &Path, orphan_count: usize) -> Fixture {
    let repo = Repo::open(root).expect("fixture repo opens");
    let history = generate(
        0x6C4A05,
        &GenOptions {
            commits: 60,
            merge_ratio: 0.2,
            ..GenOptions::default()
        },
    );
    let blobs = history.blob_contents.clone();
    let is_gitlink = |id: &Id| {
        id.as_bytes()[..12].iter().all(|b| *b == 0)
            && blobs
                .binary_search_by(|(k, _)| k.as_bytes().cmp(id.as_bytes()))
                .is_ok_and(|i| blobs[i].1 == id.as_bytes())
    };
    for cs in &history.changesets {
        let mut inline = BTreeMap::new();
        for change in &cs.changes {
            let Some(blob) = change.blob else { continue };
            if change.mode == Mode::Gitlink || is_gitlink(&blob) {
                continue;
            }
            inline.insert(blob.to_hex(), history.blob(&blob).expect("blob").to_vec());
        }
        repo.create_changeset(cs, &inline).expect("history ingests");
    }
    repo.cas_ref("main", None, history.head)
        .expect("main ref set");

    // Old orphans: real blob objects under unreferenced ids, backdated —
    // valid content, because the checksum sweep runs mid-test while the
    // orphans still exist and must pass it too.
    let mut orphans = Vec::new();
    let mut orphan_contents = Vec::new();
    for i in 0..orphan_count {
        let content = format!("gc-chaos-orphan-{i}").into_bytes();
        let id = repo.put_blob(&content).expect("orphan blob uploads");
        let hex = id.to_hex();
        let path = root.join("objects").join(&hex[..2]).join(&hex[2..]);
        backdate(&path);
        orphans.push(id);
        orphan_contents.push(content);
    }

    // Acknowledged baseline: three blobs the history references.
    let baseline = {
        let mut out = Vec::new();
        for (id, content) in blobs.iter().take(3) {
            out.push((*id, content.clone()));
        }
        out
    };

    Fixture {
        root: root.to_path_buf(),
        main: history.head,
        baseline,
        orphans,
        orphan_contents,
    }
}

/// The shard path for an object id.
fn orphan_path(root: &Path, id: &Id) -> PathBuf {
    let hex = id.to_hex();
    root.join("objects").join(&hex[..2]).join(&hex[2..])
}

// ------------------------------------------------------------------- V3

/// V3: SIGKILL `gc` at swept delays across mark and sweep; after every
/// kill the store is sound, and a follow-up gc converges.
#[tokio::test]
async fn v3_sigkill_mid_gc_leaves_sound_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fx = build_fixture(dir.path(), 40);
    let root = &fx.root;

    // Fresh-repo oracle: what a complete gc must leave (the brute-force
    // live set over the disk — established by one clean gc first, whose
    // result we then re-establish after every kill).
    let (code, out) = run_gc(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "priming gc runs: {out}");
    assert!(out.contains("40 blob(s)"), "orphans collected: {out}");
    let pristine = disk_ids(root);
    sweep_objects(root);
    integrity_check(root);

    // Replant orphans identically for the kill loop (the store must
    // converge back to `pristine` after every kill + follow-up).
    let iters = kill_iters();
    for i in 0..iters {
        // Kill delay sweep: 0..64 ms covers spawn → mark → sweep → exit
        // for this fixture size (mark ~10 ms, sweep ~2 ms); long delays
        // let runs complete (the acknowledged-complete path).
        let delay_ms = [0u64, 1, 2, 3, 5, 8, 12, 20, 34, 64][i % 10];
        // Replant: re-put through the store (valid envelope), then
        // backdate past every gate.
        let repo = Repo::open(root).expect("repo reopens for replant");
        for (k, id) in fx.orphans.iter().enumerate() {
            let content = format!("gc-chaos-orphan-{k}").into_bytes();
            debug_assert_eq!(repo.put_blob(&content).expect("orphan re-put"), *id);
            backdate(&orphan_path(root, id));
        }
        drop(repo);

        let mut child = spawn_gc(root, &["--min-age", "0"]);
        sleep(Duration::from_millis(delay_ms)).await;
        let killed = child.kill().is_ok();
        let _ = child.wait();

        // 1. Every object on disk passes its checksum.
        sweep_objects(root);
        // 2. Metadata is intact.
        integrity_check(root);
        // 3. Acknowledged reads still byte-exact.
        let repo = Repo::open(root).expect("repo reopens after kill");
        for (id, want) in &fx.baseline {
            let got = repo.get_blob(*id).expect("blob read works after kill");
            assert_eq!(
                &got.expect("baseline blob present"),
                want,
                "byte-exact {id}"
            );
        }
        let _ = repo;

        // 4. Follow-up gc converges to the pristine set.
        let (code, out) = run_gc(root, &["--min-age", "0"]);
        assert_eq!(
            code, 0,
            "iter {i} (delay {delay_ms} ms, killed={killed}): follow-up gc runs: {out}"
        );
        assert_eq!(disk_ids(root), pristine, "iter {i}: converged to pristine");
        let (code, out) = run_gc(root, &["--min-age", "0"]);
        assert_eq!(code, 0, "iter {i}: second follow-up runs: {out}");
        assert!(
            out.contains("0 blob(s) and 0 tree(s) deleted"),
            "iter {i}: second follow-up deletes zero: {out}"
        );
    }
}

/// The on-disk object id set (all shards, no dotfiles).
fn disk_ids(root: &Path) -> HashSet<Id> {
    let mut out = HashSet::new();
    let objects = root.join("objects");
    for entry in std::fs::read_dir(&objects).expect("objects dir reads") {
        let shard = entry.expect("shard entry").path();
        if !shard.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&shard).expect("shard reads") {
            let file = file.expect("file entry").path();
            let name = file
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            if name.starts_with('.') {
                continue;
            }
            let shard_hex = shard.file_name().expect("shard name").to_string_lossy();
            out.insert(Id::from_str(&format!("{shard_hex}{name}")).expect("64-hex name"));
        }
    }
    out
}

// ------------------------------------------------------------------- V4

/// V4: `gc --force` under a serving server with concurrent readers and
/// writers. No acknowledged read fails; a changeset created mid-sweep is
/// either fully intact (its objects survived) or the sweep aborted and a
/// retry gc is conservative.
#[tokio::test]
async fn v4_force_gc_under_concurrent_load() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fx = build_fixture(dir.path(), 60);
    let root = fx.root.clone();

    let mut server = ServerProc::spawn(&root).await;
    let client = GardenbedClient::connect(&format!("http://{}", server.addr))
        .await
        .expect("client connects");

    // Concurrent reader loop: GetBlob over the baseline blobs for the
    // whole test; every acknowledged answer must be byte-exact.
    let reader = tokio::spawn({
        let client = client.clone();
        let baseline = fx.baseline.clone();
        async move {
            let mut reads = 0u64;
            for _ in 0..200 {
                for (id, want) in &baseline {
                    let mut stream = client
                        .get_blob("default", *id)
                        .await
                        .unwrap_or_else(|e| panic!("acknowledged read of {id} failed: {e}"));
                    let mut content = Vec::with_capacity(want.len());
                    while let Some(chunk) = stream.next().await {
                        let chunk =
                            chunk.unwrap_or_else(|e| panic!("read of {id} failed mid-stream: {e}"));
                        content.extend_from_slice(&chunk.data);
                    }
                    assert_eq!(&content, want, "acknowledged read {id} byte-exact");
                    reads += 1;
                }
            }
            reads
        }
    });

    // Concurrent writer: CreateChangeset traffic — the ingest the
    // force-sweep must detect. EVERY changeset references one fixture
    // orphan (on disk, backdated past the cutoff, unreferenced at any
    // plausible mark time) plus its own fresh inline blob: the exact
    // race shape, thirty times over — blob present, mark says orphan,
    // row lands mid-sweep. Under the fixed order, any pre-row unlink is
    // a lost upload the retry recovers (re-put recreates the object;
    // the row then references it and the watermark moves); under the
    // old unlink-then-check order an adoption whose row landed before
    // the unlink is acknowledged data loss no retry can paper over,
    // because the CONTENT was collected while the row names it.
    let writer = tokio::spawn({
        let client = client.clone();
        let parent = fx.main;
        let orphans = fx.orphans.clone();
        let orphan_contents = fx.orphan_contents.clone();
        async move {
            let mut created: Vec<Id> = Vec::new();
            let mut blobs: Vec<(Id, Vec<u8>)> = Vec::new();
            for i in 0..30u64 {
                let content = format!("v4 concurrent blob {i}\n").into_bytes();
                let blob = blob_id(&content);
                let orphan = orphans[i as usize % orphans.len()];
                let orphan_content = orphan_contents[i as usize % orphan_contents.len()].clone();
                let cs = Changeset {
                    parents: vec![parent],
                    changes: vec![
                        Change {
                            path: format!("v4/f{i:03}.txt"),
                            op: Op::Add,
                            blob: Some(blob),
                            src_path: None,
                            mode: Mode::Regular,
                        },
                        Change {
                            path: format!("v4/orphan-adopt-{i:03}.txt"),
                            op: Op::Add,
                            blob: Some(orphan),
                            src_path: None,
                            mode: Mode::Regular,
                        },
                    ],
                    message: format!("v4 {i}"),
                    author: Identity {
                        name: "V4".into(),
                        email: "v4@bed.example".into(),
                        time_secs: 1_800_000_000 + i,
                        tz_min: 0,
                    },
                    committer: Identity {
                        name: "V4".into(),
                        email: "v4@bed.example".into(),
                        time_secs: 1_800_000_000 + i,
                        tz_min: 0,
                    },
                    signature: None,
                };
                blobs.push((blob, content.clone()));
                blobs.push((orphan, orphan_content.clone()));
                // Retry loop: a collected in-flight upload fails naming
                // the blob; the client re-uploads and retries (the V4
                // contract).
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    let inline = vec![(blob, content.clone())];
                    match client.create_changeset("default", &cs, inline).await {
                        Ok(id) => {
                            created.push(id);
                            break;
                        }
                        Err(_) if attempt < 5 => {
                            // Re-upload BOTH referenced contents (the V4
                            // contract: a collected in-flight upload fails
                            // naming the blob; retry recovers). gc may
                            // have collected either while the row was in
                            // flight — the fresh blob or the adopted
                            // orphan.
                            for body in [content.clone(), orphan_content.clone()] {
                                let body = tokio_stream::iter(vec![body]);
                                let _ = client.put_blob("default", body).await;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        Err(e) => panic!("concurrent CreateChangeset failed after retries: {e}"),
                    }
                }
            }
            (created, blobs)
        }
    });

    // The force gc under load: must complete (0) or abort (0 with the
    // aborted note); never a hard error. spawn_blocking so the gc
    // subprocess's blocking wait runs off the reactor while the writer
    // and reader tasks are actually polled — a direct Command::output
    // here would freeze the test thread and the writer would only run
    // after gc returned, testing nothing about the race window.
    let gc_root = root.clone();
    let gc = tokio::task::spawn_blocking(move || run_gc(&gc_root, &["--force", "--min-age", "0"]));
    let (code, out) = gc.await.expect("gc task finishes");
    assert_eq!(code, 0, "force gc exits 0 under load: {out}");

    let reads = reader.await.expect("reader task finishes");
    assert!(reads >= 200, "reader acknowledged {reads} reads");
    let (created, blobs) = writer.await.expect("writer task finishes");
    assert_eq!(created.len(), 30, "all concurrent changesets created");

    // The changesets' *objects* must survive — the regression assertion
    // for the force-mode race: a changeset that lands after the mark
    // references a blob the mark may have classified as an orphan (rows
    // don't refresh blob mtimes), so a sweep that unlinks before checking
    // the ingest watermark loses acknowledged data. Reading the row
    // (`log`) proves nothing about its objects; GetBlob byte-exact does.
    // Run it through the still-live server: exactly what an
    // acknowledged-read client would observe.
    {
        let client = GardenbedClient::connect(&format!("http://{}", server.addr))
            .await
            .expect("reconnect");
        for (id, want) in &blobs {
            let mut stream = client
                .get_blob("default", *id)
                .await
                .unwrap_or_else(|e| panic!("blob {id} readable after force sweep: {e}"));
            let mut content = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap_or_else(|e| panic!("read of {id} failed mid-stream: {e}"));
                content.extend_from_slice(&chunk.data);
            }
            assert_eq!(
                &content, want,
                "writer blob {id} byte-exact after force sweep"
            );
        }
    }
    let repo = Repo::open(&root).expect("repo opens after v4");
    for id in &created {
        let log = repo.log(*id, 1).expect("log of created cs");
        assert_eq!(log.len(), 1, "created changeset {id} is intact");
    }

    // Follow-up force gc converges; then, with the server gone, the
    // offline gc works again (the exclusive lock is free).
    let (code, out) = run_gc(&root, &["--force", "--min-age", "0"]);
    assert_eq!(code, 0, "follow-up force gc runs: {out}");
    server.kill().await;
    let (code, out) = run_gc(&root, &["--min-age", "0"]);
    assert_eq!(code, 0, "final offline gc runs after shutdown: {out}");
}

/// V4b: a *collected* in-flight upload fails naming the blob, and the
/// retry recovers. Direct simulation: an old orphan (deleted by gc) is
/// then fetched through a fresh open — must answer absent, not corrupt;
/// re-putting the content restores it.
#[tokio::test]
async fn v4b_collected_upload_fails_cleanly_and_retry_recovers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fx = build_fixture(dir.path(), 10);
    let root = &fx.root;

    // A real orphan blob with real content, backdated.
    let content = b"v4b in-flight upload".to_vec();
    let id = {
        let repo = Repo::open(root).expect("repo opens");
        repo.put_blob(&content).expect("orphan uploaded")
    };
    let hex = id.to_hex();
    let path = root.join("objects").join(&hex[..2]).join(&hex[2..]);
    backdate(&path);

    // gc collects it.
    let (code, out) = run_gc(root, &["--min-age", "0"]);
    assert_eq!(code, 0, "gc runs: {out}");
    assert!(
        out.contains("1 blob(s)"),
        "the orphan blob was collected: {out}"
    );
    assert!(!path.exists(), "orphan file unlinked");

    // A late reader of the collected upload: absent, cleanly.
    let repo = Repo::open(root).expect("repo reopens");
    let got = repo
        .get_blob(id)
        .expect("read of collected blob is not an error");
    assert!(got.is_none(), "collected blob reads absent");

    // The retry recovers: re-put, then read.
    repo.put_blob(&content).expect("retry upload succeeds");
    let got = repo.get_blob(id).expect("re-uploaded blob reads");
    assert_eq!(
        got.expect("re-uploaded blob present"),
        content,
        "retry is byte-exact"
    );
}

// ------------------------------------------------------------------- V6

/// V6 (round-2 review, P2-1 + P2-2): the tree-closure verify and the
/// per-handle repo.lock, in one race.
///
/// * **P2-2 surface**: the repository `late` is created AFTER the server
///   started (a fresh `<root>/late`, never in `served_names()` at
///   startup). The first request opens it through `RepoHandle`, which
///   must take the shared `repo.lock` for the handle's lifetime —
///   offline gc on that directory must then be refused while the server
///   runs (the startup snapshot alone never covered it).
/// * **P2-1 surface**: inside `late`, a mid-size changeset is derived
///   TWICE with identical content (the deterministic-replay shape: a
///   failed commit retried, a re-run rebase). The first derivation's
///   tree objects are planted as backdated orphans (`put_tree`'s
///   idempotent path does not refresh mtime, fs.rs); the second
///   derivation REUSES those exact tree ids, so a force sweep that
///   classifies them as garbage can unlink an intermediate tree while
///   the writer's row commits. The root-only verify of round 1 passes
///   in that window (the root may sit in a later chunk); the
///   tree-closure verify must not: either the commit fails naming the
///   tree (retry recovers, objects re-put) or it lands with its FULL
///   tree readable. The postcondition asserts the acknowledged
///   changeset's complete closure, byte-exact, through the live server.
#[tokio::test]
async fn v6_tree_closure_verify_and_post_startup_repo_lock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    // 1. A pre-existing `default` repo (so the server's startup snapshot
    //    has something to lock), then START THE SERVER before `late`
    //    exists — the gap the round-2 review flagged.
    let default_repo = Repo::open(&root.join("default")).expect("default repo opens");
    drop(default_repo);
    let mut server = ServerProc::spawn(&root).await;
    let client = GardenbedClient::connect(&format!("http://{}", server.addr))
        .await
        .expect("client connects");

    // 2. `late` is created after startup. Priming it through the SERVER
    //    (one Log request) forces `open_handle` — the handle now holds
    //    the shared repo.lock, and offline gc on `late` must refuse.
    let late_dir = root.join("late");
    let late_head: Id;
    {
        let repo = Repo::open(&late_dir).expect("late repo opens");
        // The parent: a small committed history in `late` (10 changesets
        // across nested directories, so derivations materialize several
        // distinct tree objects).
        let history = generate(
            0x5EC0DE,
            &GenOptions {
                commits: 10,
                merge_ratio: 0.0,
                ..GenOptions::default()
            },
        );
        let blobs = history.blob_contents.clone();
        let is_gitlink = |id: &Id| {
            id.as_bytes()[..12].iter().all(|b| *b == 0)
                && blobs
                    .binary_search_by(|(k, _)| k.as_bytes().cmp(id.as_bytes()))
                    .is_ok_and(|i| blobs[i].1 == id.as_bytes())
        };
        for cs in &history.changesets {
            let mut inline = BTreeMap::new();
            for change in &cs.changes {
                let Some(blob) = change.blob else { continue };
                if change.mode == Mode::Gitlink || is_gitlink(&blob) {
                    continue;
                }
                inline.insert(blob.to_hex(), history.blob(&blob).expect("blob").to_vec());
            }
            repo.create_changeset(cs, &inline)
                .expect("late history ingests");
        }
        repo.cas_ref("main", None, history.head)
            .expect("late main set");
        late_head = history.head;
    }
    // Prime through the server: opens the handle (and its lock). Log from
    // the real head so the request resolves (a zero id 404s).
    {
        let mut stream = client
            .log("late", late_head, 1)
            .await
            .expect("late serves through the post-startup handle");
        let first = stream.next().await;
        assert!(
            first.is_some(),
            "late history serves through the post-startup handle"
        );
    }

    // 3. Offline gc on `late` must now be REFUSED (exit 1, Held): the
    //    handle's shared lock is live. Round 1's startup snapshot missed
    //    exactly this repository.
    {
        let (code, out) = run_gc(&late_dir, &["--min-age", "0"]);
        assert_ne!(
            code, 0,
            "offline gc on post-startup repo must refuse: {out}"
        );
        assert!(
            out.contains("--force") || out.to_lowercase().contains("lock"),
            "refusal names the lock or --force: {out}"
        );
    }

    // 4. The P2-1 race: pre-derive a mid-size changeset (fresh blobs in
    //    NESTED directories), let its tree objects land, backdate them,
    //    leave them unreferenced (no row names them), then commit the
    //    IDENTICAL changeset while a force gc sweeps. The derivation
    //    reuses the backdated tree ids; a root-only verify passes if the
    //    root survived; the closure verify must catch any missing
    //    intermediate.
    let parent = {
        let repo = Repo::open(&late_dir).expect("repo reopens");
        repo.get_ref("main").expect("main reads").expect("main set")
    };
    // Build the changeset: 40 files across 8 nested directories, all
    // fresh inline blobs — enough distinct tree objects that a sweep
    // has several to unlink (multi-chunk is not required for the
    // closure check to bite: any unlinked intermediate is a missing
    // subtree).
    let mut changes: Vec<Change> = (0..40u32)
        .map(|i| Change {
            path: format!("v6/dir{}/sub{}/file-{:03}.txt", i % 8, i % 4, i),
            op: Op::Add,
            blob: Some(blob_id(format!("v6 blob {i}\n").as_bytes())),
            src_path: None,
            mode: Mode::Regular,
        })
        .collect::<Vec<_>>();
    // Changesets require changes sorted by path (strictly ascending).
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut inline = BTreeMap::new();
    for i in 0..40u32 {
        let content = format!("v6 blob {i}\n").into_bytes();
        inline.insert(blob_id(&content).to_hex(), content);
    }
    // The client's inline form is (Id, content) pairs.
    let inline_pairs: Vec<(Id, Vec<u8>)> = inline
        .iter()
        .map(|(hex, content)| (Id::from_str(hex).expect("hex id parses"), content.clone()))
        .collect();
    let v6_cs = Changeset {
        parents: vec![parent],
        changes: changes.clone(),
        message: "v6 deterministic replay".into(),
        author: Identity {
            name: "V6".into(),
            email: "v6@bed.example".into(),
            time_secs: 1_800_000_000,
            tz_min: 0,
        },
        committer: Identity {
            name: "V6".into(),
            email: "v6@bed.example".into(),
            time_secs: 1_800_000_000,
            tz_min: 0,
        },
        signature: None,
    };

    // The deterministic-replay orphan shape: put the blobs, pre-derive
    // via the ENGINE (writes the tree objects but NO changeset row —
    // nothing references the trees), then backdate everything. The
    // parent is certified (committed), so validated_tree_for_new works.
    let tree_ids = {
        let repo = Repo::open(&late_dir).expect("repo reopens");
        let engine = repo.engine_handle();
        // Materialize blobs first (validated_tree_for_new requires them).
        for (_, content) in inline.iter() {
            repo.put_blob(content).expect("blob pre-put");
        }
        let root = engine
            .validated_tree_for_new(&v6_cs)
            .expect("engine derives the v6 tree");
        // Walk the closure to enumerate every object. Tree ids expand;
        // blob ids must NOT be fed to engine.tree (their envelope bytes
        // do not hash to a tree id — engine.tree would Corrupt). Collect
        // them separately: they are orphans too.
        let mut tree_ids: Vec<Id> = Vec::new();
        let mut blob_ids: Vec<Id> = Vec::new();
        let mut queue: Vec<Id> = root.into_iter().collect();
        let mut seen: HashSet<Id> = queue.iter().copied().collect();
        while let Some(id) = queue.pop() {
            tree_ids.push(id);
            let tree = engine
                .tree(id)
                .expect("tree decodes")
                .unwrap_or_else(|| panic!("tree {id} missing right after derive"));
            for entry in &tree.entries {
                match entry.kind {
                    gardenbed_model::TreeKind::File => {
                        let blob = entry.blob.expect("file entry carries a blob");
                        // Gitlinks are padded upstream SHAs, not objects.
                        if entry.mode == gardenbed_model::Mode::Gitlink {
                            continue;
                        }
                        if seen.insert(blob) {
                            blob_ids.push(blob);
                        }
                    }
                    gardenbed_model::TreeKind::Dir => {
                        let child = entry.tree.expect("dir entry carries a tree");
                        if seen.insert(child) {
                            queue.push(child);
                        }
                    }
                }
            }
        }
        tree_ids.extend(blob_ids);
        tree_ids
    };
    // No changeset row was written for v6_cs: every object in tree_ids
    // is unreferenced garbage from gc's point of view. Backdate them.
    for id in &tree_ids {
        backdate(&orphan_path(&late_dir, id));
    }

    // 5. The race: force gc over `late` (shared lock allows it — the
    //    handle holds only shared) while the writer commits the
    //    IDENTICAL changeset through the server. The derivation reuses
    //    the backdated tree ids; the sweep wants them.
    let writer = tokio::spawn({
        let client = client.clone();
        let v6_cs = v6_cs.clone();
        let inline_pairs = inline_pairs.clone();
        async move {
            let mut attempts = 0u32;
            loop {
                attempts += 1;
                match client
                    .create_changeset("late", &v6_cs, inline_pairs.clone())
                    .await
                {
                    Ok(id) => return (id, attempts),
                    Err(e) if attempts < 10 => {
                        // The V4b contract: a collected object fails
                        // naming it; re-put the blobs and retry (the
                        // trees are re-materialized by the derivation).
                        let _ = e;
                        for (_, content) in inline_pairs.iter() {
                            let body = tokio_stream::iter(vec![content.clone()]);
                            let _ = client.put_blob("late", body).await;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(e) => panic!("v6 commit failed after retries: {e}"),
                }
            }
        }
    });
    let gc = tokio::task::spawn_blocking({
        let late_dir = late_dir.clone();
        move || run_gc(&late_dir, &["--force", "--min-age", "0"])
    });
    let (code, out) = gc.await.expect("gc task finishes");
    assert_eq!(code, 0, "force gc over late exits 0: {out}");
    let (cs_id, attempts) = writer.await.expect("writer task finishes");
    assert!(attempts <= 10, "writer succeeded (attempt {attempts})");

    // 6. The P2-1 postcondition: the acknowledged changeset's FULL tree
    //    closure is readable, byte-exact, through the live server. A
    //    root-only verify would have blessed a missing intermediate.
    {
        let repo = Repo::open(&late_dir).expect("repo reopens for the closure walk");
        let engine = repo.engine_handle();
        let root = engine
            .derive_tree(cs_id)
            .expect("root derives")
            .expect("v6 tree is non-empty");
        let mut queue: Vec<Id> = vec![root];
        let mut seen: HashSet<Id> = HashSet::from([root]);
        let mut walked = 0u32;
        while let Some(id) = queue.pop() {
            let tree = engine
                .tree(id)
                .expect("tree read works")
                .unwrap_or_else(|| panic!("tree {id} missing from the acknowledged changeset's closure (P2-1: intermediate tree collected)"));
            walked += 1;
            for entry in &tree.entries {
                match entry.kind {
                    gardenbed_model::TreeKind::File => {
                        let blob = entry.blob.expect("file entry carries a blob");
                        // Gitlinks are padded upstream SHAs, not objects.
                        if entry.mode == gardenbed_model::Mode::Gitlink {
                            continue;
                        }
                        let got = repo
                            .get_blob(blob)
                            .expect("blob read works")
                            .unwrap_or_else(|| panic!("blob {blob} missing (P2-1)"));
                        // The closure mixes v6 blobs with the parent
                        // history's; byte-check ours, presence-check the
                        // rest.
                        if let Some(want) = inline.get(&blob.to_hex()) {
                            assert_eq!(&got, want, "blob {blob} byte-exact");
                        }
                    }
                    gardenbed_model::TreeKind::Dir => {
                        let child = entry.tree.expect("dir entry carries a tree");
                        if seen.insert(child) {
                            queue.push(child);
                        }
                    }
                }
            }
        }
        assert!(
            walked >= 24,
            "the closure walk covered the tree ({walked} trees)"
        );
    }

    // 7. Offline gc on `late` is STILL refused while the server holds
    //    the handle (the lock is per-handle, not per-request).
    {
        let (code, out) = run_gc(&late_dir, &["--min-age", "0"]);
        assert_ne!(code, 0, "offline gc still refuses while serving: {out}");
    }
    server.kill().await;
    // With the server gone the exclusive lock is free: gc runs.
    let (code, out) = run_gc(&late_dir, &["--min-age", "0"]);
    assert_eq!(code, 0, "offline gc runs after shutdown: {out}");
}

//! Resumability of the want-list fetch (v0.1.md §M4.3): an interrupted
//! `bed sparse add` resumes with **zero duplicates** across the wire.
//!
//! The proof is exact, not observational. The server counts every
//! `GetTree`/`GetBlob` it receives (bumped at handler start, so a snapshot
//! after the client dies is final); the working copy's cache contents are read from
//! `.bed/objects` directly. Because each distinct want id is requested at
//! most once per run (tree singleflight + blob dedup), and a rerun
//! requests exactly the ids that never made it into the cache:
//!
//! * run-2's fetch count == distinct want − |cached at the kill|, for a
//!   kill anywhere in the fetch phase (cycle A);
//! * with a kill after every request has been served but before
//!   materialization ends, the cache holds the whole want-list and the
//!   rerun fetches nothing — so the *total* server count equals the
//!   distinct want-list size exactly: nothing crossed the wire twice
//!   (cycle B).
//!
//! Fixture ingestion runs through the server library, so the counters the
//! tests assert on see only real bed traffic.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gardenbed_harness::generator::{GenOptions, GeneratedHistory, files_at, generate};
use gardenbed_model::Id;
use tempfile::TempDir;

use common::{Server, bed, bed_async};

/// One server carrying a large generated history, plus two gardens:
/// one for each resumability cycle.
struct World {
    /// Temp root holding the server and the repositorys.
    _dir: TempDir,
    /// Garden for cycle A (kill mid-fetch).
    garden1: PathBuf,
    /// Garden for cycle B (kill after the fetch phase, during refresh).
    garden2: PathBuf,
    server: Server,
    history: GeneratedHistory,
    /// The head changeset id (what `main` names).
    head: Id,
}

impl World {
    fn state_dir(&self, garden: &std::path::Path) -> PathBuf {
        garden.join(".bed")
    }
}

/// Boots a 40-commit history (a few hundred files at the tip) into a
/// fresh server. The seed search is bounded and falls back to seed 1:
/// the counter proof only needs a sizeable tree, not every file mode
/// (fidelity across modes is proven by `read_path.rs`).
async fn setup() -> World {
    let opts = GenOptions {
        commits: 40,
        max_files: 300,
        ..GenOptions::default()
    };
    let history = (1..10u64)
        .map(|s| (s, generate(s, &opts)))
        .find(|(_, h)| {
            let files = files_at(h, h.changesets.len() - 1);
            let has = |want: gardenbed_model::Mode| files.values().any(|(_, m)| *m == want);
            has(gardenbed_model::Mode::Exec)
                && has(gardenbed_model::Mode::Symlink)
                && has(gardenbed_model::Mode::Gitlink)
        })
        .or_else(|| Some((1u64, generate(1, &opts))))
        .map(|(_, h)| h)
        .expect("generate never fails");

    let dir = tempfile::tempdir().expect("tempdir");
    let server = dir.path().join("server").join(common::REPO);
    let garden1 = dir.path().join("garden-one");
    let garden2 = dir.path().join("garden-two");
    fs::create_dir_all(&garden1).expect("garden one");
    fs::create_dir_all(&garden2).expect("garden two");
    let head = common::ingest(&history, &server);
    let server = Server::spawn(server.parent().unwrap()).await;
    World {
        _dir: dir,
        garden1,
        garden2,
        server,
        history,
        head,
    }
}

/// The distinct-object size of the want-list: trees + non-gitlink blobs.
fn want_size(world: &World) -> (u64, u64, u64) {
    let (trees, blobs) = common::want_list(&world.server.root, world.head, &["."]);
    let t = trees.len() as u64;
    let b = blobs.len() as u64;
    (t, b, t + b)
}

/// Relative paths of every file under `root` (`.bed` pruned wherever it
/// appears; gitlink pointers never materialize so they are absent).
fn files_under(root: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, rel: &str, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().unwrap().is_dir() {
                if name == ".bed" {
                    continue;
                }
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                walk(&entry.path(), &path, out);
            } else {
                let path = if rel.is_empty() {
                    name
                } else {
                    format!("{rel}/{name}")
                };
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out.sort();
    out
}

/// The head changeset's path set (every mode except gitlink pointers
/// materializes as a file).
fn expected_paths(world: &World) -> Vec<String> {
    files_at(&world.history, world.history.changesets.len() - 1)
        .into_iter()
        .filter(|(_, (_, mode))| *mode != gardenbed_model::Mode::Gitlink)
        .map(|(path, _)| path)
        .collect()
}

/// Deadline for [`await_counters`].
///
/// Both call sites' healthy waits measure ~110-120 ms (instrumented run:
/// 106.7 ms for the quarter-fetch, 120.1 ms for the whole want-list), so
/// 10 minutes is ~5,000x the healthy path — sized for the machine-load
/// stall class in dogfood #16 (box in D-state), not for a slow product.
/// The cap also has to sit under the 30-minute CI job timeout: a stuck run
/// must fail with our "no progress"/"deadline" message before the job is
/// killed with no diagnostic. It is never a skip — the helper keeps
/// asserting on every poll until it fires.
const AWAIT_COUNTERS_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// Polls `GetStats` until `pred` fires, with load-tolerant pacing.
///
/// Dogfood #16: the old shape slept a fixed 10 ms and gave up after a
/// fixed 120 s. Under a machine-load stall (the box in D-state behind a
/// ~500 GB corpus read storm) the server's counter cannot advance at all,
/// so that cap declared a *test* failure where the product was healthy —
/// and every 10 ms poll added more load precisely when the box had none
/// to spare. Three changes, all test-infra only:
///
/// * **Progress-sensitive exponential backoff** 10 ms → 250 ms (doubling
///   only while the counters sit still, reset to 10 ms the moment any
///   counter moves). A fast transition is still caught at the tight
///   cadence — cycle A kills a quarter of the way in, so a coarse poll
///   would let the fetch finish first — while a genuinely stalled server
///   is polled ~4×/s instead of 100×/s, so the helper stops competing
///   with the I/O that is blocking it.
/// * **A 10-minute deadline.** The measured healthy waits are ~110-120 ms
///   (see [`AWAIT_COUNTERS_DEADLINE`]); 10 min is ~5,000× that and still
///   under the 30-minute CI job timeout, so a stuck run fails with a
///   diagnostic instead of being killed silently. It is not a skip: the
///   helper always keeps asserting.
/// * **Cause-distinguishing failure messages**: "no progress" names
///   counters that never advanced past their start (a product stall),
///   "deadline" names counters that advanced but never reached the target
///   (a load/slow-path case). A silent skip is deliberately not an
///   option: it would hide exactly the product stall this assertion
///   exists to catch.
async fn await_counters(
    world: &World,
    client: &gardenbed_protocol::client::GardenbedClient,
    pred: impl Fn(u64, u64) -> bool,
    what: &str,
    timeout: Duration,
) {
    /// Ceiling on the poll interval: ~4 GetStats/s while stalled.
    const MAX_POLL: Duration = Duration::from_millis(250);
    const FIRST_POLL: Duration = Duration::from_millis(10);

    let deadline = Instant::now() + timeout;
    let mut poll = FIRST_POLL;
    let (t_start, b_start) = world.server.counted(client).await;
    let mut last = (t_start, b_start);
    loop {
        tokio::time::sleep(poll).await;
        let (t, b) = world.server.counted(client).await;
        if pred(t, b) {
            return;
        }
        if (t, b) != last {
            // Progress since the previous poll: a transition is underway,
            // so keep watching closely. Backoff is for stalls only.
            last = (t, b);
            poll = FIRST_POLL;
        } else {
            poll = (poll * 2).min(MAX_POLL);
        }
        if Instant::now() >= deadline {
            let (t, b) = (t - t_start, b - b_start);
            if t == 0 && b == 0 {
                panic!(
                    "no progress after {timeout:?} waiting for {what}: \
                     GetStats counters never advanced (get_tree {t}, get_blob {b}); \
                     the server made no observable progress at all"
                );
            }
            panic!(
                "deadline after {timeout:?} waiting for {what}: counters advanced \
                 (get_tree {t}, get_blob {b}) but the target was never reached"
            );
        }
    }
}

/// Kills `child` (SIGKILL) and reaps it.
async fn kill(mut child: tokio::process::Child) {
    child.kill().await.expect("SIGKILL lands");
    child.wait().await.expect("reap the killed bed");
}

/// The working copy's cache should be exactly `expected` after a full run.
fn assert_clean(world: &World, garden: &std::path::Path) {
    let (code, out, stderr) = bed(garden, &["status", "--full-scan"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(out.contains("working copy is clean"), "out: {out}");
    assert_eq!(files_under(garden), expected_paths(world), "working copy");
}

#[tokio::test]
async fn killed_mid_fetch_resumes_with_zero_duplicates() {
    let mut world = setup().await;
    let (want_trees, want_blobs, want_total) = want_size(&world);
    assert!(want_total > 0, "the fixture has a non-empty want-list");

    let client = world.server.client().await;
    let (t0, b0) = world.server.counted(&client).await;

    let (code, _, stderr) = bed(
        &world.garden1,
        &["clone", &format!("{}/garden", world.server.http())],
    );
    assert_eq!(code, 0, "clone: {stderr}");

    // ---- cycle A: SIGKILL a quarter of the way through the blob fetch ----
    let child = bed_async(&world.garden1, &["sparse", "add", "."]);
    let quarter = want_blobs / 4;
    await_counters(
        &world,
        &client,
        |_, b| b - b0 >= quarter.max(1),
        "the blob fetch to reach a quarter of the want-list",
        AWAIT_COUNTERS_DEADLINE,
    )
    .await;
    kill(child).await;
    // The counters bump at handler start, so they are final already; the
    // short grace just lets the server's streams tear down before we read.
    tokio::time::sleep(Duration::from_millis(250)).await;

    let bed1 = world.state_dir(&world.garden1);
    let cached_trees = common::cached_count(&bed1, &{
        let (trees, _) = common::want_list(&world.server.root, world.head, &["."]);
        trees
    });
    let cached_blobs = common::cached_count(&bed1, &{
        let (_, blobs) = common::want_list(&world.server.root, world.head, &["."]);
        blobs
    });
    let cached_total = (cached_trees + cached_blobs) as u64;
    assert!(
        cached_total < want_total,
        "the kill must leave work to resume"
    );

    let (t1, b1) = world.server.counted(&client).await;
    // Singleflight/dedup: even mid-flight, no distinct id was asked twice.
    let run1_requested = (t1 - t0) + (b1 - b0);
    assert!(
        run1_requested <= want_total,
        "run 1 requested {run1_requested} ids, want-list is {want_total}"
    );

    // ---- resume: the rerun fetches exactly the uncached remainder ----
    let (code, out, stderr) = bed(&world.garden1, &["sparse", "add", "."]);
    assert_eq!(code, 0, "rerun: {stderr}\n{out}");
    let (t2, b2) = world.server.counted(&client).await;
    let run2 = (t2 - t1) + (b2 - b1);
    assert_eq!(
        run2,
        want_total - cached_total,
        "run 2 fetched exactly the ids run 1 never cached \
         (run 1 requested {run1_requested}, cached {cached_total}, want {want_total})"
    );
    assert_clean(&world, &world.garden1);
    world.server.shutdown().await;

    println!(
        "cycle A: want {want_total} ({want_trees} trees, {want_blobs} blobs), \
         cached at kill {cached_total}, run 1 requested {run1_requested}, run 2 fetched {run2}"
    );
}

#[tokio::test]
async fn killed_after_fetch_phase_caches_everything_and_rerun_fetches_zero() {
    let mut world = setup().await;
    let (want_trees, want_blobs, want_total) = want_size(&world);

    let client = world.server.client().await;
    let (t0, b0) = world.server.counted(&client).await;

    let (code, _, stderr) = bed(
        &world.garden2,
        &["clone", &format!("{}/garden", world.server.http())],
    );
    assert_eq!(code, 0, "clone: {stderr}");
    let (t2, b2) = world.server.counted(&client).await;
    assert_eq!((t2, b2), (t0, b0), "clone itself fetches no objects");

    // ---- cycle B: let the fetch phase finish, then stop the working copy during
    // ---- the refresh (or, if it beats us to it, join it — the state to
    // ---- prove is the same: everything fetched, everything cached). ----
    let mut child = bed_async(&world.garden2, &["sparse", "add", "."]);
    await_counters(
        &world,
        &client,
        |t, b| (t - t2) + (b - b2) >= want_total,
        "every distinct want id to have been requested",
        AWAIT_COUNTERS_DEADLINE,
    )
    .await;
    // A short grace for the last fetch's cache write; fetching strictly
    // precedes creating, so a still-running bed is now mid-materialization.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let killed_mid_refresh = matches!(child.try_wait(), Ok(None));
    if killed_mid_refresh {
        kill(child).await;
    } else {
        child.wait().await.expect("join the finished bed");
    }
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Every want id crossed the wire exactly once in total...
    let (t3, b3) = world.server.counted(&client).await;
    assert_eq!(
        (t3 - t2) + (b3 - b2),
        want_total,
        "total GetTree+GetBlob must equal the distinct want-list exactly"
    );
    // ...and every one of them sits in the local cache.
    let bed2 = world.state_dir(&world.garden2);
    let cached_total = (common::cached_count(&bed2, &{
        let (trees, _) = common::want_list(&world.server.root, world.head, &["."]);
        trees
    }) + common::cached_count(&bed2, &{
        let (_, blobs) = common::want_list(&world.server.root, world.head, &["."]);
        blobs
    })) as u64;
    assert_eq!(
        cached_total, want_total,
        "everything fetched is in the cache"
    );

    // The rerun therefore fetches nothing and repairs the half-initialized
    // garden to a clean, faithful state.
    let (code, out, stderr) = bed(&world.garden2, &["sparse", "add", "."]);
    assert_eq!(code, 0, "rerun: {stderr}\n{out}");
    assert!(
        out.contains("fetched 0 tree(s) and 0 blob(s)"),
        "the rerun fetched nothing: {out}"
    );
    let (t4, b4) = world.server.counted(&client).await;
    assert_eq!((t4, b4), (t3, b3), "the rerun fetched nothing");
    assert_clean(&world, &world.garden2);
    world.server.shutdown().await;

    println!(
        "cycle B: want {want_total} ({want_trees} trees, {want_blobs} blobs), \
         total GetTree+GetBlob after both runs: {}, stopped mid-refresh: {killed_mid_refresh}",
        (t4 - t0) + (b4 - b0)
    );
}

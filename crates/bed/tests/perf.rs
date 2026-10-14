//! Performance budgets for the M4 read path (v0.1.md §M4.4), as
//! `#[ignore]`d tests — they build real fixtures and spawn real processes,
//! and their budgets only mean something in a release build:
//!
//! ```sh
//! cargo test -p gardenbed --release -- --ignored --nocapture
//! ```
//!
//! * a ~1k-file `bed plot add` finishes in under 5 s (fetch + plant,
//!   end to end);
//! * a warm `bed status` over a 10k-file plot answers in under 500 ms
//!   (hint-guided: nothing is hashed);
//! * a cold `bed file` (fresh process, warm cache, live arboretum) has a
//!   p95 under 50 ms across 20 runs.
//!
//! Each test asserts its budget and prints its numbers.
//!
//! Fixtures live on tmpfs when one exists (`/dev/shm`): the budgets
//! measure the fetch + plant *design*, not the host disk's fsync
//! latency. The object store fsyncs every object it publishes (the M3
//! torn-write guarantee, ~10 ms/object on ext4), which on a spinning-disk
//! class filesystem dwarfs everything else: 545 files took 6.8 s on this
//! machine's ext4 `/tmp` and 0.53 s on tmpfs. The read path itself is
//! identical in both cases.

mod common;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gardenbed::bed::Bed;
use gardenbed::treestate::{TreestateEntry, MODE_REGULAR, HINT_VERIFIED};
use gardenbed_harness::generator::{files_at, generate, GenOptions};
use gardenbed_model::blob_id;
use tempfile::TempDir;

use common::{bed, Arboretum};

/// Fails `what` if the measured `elapsed` exceeds `budget`.
fn assert_budget(what: &str, elapsed: Duration, budget: Duration) {
    println!("{what}: {:.1?} (budget {budget:?})", elapsed);
    assert!(
        elapsed <= budget,
        "{what} took {:.1?}, over the {budget:?} budget",
        elapsed
    );
}

/// The perf fixtures' temp root: tmpfs (`/dev/shm`) when it exists, the
/// standard temp dir otherwise. See the module docs for why.
fn temp_root() -> TempDir {
    let shm = Path::new("/dev/shm");
    if shm.is_dir() {
        tempfile::TempDir::new_in(shm)
    } else {
        tempfile::tempdir()
    }
    .expect("perf tempdir")
}

/// The p95 of `samples` (they are already measured durations).
fn p95(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let idx = (samples.len() * 95).div_ceil(100) - 1;
    samples[idx]
}

/// A release-binary sanity gate: in a debug build these budgets measure
/// the optimizer's absence, not the design.
fn assert_release() {
    assert!(
        !cfg!(debug_assertions),
        "these budget tests are release-only — run `cargo test -p gardenbed --release -- --ignored`"
    );
}

/// One arboretum with an ingested history and a garden beside it.
struct World {
    _dir: TempDir,
    garden: PathBuf,
    server: Arboretum,
    head_files: std::collections::BTreeMap<String, (gardenbed_model::Id, gardenbed_model::Mode)>,
}

async fn setup(opts: GenOptions) -> World {
    let seed = (1..100u64)
        .map(|s| (s, generate(s, &opts)))
        .find(|(_, h)| {
            let files = files_at(h, h.changesets.len() - 1);
            let has = |want: gardenbed_model::Mode| files.values().any(|(_, m)| *m == want);
            has(gardenbed_model::Mode::Exec)
                && has(gardenbed_model::Mode::Symlink)
                && has(gardenbed_model::Mode::Gitlink)
        })
        .map(|(_, h)| h)
        .expect("a qualifying seed exists in 1..100");
    let dir = temp_root();
    let garden = dir.path().join("garden");
    fs::create_dir_all(&garden).expect("garden dir");
    let head = common::ingest(&seed, &dir.path().join("arboretum"));
    let server = Arboretum::spawn(&dir.path().join("arboretum")).await;
    assert_eq!(head, seed.head);
    World {
        _dir: dir,
        garden,
        server,
        head_files: files_at(&seed, seed.changesets.len() - 1),
    }
}

/// M4 perf 1: a plot add that fetches and plants ~1k files, end to end
/// (process spawn → exit), under 5 s.
#[tokio::test]
#[ignore = "performance budget; run in release: cargo test -p gardenbed --release -- --ignored"]
async fn plot_add_of_1k_files_under_5s() {
    assert_release();
    let mut w = setup(GenOptions {
        commits: 20,
        max_files: 1000,
        ..GenOptions::default()
    })
    .await;
    let (code, _, stderr) = bed(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "clone: {stderr}");

    let files = w.head_files.len();
    assert!(files >= 500, "fixture should be sizeable: {files}");
    let start = Instant::now();
    let (code, out, stderr) = bed(&w.garden, &["plot", "add", "."]);
    assert_budget(&format!("plot add of {files} files"), start.elapsed(), Duration::from_secs(5));
    assert_eq!(code, 0, "plot add: {stderr}\n{out}");

    let (code, out, _) = bed(&w.garden, &["status"]);
    assert_eq!(code, 0);
    assert!(out.contains("the plot is clean"), "out: {out}");
    w.server.shutdown().await;
}

/// M4 perf 2: a warm `bed status` over a 10k-file plot (planted lib-side,
/// no server involved) answers in under 500 ms — the hint path, nothing
/// hashed. Three runs, each within budget.
#[test]
#[ignore = "performance budget; run in release: cargo test -p gardenbed --release -- --ignored"]
fn warm_status_10k_under_500ms() {
    assert_release();
    let dir = temp_root();
    let bed_dir = dir.path().join(".bed");
    let mut planted = Bed::init(&bed_dir, "http://127.0.0.1:1", "garden").expect("bed inits");
    planted.config_mut().plot.push(".".to_owned());
    planted.save_config().expect("config saved");
    let treestate = planted.treestate().expect("treestate opens");

    // 10k regular files planted with rows whose size/mtime/hint match —
    // exactly the "warm" state after a real plot add.
    let garden = dir.path();
    const N: usize = 10_000;
    let mut content = Vec::with_capacity(64);
    for i in 0..N {
        content.clear();
        write!(content, "row {i} of the warm status perf fixture\n").unwrap();
        let path = format!("file-{i:05}.txt");
        fs::write(garden.join(&path), &content).expect("file written");
        let md = fs::metadata(garden.join(&path)).expect("metadata");
        treestate
            .upsert(&TreestateEntry {
                path: path.clone(),
                blob_id: blob_id(&content),
                mode: MODE_REGULAR,
                size: content.len() as i64,
                mtime_ns: i64::try_from(md.mtime_nsec()).expect("mtime fits"),
                hint: HINT_VERIFIED,
            })
            .expect("row upserted");
    }

    // The fixture must read clean before it is worth timing.
    let (code, out, stderr) = bed(garden, &["status"]);
    assert_eq!(code, 0, "status: {stderr}");
    assert!(out.contains("the plot is clean"), "out: {out}");

    // One untimed warm-up (page cache), then the measured runs.
    let mut samples = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        let (code, out, _) = bed(garden, &["status"]);
        let elapsed = start.elapsed();
        assert_eq!(code, 0);
        assert!(out.contains("the plot is clean"), "out: {out}");
        samples.push(elapsed);
    }
    for (i, s) in samples.iter().enumerate() {
        println!("warm status run {i}: {s:.1?}");
    }
    let worst = *samples.iter().max().unwrap();
    assert_budget("warm status over 10k files (worst of 3)", worst, Duration::from_millis(500));
}

/// M4 perf 3: a cold `bed file` — fresh process every time, warm local
/// cache, live arboretum — stays under 50 ms at p95 over 20 runs.
#[tokio::test]
#[ignore = "performance budget; run in release: cargo test -p gardenbed --release -- --ignored"]
async fn cold_file_p95_under_50ms() {
    assert_release();
    let mut w = setup(GenOptions {
        commits: 5,
        max_files: 40,
        ..GenOptions::default()
    })
    .await;
    let (code, _, stderr) = bed(&w.garden, &["clone", &w.server.http()]);
    assert_eq!(code, 0, "clone: {stderr}");
    let (code, _, stderr) = bed(&w.garden, &["plot", "add", "."]);
    assert_eq!(code, 0, "plot add: {stderr}");

    // Any plain file of the plot.
    let path = w
        .head_files
        .iter()
        .find(|(_, (_, m))| *m == gardenbed_model::Mode::Regular)
        .map(|(p, _)| p.clone())
        .expect("fixture has a regular file");

    // One untimed warm-up, then 20 timed cold runs.
    let (code, _, _) = bed(&w.garden, &["file", &path]);
    assert_eq!(code, 0);
    let mut samples = Vec::new();
    for _ in 0..20 {
        let start = Instant::now();
        let (code, _, _) = bed(&w.garden, &["file", &path]);
        let elapsed = start.elapsed();
        assert_eq!(code, 0);
        samples.push(elapsed);
    }
    let slowest = *samples.iter().max().unwrap();
    let p = p95(samples);
    println!("cold file p95: {p:.1?} (slowest {slowest:.1?})");
    assert_budget("cold bed file p95 of 20 runs", p, Duration::from_millis(50));
    w.server.shutdown().await;
}

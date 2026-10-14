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
//!   p95 under 50 ms across 20 runs;
//! * **M5**: sowing the same 100-file change is size-independent — the
//!   p50 sow on a 1M-file arboretum is at most twice the p50 sow on a
//!   10k-file one (the write path costs the change, not the repository).
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
use gardenbed::treestate::{HINT_VERIFIED, MODE_REGULAR, TreestateEntry};
use gardenbed_harness::generator::{GenOptions, files_at, generate};
use gardenbed_model::blob_id;
use tempfile::TempDir;

use common::{Arboretum, bed};

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
    assert_budget(
        &format!("plot add of {files} files"),
        start.elapsed(),
        Duration::from_secs(5),
    );
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
        writeln!(content, "row {i} of the warm status perf fixture").unwrap();
        let path = format!("file-{i:05}.txt");
        fs::write(garden.join(&path), &content).expect("file written");
        let md = fs::metadata(garden.join(&path)).expect("metadata");
        treestate
            .upsert(&TreestateEntry {
                path: path.clone(),
                blob_id: blob_id(&content),
                mode: MODE_REGULAR,
                size: content.len() as i64,
                mtime_ns: md.mtime_nsec(),
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
    assert_budget(
        "warm status over 10k files (worst of 3)",
        worst,
        Duration::from_millis(500),
    );
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

/// Ingests `total_files` bulk files (in chunks of `per_cs` adds per
/// changeset, spread over per-changeset directories so each tree apply
/// touches a handful of nodes — real repositories grow breadth-wise, and
/// the fixture must not smuggle an O(repo) shape into the *fixture* build)
/// plus a 100-file `small/` plot directory, and points `main` at the head.
/// Runs through the server library in-process, so no request counters move.
///
/// The 2^20-changes-per-changeset codec bound caps `per_cs`; 10k keeps
/// every changeset far under it while the 1M-file repo takes ~100
/// changesets. The fixture lives on tmpfs (see [`temp_root`]), where the
/// object store's fsyncs are cheap; no durability toggles are touched.
fn build_sized_repo(root: &Path, total_files: usize, per_cs: usize) -> gardenbed_model::Id {
    use gardenbed_model::{Change, Changeset, Identity, Mode, Op};
    use gardenbed_server::Repo;

    let repo = Repo::open(root).expect("repo opens");
    let ident = |secs: u64| Identity {
        name: "Seeder".into(),
        email: "seed@bed.example".into(),
        time_secs: secs,
        tz_min: 0,
    };
    let mut parent: Option<gardenbed_model::Id> = None;
    let mut made = 0usize;
    let mut cs_no = 0usize;
    while made < total_files {
        let n = per_cs.min(total_files - made);
        let mut changes = Vec::with_capacity(n + if cs_no == 0 { 100 } else { 0 });
        let mut inline = std::collections::BTreeMap::new();
        for k in 0..n {
            let path = format!("bulk/d{cs_no:03}/f{k:05}.txt");
            let content = format!("row {} of the bulk fixture\n", made + k);
            let blob = blob_id(content.as_bytes());
            changes.push(Change {
                path,
                op: Op::Add,
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            });
            inline.insert(blob.to_hex(), content.into_bytes());
        }
        if cs_no == 0 {
            for k in 0..100 {
                let path = format!("small/f{k:02}.txt");
                let content = format!("small file {k}\n");
                let blob = blob_id(content.as_bytes());
                changes.push(Change {
                    path,
                    op: Op::Add,
                    blob: Some(blob),
                    src_path: None,
                    mode: Mode::Regular,
                });
                inline.insert(blob.to_hex(), content.into_bytes());
            }
        }
        let cs = Changeset {
            parents: parent.into_iter().collect(),
            changes,
            message: format!("bulk seed {cs_no}"),
            author: ident(1_700_000_000 + cs_no as u64),
            committer: ident(1_700_000_000 + cs_no as u64),
            signature: None,
        };
        let id = repo
            .create_changeset(&cs, &inline)
            .expect("bulk changeset ingests");
        parent = Some(id);
        made += n;
        cs_no += 1;
    }
    let head = parent.expect("at least one changeset");
    repo.cas_ref("main", None, head).expect("main created");
    head
}

/// M5 perf: sow latency is independent of repository size. Two synthetic
/// arboreta (10k and 1M files) are ingested in-process and served; on
/// each, seven fresh beds sow the identical 100-file change (the `small/`
/// plot only — nothing else is ever materialized). The p50 of the 1M sows
/// must stay within 2× the p50 of the 10k sows.
#[tokio::test]
#[ignore = "size-independence budget; run in release: cargo test -p gardenbed --release -- --ignored"]
async fn sow_p50_is_size_independent_10k_vs_1m() {
    assert_release();
    const REPS: usize = 7;
    const SMALL_FILES: usize = 100;

    let dir = temp_root();
    let small_repo = dir.path().join("arboretum-10k");
    let big_repo = dir.path().join("arboretum-1m");

    // Fixture ingest (uncounted, in-process). 1M files arrive as ~100
    // changesets of 10k adds each.
    let t0 = Instant::now();
    build_sized_repo(&small_repo, 10_000, 10_000);
    build_sized_repo(&big_repo, 1_000_000, 10_000);
    println!("fixtures ingested in {:.1?} (10k + 1M files)", t0.elapsed());

    let mut small_server = Arboretum::spawn(&small_repo).await;
    let mut big_server = Arboretum::spawn(&big_repo).await;

    // One fresh bed per rep: clone, plot the small/ subtree, rewrite its
    // 100 files, sow. Only the sow is timed.
    let sow_once = |server_http: &str, garden: &Path, rep: usize| -> Duration {
        fs::create_dir_all(garden).expect("garden dir");
        let (code, _, stderr) = bed(garden, &["clone", server_http]);
        assert_eq!(code, 0, "clone: {stderr}");
        let (code, _, stderr) = bed(garden, &["plot", "add", "small"]);
        assert_eq!(code, 0, "plot add: {stderr}");
        for k in 0..SMALL_FILES {
            fs::write(
                garden.join(format!("small/f{k:02}.txt")),
                format!("small file {k} — sown by rep {rep}\n"),
            )
            .expect("working file");
        }
        let start = Instant::now();
        let (code, out, stderr) = bed(garden, &["sow", "-m", &format!("rep {rep}")]);
        let elapsed = start.elapsed();
        assert_eq!(code, 0, "sow: {stderr}\n{out}");
        elapsed
    };

    // One untimed warm-up per size (page cache, code paths), then the
    // measured reps.
    let _ = sow_once(
        &small_server.http(),
        &dir.path().join("garden-warm-10k"),
        100,
    );
    let _ = sow_once(&big_server.http(), &dir.path().join("garden-warm-1m"), 100);
    let mut small_samples = Vec::new();
    let mut big_samples = Vec::new();
    for rep in 0..REPS {
        small_samples.push(sow_once(
            &small_server.http(),
            &dir.path().join(format!("garden-10k-{rep}")),
            rep,
        ));
        big_samples.push(sow_once(
            &big_server.http(),
            &dir.path().join(format!("garden-1m-{rep}")),
            rep,
        ));
    }
    small_server.shutdown().await;
    big_server.shutdown().await;

    small_samples.sort();
    big_samples.sort();
    let p50_small = small_samples[REPS / 2];
    let p50_big = big_samples[REPS / 2];
    println!("p50 sow over 10k files: {p50_small:.1?}");
    println!("p50 sow over 1M files:  {p50_big:.1?}");
    let ratio = p50_big.as_secs_f64() / p50_small.as_secs_f64();
    println!("ratio: {ratio:.2} (budget 2.00)");
    assert!(
        p50_big <= p50_small * 2,
        "sow is not size-independent: 1M p50 {p50_big:.1?} vs 10k p50 {p50_small:.1?}"
    );
}

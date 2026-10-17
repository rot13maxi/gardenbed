//! Warm `bed status` over a 10k-file sparse (the M4 offline heart,
//! v0.1.md §M4.4): the hint-guided scan hashes only suspects — on a clean
//! sparse, nothing at all.
//!
//! Two measurements on the same initialized fixture (10k regular files on
//! disk, treestate rows whose size/mtime/hint match — exactly the state
//! after a real sparse add):
//!
//! * **hint_scan** — the default `bed status`: stat-only walk, nothing
//!   hashed (the warm path; the 500 ms budget rides on this).
//! * **full_scan** — `--full-scan`: every tracked file is hashed. The
//!   correctness path after a hint loss, and the contrast that shows what
//!   the hint path buys.
//!
//! No server is involved: status is the offline heart. The scan may
//! refresh hint metadata in the treestate (a sqlite write on tmpfs); the
//! first pass refreshes, later passes find everything verified.

mod world;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use gardenbed::treestate::{HINT_VERIFIED, MODE_REGULAR, TreestateEntry};
use gardenbed::workspace::WorkingCopy;
use gardenbed_model::blob_id;

/// The initialized fixture: a working copy with a 10k-file sparse, all rows warm.
struct World {
    _dir: tempfile::TempDir,
    workspace_root: std::path::PathBuf,
    treestate: gardenbed::treestate::Treestate,
    files: usize,
}

fn setup() -> World {
    let dir = world::temp_root();
    let workspace_root = dir.path().to_path_buf();
    let state_dir = workspace_root.join(".bed");
    let mut planted =
        WorkingCopy::init(&state_dir, "http://127.0.0.1:1", world::REPO).expect("bed inits");
    planted.config_mut().sparse.push(".".to_owned());
    planted.save_config().expect("config saved");
    let treestate = planted.treestate().expect("treestate opens");

    let garden = workspace_root.clone();
    let n = world::env_usize("GARDENBED_BENCH_STATUS_FILES", 10_000, 100);
    let mut content = Vec::with_capacity(64);
    for i in 0..n {
        content.clear();
        writeln!(content, "row {i} of the warm status bench fixture").unwrap();
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
    let report = gardenbed::status::scan_status(&garden, &[".".to_owned()], &treestate, false)
        .expect("warm scan works");
    assert!(report.is_clean(), "the planted fixture must be clean");

    World {
        _dir: dir,
        workspace_root,
        treestate,
        files: n,
    }
}

fn bench_status(c: &mut Criterion) {
    let w = setup();
    println!("status fixture: {} files planted warm", w.files);

    let mut group = c.benchmark_group("warm_status_10k");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("hint_scan", |b| {
        b.iter(|| {
            gardenbed::status::scan_status(
                &w.workspace_root,
                &[".".to_owned()],
                &w.treestate,
                false,
            )
            .expect("hint scan works")
        })
    });

    group.bench_function("full_scan", |b| {
        b.iter(|| {
            gardenbed::status::scan_status(&w.workspace_root, &[".".to_owned()], &w.treestate, true)
                .expect("full scan works")
        })
    });

    group.finish();
}

criterion_group!(benches, bench_status);
criterion_main!(benches);

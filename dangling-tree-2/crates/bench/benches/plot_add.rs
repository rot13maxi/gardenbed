//! `bed plot add` over a ~1k-file plot (the M4 read path, v0.1.md §M4.4).
//!
//! Two measurements on the same fixture (a generated history whose head
//! mixes exec/symlink/gitlink files in with ~1k regular ones):
//!
//! * **warm_retend** — re-adding a plot the bed already materialized. The
//!   want-list plan runs from the local tree cache, every blob is a cache
//!   hit, and the sync pass re-verifies the files: this is the "reruns
//!   cost nothing" steady state, and the number a user feels on a second
//!   `plot add`.
//! * **cold_fetch** — the same command after the object cache is wiped
//!   (setup, untimed): every tree and blob crosses the wire again, but the
//!   files on disk stay faithful, so no rewrite happens. This isolates the
//!   fetch path; the full first-add (fetch + materialize) end-to-end
//!   budget is bed's M4 perf test (5 s for ~1k files).
//!
//! End-to-end process spawn costs are deliberately excluded here (criterion
//! measures the library path the binary calls); the perf budgets cover the
//! process view.

mod world;

use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use gardenbed_harness::GenOptions;
use gardenbed_harness::generator::files_at;

use world::Arboretum;

/// The bench fixture: a seeded history with a sizeable head tree, ingested
/// in-process and served by a spawned arboretum; a bed planted over it.
struct World {
    _dir: tempfile::TempDir,
    garden: world::Garden,
    server: Arboretum,
    file_count: usize,
}

async fn setup() -> World {
    let dir = world::temp_root();
    let history = world::seeded_history(&GenOptions {
        commits: 20,
        max_files: 1000,
        ..GenOptions::default()
    });
    let file_count = files_at(&history, history.changesets.len() - 1).len();
    world::ingest(&history, &dir.path().join("arboretum"));
    let server = Arboretum::spawn(&dir.path().join("arboretum")).await;
    let garden = world::Garden::plant(&dir.path().join("garden"), &server.http()).await;
    World {
        _dir: dir,
        garden,
        server,
        file_count,
    }
}

/// Empties the bed's local object cache (the cold_fetch setup): the
/// documented `objects/<2 hex>/<62 hex>` CAS layout makes this a directory
/// removal — the store recreates it on the next open.
fn wipe_object_cache(bed_dir: &std::path::Path) {
    let objects = bed_dir.join("objects");
    std::fs::remove_dir_all(objects).expect("object cache wiped");
}

fn bench_plot_add(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut w = rt.block_on(setup());
    println!(
        "plot_add fixture: {} files at head (tmpfs fixtures, fsync on)",
        w.file_count
    );

    // One untimed warm-up: caches the trees + blobs and writes the
    // treestate rows, so warm_retend measures the steady state.
    let out = rt
        .block_on(gardenbed::ops::plot_add(
            &w.garden.bed,
            &[".".to_owned()],
            8,
        ))
        .expect("warm-up plot add works");
    assert!(
        out.contains(&format!("{} file(s) in the plot", w.file_count)),
        "the whole head tree must be in the plot: {out}"
    );

    let mut group = c.benchmark_group("plot_add_1k");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("warm_retend", |b| {
        b.iter(|| {
            rt.block_on(gardenbed::ops::plot_add(
                &w.garden.bed,
                &[".".to_owned()],
                8,
            ))
            .expect("warm re-tend works")
        })
    });

    group.bench_function("cold_fetch", |b| {
        b.iter_batched(
            || wipe_object_cache(&w.garden.bed),
            |()| {
                rt.block_on(gardenbed::ops::plot_add(
                    &w.garden.bed,
                    &[".".to_owned()],
                    8,
                ))
                .expect("cold re-fetch works")
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
    rt.block_on(w.server.shutdown());
}

criterion_group!(benches, bench_plot_add);
criterion_main!(benches);

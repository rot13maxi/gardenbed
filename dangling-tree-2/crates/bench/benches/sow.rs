//! `bed sow` — the write path's size-independence property (M5,
//! v0.1.md §M4.4/M5): the identical delta sown onto arboreta of different
//! total size must cost the same, because the write path costs the change,
//! not the repository.
//!
//! Two fixtures (bed's M5 shape, in-process ingest): a
//! `GARDENBED_BENCH_SOW_SMALL_FILES`-file repo and a
//! `GARDENBED_BENCH_SOW_BIG_FILES`-file one, both carrying a `small/` plot
//! of `GARDENBED_BENCH_SOW_DELTA` files that nothing else ever touches.
//! Each iteration rewrites the delta files (untimed setup) and sows them
//! (timed): scan → delta build → inline upload → changeset create → bed
//! move. The two numbers belong side by side; bed's budget test asserts
//! the ratio, the bench records the absolute costs.

mod world;

use std::cell::Cell;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

struct Repo {
    arboretum: world::Arboretum,
    garden: world::Garden,
}

struct World {
    _dir: tempfile::TempDir,
    small: Repo,
    big: Repo,
    delta: usize,
}

async fn plant_repo(dir: &std::path::Path, name: &str, total_files: usize, delta: usize) -> Repo {
    let root = dir.join(format!("arboretum-{name}"));
    world::build_sized_repo(&root, total_files, 10_000, delta);
    let arboretum = world::Arboretum::spawn(&root).await;
    let garden = world::Garden::plant(&dir.join(format!("garden-{name}")), &arboretum.http()).await;
    gardenbed::ops::plot_add(&garden.bed, &["small".to_owned()], 8)
        .await
        .expect("the small plot materializes");
    Repo { arboretum, garden }
}

async fn setup() -> World {
    let dir = world::temp_root();
    let small_files = world::env_usize("GARDENBED_BENCH_SOW_SMALL_FILES", 10_000, 1_000);
    let big_files = world::env_usize("GARDENBED_BENCH_SOW_BIG_FILES", 100_000, small_files);
    let delta = world::env_usize("GARDENBED_BENCH_SOW_DELTA", 100, 1);
    let small = plant_repo(dir.path(), "small", small_files, delta).await;
    let big = plant_repo(dir.path(), "big", big_files, delta).await;
    World {
        _dir: dir,
        small,
        big,
        delta,
    }
}

/// Rewrites the delta files with fresh content (the untimed setup of one
/// sow iteration); `round` distinguishes iterations so every sow carries a
/// real content change.
fn dirty_plot(garden: &world::Garden, delta: usize, round: usize) {
    for k in 0..delta {
        std::fs::write(
            garden.dir.join(format!("small/f{k:03}.txt")),
            format!("sown by round {round}, file {k}\n"),
        )
        .expect("working file rewritten");
    }
}

fn bench_sow(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut w = rt.block_on(setup());
    println!(
        "sow fixtures: small repo + big repo, identical delta of {} files",
        w.delta
    );

    let mut group = c.benchmark_group("sow");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));
    // Throughput context in the report: the delta each sow carries.
    group.throughput(criterion::Throughput::Elements(w.delta as u64));

    for (name, repo) in [("small_repo", &w.small), ("big_repo", &w.big)] {
        let round = Cell::new(0usize);
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let r = round.get();
                    round.set(r + 1);
                    dirty_plot(&repo.garden, w.delta, r);
                },
                |()| {
                    rt.block_on(gardenbed::sow::sow(&repo.garden.bed, "bench sow", None))
                        .expect("sow works")
                },
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
    rt.block_on(w.small.arboretum.shutdown());
    rt.block_on(w.big.arboretum.shutdown());
}

criterion_group!(benches, bench_sow);
criterion_main!(benches);

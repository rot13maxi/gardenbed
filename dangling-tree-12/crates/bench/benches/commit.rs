//! `bed commit` — the write path's size-independence property (M5,
//! v0.1.md §M4.4/M5): the identical delta created onto servers of different
//! total size must cost the same, because the write path costs the change,
//! not the repository.
//!
//! Two fixtures (working copy's M5 shape, in-process ingest): a
//! `GARDENBED_BENCH_SOW_SMALL_FILES`-file repo and a
//! `GARDENBED_BENCH_SOW_BIG_FILES`-file one, both carrying a `small/` sparse
//! of `GARDENBED_BENCH_SOW_DELTA` files that nothing else ever touches.
//! Each iteration rewrites the delta files (untimed setup) and commits them
//! (timed): scan → delta build → inline upload → changeset create → bed
//! move. The two numbers belong side by side; working copy's budget test asserts
//! the ratio, the bench records the absolute costs.

mod world;

use std::cell::Cell;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

struct Repo {
    server: world::Server,
    garden: world::Garden,
}

struct World {
    _dir: tempfile::TempDir,
    small: Repo,
    big: Repo,
    delta: usize,
}

async fn plant_repo(dir: &std::path::Path, name: &str, total_files: usize, delta: usize) -> Repo {
    let root = dir.join(format!("server-{name}"));
    world::build_sized_repo(&root, total_files, 10_000, delta);
    let server = world::Server::spawn(&root).await;
    let garden = world::Garden::plant(&dir.join(format!("garden-{name}")), &server.http()).await;
    gardenbed::ops::sparse_add(&garden.bed, &["small".to_owned()], 8)
        .await
        .expect("the small sparse materializes");
    Repo { server, garden }
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
/// commit iteration); `round` distinguishes iterations so every commit carries a
/// real content change.
fn dirty_sparse(garden: &world::Garden, delta: usize, round: usize) {
    for k in 0..delta {
        std::fs::write(
            garden.dir.join(format!("small/f{k:03}.txt")),
            format!("created by round {round}, file {k}\n"),
        )
        .expect("working file rewritten");
    }
}

fn bench_commit(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut w = rt.block_on(setup());
    println!(
        "commit fixtures: small repo + big repo, identical delta of {} files",
        w.delta
    );

    let mut group = c.benchmark_group("commit");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));
    // Throughput context in the report: the delta each commit carries.
    group.throughput(criterion::Throughput::Elements(w.delta as u64));

    for (name, repo) in [("small_repo", &w.small), ("big_repo", &w.big)] {
        let round = Cell::new(0usize);
        group.bench_function(name, |b| {
            b.iter_batched(
                || {
                    let r = round.get();
                    round.set(r + 1);
                    dirty_sparse(&repo.garden, w.delta, r);
                },
                |()| {
                    rt.block_on(gardenbed::commit::commit(
                        &repo.garden.bed,
                        "bench commit",
                        None,
                    ))
                    .expect("commit works")
                },
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
    rt.block_on(w.small.server.shutdown());
    rt.block_on(w.big.server.shutdown());
}

criterion_group!(benches, bench_commit);
criterion_main!(benches);

//! `bed file <path>` — one file's raw bytes at the current changeset
//! (the M4 point-read path).
//!
//! * **warm** — blob in the local cache: the tree walk crosses the wire
//!   (a fresh session per command holds no tree cache), the blob is read
//!   from `.bed/objects`, the bytes are written out.
//! * **cold** — the blob removed from the cache in the untimed setup: one
//!   `GetBlob` round-trip plus the cache write ride on top. working copy's M4 perf
//!   budget (p95 < 50 ms over real processes) covers this same path with
//!   process-spawn costs included.

mod world;

use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use gardenbed_harness::GenOptions;
use gardenbed_model::Mode;

use world::Server;

struct World {
    _dir: tempfile::TempDir,
    garden: world::Garden,
    server: Server,
    /// A plain regular file of the sparse selection (and its blob id for the cold setup).
    path: String,
    blob: gardenbed_model::Id,
    content_len: usize,
}

async fn setup() -> World {
    let dir = world::temp_root();
    let history = world::seeded_history(&GenOptions {
        commits: 5,
        max_files: 40,
        ..GenOptions::default()
    });
    world::ingest(&history, &dir.path().join("server"));
    let server = Server::spawn(&dir.path().join("server")).await;
    let garden = world::Garden::plant(&dir.path().join("garden"), &server.http()).await;

    // Materialize the sparse selection, then pick one regular file.
    gardenbed::ops::sparse_add(&garden.bed, &[".".to_owned()], 8)
        .await
        .expect("sparse add materializes the fixture");
    let head_files = gardenbed_harness::generator::files_at(&history, history.changesets.len() - 1);
    let (path, (blob, _)) = head_files
        .iter()
        .find(|(_, (_, m))| *m == Mode::Regular)
        .expect("fixture has a regular file");
    let blob = *blob;
    let path = path.clone();

    // The warm read must work, and tell us the content length.
    let mut sink = Vec::new();
    gardenbed::ops::file(&garden.bed, &path, None, &mut sink)
        .await
        .expect("warm file read works");
    let content_len = sink.len();
    assert!(content_len > 0, "the fixture file is non-empty");

    World {
        _dir: dir,
        garden,
        server,
        path,
        blob,
        content_len,
    }
}

/// The object file for `id` in the working copy's cache (documented CAS layout:
/// `objects/<2 hex>/<62 hex>`).
fn cache_path(state_dir: &std::path::Path, id: gardenbed_model::Id) -> std::path::PathBuf {
    let hex = id.to_hex();
    state_dir.join("objects").join(&hex[..2]).join(&hex[2..])
}

fn bench_file_read(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut w = rt.block_on(setup());
    println!(
        "file_read fixture: {} ({} bytes, blob {})",
        w.path,
        w.content_len,
        w.blob.short()
    );

    let mut group = c.benchmark_group("file_read");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("warm", |b| {
        b.iter(|| {
            let mut sink = Vec::with_capacity(w.content_len);
            rt.block_on(gardenbed::ops::file(
                &w.garden.bed,
                &w.path,
                None,
                &mut sink,
            ))
            .expect("warm file read works");
            assert_eq!(sink.len(), w.content_len);
        })
    });

    group.bench_function("cold", |b| {
        b.iter_batched(
            || std::fs::remove_file(cache_path(&w.garden.bed, w.blob)).expect("blob uncached"),
            |()| {
                let mut sink = Vec::with_capacity(w.content_len);
                rt.block_on(gardenbed::ops::file(
                    &w.garden.bed,
                    &w.path,
                    None,
                    &mut sink,
                ))
                .expect("cold file read works");
                assert_eq!(sink.len(), w.content_len);
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
    rt.block_on(w.server.shutdown());
}

criterion_group!(benches, bench_file_read);
criterion_main!(benches);

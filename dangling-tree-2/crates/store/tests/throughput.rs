//! M1 verification: throughput baseline for loam. `#[ignore]`d so normal CI
//! stays fast; run with:
//!
//! ```sh
//! cargo test -p gardenbed-store --release --test throughput -- --ignored --nocapture
//! ```
//!
//! Puts and gets `BLOBS` blobs (default 100k) and prints ops/sec. Run once
//! per milestone; the numbers are recorded in the M1 report as the baseline
//! for the CI trend (v0.1.md M1 criterion 5 wants 1M-object baselines, which
//! is `GARDENBED_BENCH_BLOBS=1000000`).

mod common;

use common::compressible_bytes;
use common::random_bytes;
use gardenbed_store::{FsObjectStore, MemoryStore, ObjectStore};
use std::time::Instant;

/// Number of blobs. Override with `GARDENBED_BENCH_BLOBS`.
fn blob_count() -> u64 {
    std::env::var("GARDENBED_BENCH_BLOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000)
}

/// Deterministic per-index content: mixed compressible sizes 256 B..8 KiB.
fn bench_content(i: u64) -> Vec<u8> {
    let seed = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let len = 256 + (seed % 7936) as usize;
    // Every 4th blob is incompressible random data.
    if i.is_multiple_of(4) {
        random_bytes(seed, len)
    } else {
        compressible_bytes(seed, len)
    }
}

fn ops_per_sec(ops: u64, elapsed: std::time::Duration) -> f64 {
    ops as f64 / elapsed.as_secs_f64()
}

#[test]
#[ignore = "throughput baseline; run explicitly with --ignored"]
fn throughput_baseline_fs_loam() {
    let n = blob_count();
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    // ---- puts ----
    let mut ids = Vec::with_capacity(n as usize);
    let t_put = Instant::now();
    for i in 0..n {
        let content = bench_content(i);
        ids.push(store.put_blob(&content).expect("put"));
    }
    let put_time = t_put.elapsed();

    // ---- gets ----
    let t_get = Instant::now();
    for id in &ids {
        // get_blob runs the full envelope parse + length + SHA-256 verify.
        let _ = store.get_blob(*id).expect("get");
    }
    let get_time = t_get.elapsed();

    println!("fs-loam baseline: {n} blobs, content 256 B..8 KiB (75% compressible, 25% random)");
    println!(
        "  put: {n} ops in {put_time:?}  ->  {:.0} puts/sec",
        ops_per_sec(n, put_time)
    );
    println!(
        "  get: {n} ops in {get_time:?}  ->  {:.0} gets/sec",
        ops_per_sec(n, get_time)
    );
    println!(
        "  combined: {:.0} ops/sec (put+get per blob counted as 2 ops)",
        ops_per_sec(2 * n, put_time + get_time)
    );
}

#[test]
#[ignore = "throughput baseline; run explicitly with --ignored"]
fn throughput_baseline_memory() {
    let n = blob_count();
    let store = MemoryStore::new();

    let mut ids = Vec::with_capacity(n as usize);
    let t_put = Instant::now();
    for i in 0..n {
        ids.push(store.put_blob(&bench_content(i)).expect("put"));
    }
    let put_time = t_put.elapsed();

    let t_get = Instant::now();
    for id in &ids {
        store.get_blob(*id).expect("get");
    }
    let get_time = t_get.elapsed();

    println!(
        "memory baseline: put {:.0} ops/sec, get {:.0} ops/sec",
        ops_per_sec(n, put_time),
        ops_per_sec(n, get_time)
    );
}

//! `bed blame <path>` — the engine's first-parent lineage blame (D7),
//! measured through the server library (`Repo::blame`, the exact walk the
//! gRPC `Blame` handler runs; the wire and rendering costs ride on top and
//! are covered by bed's read-path budgets).
//!
//! The fixture is a first-parent chain of `GARDENBED_BENCH_BLAME_COMMITS`
//! (default 100k) changesets, each rewriting `log/tip.txt`'s first line —
//! a path touched by *every* commit, so the segment bloom can never skip:
//! this is the walk's worst case, where every step needs a changeset, a
//! blob, and a line diff. Two entry points into one chain:
//!
//! * **walk_10k** — blame at changeset 10_000 (10k-chain walk);
//! * **walk_full** — blame at the head (the whole chain).
//!
//! The chain is built in-process on tmpfs (fsync on, as deployed — tmpfs
//! keeps the fixture build out of disk-latency land). The walk descends
//! the chain from the queried changeset, so one fixture serves both sizes.
//!
//! Deliberately *not* measured here: blame of an untouched path in a huge
//! history (the segment-skip latency win — proven by the differential
//! tests) and the naive `blame_no_skip` reference. Those are design
//! properties, asserted in the derive test suite, not latencies to record.

mod world;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use gardenbed_model::{Change, Changeset, Identity, Mode, Op, blob_id};
use gardenbed_server::Repo;

/// The one path every commit of the chain touches.
const TIP: &str = "log/tip.txt";

/// Builds the chain: `n` changesets, each rewriting `TIP`'s first line
/// (line 2 stays stable so the walk's diff pairing has both shapes: an
/// added line and an inherited one). Returns every changeset id, oldest
/// first, and points `main` at the head.
fn build_blame_chain(root: &std::path::Path, n: usize) -> Vec<gardenbed_model::Id> {
    let repo = Repo::open(root).expect("repo opens");
    let ident = |k: usize| Identity {
        name: "Blamer".into(),
        email: "blame@bed.example".into(),
        time_secs: 1_750_000_000 + k as u64,
        tz_min: 0,
    };
    let mut parent: Option<gardenbed_model::Id> = None;
    let mut ids = Vec::with_capacity(n);
    let mut content = String::new();
    for k in 0..n {
        content.clear();
        write!(content, "tip line {k}\nstable second line\n").unwrap();
        let blob = blob_id(content.as_bytes());
        let cs = Changeset {
            parents: parent.into_iter().collect(),
            changes: vec![Change {
                path: TIP.into(),
                op: if k == 0 { Op::Add } else { Op::Modify },
                blob: Some(blob),
                src_path: None,
                mode: Mode::Regular,
            }],
            message: format!("blame chain {k}"),
            author: ident(k),
            committer: ident(k),
            signature: None,
        };
        let mut inline = BTreeMap::new();
        inline.insert(blob.to_hex(), content.as_bytes().to_vec());
        let id = repo
            .create_changeset(&cs, &inline)
            .expect("chain changeset ingests");
        ids.push(id);
        parent = Some(id);
    }
    repo.cas_ref("main", None, *ids.last().expect("non-empty chain"))
        .expect("main ref created");
    ids
}

fn bench_blame(c: &mut Criterion) {
    let n = world::env_usize("GARDENBED_BENCH_BLAME_COMMITS", 100_000, 10_000);
    let dir = world::temp_root();
    let root = dir.path().join("arboretum");
    let t0 = Instant::now();
    let ids = build_blame_chain(&root, n);
    println!(
        "blame fixture: {n} changesets ingested in {:.1?} (tmpfs, fsync on)",
        t0.elapsed()
    );

    // One Repo for all measurements: the derived data lives in the seedbed
    // and is read per call — this is the warm-server view of blame.
    let repo = Repo::open(&root).expect("repo reopens");

    let mut group = c.benchmark_group("blame");
    group.measurement_time(Duration::from_secs(3));

    // The 10k-chain walk (30 samples is plenty at this latency).
    group.sample_size(30);
    let cs_10k = ids[9_999.min(n - 1)];
    group.bench_function("walk_10k", |b| {
        b.iter(|| repo.blame(cs_10k, TIP).expect("blame walk works"))
    });

    // The full-chain walk — 100k steps at ~10 µs/step means second-scale
    // samples; fewer of them.
    if n > 10_000 {
        group.sample_size(10);
        let head = *ids.last().expect("non-empty chain");
        group.bench_function("walk_full", |b| {
            b.iter(|| repo.blame(head, TIP).expect("blame walk works"))
        });
    }

    group.finish();
}

criterion_group!(benches, bench_blame);
criterion_main!(benches);

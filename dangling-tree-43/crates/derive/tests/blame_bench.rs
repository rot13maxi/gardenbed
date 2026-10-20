//! M6 latency bench: blame wall time vs history length (M6 criterion 2).
//!
//! Ignored by default; run with:
//!
//! ```text
//! cargo test -p gardenbed-derive --release --test blame_bench -- --ignored --nocapture
//! ```
//!
//! One history shape at every size {1k, 10k, 100k} mainline commits:
//!
//! * a 2-parent merge (segment head) every [`SEGMENT`] mainline commits,
//!   each bringing one filler rewrite in from its side branch;
//! * plain commits rewrite one of [`FILLER`] filler files, so every
//!   changeset carries a change and segment blooms stay non-trivial;
//! * `flat.txt` is fully rewritten (all-new globally-unique lines) every
//!   ~[`TOUCH_EVERY`] mainline commits (~1% touch rate). Because every
//!   toucher rewrites the whole file, the walk terminates at the
//!   tip-ward-most toucher — at most [`TOUCH_EVERY`] commits below the
//!   tip — so the per-blame cost is set by the touch rate, not the
//!   history length. This is the gated curve (log-log slope < 0.3,
//!   100k < 200 ms): blame latency must not track history length;
//! * `deep.txt` is appended (never rewritten) on the same cadence — the
//!   adversarial deep lineage: every toucher leaves its appended lines
//!   unattributed, so the walk must visit every toucher down to the root.
//!   Cost is O(#segments + #touches): linear in n by design, with far
//!   fewer store reads than the naive walk. Reported for honesty, not
//!   gated.
//!
//! Gates (asserted): skip == naive at every size (the semantic
//! invariant); 100k-commit `blame` wall time < 200 ms; flat-curve
//! log-log slope < 0.3.

mod common;

use std::collections::HashSet;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

use common::ingest;
use gardenbed_derive::Engine;
use gardenbed_harness::GeneratedHistory;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore, ObjectStore};

/// Mainline commits between merges; merges are the segment heads.
const SEGMENT: u64 = 32;
/// Filler files rewritten by plain commits and merges (constant tree size,
/// so tree-derivation cost does not drift with history length).
const FILLER: usize = 64;
/// Hot-path touch cadence: ~1% of mainline commits touch the hot paths.
const TOUCH_EVERY: u64 = 100;
/// Lines per hot-file version.
const HOT_LINES: usize = 20;
/// History ladder (mainline commits) from M6 criterion 2. The doc's full
/// ladder also lists 1M commits; that is a nightly-scale job (ingest time
/// and memory), so the in-repo bench stops at 100k — the 10k→100k pair
/// already decides the curve's shape.
const SIZES: &[u64] = &[1_000, 10_000, 100_000];
/// M6 gate: 100k-commit blame < 200 ms.
const GATE_100K_SECS: f64 = 0.200;
/// M6 target: log-log slope < 0.3 (≤ O(log n) empirically).
const GATE_SLOPE: f64 = 0.3;

fn ident() -> Identity {
    Identity {
        name: "bench".into(),
        email: "bench@gardenbed".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

fn filler_path(i: u64) -> String {
    format!("filler{:02}.txt", i as usize % FILLER)
}

/// `flat.txt` version `i`: `HOT_LINES` globally-unique lines, so every
/// toucher's diff vs its parent rewrites the file completely.
fn flat_content(i: u64) -> Vec<u8> {
    (0..HOT_LINES)
        .map(|j| format!("flat line {i} row {j}\n"))
        .collect::<String>()
        .into_bytes()
}

fn deep_initial() -> Vec<u8> {
    (0..HOT_LINES)
        .map(|j| format!("deep base row {j}\n"))
        .collect::<String>()
        .into_bytes()
}

struct BenchHistory {
    h: GeneratedHistory,
    tip: Id,
    /// Hot-file touchers, oldest first (both hot paths are touched by the
    /// same commits).
    touchers: Vec<Id>,
    mainline: u64,
}

/// Deterministic append-only/filler history: no generator, no git.
fn build_history(mainline: u64) -> BenchHistory {
    struct B {
        cs: Vec<Changeset>,
        blobs: Vec<(Id, Vec<u8>)>,
        seen: HashSet<Id>,
    }
    impl B {
        fn blob(&mut self, content: Vec<u8>) -> Id {
            let id = blob_id(&content);
            if self.seen.insert(id) {
                self.blobs.push((id, content));
            }
            id
        }
        fn add(&mut self, path: &str, content: Vec<u8>) -> Change {
            Change {
                path: path.into(),
                op: Op::Add,
                blob: Some(self.blob(content)),
                src_path: None,
                mode: Mode::Regular,
            }
        }
        fn modify(&mut self, path: &str, content: Vec<u8>) -> Change {
            Change {
                path: path.into(),
                op: Op::Modify,
                blob: Some(self.blob(content)),
                src_path: None,
                mode: Mode::Regular,
            }
        }
        fn commit(&mut self, parents: Vec<Id>, mut changes: Vec<Change>, msg: String) -> Id {
            changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
            let cs = Changeset {
                parents,
                changes,
                message: msg,
                author: ident(),
                committer: ident(),
                signature: None,
            };
            let id = cs.id().expect("bench changeset encodes");
            self.cs.push(cs);
            id
        }
        fn finish(mut self, tip: Id) -> GeneratedHistory {
            self.blobs
                .sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            GeneratedHistory {
                changesets: self.cs,
                blob_contents: self.blobs,
                head: tip,
            }
        }
    }

    let mut b = B {
        cs: Vec::with_capacity(mainline as usize + mainline as usize / SEGMENT as usize + 1),
        blobs: Vec::new(),
        seen: HashSet::new(),
    };

    // Root: the two hot files plus the whole filler pool.
    let mut root_changes: Vec<Change> = (0..FILLER)
        .map(|f| {
            b.add(
                &format!("filler{f:02}.txt"),
                format!("filler base {f}\n").into_bytes(),
            )
        })
        .collect();
    root_changes.push(b.add("deep.txt", deep_initial()));
    root_changes.push(b.add("flat.txt", flat_content(0)));
    let root = b.commit(vec![], root_changes, "root".into());
    let mut prev = root;
    let mut touchers = Vec::new();

    // deep.txt accumulates one line per touch (append-only adversarial
    // lineage); flat.txt is fully rewritten at each touch.
    let mut deep: Vec<u8> = deep_initial();

    for i in 1..mainline {
        if i % SEGMENT == 0 {
            // Side branch rewrites one filler file; the merge brings the
            // same change in, so the merge's own diff vs its first parent
            // is that single rewrite (a head with a bounded path list).
            let content = format!("filler rewrite {i}\n").into_bytes();
            let path = filler_path(i);
            let side_change = b.modify(&path, content.clone());
            let side = b.commit(vec![prev], vec![side_change], format!("side {i}"));
            let merge_change = b.modify(&path, content);
            prev = b.commit(vec![prev, side], vec![merge_change], format!("merge {i}"));
        } else if i % TOUCH_EVERY == 0 {
            deep.extend(format!("deep appended {i}\n").as_bytes());
            let changes = vec![
                b.modify("deep.txt", deep.clone()),
                b.modify("flat.txt", flat_content(i)),
            ];
            let id = b.commit(vec![prev], changes, format!("hot {i}"));
            touchers.push(id);
            prev = id;
        } else {
            let changes = vec![b.modify(
                &filler_path(i),
                format!("filler rewrite {i}\n").into_bytes(),
            )];
            prev = b.commit(vec![prev], changes, format!("fill {i}"));
        }
    }

    BenchHistory {
        h: b.finish(prev),
        tip: prev,
        touchers,
        mainline,
    }
}

/// Best-of-N wall time (seconds): warm up once, then keep sampling until
/// either 50 ms has been spent or 512 reps are done; report the minimum
/// (the least-noise estimator for microbenchmarks).
fn measure<F: FnMut() -> R, R>(mut f: F) -> f64 {
    let _ = black_box(f());
    let mut best = f64::INFINITY;
    let mut spent = 0.0f64;
    let mut reps = 0usize;
    while reps < 4 || (spent < 0.050 && reps < 512) {
        let t = Instant::now();
        let _ = black_box(f());
        let d = t.elapsed().as_secs_f64();
        spent += d;
        if d < best {
            best = d;
        }
        reps += 1;
    }
    best
}

/// Skip-enabled blame wall time, measured per fresh engine.
///
/// A fresh engine per rep is what keeps this an honest walk measurement:
/// the engine memoizes finished blame walks (immutable history makes a
/// finished walk's answer final), so repeat queries on one engine would
/// time the memo hit, not the walk — and the M6 gate exists to bound the
/// walk. The stores are shared (warm), the engine's caches are not.
fn measure_fresh(
    objects: &Arc<dyn ObjectStore>,
    metadata: &Arc<dyn MetadataStore>,
    tip: Id,
    path: &str,
) -> f64 {
    measure(|| {
        Engine::new(objects.clone(), metadata.clone())
            .blame(tip, path)
            .expect("blame")
    })
}

/// The naive no-skip walk (never memoized), same fresh-engine discipline.
fn measure_naive(
    objects: &Arc<dyn ObjectStore>,
    metadata: &Arc<dyn MetadataStore>,
    tip: Id,
    path: &str,
) -> f64 {
    measure(|| {
        Engine::new(objects.clone(), metadata.clone())
            .blame_no_skip(tip, path)
            .expect("naive blame")
    })
}

fn fmt_secs(s: f64) -> String {
    if s < 1e-6 {
        format!("{:.0} ns", s * 1e9)
    } else if s < 1e-3 {
        format!("{:.1} µs", s * 1e6)
    } else if s < 1.0 {
        format!("{:.2} ms", s * 1e3)
    } else {
        format!("{:.2} s", s)
    }
}

/// Least-squares log-log slope over `(ln n, ln t)` points.
fn loglog_slope(points: &[(f64, f64)]) -> f64 {
    let k = points.len() as f64;
    let sx: f64 = points.iter().map(|p| p.0).sum();
    let sy: f64 = points.iter().map(|p| p.1).sum();
    let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    (k * sxy - sx * sy) / (k * sxx - sx * sx)
}

#[test]
#[ignore = "latency bench: cargo test -p gardenbed-derive --release --test blame_bench -- --ignored --nocapture"]
fn latency_curve_blame_vs_history_length() {
    println!("== M6 blame latency bench (best-of-N wall time per call) ==");
    println!(
        "history shape: mainline n; merge every {SEGMENT} (segment heads); \
         {FILLER}-file filler pool; hot paths touched every {TOUCH_EVERY} mainline commits"
    );

    // (n, build+ingest, flat skip, flat naive, deep skip, deep naive)
    // One measured row: `(n, prep, flat_skip, flat_naive, deep_skip, deep_naive)` seconds.
    type BenchRow = (u64, f64, f64, f64, f64, f64);
    let mut rows: Vec<BenchRow> = Vec::new();
    let mut touch_pct = 0.0;

    for &n in SIZES {
        let t0 = Instant::now();
        let bench = build_history(n);
        let (objects, metadata) = {
            let m = Arc::new(MemoryStore::new());
            (
                m.clone() as Arc<dyn ObjectStore>,
                m as Arc<dyn MetadataStore>,
            )
        };
        let _ids = ingest(&objects, &metadata, &bench.h, true);
        // Clone for the sanity-check engine; the measured reps below build
        // their own fresh engines from the same shared stores.
        let engine = Engine::new(objects.clone(), metadata.clone());
        let prep = t0.elapsed().as_secs_f64();

        // Semantic sanity at this size: skip == naive, and the expected
        // shapes (the flat walk stops at the tip-ward-most toucher; the
        // deep walk yields one range per append plus the root block).
        let flat_skip = engine.blame(bench.tip, "flat.txt").expect("flat blame");
        let flat_naive = engine
            .blame_no_skip(bench.tip, "flat.txt")
            .expect("flat naive");
        assert_eq!(flat_skip, flat_naive, "skip == naive (flat, n={n})");
        let nearest = *bench.touchers.last().expect("hot touches exist");
        assert_eq!(
            flat_skip,
            vec![(1, HOT_LINES as u64, nearest)],
            "flat walk terminates at the tip-ward-most toucher (n={n})"
        );
        let deep_skip = engine.blame(bench.tip, "deep.txt").expect("deep blame");
        let deep_naive = engine
            .blame_no_skip(bench.tip, "deep.txt")
            .expect("deep naive");
        assert_eq!(deep_skip, deep_naive, "skip == naive (deep, n={n})");
        assert_eq!(
            deep_skip.len(),
            bench.touchers.len() + 1,
            "one range per append + the root block (n={n})"
        );
        touch_pct = bench.touchers.len() as f64 * 100.0 / bench.mainline as f64;

        let t_flat_skip = measure_fresh(&objects, &metadata, bench.tip, "flat.txt");
        let t_flat_naive = measure_naive(&objects, &metadata, bench.tip, "flat.txt");
        let t_deep_skip = measure_fresh(&objects, &metadata, bench.tip, "deep.txt");
        let t_deep_naive = measure_naive(&objects, &metadata, bench.tip, "deep.txt");
        rows.push((
            n,
            prep,
            t_flat_skip,
            t_flat_naive,
            t_deep_skip,
            t_deep_naive,
        ));
    }

    let pct = |n: u64| match n {
        1_000 => "   1k".to_owned(),
        10_000 => "  10k".to_owned(),
        _ => " 100k".to_owned(),
    };
    println!(
        "{:>5} {:>14} {:>12} {:>12} {:>12} {:>12}",
        "size", "build+ingest", "flat skip", "flat naive", "deep skip", "deep naive"
    );
    for (n, prep, fs, fnv, ds, dn) in &rows {
        println!(
            "{} {:>14} {:>12} {:>12} {:>12} {:>12}",
            pct(*n),
            fmt_secs(*prep),
            fmt_secs(*fs),
            fmt_secs(*fnv),
            fmt_secs(*ds),
            fmt_secs(*dn)
        );
    }

    let slope = |pick: fn(&BenchRow) -> f64| {
        loglog_slope(
            &rows
                .iter()
                .map(|r| (r.0 as f64).ln())
                .zip(rows.iter().map(pick).map(f64::ln))
                .collect::<Vec<_>>(),
        )
    };
    let slope_flat_skip = slope(|r| r.2);
    let slope_flat_naive = slope(|r| r.3);
    let slope_deep_skip = slope(|r| r.4);
    let slope_deep_naive = slope(|r| r.5);

    println!("log-log slope over {{1k, 10k, 100k}}:");
    println!(
        "  flat skip = {slope_flat_skip:.3}   flat naive = {slope_flat_naive:.3}   \
         (target < {GATE_SLOPE})"
    );
    println!(
        "  deep skip = {slope_deep_skip:.3}   deep naive = {slope_deep_naive:.3}   \
         (adversarial deep lineage: O(#segments + #touches), linear by design — not gated)"
    );
    println!(
        "empirical hot-path touch rate: {touch_pct:.2}% of mainline commits; \
         naive/skip read amplification at 100k: flat {:.1}x, deep {:.1}x",
        rows[2].3 / rows[2].2,
        rows[2].5 / rows[2].4
    );

    let (_, _, flat_skip_100k, _, _, _) = rows[2];
    assert!(
        flat_skip_100k < GATE_100K_SECS,
        "100k-commit blame took {}, gate is < 200 ms",
        fmt_secs(flat_skip_100k)
    );
    assert!(
        slope_flat_skip < GATE_SLOPE,
        "flat-curve log-log slope {slope_flat_skip:.3} exceeds the < {GATE_SLOPE} target"
    );
    println!(
        "gates: 100k flat skip {} < 200 ms ✓; slope {slope_flat_skip:.3} < {GATE_SLOPE} ✓",
        fmt_secs(flat_skip_100k)
    );
}

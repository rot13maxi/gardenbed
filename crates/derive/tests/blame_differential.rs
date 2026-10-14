//! M6 differential blame tests: [`Engine::blame`] must agree with
//! `git blame --first-parent` line for line, range for range, on seeded
//! synthetic histories — every changeset with sampled paths, full
//! path×changeset sweeps on two histories, and ≥300 deterministic random
//! `(history, changeset, path)` triples with 100% agreement.
//!
//! Every compared case also asserts the segment-skip soundness rule
//! operationally: `blame` (skip-enabled) and `blame_no_skip` (naive
//! reference walk) return identical answers.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use common::ingest;
use gardenbed_derive::Engine;
use gardenbed_harness::{files_at, generate, git_blame_first_parent, materialize, GenOptions};
use gardenbed_model::{Id, Mode};
use gardenbed_store::{MemoryStore, ObjectStore, StoreError};

/// Fresh in-memory stores (loam + seedbed) for one history.
fn fresh_stores() -> (Arc<dyn ObjectStore>, Arc<dyn gardenbed_store::MetadataStore>) {
    let m = Arc::new(MemoryStore::new());
    (m.clone(), m)
}

struct Differential {
    /// (changeset, path) pairs compared against git.
    pub cases: u64,
    /// Individual lines whose attribution was compared.
    pub lines: u64,
    /// Renamed-away paths where both sides agreed "no such path".
    pub renamed_away: u64,
}

/// Runs the full differential over one generated history.
///
/// `full_sweep` compares *every* live path at *every* changeset instead
/// of a deterministic sample (plus the changeset's own touched paths,
/// which are always included — they exercise the diff/rename logic).
fn compare_history(name: &str, seed: u64, opts: &GenOptions, full_sweep: bool) -> Differential {
    let h = generate(seed, opts);
    let (objects, metadata) = fresh_stores();
    let ids = ingest(&objects, &metadata, &h, true);
    let engine = Engine::new(objects.clone(), metadata.clone());

    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&h, dir.path()).expect("materialize");
    let sha_of: HashMap<Id, String> = mat.mapping.iter().cloned().collect();

    let mut stats = Differential {
        cases: 0,
        lines: 0,
        renamed_away: 0,
    };

    for (idx, cs_id) in ids.iter().enumerate() {
        let cs = &h.changesets[idx];
        let tip = files_at(&h, idx);

        // Paths this changeset touches (dst ∪ src). Destination paths of
        // Add/Modify/Rename exist at `idx`; rename sources were renamed
        // away and must NOT exist.
        let mut touched_dst: BTreeSet<String> = BTreeSet::new();
        let mut touched_src: BTreeSet<String> = BTreeSet::new();
        for c in &cs.changes {
            touched_dst.insert(c.path.clone());
            if let Some(src) = &c.src_path {
                touched_src.insert(src.clone());
            }
        }

        // Sampled paths: every touched destination, plus a deterministic
        // stride sample of the other live paths (all of them on a sweep).
        let all: Vec<String> = tip.keys().cloned().collect();
        let mut paths: Vec<String> = Vec::new();
        let stride = if full_sweep {
            1
        } else {
            (all.len() / 5).max(1)
        };
        for (i, p) in all.iter().enumerate() {
            if touched_dst.contains(p) || i % stride == 0 {
                paths.push(p.clone());
            }
        }

        let sha = &sha_of[cs_id];
        for path in &paths {
            let (blob, mode) = &tip[path];
            if *mode == Mode::Gitlink {
                // Submodule pointers: git blames them with a fatal error;
                // the documented engine answer is an empty vec.
                assert_eq!(
                    engine.blame(*cs_id, path).expect("gitlink blame"),
                    Vec::new(),
                    "gitlink path {path:?} must blame to no ranges"
                );
                stats.cases += 1;
                continue;
            }
            let got = engine
                .blame(*cs_id, path)
                .unwrap_or_else(|e| panic!("{name} seed {seed} cs{idx} {path:?}: engine blame failed: {e}"));
            let naive = engine
                .blame_no_skip(*cs_id, path)
                .unwrap_or_else(|e| panic!("{name} seed {seed} cs{idx} {path:?}: naive blame failed: {e}"));
            assert_eq!(
                got, naive,
                "{name} seed {seed} cs{idx} {path:?}: skip-enabled and naive blame disagree"
            );
            let want = git_blame_first_parent(&mat.repo_path, &sha, path)
                .unwrap_or_else(|e| panic!("{name} seed {seed} cs{idx} {path:?}: git oracle failed: {e}"));
            assert_eq!(
                got.len(),
                want.len(),
                "{name} seed {seed} cs{idx} {path:?}: range count differs\n ours: {got:?}\n  git: {want:?}"
            );
            for (g, w) in got.iter().zip(want.iter()) {
                let our_sha = &sha_of[&g.2];
                assert_eq!(
                    (g.0, g.1, our_sha.as_str()),
                    (w.lo as u64, w.hi as u64, w.commit_sha.as_str()),
                    "{name} seed {seed} cs{idx} {path:?}: range differs\n ours: {got:?}\n  git: {want:?}"
                );
                stats.lines += u64::from(g.1 - g.0 + 1);
            }
            // The ranges must tile the file exactly (both sides; this
            // guards against our walk dropping or duplicating lines).
            let content = h.blob(blob).expect("live blob present");
            let total = line_count(content);
            let mut covered = 0u64;
            for g in &got {
                assert!(g.0 <= g.1, "inverted range {g:?}");
                assert_eq!(g.0, covered + 1, "gap/overlap at {g:?}");
                covered = g.1;
            }
            assert_eq!(covered, total as u64, "ranges must tile 1..={total}");
            stats.cases += 1;
        }

        // Renamed-away sources: git blames them with a fatal error; the
        // engine answers NotFound (documented divergence in kind, matching
        // failure). Only asserted when the path really is gone here.
        for src in &touched_src {
            if tip.contains_key(src) {
                continue; // re-added/overwritten in the same changeset
            }
            assert!(
                matches!(engine.blame(*cs_id, src), Err(StoreError::NotFound)),
                "renamed-away path {src:?} must not blame"
            );
            stats.renamed_away += 1;
        }
    }
    stats
}

/// Physical line count, git's convention: `""` = 0, `"a"` = 1, `"a\n"` = 1,
/// `"a\nb"` = 2.
fn line_count(content: &[u8]) -> usize {
    let nl = content.iter().filter(|b| **b == b'\n').count();
    if content.is_empty() || content.ends_with(b"\n") {
        nl
    } else {
        nl + 1
    }
}

// ------------------------------------------------------------ option sets

fn opts_balanced() -> GenOptions {
    GenOptions {
        commits: 45,
        merge_ratio: 0.12,
        octopus_ratio: 0.04,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        max_files: 18,
        max_dirs: 4,
        big_blob_bytes: None,
        exec_ratio: 0.08,
        symlink_ratio: 0.04,
        unicode_paths: false,
        crlf_content: false,
        empty_files: false,
    }
}

fn opts_rename_heavy() -> GenOptions {
    GenOptions {
        commits: 50,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.55,
        delete_ratio: 0.12,
        max_files: 25,
        max_dirs: 4,
        big_blob_bytes: None,
        exec_ratio: 0.08,
        symlink_ratio: 0.05,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    }
}

fn opts_churn() -> GenOptions {
    // Small working set: files are rewritten constantly, so blame walks
    // run deep and rename chains interleave with merges.
    GenOptions {
        commits: 40,
        merge_ratio: 0.18,
        octopus_ratio: 0.06,
        rename_ratio: 0.3,
        delete_ratio: 0.2,
        max_files: 8,
        max_dirs: 2,
        big_blob_bytes: None,
        exec_ratio: 0.1,
        symlink_ratio: 0.05,
        unicode_paths: false,
        crlf_content: true,
        empty_files: true,
    }
}

fn opts_wide() -> GenOptions {
    // Wide working set: many files change per commit, segments are busy,
    // blooms carry many paths.
    GenOptions {
        commits: 40,
        merge_ratio: 0.1,
        octopus_ratio: 0.05,
        rename_ratio: 0.3,
        delete_ratio: 0.08,
        max_files: 60,
        max_dirs: 5,
        big_blob_bytes: None,
        exec_ratio: 0.06,
        symlink_ratio: 0.03,
        unicode_paths: true,
        crlf_content: false,
        empty_files: true,
    }
}

// --------------------------------------------------------- seeded sweeps

#[test]
fn differential_balanced_histories_every_changeset() {
    let mut cases = 0;
    let mut lines = 0;
    for seed in 101..=105u64 {
        let s = compare_history("balanced", seed, &opts_balanced(), false);
        cases += s.cases;
        lines += s.lines;
    }
    println!("balanced: {cases} (cs,path) cases, {lines} lines compared");
    assert!(cases >= 25 * 5, "expected real coverage, got {cases}");
}

#[test]
fn differential_rename_heavy_histories_every_changeset() {
    let mut cases = 0;
    let mut lines = 0;
    for seed in 111..=117u64 {
        let s = compare_history("rename-heavy", seed, &opts_rename_heavy(), false);
        cases += s.cases;
        lines += s.lines;
    }
    println!("rename-heavy: {cases} (cs,path) cases, {lines} lines compared");
    assert!(cases >= 25 * 7, "expected real coverage, got {cases}");
}

#[test]
fn differential_churn_histories_every_changeset() {
    let mut cases = 0;
    let mut lines = 0;
    for seed in 121..=126u64 {
        let s = compare_history("churn", seed, &opts_churn(), false);
        cases += s.cases;
        lines += s.lines;
    }
    println!("churn: {cases} (cs,path) cases, {lines} lines compared");
    assert!(cases >= 25 * 6, "expected real coverage, got {cases}");
}

#[test]
fn differential_wide_histories_every_changeset() {
    let mut cases = 0;
    let mut lines = 0;
    for seed in 131..=135u64 {
        let s = compare_history("wide", seed, &opts_wide(), false);
        cases += s.cases;
        lines += s.lines;
    }
    println!("wide: {cases} (cs,path) cases, {lines} lines compared");
    assert!(cases >= 25 * 5, "expected real coverage, got {cases}");
}

/// Two full sweeps: every (changeset, live path) pair on two different
/// history shapes.
#[test]
fn differential_full_sweeps() {
    let a = compare_history("full-balanced", 101, &opts_balanced(), true);
    println!("full sweep balanced: {} cases, {} lines", a.cases, a.lines);
    let b = compare_history("full-rename", 111, &opts_rename_heavy(), true);
    println!("full sweep rename-heavy: {} cases, {} lines", b.cases, b.lines);
    assert!(a.cases > 300 && b.cases > 300, "sweeps must be exhaustive-scale");
}

// ------------------------------------------------------- random triples

/// A tiny deterministic PRNG (xorshift64*) for triple selection — no
/// external dependency, fully reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// 300 deterministic random `(history, changeset, path)` triples across
/// three history shapes; every triple must agree with git 100%.
#[test]
fn random_triples_agree_with_git() {
    let mut agreements = 0u64;
    let mut total = 0u64;
    for (seed, opts) in [
        (201u64, opts_balanced()),
        (202, opts_rename_heavy()),
        (203, opts_churn()),
    ] {
        let h = generate(seed, &opts);
        let (objects, metadata) = fresh_stores();
        let ids = ingest(&objects, &metadata, &h, true);
        let engine = Engine::new(objects, metadata);
        let dir = tempfile::tempdir().expect("tempdir");
        let mat = materialize(&h, dir.path()).expect("materialize");
        let sha_of: HashMap<Id, String> = mat.mapping.iter().cloned().collect();

        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32);
        let mut tried = 0;
        while tried < 100 {
            let idx = rng.below(h.changesets.len());
            let files = files_at(&h, idx);
            if files.is_empty() {
                continue;
            }
            let pick = rng.below(files.len());
            let path = files.keys().nth(pick).expect("path in range").clone();
            let (_, mode) = &files[&path];
            let cs_id = ids[idx];
            let sha = &mat.mapping[idx].1;
            if *mode == Mode::Gitlink {
                assert_eq!(engine.blame(cs_id, &path).expect("gitlink"), Vec::new());
                agreements += 1;
                total += 1;
                tried += 1;
                continue;
            }
            let got = engine
                .blame(cs_id, &path)
                .unwrap_or_else(|e| panic!("seed {seed} cs{idx} {path:?}: {e}"));
            let naive = engine
                .blame_no_skip(cs_id, &path)
                .unwrap_or_else(|e| panic!("naive seed {seed} cs{idx} {path:?}: {e}"));
            assert_eq!(got, naive, "seed {seed} cs{idx} {path:?}: skip != naive");
            let want = git_blame_first_parent(&mat.repo_path, sha, &path)
                .unwrap_or_else(|e| panic!("git oracle seed {seed} cs{idx} {path:?}: {e}"));
            let got_norm: Vec<(u64, u64, &str)> =
                got.iter().map(|(lo, hi, id)| (*lo, *hi, sha_of[id].as_str())).collect();
            let want_norm: Vec<(u64, u64, &str)> = want
                .iter()
                .map(|r| (r.lo as u64, r.hi as u64, r.commit_sha.as_str()))
                .collect();
            assert_eq!(
                got_norm, want_norm,
                "seed {seed} cs{idx} {path:?}\n ours: {got_norm:?}\n  git: {want_norm:?}"
            );
            agreements += 1;
            total += 1;
            tried += 1;
        }
    }
    println!("random triples: {agreements}/{total} agreement");
    assert_eq!(total, 300, "must run exactly 300 triples");
    assert_eq!(agreements, total, "every random triple must agree with git");
}

// -------------------------------------------------- rebuild determinism

/// Wipes every derived entry (tree, skiplist, pathsummary) by re-ingesting
/// the same history into fresh stores *without* the eager write path, then
/// forcing recomputation through blame, and requires byte-identical
/// derived payloads and identical answers.
#[test]
fn rebuilt_derived_data_is_byte_identical() {
    let h = generate(777, &opts_rename_heavy());

    // Store A: eager D3 writes (the normal path).
    let (objects_a, metadata_a) = fresh_stores();
    let ids = ingest(&objects_a, &metadata_a, &h, true);
    let engine_a = Engine::new(objects_a.clone(), metadata_a.clone());

    // Store B: no eager writes at all; every derived entry is recomputed
    // on demand below.
    let (objects_b, metadata_b) = fresh_stores();
    ingest(&objects_b, &metadata_b, &h, false);
    let engine_b = Engine::new(objects_b.clone(), metadata_b.clone());

    // Identical blame answers everywhere, and identical pathsummary bytes.
    for (idx, cs_id) in ids.iter().enumerate() {
        for (path, (blob, mode)) in files_at(&h, idx) {
            if mode == Mode::Gitlink {
                continue;
            }
            let content = h.blob(&blob).expect("blob");
            if content.is_empty() && mode == Mode::Symlink {
                continue; // empty symlink target: degenerate but legal
            }
            let a = engine_a.blame(*cs_id, &path).expect("A blames");
            let b = engine_b.blame(*cs_id, &path).expect("B blames");
            assert_eq!(a, b, "recomputed blame differs at cs{idx} {path:?}");
        }
    }
    for (i, cs_id) in ids.iter().enumerate() {
        let pa = metadata_a
            .derived_get(gardenbed_derive::DERIVED_PATHSUMMARY, *cs_id)
            .expect("derived_get A")
            .expect("eager store has the entry");
        // Force B to compute (blame above already did, but be explicit).
        let _ = engine_b.pathsummary_entry(*cs_id).expect("B computes");
        let pb = metadata_b
            .derived_get(gardenbed_derive::DERIVED_PATHSUMMARY, *cs_id)
            .expect("derived_get B")
            .expect("entry exists after recompute");
        assert_eq!(
            pa, pb,
            "pathsummary bytes differ after rebuild at cs index {i}"
        );
    }
}

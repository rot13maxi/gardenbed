//! Verification M2.6: `merge_base` / `is_ancestor` differential tests
//! against `git merge-base` on materialized histories (≥ 50 random pairs
//! per history), with the segmented skip-list path asserted equal to the
//! naive walk on every query, plus structural invariants of the stored
//! entries and lazy (cold-cache) recomputation.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use gardenbed_derive::{DERIVED_SKIPLIST, Engine, SegmentEntry};
use gardenbed_harness::{GenOptions, GeneratedHistory, generate, materialize};
use gardenbed_model::{Changeset, Id};
use gardenbed_store::{MemoryStore, MetadataStore, StoreError};

use common::git_merge_base;

fn history(seed: u64, commits: u32) -> GeneratedHistory {
    let opts = GenOptions {
        commits,
        merge_ratio: 0.3,
        octopus_ratio: 0.1,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        max_files: 16,
        max_dirs: 4,
        unicode_paths: true,
        ..Default::default()
    };
    generate(seed, &opts)
}

/// Deterministic xorshift pair source (mirrors the other suites).
fn pair_rng(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed ^ 0x9e3779b97f4a7c15;
    move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    }
}

/// Asserts the stored [`SegmentEntry`] of every changeset is structurally
/// consistent: heights match the parent recursion, `head` is a root or a
/// merge at exactly `head_dist` first-parent steps below, samples ascend by
/// powers of two and point at the right ancestors. Returns the entries.
fn check_entry_invariants(
    engine: &Engine,
    ids: &[Id],
    by_id: &HashMap<Id, Changeset>,
) -> HashMap<Id, SegmentEntry> {
    let entries: HashMap<Id, SegmentEntry> = ids
        .iter()
        .map(|&id| (id, engine.skiplist_entry(id).expect("segment entry")))
        .collect();
    for id in ids {
        let cs = &by_id[id];
        let e = &entries[id];
        // Height recursion: h(cs) = 1 + max(h(parents)), h(root) = 0.
        let expected_height = cs
            .parents
            .iter()
            .map(|p| entries[p].height)
            .max()
            .map_or(0, |h| h + 1);
        assert_eq!(e.height, expected_height, "height of {id}");
        // Head structure: a head is exactly a non-first-parent-only node.
        assert_eq!(e.is_head, cs.parents.len() != 1, "is_head of {id}");
        if cs.parents.len() == 1 {
            let pe = &entries[&cs.parents[0]];
            if pe.is_head {
                assert_eq!(e.head.as_ref(), Some(&cs.parents[0]), "head of {id}");
                assert_eq!(e.head_dist, 1, "head_dist of {id}");
            } else {
                assert_eq!(e.head, pe.head, "head of {id} inherited");
                assert_eq!(e.head_dist, pe.head_dist + 1, "head_dist of {id}");
            }
        } else {
            assert_eq!(e.head, None, "a head carries no head");
            assert_eq!(e.head_dist, 0, "a head sits at distance 0");
        }
        // Samples: distances 1, 2, 4, … ascending; each target is exactly
        // `d` first-parent steps below `id` and stays within the segment.
        let mut dist = 1u64;
        for (d, target) in &e.samples {
            assert_eq!(*d, dist, "sample distance sequence of {id}");
            dist *= 2;
            let mut cur = *id;
            for _ in 0..*d {
                cur = by_id[&cur].parents[0];
            }
            assert_eq!(*target, cur, "sample target at distance {d} of {id}");
            // The target stays within the segment — either strictly inside
            // it (same head) or exactly at the segment head itself (whose
            // own `head` field is legitimately `None`).
            assert!(
                entries[&cur].head == e.head || Some(cur) == e.head,
                "sample {d} of {id} left the segment"
            );
        }
        assert!(
            e.samples.is_empty() || e.samples.last().expect("non-empty").0 <= e.head_dist,
            "samples of {id} overshoot the segment"
        );
        // Distances are powers of two (binary lifting).
        for (d, _) in &e.samples {
            assert!(d.is_power_of_two(), "sample distance {d} of {id}");
        }
    }
    entries
}

/// Checks one (a, b) pair through every implementation:
/// naive walk, skip-list walk, the public merged entry point, and git.
fn check_pair(
    engine: &Engine,
    repo: &std::path::Path,
    sha_by_cs: &HashMap<Id, String>,
    cs_by_sha: &HashMap<String, Id>,
    a: Id,
    b: Id,
) {
    let naive_anc = engine.is_ancestor_naive(a, b).expect("naive is_ancestor");
    let skip_anc = engine.is_ancestor_skip(a, b).expect("skip is_ancestor");
    let merged_anc = engine.is_ancestor(a, b).expect("is_ancestor");
    assert_eq!(
        naive_anc, skip_anc,
        "skip-list is_ancestor({a},{b}) diverged from naive"
    );
    assert_eq!(
        naive_anc, merged_anc,
        "public is_ancestor({a},{b}) diverged from naive"
    );

    let naive_base = engine.merge_base_naive(a, b).expect("naive merge base");
    let merged_base = engine.merge_base(a, b).expect("merge base");
    assert_eq!(
        naive_base, merged_base,
        "public merge_base({a},{b}) diverged from naive"
    );

    // git is the oracle. `a` is an ancestor of `b` iff merge-base(a,b) == a.
    let (sha_a, sha_b) = (&sha_by_cs[&a], &sha_by_cs[&b]);
    let git_base_sha = git_merge_base(repo, sha_a, sha_b);
    let git_base = git_base_sha.as_ref().map(|sha| {
        cs_by_sha
            .get(sha)
            .copied()
            .unwrap_or_else(|| panic!("git merge-base sha {sha} is not a history changeset"))
    });
    assert_eq!(
        naive_base, git_base,
        "merge base({a},{b}) disagrees with git (ours {naive_base:?}, git {git_base_sha:?})"
    );
    let git_anc = git_base == Some(a);
    assert_eq!(
        naive_anc, git_anc,
        "is_ancestor({a},{b}) disagrees with git"
    );
}

/// Runs the full differential battery on one generated history.
///
/// `eager = true` exercises the D3 write path (`on_changeset_created`
/// derives every entry at ingest); `eager = false` leaves the derived
/// store empty so the queries must recompute lazily — both must agree.
fn run_history(seed: u64, commits: u32, eager: bool) {
    let h = history(seed, commits);
    let m = Arc::new(MemoryStore::new());
    let objects: Arc<dyn gardenbed_store::ObjectStore> = m.clone();
    let metadata: Arc<dyn MetadataStore> = m.clone();
    let engine = Engine::new(objects.clone(), metadata.clone());
    let ids = common::ingest(&objects, &metadata, &h, eager);
    assert_eq!(ids.len(), h.changesets.len());

    let by_id: HashMap<Id, Changeset> = h
        .changesets
        .iter()
        .map(|cs| (cs.id().expect("changeset id"), cs.clone()))
        .collect();
    assert_eq!(by_id.len(), ids.len(), "generated changeset ids are unique");

    // Materialize into git for the oracle; build both id→sha and sha→id.
    let git_dir = tempfile::tempdir().expect("git tempdir");
    let mat = materialize(&h, git_dir.path()).expect("materialize");
    let sha_by_cs: HashMap<Id, String> = mat
        .mapping
        .iter()
        .map(|(cs, sha)| (*cs, sha.clone()))
        .collect();
    let cs_by_sha: HashMap<String, Id> = mat
        .mapping
        .iter()
        .map(|(cs, sha)| (sha.clone(), *cs))
        .collect();
    assert_eq!(sha_by_cs.len(), ids.len());

    // Structural invariants of every skip-list entry.
    check_entry_invariants(&engine, &ids, &by_id);

    if eager {
        // Wipe every skiplist entry (a zero-length payload decodes to a
        // miss) and re-derive through the read path; the recomputed entries
        // must be structurally identical.
        for &id in &ids {
            m.derived_put(DERIVED_SKIPLIST, id, &[])
                .expect("clear entry");
        }
        check_entry_invariants(&engine, &ids, &by_id);
    }

    // Random pairs: ≥ 50 required; run 60.
    let mut next = pair_rng(seed ^ 0x51ed270b);
    for _ in 0..60 {
        let a = ids[(next() as usize) % ids.len()];
        let b = ids[(next() as usize) % ids.len()];
        check_pair(&engine, git_dir.path(), &sha_by_cs, &cs_by_sha, a, b);
    }

    // Anchor cases: every changeset is its own merge base and ancestor, on
    // every path and in git; the root is an ancestor of everything.
    for (i, &id) in ids.iter().enumerate() {
        assert_eq!(engine.merge_base(id, id).expect("self base"), Some(id));
        assert!(engine.is_ancestor(id, id).expect("self ancestor"));
        assert!(
            engine
                .is_ancestor_skip(id, id)
                .expect("self ancestor (skip)")
        );
        assert!(
            engine
                .is_ancestor_naive(id, id)
                .expect("self ancestor (naive)")
        );
        let sha = &sha_by_cs[&id];
        assert_eq!(
            git_merge_base(git_dir.path(), sha, sha).as_deref(),
            Some(sha.as_str())
        );
        if i > 0 {
            let root = ids[0];
            assert!(engine.is_ancestor(root, id).expect("root ancestor"));
            assert_eq!(engine.merge_base(root, id).expect("root base"), Some(root));
        }
    }

    // Unknown changesets must error, never silently answer.
    let bogus = Id::from_bytes([7u8; 32]);
    assert!(matches!(
        engine.merge_base(bogus, ids[0]),
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        engine.is_ancestor_skip(bogus, ids[0]),
        Err(StoreError::NotFound)
    ));
}

#[test]
fn ancestry_differential_seed21_eager() {
    run_history(21, 60, true);
}

#[test]
fn ancestry_differential_seed77_lazy() {
    run_history(77, 40, false);
}

#[test]
fn ancestry_differential_seed555_eager() {
    run_history(555, 50, true);
}

// ------------------------------------------------ error propagation

/// A metadata wrapper whose `get_changeset` fails with `StoreError::Db`
/// once armed: exercises that genuine store errors propagate out of
/// `is_ancestor` instead of silently degrading to the naive walk.
struct FailingMetadata {
    inner: MemoryStore,
    fail: std::sync::atomic::AtomicBool,
}

impl MetadataStore for FailingMetadata {
    fn put_changeset(&self, cs: &Changeset) -> Result<Id, StoreError> {
        self.inner.put_changeset(cs)
    }
    fn get_changeset(&self, id: Id) -> Result<Option<(Changeset, Vec<u8>)>, StoreError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Db("injected failure".into()));
        }
        self.inner.get_changeset(id)
    }
    fn resolve_ref(&self, name: &str) -> Result<Option<Id>, StoreError> {
        self.inner.resolve_ref(name)
    }
    fn list_refs(&self) -> Result<Vec<(String, Id)>, StoreError> {
        self.inner.list_refs()
    }
    fn cas_ref(
        &self,
        name: &str,
        expected: Option<Id>,
        new: Id,
    ) -> Result<gardenbed_store::RefCas, StoreError> {
        self.inner.cas_ref(name, expected, new)
    }
    fn record_git_mapping(&self, git_sha_hex: &str, cs: Id) -> Result<(), StoreError> {
        self.inner.record_git_mapping(git_sha_hex, cs)
    }
    fn cs_for_git(&self, git_sha_hex: &str) -> Result<Option<Id>, StoreError> {
        self.inner.cs_for_git(git_sha_hex)
    }
    fn git_for_cs(&self, cs: Id) -> Result<Option<String>, StoreError> {
        self.inner.git_for_cs(cs)
    }
    fn record_export_mappings(
        &self,
        pairs: &[(String, Id)],
    ) -> Result<gardenbed_store::ExportMappingReport, StoreError> {
        self.inner.record_export_mappings(pairs)
    }
    fn derived_put(&self, kind: &str, cs: Id, bytes: &[u8]) -> Result<(), StoreError> {
        self.inner.derived_put(kind, cs, bytes)
    }
    fn derived_get(&self, kind: &str, cs: Id) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.derived_get(kind, cs)
    }
    fn recent_changesets(&self, limit: u32) -> Result<Vec<Id>, StoreError> {
        self.inner.recent_changesets(limit)
    }
}

/// The same check with real data: ingest normally, then arm the failure
/// and verify every public entry point errors with `Db`.
#[test]
fn is_ancestor_propagates_db_errors_with_data() {
    let h = history(4321, 12);
    let shared = Arc::new(MemoryStore::new());
    let objects: Arc<dyn gardenbed_store::ObjectStore> = shared.clone();

    // Ingest into the wrapper's inner store first (fail disarmed).
    let wrapped = Arc::new(FailingMetadata {
        inner: MemoryStore::new(),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    {
        let inner_meta: Arc<dyn MetadataStore> = wrapped.clone();
        common::ingest(&objects, &inner_meta, &h, true);
    }

    let engine = Engine::new(objects, wrapped.clone());
    let a = h.changesets[0].id().expect("encodes");
    let b = h.changesets.last().unwrap().id().expect("encodes");

    // Sanity: healthy store answers fine through the wrapper.
    engine.is_ancestor(a, b).expect("healthy is_ancestor");

    // Arm the failure: every path must surface the Db error.
    wrapped
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(engine.is_ancestor(a, b), Err(StoreError::Db(_))));
    assert!(matches!(engine.merge_base(a, b), Err(StoreError::Db(_))));
}

//! `--limit` slicing: the newest-N suffix of the topological order,
//! re-rooting at slice boundaries, exact counters, and parent-structure
//! rules on merged history.

mod common;

use std::collections::BTreeMap;

use common::{git_map_rows, import, open_stores, ref_rows, Fixture};
use gardenbed_harness::{files_at, git_log, GenOptions};
use gardenbed_model::{Id, Mode, Op};
use gardenbed_rootstock::ImportOptions;
use gardenbed_store::MetadataStore;

fn linear_opts() -> GenOptions {
    GenOptions {
        commits: 40,
        merge_ratio: 0.0,
        octopus_ratio: 0.0,
        rename_ratio: 0.1,
        delete_ratio: 0.1,
        ..GenOptions::default()
    }
}

fn merged_opts() -> GenOptions {
    GenOptions {
        commits: 40,
        merge_ratio: 0.2,
        octopus_ratio: 0.05,
        rename_ratio: 0.15,
        delete_ratio: 0.1,
        ..GenOptions::default()
    }
}

/// The imported changeset of a git commit sha, plus its changeset id.
fn cs_of(garden: &std::path::Path, sha: &str) -> (gardenbed_model::Changeset, Id) {
    let (_, metadata, _) = open_stores(garden);
    let cs_id = metadata
        .cs_for_git(sha)
        .expect("cs_for_git")
        .unwrap_or_else(|| panic!("commit {sha} not mapped"));
    let cs = metadata.get_changeset(cs_id).expect("get").expect("exists").0;
    (cs, cs_id)
}

/// The changeset id mapped to a git sha, if any.
fn mapped_cs(garden: &std::path::Path, sha: &str) -> Option<Id> {
    let (_, metadata, _) = open_stores(garden);
    metadata.cs_for_git(sha).expect("cs_for_git")
}

/// The derived tree of an imported commit as a path → (blob, mode) map.
fn derived(garden: &std::path::Path, cs_id: Id) -> BTreeMap<String, (Id, Mode)> {
    let (_, _, engine) = open_stores(garden);
    match engine.derive_tree(cs_id).expect("derive") {
        None => BTreeMap::new(),
        Some(t) => engine
            .flatten_tree(Some(t))
            .expect("flatten")
            .into_iter()
            .map(|(p, m, b)| (p, (b, m)))
            .collect(),
    }
}

/// Asserts that the commit at generated `index` is stored as a re-rooted
/// full-tree root changeset reproducing the generated state.
fn assert_reroot_full_tree(fixture: &Fixture, index: usize) {
    let sha = &fixture.mapping[index].1;
    let (cs, cs_id) = cs_of(&fixture.garden, sha);
    assert!(
        cs.parents.is_empty(),
        "commit {sha}: slice boundary must be re-rooted (no parents)"
    );
    assert!(!cs.changes.is_empty(), "commit {sha}: re-root carries adds");
    for c in &cs.changes {
        assert_eq!(c.op, Op::Add, "commit {sha}: re-root changes are all adds");
    }
    let want = files_at(&fixture.history, index);
    let got = derived(&fixture.garden, cs_id);
    assert_eq!(got, want, "commit {sha}: re-rooted tree must equal the history state");
}

/// A linear history sliced to its newest 10 commits: exact counts, a
/// re-rooted boundary carrying the full tree, normal deltas above it,
/// a free re-import, and an exact extension to limit=20.
#[test]
fn limit_slices_newest_commits_and_reroots_boundary() {
    let fixture = Fixture::generate(424_242, &linear_opts());
    let total = fixture.history.changesets.len();
    assert!(total >= 40, "generator should honor the commit count");

    let opts = ImportOptions {
        limit: Some(10),
        ..Default::default()
    };
    let report = import(&fixture.repo, &fixture.garden, &opts);
    assert_eq!(report.commits_new, 10, "exactly the slice is ingested");
    assert_eq!(report.commits_skipped, 0, "fresh store: nothing mapped");
    assert_eq!(report.commits_total, 10);
    assert_eq!(report.refs_updated.len(), 1, "the tip is inside the slice");
    assert_eq!(git_map_rows(&fixture.garden).len(), 10);

    // The boundary (oldest sliced commit) is re-rooted with the full tree.
    assert_reroot_full_tree(&fixture, total - 10);

    // Everything above the boundary is a normal single-parent delta whose
    // derived tree equals the generated state.
    for i in total - 9..total {
        let sha = &fixture.mapping[i].1;
        let (cs, _) = cs_of(&fixture.garden, sha);
        let parent_id = mapped_cs(&fixture.garden, &fixture.mapping[i - 1].1)
            .expect("slice parent is mapped");
        assert_eq!(
            cs.parents,
            vec![parent_id],
            "commit {sha}: linear history links its single parent"
        );
        let (_, cs_id) = cs_of(&fixture.garden, sha);
        assert_eq!(
            derived(&fixture.garden, cs_id),
            files_at(&fixture.history, i),
            "commit {sha}: derived tree differs from the generated history"
        );
    }

    // Re-importing the same limit is free.
    let again = import(&fixture.repo, &fixture.garden, &opts);
    assert_eq!(again.commits_new, 0);
    assert_eq!(again.objects_written, 0);
    assert!(again.refs_updated.is_empty());
    assert_eq!(again.commits_total, 10, "the slice is skipped entirely");
    assert_eq!(again.commits_skipped, 10);

    // Extending to limit=20 ingests exactly the 10 older commits; the new
    // boundary is re-rooted; the previous slice is untouched.
    let rows_before = git_map_rows(&fixture.garden);
    let opts20 = ImportOptions {
        limit: Some(20),
        ..Default::default()
    };
    let report20 = import(&fixture.repo, &fixture.garden, &opts20);
    assert_eq!(report20.commits_new, 10, "exactly the extension");
    assert_eq!(report20.commits_skipped, 10, "mapped commits inside the slice");
    assert_eq!(report20.commits_total, 20);
    assert_eq!(
        report20.commits_total,
        report20.commits_new + report20.commits_skipped
    );
    assert!(report20.objects_written > 0);
    assert!(report20.refs_updated.is_empty(), "the tip does not move");
    let rows_after = git_map_rows(&fixture.garden);
    assert_eq!(rows_after.len(), 20);
    for row in &rows_before {
        assert!(rows_after.contains(row), "previous slice rows are untouched");
    }
    assert_reroot_full_tree(&fixture, total - 20);
}

/// `limit = 0` imports nothing and records no refs.
#[test]
fn limit_zero_is_a_noop() {
    let fixture = Fixture::generate(121, &linear_opts());
    let opts = ImportOptions {
        limit: Some(0),
        ..Default::default()
    };
    let report = import(&fixture.repo, &fixture.garden, &opts);
    assert_eq!(report.commits_new, 0);
    assert_eq!(report.commits_total, 0);
    assert!(
        report.refs_updated.is_empty(),
        "tips outside the slice are not recorded"
    );
    assert!(git_map_rows(&fixture.garden).is_empty());
    assert!(ref_rows(&fixture.garden).is_empty());
}

/// On merged history, a limit slice keeps every parent that is mapped or
/// inside the slice (in git order) and drops the others; a boundary
/// commit whose first parent is outside is re-rooted (no parents, full
/// tree).
#[test]
fn limit_on_merged_history_links_only_slice_parents() {
    let fixture = Fixture::generate(555, &merged_opts());
    let opts = ImportOptions {
        limit: Some(12),
        ..Default::default()
    };
    let report = import(&fixture.repo, &fixture.garden, &opts);
    assert_eq!(report.commits_new, 12, "exactly the slice is ingested");
    assert_eq!(git_map_rows(&fixture.garden).len(), 12);

    // Generated index of a git sha (fixture.mapping is index-aligned with
    // the generated changesets).
    let index_of = |sha: &str| -> usize {
        fixture
            .mapping
            .iter()
            .position(|(_, s)| s == sha)
            .unwrap_or_else(|| panic!("sha {sha} is not part of the generated history"))
    };

    for sha_hex in git_map_rows(&fixture.garden).iter().map(|(s, _)| s.clone()) {
        let log = &git_log(&fixture.repo, &sha_hex, 1).expect("git log")[0];
        let (cs, cs_id) = cs_of(&fixture.garden, &sha_hex);

        let first_outside = match log.parents.first() {
            None => false,
            Some(p) => mapped_cs(&fixture.garden, p).is_none(),
        };
        if first_outside {
            assert!(
                cs.parents.is_empty(),
                "commit {sha_hex}: boundary commit must be re-rooted"
            );
            assert_reroot_full_tree(&fixture, index_of(&sha_hex));
            continue;
        }

        // Parents are exactly the mapped git parents, in git order.
        let mapped: Vec<Id> = log
            .parents
            .iter()
            .filter_map(|p| mapped_cs(&fixture.garden, p))
            .collect();
        assert_eq!(
            cs.parents, mapped,
            "commit {sha_hex}: parents must be the mapped git parents in order"
        );
        // Non-root sliced commits with in-slice parents derive correctly.
        if !cs.parents.is_empty() {
            assert_eq!(
                derived(&fixture.garden, cs_id),
                files_at(&fixture.history, index_of(&sha_hex)),
                "commit {sha_hex}: derived tree differs from the generated history"
            );
        }
    }
}

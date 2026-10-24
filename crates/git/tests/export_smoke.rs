//! The native Windows export smoke: the git pack-writer publishes through
//! `fsync_rename`, whose directory fsync needs `FILE_FLAG_BACKUP_SEMANTICS`
//! **and** `GENERIC_WRITE` on Windows (FlushFileBuffers' documented access
//! requirement). Without the write-capable handle the export fails with
//! `ERROR_ACCESS_DENIED` — the same os error 5 the store's gblo2 suite hit
//! on the first native runs. This smoke proves the export path works on a
//! real Windows kernel: import a tiny generated repo, export it back, and
//! let git itself verify (`fsck --strict` + ref presence + exact SHA
//! reproduction, the export suite's own oracles).
//!
//! It is portable and also runs on Linux (same coverage for free); git must
//! be on PATH (it is on GitHub windows runners and everywhere Linux CI runs).

mod common;

use std::collections::BTreeMap;

use gardenbed_git::{ExportOptions, import_from_path};
use gardenbed_harness::GenOptions;
use gardenbed_harness::oracle::git_fsck;

use common::{Fixture, open_stores};

/// Small history — enough shape (merges off, a handful of files) to
/// exercise the pack write and its directory-entry sync, without the
/// full suite's 50-commit volume.
fn small_opts() -> GenOptions {
    GenOptions {
        commits: 5,
        merge_ratio: 0.0,
        symlink_ratio: 0.0,
        ..GenOptions::default()
    }
}

#[test]
fn export_publishes_through_the_directory_fsync_smoke() {
    let fx = Fixture::generate(9021, &small_opts());
    let (objects, metadata, engine) = open_stores(&fx.garden);
    let report = import_from_path(
        &fx.repo,
        objects.clone(),
        metadata.clone(),
        engine,
        &gardenbed_git::ImportOptions::default(),
    )
    .expect("import succeeds");
    assert_eq!(report.commits_new, fx.mapping.len() as u64);

    // The export publishes its pack via rename + parent-dir fsync; on
    // Windows this is the FlushFileBuffers path under test.
    let target = fx.dir.path().join("export.git");
    let export =
        gardenbed_git::export_to_path(&target, objects, metadata, &ExportOptions::default())
            .expect("export publishes (directory fsync included)");

    assert_eq!(
        export.commits_exported,
        fx.mapping.len() as u64,
        "one commit per imported changeset"
    );

    // git itself is the oracle: fsck-strict clean, and every exported
    // (changeset → sha) pair reproduces the fixture's materialized shas.
    git_fsck(&target).expect("exported repository is fsck-clean");
    let mut exported: BTreeMap<String, String> = export
        .commits
        .iter()
        .map(|(cs, sha)| (cs.to_hex(), sha.clone()))
        .collect();
    for (cs, sha) in &fx.mapping {
        assert_eq!(
            exported.remove(&cs.to_hex()),
            Some(sha.clone()),
            "exported sha for {cs} must reproduce the materialized one"
        );
    }
    assert!(exported.is_empty(), "no extra commits exported");
}

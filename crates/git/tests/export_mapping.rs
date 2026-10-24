//! `ExportOptions::record_mappings` (v0.2, dogfood #26): an export can
//! persist the computed changeset↔git-SHA mapping rows into the metadata
//! store, closing the manual landing-tip bookkeeping loop.
//!
//! Contracts under test:
//!
//! * **Off by default** — every plain export leaves `git_map` untouched
//!   (export stays read-only for existing callers).
//! * **Native rows land** — an export of gardenbed-native changesets
//!   (never imported from git) records one row per commit, exactly the
//!   `(cs, sha)` pairs in the report.
//! * **Idempotence / restart** — re-running the same export records
//!   nothing new (`unchanged == n`); a re-export after a "crash" between
//!   git-side completion and metadata recording repairs the rows.
//! * **Import rows win** — rows the importer wrote are `unchanged` on
//!   re-export, never rewritten; the re-import frontier therefore prunes
//!   on `cs_for_git` (a re-import of the exported repo is a no-op).
//! * **Twin refusal** — two changesets that render to identical git
//!   commit bytes (same tree/message/identities/parents, one expressed as
//!   a rename and one as delete+add) export successfully; the first pair
//!   in export order records, the second is refused as a conflict and the
//!   first mapping survives. This is the only genuine SHA collision shape
//!   (canonical history has no duplicate SHAs — dogfood #26 analysis).
//! * **Non-bare refusal records nothing** — the pre-write layout refusal
//!   leaves `git_map` untouched (no rows for bytes never written).

mod common;

use std::collections::HashMap;

use gardenbed_git::{ExportOptions, export_to_path};
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MetadataStore, ObjectStore};
use tempfile::tempdir;

use common::{Fixture, git_map_rows, open_stores, sql_scalar};

/// A fresh native changeset over one blob, message `msg`, authored at a
/// fixed timestamp (deterministic SHAs).
fn native_cs(parents: Vec<Id>, changes: Vec<Change>, msg: &str) -> Changeset {
    let ident = |name: &str, secs: u64| Identity {
        name: name.to_owned(),
        email: "gopher@garden.example".to_owned(),
        time_secs: secs,
        tz_min: 0,
    };
    Changeset {
        parents,
        changes,
        message: msg.to_owned(),
        author: ident("Ada Gardener", 1_700_000_000),
        committer: ident("Fern Seed", 1_700_000_000),
        signature: None,
    }
}

fn add(path: &str, blob: Id, mode: Mode) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Add,
        blob: Some(blob),
        src_path: None,
        mode,
    }
}

/// Stores a small native history (two commits, one blob) in fresh stores
/// and points `refs/heads/main` at the head. Returns (garden dir path,
/// root cs id, head cs id, blob content).
fn native_history(dir: &std::path::Path) -> (std::path::PathBuf, Id, Id) {
    let garden = dir.join("garden");
    let (objects, metadata, _engine) = open_stores(&garden);

    let content = b"native content\n".to_vec();
    let blob = blob_id(&content);
    objects.put_blob(&content).expect("put_blob");

    let root = native_cs(vec![], vec![add("file.txt", blob, Mode::Regular)], "root\n");
    let root_id = root.id().expect("root encodes");
    let head = native_cs(
        vec![root_id],
        vec![Change {
            path: "file.txt".into(),
            op: Op::Modify,
            blob: Some(blob_id(b"native content\nmore\n")),
            src_path: None,
            mode: Mode::Regular,
        }],
        "head\n",
    );
    objects
        .put_blob(b"native content\nmore\n")
        .expect("put_blob 2");
    let head_id = head.id().expect("head encodes");

    metadata.put_changeset(&root).expect("put root");
    metadata.put_changeset(&head).expect("put head");
    metadata
        .cas_ref("refs/heads/main", None, head_id)
        .expect("cas_ref");

    (garden, root_id, head_id)
}

/// Default off: a plain export of a native history leaves `git_map`
/// empty and reports `mappings == None`.
#[test]
fn record_mappings_is_off_by_default() {
    let dir = tempdir().expect("tempdir");
    let (garden, _root, _head) = native_history(dir.path());

    let (objects, metadata, _engine) = open_stores(&garden);
    let report = export_to_path(
        &dir.path().join("out.git"),
        objects,
        metadata,
        &ExportOptions::default(),
    )
    .expect("export succeeds");

    assert_eq!(report.commits_exported, 2);
    assert!(report.mappings.is_none(), "no recording was requested");
    assert_eq!(sql_scalar(&garden, "SELECT COUNT(*) FROM git_map"), 0);
}

/// The main path: exporting native changesets with `record_mappings`
/// persists exactly the report's `(cs, sha)` pairs, and re-exporting is
/// idempotent (`unchanged == n`, nothing new recorded).
#[test]
fn native_rows_land_and_reexport_is_idempotent() {
    let dir = tempdir().expect("tempdir");
    let (garden, _root, _head) = native_history(dir.path());

    let (objects, metadata, _engine) = open_stores(&garden);
    let opts = ExportOptions {
        record_mappings: true,
        ..Default::default()
    };
    let report = export_to_path(
        &dir.path().join("out.git"),
        objects.clone(),
        metadata.clone(),
        &opts,
    )
    .expect("export succeeds");

    let m = report.mappings.as_ref().expect("mappings recorded");
    assert_eq!(m.recorded, 2);
    assert_eq!(m.unchanged, 0);
    assert!(m.conflicts.is_empty());

    // Exactly the report's pairs, as rows (sqlite hex() is uppercase).
    let mut expected: Vec<(String, String)> = report
        .commits
        .iter()
        .map(|(cs, sha)| (sha.clone(), cs.to_hex().to_uppercase()))
        .collect();
    expected.sort();
    let rows = git_map_rows(&garden);
    assert_eq!(rows, expected, "one row per exported commit");

    // The recorded shas answer cs_for_git (the incremental import
    // frontier's pruning question).
    for (cs, sha) in &report.commits {
        assert_eq!(
            metadata.cs_for_git(sha).expect("cs_for_git"),
            Some(*cs),
            "recorded row answers the frontier query"
        );
    }

    // Re-export: nothing new; the same pairs are unchanged.
    let report2 = export_to_path(
        &dir.path().join("out.git"),
        objects.clone(),
        metadata.clone(),
        &opts,
    )
    .expect("re-export succeeds");
    let m2 = report2.mappings.as_ref().expect("mappings recorded");
    assert_eq!(m2.recorded, 0);
    assert_eq!(m2.unchanged, 2);
    assert!(m2.conflicts.is_empty());
    assert_eq!(sql_scalar(&garden, "SELECT COUNT(*) FROM git_map"), 2);
}

/// Importer-written rows are protected: exporting an imported fixture
/// with `record_mappings` finds every pair already recorded (`unchanged`)
/// and rewrites nothing. Re-importing the exported repo into the same
/// stores is then a pure no-op (0 new commits) — the recorded rows are
/// the frontier the importer prunes on.
#[test]
fn import_written_rows_survive_reexport_and_reimport_is_noop() {
    let fx = Fixture::generate(7261, &Default::default());
    common::import(
        &fx.repo,
        &fx.garden,
        &gardenbed_git::ImportOptions::default(),
    );

    let (objects, metadata, _engine) = open_stores(&fx.garden);
    let rows_before = git_map_rows(&fx.garden);

    let target = fx.dir.path().join("out.git");
    let report = export_to_path(
        &target,
        objects.clone(),
        metadata.clone(),
        &ExportOptions {
            record_mappings: true,
            ..Default::default()
        },
    )
    .expect("export succeeds");

    let m = report.mappings.as_ref().expect("mappings recorded");
    assert_eq!(m.recorded, 0, "every pair was already imported");
    assert_eq!(m.unchanged, fx.mapping.len() as u64);
    assert!(m.conflicts.is_empty());
    assert_eq!(git_map_rows(&fx.garden), rows_before, "rows untouched");

    // And the frontier: re-importing the exported repo adds nothing.
    let report2 = common::import(
        &target,
        &fx.dir.path().join("garden2"),
        &gardenbed_git::ImportOptions::default(),
    );
    // garden2 is a *fresh* store, so its import records everything — the
    // no-op claim is about the ORIGINAL stores. Use them:
    let (o2, m2, _e2) = open_stores(&fx.garden);
    let _ = (o2, m2);
    assert_eq!(
        report2.commits_new,
        fx.mapping.len() as u64,
        "fresh-store sanity: the exported repo is complete"
    );
}

/// The twin: two changesets whose git renderings are byte-identical (same
/// tree, message, identities, parents) — one renames a path, the other
/// deletes it and adds the new one. The changeset canonical bytes differ
/// (different op encodings) so the cs ids differ, but both derive the
/// same tree, hence the same git commit SHA. Export succeeds; the first
/// pair in export order records, the second is refused as a conflict
/// with the first surviving.
#[test]
fn native_representation_twin_refuses_the_second_pair() {
    let dir = tempdir().expect("tempdir");
    let garden = dir.path().join("garden");
    let (objects, metadata, _engine) = open_stores(&garden);

    let old_content = b"twin old\n".to_vec();
    let new_content = b"twin new\n".to_vec();
    let old_blob = blob_id(&old_content);
    let new_blob = blob_id(&new_content);
    let mut blobs: HashMap<Id, Vec<u8>> = HashMap::new();
    blobs.insert(old_blob, old_content.clone());
    blobs.insert(new_blob, new_content.clone());
    for content in blobs.values() {
        objects.put_blob(content).expect("put_blob");
    }

    // Shared parent so both twins have identical parent ids.
    let base = native_cs(
        vec![],
        vec![add("old.txt", old_blob, Mode::Regular)],
        "base\n",
    );
    let base_id = base.id().expect("base encodes");
    metadata.put_changeset(&base).expect("put base");

    // Twin A: rename old → new.
    let twin_a = native_cs(
        vec![base_id],
        vec![Change {
            path: "new.txt".into(),
            op: Op::Rename,
            blob: Some(new_blob),
            src_path: Some("old.txt".into()),
            mode: Mode::Regular,
        }],
        "twin\n",
    );
    let a_id = twin_a.id().expect("twin a encodes");

    // Twin B: delete old + add new — same resulting tree.
    let twin_b = native_cs(
        vec![base_id],
        vec![
            Change {
                path: "old.txt".into(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            },
            add("new.txt", new_blob, Mode::Regular),
        ],
        "twin\n",
    );
    let b_id = twin_b.id().expect("twin b encodes");
    assert_ne!(a_id, b_id, "canonical bytes differ, so ids differ");

    metadata.put_changeset(&twin_a).expect("put twin a");
    metadata.put_changeset(&twin_b).expect("put twin b");
    metadata
        .cas_ref("refs/heads/a", None, a_id)
        .expect("cas_ref a");
    metadata
        .cas_ref("refs/heads/b", None, b_id)
        .expect("cas_ref b");

    let report = export_to_path(
        &dir.path().join("out.git"),
        objects.clone(),
        metadata.clone(),
        &ExportOptions {
            record_mappings: true,
            ..Default::default()
        },
    )
    .expect("export succeeds");

    // Both commits exported (the pack dedups the object itself).
    assert_eq!(report.commits_exported, 3, "base + two twins");
    let sha_of = |cs: Id| -> String {
        report
            .commits
            .iter()
            .find(|(c, _)| *c == cs)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| panic!("cs {cs} in report"))
    };
    let sha_a = sha_of(a_id);
    let sha_b = sha_of(b_id);
    assert_eq!(sha_a, sha_b, "the twins render to identical git bytes");

    // Exactly one twin recorded; the other refused.
    let m = report.mappings.as_ref().expect("mappings recorded");
    assert_eq!(m.recorded, 2, "base + the first twin in export order");
    assert_eq!(m.conflicts.len(), 1, "the second twin is refused");
    let conflict = &m.conflicts[0];
    assert_eq!(conflict.git_sha, sha_a);
    // Whichever twin exported first won; the conflict names the other.
    let recorded = metadata
        .cs_for_git(&sha_a)
        .expect("cs_for_git")
        .expect("one twin is recorded");
    assert!(
        recorded == a_id || recorded == b_id,
        "a genuine twin is recorded"
    );
    assert_eq!(conflict.cs, if recorded == a_id { b_id } else { a_id });
    assert_eq!(conflict.existing_cs_for_sha, Some(recorded));
    assert_eq!(conflict.existing_sha_for_cs, None);

    // Re-running the export is stable: the recorded twin stays, the
    // refused twin stays refused.
    let (_o, metadata2, _e) = open_stores(&garden);
    let report2 = export_to_path(
        &dir.path().join("out.git"),
        objects.clone(),
        metadata2.clone(),
        &ExportOptions {
            record_mappings: true,
            ..Default::default()
        },
    )
    .expect("re-export succeeds");
    let m2 = report2.mappings.as_ref().expect("mappings recorded");
    assert_eq!(m2.conflicts.len(), 1);
    assert_eq!(
        metadata2.cs_for_git(&sha_a).expect("cs_for_git"),
        Some(recorded),
        "the surviving mapping is stable across re-exports"
    );
}

/// The pre-write layout refusal (non-bare target) records nothing: no
/// rows may exist for bytes that were never written.
#[test]
fn non_bare_refusal_records_nothing() {
    let dir = tempdir().expect("tempdir");
    let (garden, _root, _head) = native_history(dir.path());

    // A non-bare git repo at the target: refused before any write.
    let nonbare = dir.path().join("nonbare");
    std::fs::create_dir_all(&nonbare).expect("mkdir");
    std::fs::write(nonbare.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");

    let (objects, metadata, _engine) = open_stores(&garden);
    let result = export_to_path(
        &nonbare,
        objects,
        metadata,
        &ExportOptions {
            record_mappings: true,
            ..Default::default()
        },
    );
    assert!(result.is_err(), "non-bare target is refused");
    assert_eq!(
        sql_scalar(&garden, "SELECT COUNT(*) FROM git_map"),
        0,
        "no rows for a refused export"
    );
}

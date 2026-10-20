//! Imports grant durable validation only after replay against certified parents.
mod common;
use common::{Fixture, import, open_stores};
use gardenbed_git::import_from_path;
use gardenbed_harness::GenOptions;
use gardenbed_model::{Change, Changeset, Identity, Mode, Op};
use gardenbed_store::MetadataStore;

#[test]
fn imported_histories_are_certified_atomically_and_survive_restart() {
    let fixture = Fixture::generate(
        8741,
        &GenOptions {
            commits: 80,
            merge_ratio: 0.2,
            octopus_ratio: 0.05,
            ..Default::default()
        },
    );
    let report = import(&fixture.repo, &fixture.garden, &Default::default());
    let (_, metadata, engine) = open_stores(&fixture.garden);
    let count = common::sql_scalar(
        &fixture.garden,
        "SELECT COUNT(*) FROM validated_changesets_v1",
    );
    assert_eq!(count, report.commits_new);
    for (_, sha) in &fixture.mapping {
        let id = metadata.cs_for_git(sha).unwrap().unwrap();
        assert_eq!(
            metadata.validation_tree(id).unwrap(),
            Some(engine.root_tree(id).unwrap())
        );
    }
    drop((metadata, engine));
    // Restarted incremental import skips the graph at mapped tips, while the
    // already-certified frontier preserves validity without traversing history.
    let again = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(again.commits_new, 0);
    assert_eq!(
        common::sql_scalar(
            &fixture.garden,
            "SELECT COUNT(*) FROM validated_changesets_v1"
        ),
        count
    );
}

#[test]
fn mapped_legacy_nonfirst_parent_and_forged_tree_cache_do_not_grant_certification() {
    let fixture = Fixture::generate(
        64,
        &GenOptions {
            commits: 1,
            ..Default::default()
        },
    );
    let (objects, metadata, engine) = open_stores(&fixture.garden);
    let who = Identity {
        name: "test".into(),
        email: "test@example.com".into(),
        time_secs: 1,
        tz_min: 0,
    };
    let cs = |parents, changes, message: &str| Changeset {
        parents,
        changes,
        message: message.into(),
        author: who.clone(),
        committer: who.clone(),
        signature: None,
    };
    let root = cs(vec![], vec![], "root");
    let root_id = metadata
        .put_validated_changeset(
            &root,
            engine.validated_tree_for_new(&root).unwrap(),
            &[],
            None,
        )
        .unwrap();
    let invalid = cs(
        vec![root_id],
        vec![Change {
            path: "missing".into(),
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        }],
        "bad parent",
    );
    let invalid_id = metadata.put_changeset(&invalid).unwrap();
    metadata.derived_put("tree", invalid_id, &[]).unwrap();
    let merge = cs(vec![root_id, invalid_id], vec![], "mapped merge");
    let merge_id = metadata.put_changeset(&merge).unwrap();
    metadata.derived_put("tree", merge_id, &[]).unwrap();
    let sha = &fixture.mapping[0].1;
    metadata.record_git_mapping(sha, merge_id).unwrap();
    assert!(
        import_from_path(
            &fixture.repo,
            objects,
            metadata.clone(),
            engine,
            &Default::default()
        )
        .is_err()
    );
    assert_eq!(metadata.validation_tree(invalid_id).unwrap(), None);
    assert_eq!(metadata.validation_tree(merge_id).unwrap(), None);
    assert!(metadata.list_refs().unwrap().is_empty());
}

#[test]
fn unchanged_legacy_reimport_migrates_valid_history_once() {
    let fixture = Fixture::generate(
        811,
        &GenOptions {
            commits: 30,
            merge_ratio: 0.2,
            ..Default::default()
        },
    );
    let initial = import(&fixture.repo, &fixture.garden, &Default::default());
    rusqlite::Connection::open(fixture.garden.join("metadata.sqlite3"))
        .unwrap()
        .execute("DELETE FROM validated_changesets_v1", [])
        .unwrap();
    let again = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(again.commits_new, 0);
    assert_eq!(
        common::sql_scalar(
            &fixture.garden,
            "SELECT COUNT(*) FROM validated_changesets_v1"
        ),
        initial.commits_new
    );
}

#[test]
fn legacy_frontier_repair_counts_recreated_tree_objects() {
    use gardenbed_store::ObjectStore;
    let fixture = Fixture::generate(
        901,
        &GenOptions {
            commits: 12,
            merge_ratio: 0.0,
            octopus_ratio: 0.0,
            ..Default::default()
        },
    );
    import(&fixture.repo, &fixture.garden, &Default::default());
    let (objects, metadata, engine) = open_stores(&fixture.garden);
    let mut roots = std::collections::BTreeSet::new();
    for (_, sha) in &fixture.mapping {
        let id = metadata.cs_for_git(sha).unwrap().unwrap();
        if let Some(root) = engine.root_tree(id).unwrap() {
            roots.insert(root.to_hex());
        }
    }
    assert!(!roots.is_empty());
    for hex in &roots {
        std::fs::remove_file(objects.objects_dir().join(&hex[..2]).join(&hex[2..])).unwrap();
    }
    rusqlite::Connection::open(fixture.garden.join("metadata.sqlite3"))
        .unwrap()
        .execute("DELETE FROM validated_changesets_v1", [])
        .unwrap();
    drop((objects, metadata, engine));
    let repaired = import(&fixture.repo, &fixture.garden, &Default::default());
    assert_eq!(repaired.commits_new, 0);
    assert_eq!(repaired.objects_written, roots.len() as u64);
    let (objects, _, _) = open_stores(&fixture.garden);
    for hex in roots {
        assert!(objects.has(hex.parse().unwrap()).unwrap());
    }
}

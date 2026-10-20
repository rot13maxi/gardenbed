//! Certificate storage invariants independent of the tree validator.
use gardenbed_model::{Changeset, Id, Identity};
use gardenbed_store::{DerivedRow, MemoryStore, MetadataStore, SqliteMetadataStore};

fn cs(parents: Vec<Id>, message: &str) -> Changeset {
    let who = Identity {
        name: "test".into(),
        email: "test@example.com".into(),
        time_secs: 1,
        tz_min: 0,
    };
    Changeset {
        parents,
        changes: vec![],
        message: message.into(),
        author: who.clone(),
        committer: who,
        signature: None,
    }
}

fn certificate_contract(store: &dyn MetadataStore) {
    let root = cs(vec![], "root");
    let id = store.put_changeset(&root).unwrap();
    store.derived_put("tree", id, &[]).unwrap();
    assert_eq!(
        store.validation_tree(id).unwrap(),
        None,
        "mutable derived data grants no proof"
    );
    let child = cs(vec![id], "child");
    let child_id = child.id().unwrap();
    assert!(
        store
            .put_validated_changeset(&child, None, &[], None, None, None)
            .is_err()
    );
    assert!(store.get_changeset(child_id).unwrap().is_none());
    store
        .put_validated_changeset(&root, None, &[], None, None, None)
        .unwrap();
    assert_eq!(store.validation_tree(id).unwrap(), Some(None));
    // Certificate root is immutable even when the canonical bytes match.
    assert!(
        store
            .put_validated_changeset(&root, Some(Id::from_bytes([3; 32])), &[], None, None, None)
            .is_err()
    );
    assert_eq!(store.validation_tree(id).unwrap(), Some(None));

    let git = "1234567890123456789012345678901234567890";
    let bad_rows = [DerivedRow {
        kind: String::new(),
        cs: child_id,
        bytes: vec![],
    }];
    assert!(
        store
            .put_validated_changeset(&child, None, &bad_rows, Some(git), None, None)
            .is_err()
    );
    assert!(store.get_changeset(child_id).unwrap().is_none());
    assert_eq!(store.cs_for_git(git).unwrap(), None);
    assert_eq!(store.validation_tree(child_id).unwrap(), None);
    store
        .put_validated_changeset(&child, None, &[], Some(git), None, None)
        .unwrap();
    assert_eq!(store.validation_tree(child_id).unwrap(), Some(None));
    assert_eq!(store.cs_for_git(git).unwrap(), Some(child_id));

    // Uncertified second parents cannot hide behind a certified first parent.
    let legacy = cs(vec![], "uncertified");
    let legacy_id = store.put_changeset(&legacy).unwrap();
    let merge = cs(vec![id, legacy_id], "merge");
    assert!(
        store
            .put_validated_changeset(&merge, None, &[], None, None, None)
            .is_err()
    );
    assert!(store.get_changeset(merge.id().unwrap()).unwrap().is_none());
    // Ordinary Git ingestion must not be mistaken for certified ingestion.
    let generic = cs(vec![], "generic ingest");
    let generic_id = store
        .ingest_changeset(
            &generic.canonical_bytes().unwrap(),
            "2222222222222222222222222222222222222222",
            &[],
        )
        .unwrap();
    assert_eq!(store.validation_tree(generic_id).unwrap(), None);
}

#[test]
fn memory_certificate_contract() {
    certificate_contract(&MemoryStore::new());
}

#[test]
fn sqlite_certificate_contract_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.sqlite3");
    let root = cs(vec![], "root").id().unwrap();
    {
        let store = SqliteMetadataStore::open(&path).unwrap();
        certificate_contract(&store);
    }
    let reopened = SqliteMetadataStore::open(&path).unwrap();
    assert_eq!(reopened.validation_tree(root).unwrap(), Some(None));
}

#[test]
fn sqlite_certificate_failure_rolls_back_the_entire_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.sqlite3");
    let store = SqliteMetadataStore::open(&path).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER abort_validation BEFORE INSERT ON validated_changesets_v1 BEGIN SELECT RAISE(ABORT, 'injected certificate failure'); END;").unwrap();
    let root = cs(vec![], "atomic ingest");
    let id = root.id().unwrap();
    let git = "3333333333333333333333333333333333333333";
    let rows = [DerivedRow {
        kind: "tree".into(),
        cs: id,
        bytes: vec![],
    }];
    assert!(
        store
            .put_validated_changeset(&root, None, &rows, Some(git), None, None)
            .is_err()
    );
    assert!(store.get_changeset(id).unwrap().is_none());
    assert_eq!(store.validation_tree(id).unwrap(), None);
    assert_eq!(store.derived_get("tree", id).unwrap(), None);
    assert_eq!(store.cs_for_git(git).unwrap(), None);
    connection
        .execute_batch("DROP TRIGGER abort_validation")
        .unwrap();
    store
        .put_validated_changeset(&root, None, &rows, Some(git), None, None)
        .unwrap();
    assert_eq!(store.validation_tree(id).unwrap(), Some(None));
}

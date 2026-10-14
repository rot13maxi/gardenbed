//! M1 verification: seedbed surface beyond CAS — changeset storage with
//! canonical bytes, the git-SHA ⇄ changeset bijection, ref listing, and the
//! derived-data cache — against both metadata backends.

mod common;

use common::{id_of, sample_changeset};
use gardenbed_model::Id;
use gardenbed_store::{MemoryStore, MetadataStore, RefCas, SqliteMetadataStore, StoreError};
use std::str::FromStr;

const GIT_SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const GIT_SHA_B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"; // uppercase on purpose

fn changeset_checks(store: &dyn MetadataStore) {
    let cs = sample_changeset(0x5EED, 3);
    let id = store.put_changeset(&cs).expect("put_changeset");
    assert_eq!(id, cs.id().expect("model id"), "id is SHA-256(canonical)");

    let (decoded, canonical) = store.get_changeset(id).expect("get").expect("present");
    assert_eq!(decoded, cs, "decoded row equals the stored changeset");
    assert_eq!(canonical, cs.canonical_bytes().unwrap(), "canonical bytes exact");
    assert_eq!(Id::from_str(&id.to_hex()).unwrap(), id);

    // Idempotent re-put.
    assert_eq!(store.put_changeset(&cs).expect("re-put"), id);
    // Absent id reads as None.
    assert!(store.get_changeset(id_of(0xDEAD)).unwrap().is_none());

    // An invalid changeset is rejected, not stored.
    let mut bad = cs.clone();
    bad.changes[0].blob = None; // Add without a blob is semantically invalid
    assert!(matches!(
        store.put_changeset(&bad),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn changeset_roundtrip_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("s.sqlite3");
    let store = SqliteMetadataStore::open(&db).expect("open");
    changeset_checks(&store);

    // Corrupt the stored canonical bytes directly; get_changeset must fail
    // loudly (Corrupt) instead of returning garbage.
    let cs = sample_changeset(0x1234, 1);
    let id = store.put_changeset(&cs).expect("put");
    let conn = rusqlite::Connection::open(&db).expect("raw conn");
    conn.execute(
        "UPDATE changesets SET canonical = x'0001020304' WHERE id = ?1",
        [id.as_bytes()],
    )
    .expect("tamper");
    assert!(matches!(
        store.get_changeset(id),
        Err(StoreError::Corrupt(_))
    ));
}

#[test]
fn changeset_roundtrip_memory() {
    let store = MemoryStore::new();
    changeset_checks(&store);
}

fn git_mapping_checks(store: &dyn MetadataStore) {
    let cs1 = id_of(0x601);
    let cs2 = id_of(0x602);

    // Fresh mapping in both directions, case-normalized.
    store.record_git_mapping(GIT_SHA_A, cs1).expect("record");
    assert_eq!(store.cs_for_git(GIT_SHA_A).unwrap(), Some(cs1));
    assert_eq!(store.cs_for_git(&GIT_SHA_A.to_uppercase()).unwrap(), Some(cs1));
    assert_eq!(store.git_for_cs(cs1).unwrap(), Some(GIT_SHA_A.to_owned()));

    // Bijection, enforced both directions.
    assert!(matches!(
        store.record_git_mapping(GIT_SHA_A, cs2),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.record_git_mapping(GIT_SHA_B, cs1),
        Err(StoreError::Invalid(_))
    ));

    // Recording the exact same pair again is idempotent.
    store.record_git_mapping(GIT_SHA_A, cs1).expect("idempotent");
    store
        .record_git_mapping(&GIT_SHA_A.to_uppercase(), cs1)
        .expect("idempotent with different case");

    // A second independent pair coexists.
    store.record_git_mapping(GIT_SHA_B, cs2).expect("record");
    assert_eq!(store.cs_for_git(GIT_SHA_B).unwrap(), Some(cs2));
    assert_eq!(
        store.git_for_cs(cs2).unwrap(),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned())
    );

    // Absent lookups are None; malformed SHAs are Invalid.
    assert_eq!(
        store.cs_for_git("0000000000000000000000000000000000000000").unwrap(),
        None
    );
    assert_eq!(store.git_for_cs(id_of(0xFFFF)).unwrap(), None);
    assert!(matches!(store.cs_for_git("short"), Err(StoreError::Invalid(_))));
}

#[test]
fn git_mapping_bijection_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteMetadataStore::open(&dir.path().join("s.sqlite3")).expect("open");
    git_mapping_checks(&store);
}

#[test]
fn git_mapping_bijection_memory() {
    let store = MemoryStore::new();
    git_mapping_checks(&store);
}

fn ref_listing_checks(store: &dyn MetadataStore) {
    let expected: [(&str, u64); 3] = [("main", 1), ("feature/a", 2), ("zeta", 3)];
    for (name, n) in expected {
        store.cas_ref(name, None, id_of(n)).expect("create ref");
    }
    let listed = store.list_refs().expect("list");
    let expected_sorted: Vec<(String, Id)> = {
        let mut v: Vec<(String, Id)> = expected
            .iter()
            .map(|(name, n)| (name.to_string(), id_of(*n)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    assert_eq!(listed, expected_sorted, "all refs, sorted by name");
    assert_eq!(store.resolve_ref("main").unwrap(), Some(id_of(1)));
    assert_eq!(store.resolve_ref("missing").unwrap(), None);
}

#[test]
fn ref_listing_sorted_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteMetadataStore::open(&dir.path().join("s.sqlite3")).expect("open");
    ref_listing_checks(&store);
}

#[test]
fn ref_listing_sorted_memory() {
    let store = MemoryStore::new();
    ref_listing_checks(&store);
}

fn derived_checks(store: &dyn MetadataStore) {
    let cs = id_of(0xD1);
    let payload = b"ancestry skip list bytes for this changeset";
    assert_eq!(store.derived_get("skiplist", cs).unwrap(), None);
    store.derived_put("skiplist", cs, payload).expect("put");
    assert_eq!(
        store.derived_get("skiplist", cs).unwrap().as_deref(),
        Some(payload.as_slice())
    );
    // Overwrite semantics.
    store.derived_put("skiplist", cs, b"v2").expect("put");
    assert_eq!(
        store.derived_get("skiplist", cs).unwrap().as_deref(),
        Some(b"v2".as_slice())
    );
    // Kinds are isolated.
    assert_eq!(store.derived_get("blame", cs).unwrap(), None);
    store.derived_put("blame", cs, b"blame data").expect("put");
    assert_eq!(
        store.derived_get("skiplist", cs).unwrap().as_deref(),
        Some(b"v2".as_slice())
    );
    // Changeset ids are isolated.
    assert_eq!(store.derived_get("skiplist", id_of(0xD2)).unwrap(), None);
    // Empty kind is rejected.
    assert!(matches!(
        store.derived_put("", cs, b"x"),
        Err(StoreError::Invalid(_))
    ));
}

#[test]
fn derived_cache_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteMetadataStore::open(&dir.path().join("s.sqlite3")).expect("open");
    derived_checks(&store);
}

#[test]
fn derived_cache_memory() {
    let store = MemoryStore::new();
    derived_checks(&store);
}

#[test]
fn sqlite_opens_in_wal_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("s.sqlite3");
    let _store = SqliteMetadataStore::open(&db).expect("open");
    // journal_mode=WAL is persisted in the database file, so a fresh
    // connection observes it.
    let conn = rusqlite::Connection::open(&db).expect("raw conn");
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode.to_ascii_lowercase(), "wal", "store must run in WAL mode");
}

#[test]
fn changesets_and_refs_persist_across_reopen_sqlite() {    // Reopen the same DB: everything persists (WAL-committed data).
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("s.sqlite3");
    let cs = sample_changeset(0x777, 2);

    let cs_id = {
        let store = SqliteMetadataStore::open(&db).expect("open");
        let id = store.put_changeset(&cs).expect("put");
        assert_eq!(
            store.cas_ref("main", None, id).unwrap(),
            RefCas::Won { current: id }
        );
        id
    };
    {
        let store = SqliteMetadataStore::open(&db).expect("reopen");
        let (decoded, canonical) = store.get_changeset(cs_id).unwrap().expect("survived");
        assert_eq!(decoded, cs);
        assert_eq!(canonical, cs.canonical_bytes().unwrap());
        assert_eq!(store.resolve_ref("main").unwrap(), Some(cs_id));
        assert_eq!(store.list_refs().unwrap().len(), 1);
    }
}

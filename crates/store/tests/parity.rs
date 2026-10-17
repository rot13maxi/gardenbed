//! Backend parity: the same backend-independent assertions as
//! `tests/metadata.rs`, `tests/cas_race.rs`, `tests/validation.rs`, and
//! the object-store assertions of `tests/roundtrip.rs` /
//! `tests/immutability.rs`, run against the scale-out backends
//! (Postgres metadata + S3 objects) instead of SQLite + fs.
//!
//! Gating: these tests need live services and are skipped (with a
//! printed note) unless BOTH of the following are set:
//!
//! ```text
//! GARDENBED_TEST_PG_URL=postgres://gardenbed:gardenbed@localhost:55432/gardenbed
//! GARDENBED_TEST_S3_ENDPOINT=http://localhost:55900
//! GARDENBED_TEST_S3_BUCKET=gardenbed-test-<pid>     # unique per run
//! GARDENBED_TEST_S3_ACCESS_KEY=gardenbed
//! GARDENBED_TEST_S3_SECRET_KEY=gardenbed-secret
//! GARDENBED_TEST_S3_REGION=us-east-1
//! GARDENBED_TEST_S3_PATH_STYLE=true
//! ```
//!
//! The Postgres side uses the given database directly: tables are
//! `CREATE IF NOT EXISTS` and every test uses ref names / changesets
//! unique to this run (a per-run tag derived from the PID), so parallel
//! runs against one database coexist. The bucket must be unique per run
//! (the simplest sound option: the S3 backend enforces immutability
//! against *content-addressed keys*, so reusing a bucket across runs
//! would make blob puts collide with identical-but-tampered fixtures);
//! create it with `mc mb` or the compose setup, and delete it after.
//!
//! The docker compose file `tools/scale/compose.yaml` (slice B) brings
//! up Postgres on 55432 and MinIO on 55900 with these credentials.

// The parity suite exercises the `scale` backends; it is meaningless in
// a --no-default-features build and does not compile there.
#![cfg(feature = "scale")]

mod common;

use common::{id_of, sample_changeset, sample_tree};
use gardenbed_model::Id;
use gardenbed_model::TreeEntry;
use gardenbed_model::TreeKind;
use gardenbed_model::blob_id;
use gardenbed_store::{
    DerivedRow, MetadataStore, ObjectStore, PostgresMetadataStore, RefCas, S3ObjectStore,
    StoreError,
};
use sha2::Digest;
use sha2::Sha256;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

/// Live-service wiring, or `None` (skip) when env vars are unset.
fn live() -> Option<(PostgresMetadataStore, S3ObjectStore, String)> {
    let pg_url = std::env::var("GARDENBED_TEST_PG_URL").ok()?;
    let endpoint = std::env::var("GARDENBED_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("GARDENBED_TEST_S3_BUCKET")
        .ok()
        .or(std::env::var("GARDENBED_TEST_BUCKET").ok())?;
    let access_key = std::env::var("GARDENBED_TEST_S3_ACCESS_KEY").ok()?;
    let secret_key = std::env::var("GARDENBED_TEST_S3_SECRET_KEY").ok()?;
    let region = std::env::var("GARDENBED_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let path_style = std::env::var("GARDENBED_TEST_S3_PATH_STYLE")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    let pg = PostgresMetadataStore::open(&pg_url).expect("connect to GARDENBED_TEST_PG_URL");
    let s3 = S3ObjectStore::open(
        &endpoint,
        &bucket,
        &region,
        &access_key,
        &secret_key,
        path_style,
    )
    .expect("connect to GARDENBED_TEST_S3_ENDPOINT");
    // Per-run tag: unique ref names and changesets so parallel runs
    // against one shared database/bucket coexist.
    let tag = format!("p{}", std::process::id());
    Some((pg, s3, tag))
}

// ---------------------------------------------------------------------------
// metadata.rs parity
// ---------------------------------------------------------------------------

fn changeset_checks(store: &dyn MetadataStore, tag: &str) {
    let cs = sample_changeset(0x5EED ^ hash_tag(tag), 3);
    let id = store.put_changeset(&cs).expect("put_changeset");
    assert_eq!(id, cs.id().expect("model id"), "id is SHA-256(canonical)");

    let (decoded, canonical) = store.get_changeset(id).expect("get").expect("present");
    assert_eq!(decoded, cs, "decoded row equals the stored changeset");
    assert_eq!(
        canonical,
        cs.canonical_bytes().unwrap(),
        "canonical bytes exact"
    );

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

fn git_mapping_checks(store: &dyn MetadataStore, tag: &str) {
    let cs1 = id_of(0x601 ^ hash_tag(tag));
    let cs2 = id_of(0x602 ^ hash_tag(tag));
    let sha_a = format!("{tag}aaaaaaaaaaaaaaaaaaaaaaaaaa"); // not 40 hex → Invalid
    assert!(matches!(
        store.cs_for_git(&sha_a),
        Err(StoreError::Invalid(_))
    ));

    // Valid, unique-per-run 40-hex SHAs.
    // 40 hex chars unique to this process: a pid+timestamp-derived salt
    // hashed into the bytes, so no prior run recorded this pair.
    let salt = run_salt();
    let sha_a = {
        let mut v = [0u8; 20];
        for (i, b) in v.iter_mut().enumerate() {
            *b = tag_bytes(tag)
                .wrapping_add(salt.to_le_bytes()[i % 8])
                .wrapping_add(i as u8);
        }
        hex(&v)
    };
    let sha_b = {
        let mut v = [0u8; 20];
        for (i, b) in v.iter_mut().enumerate() {
            *b = tag_bytes(tag)
                .wrapping_add(0x7F)
                .wrapping_add(salt.to_le_bytes()[i % 8])
                .wrapping_add(i as u8);
        }
        hex(&v).to_uppercase() // uppercase on purpose
    };

    store.record_git_mapping(&sha_a, cs1).expect("record");
    assert_eq!(store.cs_for_git(&sha_a).unwrap(), Some(cs1));
    assert_eq!(
        store.cs_for_git(&sha_a.to_uppercase()).unwrap(),
        Some(cs1),
        "case-normalized lookup"
    );
    assert_eq!(store.git_for_cs(cs1).unwrap(), Some(sha_a.to_lowercase()));

    // Bijection, enforced both directions.
    assert!(matches!(
        store.record_git_mapping(&sha_a, cs2),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        store.record_git_mapping(&sha_b, cs1),
        Err(StoreError::Invalid(_))
    ));

    // Recording the exact same pair again is idempotent.
    store.record_git_mapping(&sha_a, cs1).expect("idempotent");
    store
        .record_git_mapping(&sha_a.to_uppercase(), cs1)
        .expect("idempotent with different case");

    // A second independent pair coexists.
    store.record_git_mapping(&sha_b, cs2).expect("record");
    assert_eq!(store.cs_for_git(&sha_b).unwrap(), Some(cs2));
    assert_eq!(store.git_for_cs(cs2).unwrap(), Some(sha_b.to_lowercase()));

    // Absent lookups are None.
    assert_eq!(store.cs_for_git(&hex(&[9; 20])).unwrap(), None);
    assert_eq!(
        store.git_for_cs(id_of(0xFFFF ^ hash_tag(tag))).unwrap(),
        None
    );
}

fn ref_listing_checks(store: &dyn MetadataStore, tag: &str) {
    let expected: [(&str, u64); 3] = [
        (&format!("{tag}/main"), 1),
        (&format!("{tag}/feature/a"), 2),
        (&format!("{tag}/zeta"), 3),
    ];
    for (name, n) in &expected {
        store
            .cas_ref(name, None, id_of(*n ^ hash_tag(tag)))
            .expect("create ref");
    }
    let listed = store.list_refs().expect("list");
    let mine: Vec<(String, Id)> = listed
        .into_iter()
        // The CAS-race tests also create `<tag>/race-*` refs; exclude them
        // (they may run in any order relative to this test).
        .filter(|(name, _)| {
            name.starts_with(&format!("{tag}/"))
                && !name.contains("/race-")
                && !name.ends_with("/seq")
        })
        .collect();
    let expected_sorted: Vec<(String, Id)> = {
        let mut v: Vec<(String, Id)> = expected
            .iter()
            .map(|(name, n)| ((*name).to_string(), id_of(*n ^ hash_tag(tag))))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    };
    assert_eq!(mine, expected_sorted, "all this run's refs, sorted by name");
    assert_eq!(
        store.resolve_ref(&format!("{tag}/main")).unwrap(),
        Some(id_of(1 ^ hash_tag(tag)))
    );
    assert_eq!(store.resolve_ref(&format!("{tag}/missing")).unwrap(), None);
}

fn derived_checks(store: &dyn MetadataStore, tag: &str) {
    let cs = id_of(0xD1 ^ hash_tag(tag));
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
        Some(b"v2".to_vec().as_slice())
    );
    // Kinds are isolated.
    assert_eq!(store.derived_get("blame", cs).unwrap(), None);
    store.derived_put("blame", cs, b"blame data").expect("put");
    assert_eq!(
        store.derived_get("skiplist", cs).unwrap().as_deref(),
        Some(b"v2".to_vec().as_slice())
    );
    // Changeset ids are isolated.
    assert_eq!(store.derived_get("skiplist", id_of(0xD2)).unwrap(), None);
    // Empty kind is rejected.
    assert!(matches!(
        store.derived_put("", cs, b"x"),
        Err(StoreError::Invalid(_))
    ));
}

// ---------------------------------------------------------------------------
// cas_race.rs parity (sequential semantics + the 32-thread race)
// ---------------------------------------------------------------------------

const THREADS: usize = 32;

fn race_from_base(store: Arc<dyn MetadataStore>, base: Option<Id>, name: &str) {
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let name = name.to_owned();
            thread::spawn(move || {
                barrier.wait();
                store.cas_ref(&name, base, id_of(0xC45 + i as u64))
            })
        })
        .collect();

    let mut wins = Vec::new();
    let mut conflicts = Vec::new();
    for h in handles {
        match h.join().expect("thread did not panic") {
            Ok(RefCas::Won { current }) => wins.push(current),
            Ok(RefCas::Conflict { current }) => conflicts.push(current),
            Err(e) => panic!("cas_ref failed: {e:?}"),
        }
    }

    assert_eq!(wins.len(), 1, "exactly one winner");
    assert_eq!(
        conflicts.len(),
        THREADS - 1,
        "every loser gets a clean conflict"
    );
    let winner_new = wins[0];
    assert!(
        conflicts.iter().all(|c| *c == winner_new),
        "all conflicts report the winner's id"
    );
    let final_ref = store
        .resolve_ref(name)
        .expect("resolve")
        .expect("ref exists");
    assert_eq!(final_ref, winner_new);
}

fn cas_sequential_semantics(store: &dyn MetadataStore, tag: &str) {
    let a = id_of(1 ^ hash_tag(tag));
    let b = id_of(2 ^ hash_tag(tag));
    let c = id_of(3 ^ hash_tag(tag));
    let main = format!("{tag}/seq");

    assert_eq!(
        store.cas_ref(&main, None, a).unwrap(),
        RefCas::Won { current: a }
    );
    assert_eq!(
        store.cas_ref(&main, None, b).unwrap(),
        RefCas::Conflict { current: a }
    );
    assert_eq!(
        store.cas_ref(&main, Some(c), b).unwrap(),
        RefCas::Conflict { current: a }
    );
    assert_eq!(
        store.cas_ref(&main, Some(a), b).unwrap(),
        RefCas::Won { current: b }
    );
    assert!(matches!(
        store.cas_ref(&format!("{tag}/ghost"), Some(a), c),
        Err(StoreError::NotFound)
    ));
    assert_eq!(store.resolve_ref(&main).unwrap(), Some(b));
}

// ---------------------------------------------------------------------------
// validation.rs parity (certificate contract)
// ---------------------------------------------------------------------------

fn cs(parents: Vec<Id>, message: &str) -> gardenbed_model::Changeset {
    use gardenbed_model::{Changeset, Identity};
    let who = Identity {
        name: "parity".into(),
        email: "parity@example.com".into(),
        // Per-run salt so the resulting changeset ids — and their
        // certificates — never collide with a previous run's rows in the
        // shared test database.
        time_secs: run_salt(),
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
            .put_validated_changeset(&child, None, &[], None, None)
            .is_err()
    );
    assert!(store.get_changeset(child_id).unwrap().is_none());
    store
        .put_validated_changeset(&root, None, &[], None, None)
        .unwrap();
    assert_eq!(store.validation_tree(id).unwrap(), Some(None));
    // Certificate root is immutable even when the canonical bytes match.
    assert!(
        store
            .put_validated_changeset(&root, Some(Id::from_bytes([3; 32])), &[], None, None)
            .is_err()
    );
    assert_eq!(store.validation_tree(id).unwrap(), Some(None));

    // Unique per run: a fixed git sha would collide with the mapping
    // recorded by a previous run against the shared test database.
    let git = &format!("{:040x}", run_salt() as u128);
    let bad_rows = [DerivedRow {
        kind: String::new(),
        cs: child_id,
        bytes: vec![],
    }];
    assert!(
        store
            .put_validated_changeset(&child, None, &bad_rows, Some(git), None)
            .is_err()
    );
    assert!(store.get_changeset(child_id).unwrap().is_none());
    assert_eq!(store.cs_for_git(git).unwrap(), None);
    assert_eq!(store.validation_tree(child_id).unwrap(), None);
    store
        .put_validated_changeset(&child, None, &[], Some(git), None)
        .unwrap();
    assert_eq!(store.validation_tree(child_id).unwrap(), Some(None));
    assert_eq!(store.cs_for_git(git).unwrap(), Some(child_id));

    // Uncertified second parents cannot hide behind a certified first parent.
    let legacy = cs(vec![], "uncertified");
    let legacy_id = store.put_changeset(&legacy).unwrap();
    let merge = cs(vec![id, legacy_id], "merge");
    assert!(
        store
            .put_validated_changeset(&merge, None, &[], None, None)
            .is_err()
    );
    assert!(store.get_changeset(merge.id().unwrap()).unwrap().is_none());
    // Ordinary Git ingestion must not be mistaken for certified ingestion.
    let generic = cs(vec![], "generic ingest");
    let generic_id = store
        .ingest_changeset(
            &generic.canonical_bytes().unwrap(),
            &format!("{:040x}", (run_salt() ^ 0xF00D) as u128),
            &[],
        )
        .unwrap();
    assert_eq!(store.validation_tree(generic_id).unwrap(), None);
}

// ---------------------------------------------------------------------------
// roundtrip.rs / immutability.rs object-store parity
// ---------------------------------------------------------------------------

fn assert_roundtrip(store: &dyn ObjectStore, content: &[u8]) {
    let id = store.put_blob(content).expect("put_blob");
    assert_eq!(id, blob_id(content), "id is SHA-256(content)");
    let got = store.get_blob(id).expect("get_blob");
    assert_eq!(got.as_deref(), Some(content), "byte-exact roundtrip");
    assert!(store.has(id).expect("has"), "has() true after put");
    assert_eq!(store.put_blob(content).expect("re-put"), id);
}

/// A tree unique to this run — `sample_tree()` is deterministic, so its
/// key would collide with tampered leftovers from a previous run against
/// the same shared bucket (trees never heal, by design).
fn run_tree() -> (gardenbed_model::Tree, Id, Vec<u8>) {
    let (mut tree, _, _) = sample_tree();
    let TreeEntry { blob, .. } = tree
        .entries
        .iter_mut()
        .find(|e| e.kind == TreeKind::File)
        .expect("sample tree has a file entry");
    *blob.as_mut().expect("file entry has a blob") =
        gardenbed_model::blob_id(&run_salt().to_le_bytes());
    let bytes = tree.canonical_bytes().expect("tree encodes");
    let id = Id::from_bytes(Sha256::digest(&bytes).into());
    (tree, id, bytes)
}

fn tree_roundtrip(store: &dyn ObjectStore) {
    let (tree, tree_id, canonical) = run_tree();
    store.put_tree(tree_id, &canonical).expect("put_tree");
    let got = store.get_tree(tree_id).expect("get_tree");
    assert_eq!(got.as_deref(), Some(canonical.as_slice()));
    assert_eq!(gardenbed_model::Tree::decode(&got.unwrap()).unwrap(), tree);
    assert!(store.has(tree_id).unwrap());
    // Re-put of identical bytes is idempotent.
    store.put_tree(tree_id, &canonical).expect("re-put");
}

fn tree_hash_mismatch_rejected(store: &dyn ObjectStore) {
    let (_tree, tree_id, _canonical) = run_tree();
    let wrong = format!("not the canonical bytes of {tree_id}").into_bytes();
    assert!(matches!(
        store.put_tree(tree_id, &wrong),
        Err(StoreError::Invalid(_))
    ));
    // has() may be true when a tree object legitimately lives at that
    // id from an earlier assertion in this run's bucket; the check that
    // matters is that the *wrong* bytes were rejected above.
}

fn absent_ids_read_as_none(store: &dyn ObjectStore) {
    let ghost = Id::from_bytes([0xDE; 32]);
    assert_eq!(store.get_blob(ghost).unwrap(), None);
    assert_eq!(store.get_tree(ghost).unwrap(), None);
    assert!(!store.has(ghost).unwrap());
}

fn type_confusion_is_corrupt(store: &dyn ObjectStore) {
    let blob_id = store.put_blob(b"some blob").expect("put");
    assert!(matches!(
        store.get_tree(blob_id),
        Err(StoreError::Corrupt(_))
    ));
    let (_tree, tree_id, canonical) = run_tree();
    store.put_tree(tree_id, &canonical).expect("put_tree");
    assert!(matches!(
        store.get_blob(tree_id),
        Err(StoreError::Corrupt(_))
    ));
}

// ---------------------------------------------------------------------------
// Small tag helpers
// ---------------------------------------------------------------------------

/// Per-process salt (pid ^ nanos): keeps git-SHA fixtures unique across
/// repeated runs against one shared test database.
fn run_salt() -> u64 {
    (std::process::id() as u64)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0x5A17)
}

fn hash_tag(tag: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in tag.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn tag_bytes(tag: &str) -> u8 {
    tag.bytes().next().unwrap_or(1) | 1
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn pg_metadata_parity() {
    let Some((store, _s3, tag)) = live() else {
        eprintln!(
            "skipping parity tests: GARDENBED_TEST_PG_URL / \
             GARDENBED_TEST_S3_* are not set (see this file's header)"
        );
        return;
    };
    changeset_checks(&store, &tag);
    git_mapping_checks(&store, &tag);
    ref_listing_checks(&store, &tag);
    derived_checks(&store, &tag);
    cas_sequential_semantics(&store, &tag);
    certificate_contract(&store);
}

#[test]
fn pg_cas_race_from_existing_base() {
    let Some((store, _s3, tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    let store = Arc::new(store);
    let base = id_of(0xB4E ^ hash_tag(&tag));
    let name = format!("{tag}/race-existing");
    assert_eq!(
        store.cas_ref(&name, None, base).expect("seed ref"),
        RefCas::Won { current: base }
    );
    race_from_base(
        Arc::clone(&store) as Arc<dyn MetadataStore>,
        Some(base),
        &name,
    );
}

#[test]
fn pg_cas_race_from_absent_ref() {
    let Some((store, _s3, tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    let store = Arc::new(store);
    race_from_base(store, None, &format!("{tag}/race-absent"));
}

#[test]
fn pg_git_mapping_concurrent_idempotence() {
    let Some((store, _s3, tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    let store = Arc::new(store);
    // A fresh (sha, cs) pair per run: concurrent identical callers all
    // race the same INSERT. The cs need not exist as a changeset row
    // (git_map carries no FK), which keeps this a pure mapping test.
    let salt = run_salt();
    let sha = {
        let mut v = [0u8; 20];
        for (i, b) in v.iter_mut().enumerate() {
            *b = tag_bytes(&format!("{tag}race"))
                .wrapping_add(salt.to_le_bytes()[i % 8])
                .wrapping_add(i as u8);
        }
        hex(&v)
    };
    let cs = id_of(0x6C1 ^ hash_tag(&tag));
    let callers = 8;
    let barrier = Arc::new(Barrier::new(callers));
    let handles: Vec<_> = (0..callers)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let sha = sha.clone();
            thread::spawn(move || {
                barrier.wait();
                store.record_git_mapping(&sha, cs)
            })
        })
        .collect();
    for h in handles {
        h.join().expect("caller thread").expect(
            "every concurrent identical record_git_mapping returns Ok — \
             the SQLite backend's idempotence contract (HIGH-1 regression)",
        );
    }
    assert_eq!(store.cs_for_git(&sha).unwrap(), Some(cs));
    assert_eq!(store.git_for_cs(cs).unwrap(), Some(sha));
}

#[test]
fn pg_ingest_concurrent_idempotence() {
    let Some((store, _s3, tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    let store = Arc::new(store);
    // A fresh (changeset, sha) pair per run: concurrent identical ingests
    // all race the same changeset + git_map INSERTs inside write_changeset.
    // validated_tree is None, so no parent-certification is involved —
    // this isolates the ingest path's own idempotence.
    let salt = run_salt();
    let changeset = sample_changeset(salt, 1);
    let canonical = changeset.canonical_bytes().unwrap();
    let cs = changeset.id().unwrap();
    let sha = {
        let mut v = [0u8; 20];
        for (i, b) in v.iter_mut().enumerate() {
            *b = tag_bytes(&format!("{tag}ingest"))
                .wrapping_add(salt.to_le_bytes()[i % 8])
                .wrapping_add(i as u8);
        }
        hex(&v)
    };
    let callers = 8;
    let barrier = Arc::new(Barrier::new(callers));
    let handles: Vec<_> = (0..callers)
        .map(|_| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let canonical = canonical.clone();
            let sha = sha.clone();
            thread::spawn(move || {
                barrier.wait();
                store.ingest_changeset(&canonical, &sha, &[])
            })
        })
        .collect();
    for h in handles {
        h.join().expect("caller thread").expect(
            "every concurrent identical ingest_changeset returns Ok — \
             the mapping block in write_changeset must claim via \
             INSERT .. ON CONFLICT, not SELECT-then-INSERT",
        );
    }
    assert_eq!(
        store.get_changeset(cs).unwrap().map(|c| c.0.id().unwrap()),
        Some(cs)
    );
    assert_eq!(store.cs_for_git(&sha).unwrap(), Some(cs));
    assert_eq!(store.git_for_cs(cs).unwrap(), Some(sha));
}
#[test]
fn s3_object_parity() {
    let Some((_pg, store, _tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    assert_roundtrip(&store, b""); // size 0
    assert_roundtrip(&store, b"x"); // size 1
    assert_roundtrip(&store, &common::compressible_bytes(0x1001, 1024));
    assert_roundtrip(&store, &common::compressible_bytes(0x1002, 1 << 20));
    assert_roundtrip(&store, &common::random_bytes(0x1003, 256 * 1024));
    tree_roundtrip(&store);
    tree_hash_mismatch_rejected(&store);
    absent_ids_read_as_none(&store);
    type_confusion_is_corrupt(&store);
}

#[test]
fn s3_put_same_id_with_different_bytes_is_violation() {
    let Some((_pg, store, _tag)) = live() else {
        eprintln!("skipping parity tests: live-service env vars are not set");
        return;
    };
    // Seed a conflicting object at the key the store would choose, via a
    // second raw object_store client over the same bucket (bypassing the
    // conditional put — the analogue of writing garbage straight to the
    // shard directory in the fs test). Two shapes:
    //
    // 1. a VALID envelope of different content at a blob's key:
    //    put_blob must refuse (ImmutabilityViolation), get_blob must
    //    refuse to hand back the wrong bytes (Corrupt — the envelope
    //    decodes but hashes to a different id), and the tampered object
    //    must NOT be overwritten.
    // 2. garbage (no valid envelope): put_blob heals it (fs.rs parity —
    //    the caller's content hashes to the id, so overwriting restores
    //    the invariant), and get_blob before the heal is Corrupt.
    let content = b"original content";
    let id = store.put_blob(content).expect("first put");

    let raw_client = raw_s3_client();
    let key = format!("{}/{}", &id.to_hex()[..2], &id.to_hex()[2..]);

    // --- 1. valid envelope, different content ----------------------
    let other = b"tampered but well-formed content";
    assert_ne!(gardenbed_model::blob_id(other), id);
    let tampered = envelope_bytes(other);
    put_raw(&raw_client, &key, tampered.clone());
    let refused = store.put_blob(content);
    assert!(
        matches!(refused, Err(StoreError::ImmutabilityViolation(_))),
        "putting the true content over a valid conflicting envelope is a violation, got {refused:?}"
    );
    assert!(
        matches!(store.get_blob(id), Err(StoreError::Corrupt(_))),
        "the tampered object never comes back as the addressed bytes"
    );
    // No overwrite: the raw bytes at the key are still the tampered ones.
    assert_eq!(get_raw(&raw_client, &key), tampered);

    // --- 2. structurally corrupt bytes: heal -----------------------
    put_raw(&raw_client, &key, b"not an envelope at all".to_vec());
    assert!(
        matches!(store.get_blob(id), Err(StoreError::Corrupt(_))),
        "garbage at the key reads as Corrupt before the heal"
    );
    assert_eq!(
        store.put_blob(content).expect("put over garbage heals"),
        id,
        "a structurally corrupt envelope is healed by the verified put"
    );
    assert_eq!(store.get_blob(id).unwrap().unwrap(), content);

    // --- tree shape: conflicting canonical bytes at a tree key ------
    let (_tree, tree_id, canonical) = run_tree();
    store.put_tree(tree_id, &canonical).expect("put_tree");
    let tree_key = format!("{}/{}", &tree_id.to_hex()[..2], &tree_id.to_hex()[2..]);
    // Trees are bare canonical bytes (no envelope); a conflicting object
    // at a tree key is a violation — trees never heal (`healable =
    // false`), because there is no envelope to judge structural
    // corruption by.
    put_raw(&raw_client, &tree_key, b"tree tamper".to_vec());
    assert!(
        matches!(
            store.put_tree(tree_id, &canonical),
            Err(StoreError::ImmutabilityViolation(_))
        ),
        "trees never heal: a conflicting object at a tree key is a violation"
    );
    assert!(matches!(
        store.get_tree(tree_id),
        Err(StoreError::Corrupt(_))
    ));
}

/// A second S3 client over the same live bucket, built from the same
/// `GARDENBED_TEST_S3_*` env — raw access to the keys the store owns.
fn raw_s3_client() -> Arc<dyn object_store::ObjectStore> {
    use object_store::aws::AmazonS3Builder;
    let endpoint = std::env::var("GARDENBED_TEST_S3_ENDPOINT").unwrap();
    let bucket = std::env::var("GARDENBED_TEST_S3_BUCKET")
        .or_else(|_| std::env::var("GARDENBED_TEST_BUCKET"))
        .unwrap();
    let mut builder = AmazonS3Builder::new()
        .with_endpoint(&endpoint)
        .with_bucket_name(&bucket)
        .with_region(
            std::env::var("GARDENBED_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        )
        .with_access_key_id(std::env::var("GARDENBED_TEST_S3_ACCESS_KEY").unwrap())
        .with_secret_access_key(std::env::var("GARDENBED_TEST_S3_SECRET_KEY").unwrap())
        .with_allow_http(true);
    let path_style = std::env::var("GARDENBED_TEST_S3_PATH_STYLE")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    builder = builder.with_virtual_hosted_style_request(!path_style);
    Arc::new(builder.build().expect("raw client builds"))
}

/// Runs one future on a throwaway runtime — the raw client is test-side
/// scaffolding, not store code, so a fresh runtime per call is fine.
fn raw_block_on<T>(fut: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("raw client runtime")
        .block_on(fut)
}

/// Unconditional raw put — the tamper path the store never exposes.
fn put_raw(client: &dyn object_store::ObjectStore, key: &str, bytes: Vec<u8>) {
    use object_store::PutPayload;
    raw_block_on(client.put(
        &object_store::path::Path::from(key),
        PutPayload::from(bytes),
    ))
    .expect("raw put");
}

/// Raw read of exactly what sits at the key.
fn get_raw(client: &dyn object_store::ObjectStore, key: &str) -> Vec<u8> {
    let bytes = raw_block_on(async {
        client
            .get(&object_store::path::Path::from(key))
            .await
            .expect("raw get")
            .bytes()
            .await
            .expect("raw get body")
    });
    bytes.to_vec()
}

/// A valid GBLO1 envelope for `content` — the same on-disk shape the fs
/// and S3 backends write (magic + raw_len + one zstd frame).
fn envelope_bytes(content: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::new();
    out.write_all(b"GBLO1").unwrap();
    out.write_all(&(content.len() as u64).to_le_bytes())
        .unwrap();
    let mut encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
    encoder.write_all(content).unwrap();
    out.write_all(&encoder.finish().unwrap()).unwrap();
    out
}

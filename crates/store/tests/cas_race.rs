//! M1 verification: CAS races — 32 threads racing the same ref from the
//! same base must produce exactly one winner and 31 clean conflicts, with
//! the final ref value equal to the winner's.

mod common;

use common::id_of;
use gardenbed_model::Id;
use gardenbed_store::{MemoryStore, MetadataStore, RefCas, SqliteMetadataStore, StoreError};
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

const THREADS: usize = 32;

/// Races `THREADS` threads on one ref from `base`, asserting exactly one
/// winner and `THREADS - 1` clean conflicts whose reported current value is
/// the base, and that the final ref equals the winner's new value.
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

    // Every conflict reports the value that blocked it: once the winner has
    // installed its id, every later racer sees the winner's id, not the base.
    assert!(
        conflicts.iter().all(|c| *c == winner_new),
        "all conflicts report the winner's id"
    );

    // Final value == the winner's new id.
    let final_ref = store
        .resolve_ref(name)
        .expect("resolve")
        .expect("ref exists");
    assert_eq!(final_ref, winner_new);
}

#[test]
fn cas_race_sqlite_from_existing_base() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("metadata.sqlite3");
    let store = Arc::new(SqliteMetadataStore::open(&db).expect("open"));

    let base = id_of(0xB4E);
    assert_eq!(
        store.cas_ref("main", None, base).expect("seed ref"),
        RefCas::Won { current: base }
    );
    race_from_base(store, Some(base), "main");
}

#[test]
fn cas_race_sqlite_from_absent_ref() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("metadata.sqlite3");
    let store = Arc::new(SqliteMetadataStore::open(&db).expect("open"));
    race_from_base(store, None, "feature/stack");
}

#[test]
fn cas_race_memory_from_existing_base() {
    let store = Arc::new(MemoryStore::new());
    let base = id_of(0xB4F);
    assert_eq!(
        store.cas_ref("main", None, base).expect("seed"),
        RefCas::Won { current: base }
    );
    race_from_base(store, Some(base), "main");
}

#[test]
fn cas_race_memory_from_absent_ref() {
    let store = Arc::new(MemoryStore::new());
    race_from_base(store, None, "feature/stack");
}

#[test]
fn cas_sequential_semantics() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SqliteMetadataStore::open(&dir.path().join("s.sqlite3")).expect("open");

    let a = id_of(1);
    let b = id_of(2);
    let c = id_of(3);

    // Won on absent ref when expecting None.
    assert_eq!(
        store.cas_ref("main", None, a).unwrap(),
        RefCas::Won { current: a }
    );
    // Conflict when expecting None but ref exists.
    assert_eq!(
        store.cas_ref("main", None, b).unwrap(),
        RefCas::Conflict { current: a }
    );
    // Conflict when expecting the wrong value; reports current.
    assert_eq!(
        store.cas_ref("main", Some(c), b).unwrap(),
        RefCas::Conflict { current: a }
    );
    // Won when expecting the right value.
    assert_eq!(
        store.cas_ref("main", Some(a), b).unwrap(),
        RefCas::Won { current: b }
    );
    // NotFound when expecting Some on an absent ref.
    assert!(matches!(
        store.cas_ref("ghost", Some(a), c),
        Err(StoreError::NotFound)
    ));
    // The losing CAS left "main" untouched.
    assert_eq!(store.resolve_ref("main").unwrap(), Some(b));

    // Empty ref names are rejected everywhere.
    assert!(matches!(
        store.cas_ref("", None, a),
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(store.resolve_ref(""), Err(StoreError::Invalid(_))));
}

#[test]
fn cas_sequential_semantics_memory() {
    let store = MemoryStore::new();
    let a = id_of(11);
    let b = id_of(12);
    assert_eq!(
        store.cas_ref("main", None, a).unwrap(),
        RefCas::Won { current: a }
    );
    assert_eq!(
        store.cas_ref("main", None, b).unwrap(),
        RefCas::Conflict { current: a }
    );
    assert_eq!(
        store.cas_ref("main", Some(a), b).unwrap(),
        RefCas::Won { current: b }
    );
    assert!(matches!(
        store.cas_ref("ghost", Some(a), b),
        Err(StoreError::NotFound)
    ));
}

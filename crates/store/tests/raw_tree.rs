//! Raw-tree capture (raw-tree fidelity): the additive, content-addressed
//! side table keyed by the ORIGINAL git tree SHA with the canonical model
//! tree id as an association.
//!
//! Invariants under test, at the trait layer across both metadata
//! backends:
//!
//! * **Hash discipline** — the row key is `SHA-1("tree <len>\0" + bytes)`;
//!   bytes that hash elsewhere are refused with `Invalid`, never stored.
//! * **Immutability** — re-putting the same key with identical bytes and
//!   the same model association is an idempotent no-op; different bytes
//!   or a different association is a loud `Invalid` (content-addressed
//!   rows never change meaning).
//! * **Model association** — `raw_tree_model_id` returns the recorded
//!   association, including the `Some(None)` (empty normalized tree)
//!   and `None` (no row) cases; a key with no row reads as absent for
//!   both getters.
//! * **Orphan safety by construction** — a put needs no changeset row;
//!   the row is only meaningful when a captured commit names its key.
//! * **Parity** — SQLite and the memory backend answer identically.

mod common;

use common::id_of;
use gardenbed_store::{MemoryStore, MetadataStore, SqliteMetadataStore, StoreError};

/// A real git tree object body: `100644 a.txt\0` + 20-byte name, plus a
/// pre-canonical `100664 b.txt\0` entry. (Bytes only; nothing here needs
/// the object to exist in a git repository.)
fn tree_body() -> Vec<u8> {
    let name_a = [0x11u8; 20];
    let name_b = [0x22u8; 20];
    let mut body = Vec::new();
    body.extend_from_slice(b"100644 a.txt\0");
    body.extend_from_slice(&name_a);
    body.extend_from_slice(b"100664 b.txt\0");
    body.extend_from_slice(&name_b);
    body
}

/// `SHA-1("tree <len>\0" + body)` — the row key, computed independently of
/// the store's own helper (a wrong-key test needs an oracle).
fn git_tree_sha1(body: &[u8]) -> [u8; 20] {
    use sha1::Digest as _;
    let mut hasher = sha1::Sha1::new();
    hasher.update(b"tree ");
    hasher.update(body.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(body);
    hasher.finalize().into()
}

fn raw_tree_checks(store: &dyn MetadataStore) {
    let body = tree_body();
    let key = git_tree_sha1(&body);

    // No row yet: both getters read absent, never an error.
    let absent = [0xABu8; 20];
    assert!(store.raw_tree(absent).unwrap().is_none());
    assert!(store.raw_tree_model_id(absent).unwrap().is_none());

    // A different body under this key: hash mismatch, refused loudly.
    let mut other = body.clone();
    other[0] = b'0';
    assert!(matches!(
        store.put_raw_tree(key, Some(id_of(1)), &other),
        Err(StoreError::Invalid(_))
    ));
    assert!(
        store.raw_tree(key).unwrap().is_none(),
        "a refused put stores nothing"
    );

    // The put itself: rows land without any changeset existing (additive,
    // crash-orphan-safe by construction).
    store.put_raw_tree(key, Some(id_of(1)), &body).expect("put");
    assert_eq!(store.raw_tree(key).unwrap().expect("row"), body);
    assert_eq!(
        store.raw_tree_model_id(key).unwrap(),
        Some(Some(id_of(1))),
        "the model association reads back"
    );

    // Idempotent re-put with identical bytes and association.
    store.put_raw_tree(key, Some(id_of(1)), &body).expect("re-put");

    // Same bytes, different model association: the row would silently
    // change meaning — loud refusal.
    assert!(matches!(
        store.put_raw_tree(key, Some(id_of(2)), &body),
        Err(StoreError::Invalid(_))
    ));
    // Same association, different bytes (hash discipline catches this
    // first: the bytes no longer SHA-1 to the key).
    assert!(matches!(
        store.put_raw_tree(key, Some(id_of(1)), &other),
        Err(StoreError::Invalid(_))
    ));

    // The empty-normalized-tree association: `Some(None)` roundtrips and
    // stays distinct from "no row".
    let empty_key = git_tree_sha1(b"");
    store.put_raw_tree(empty_key, None, b"").expect("empty put");
    assert_eq!(store.raw_tree(empty_key).unwrap().expect("row"), b"");
    assert_eq!(
        store.raw_tree_model_id(empty_key).unwrap(),
        Some(None),
        "an empty normalized tree reads back as Some(None), not None"
    );
}

#[test]
fn raw_tree_roundtrip_memory() {
    raw_tree_checks(&MemoryStore::new());
}

#[test]
fn raw_tree_roundtrip_sqlite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("s.sqlite3");
    let store = SqliteMetadataStore::open(&db).expect("open");
    raw_tree_checks(&store);

    // Tampered row (direct DB edit — the only path that can produce one):
    // the frame decodes to bytes that no longer SHA-1 to the key. The
    // getters must surface the bytes as stored (identity truth), and the
    // export's per-tree SHA assertion is the consumer-side guard; here we
    // pin that a direct edit is observable, not silent.
    let key = git_tree_sha1(&tree_body());
    let mut tampered = tree_body();
    tampered.extend_from_slice(b"junk");
    let frame = zstd::bulk::compress(&tampered, 3).expect("frame");
    let conn = rusqlite::Connection::open(&db).expect("raw conn");
    conn.execute(
        "UPDATE git_raw_tree SET data = ?1 WHERE git_sha = ?2",
        rusqlite::params![frame, key.as_slice()],
    )
    .expect("tamper");
    let read_back = store.raw_tree(key).unwrap().expect("row present");
    assert_eq!(
        read_back, tampered,
        "a tampered row is observable through the getter (the export's SHA \
         assertion refuses it loudly)"
    );
}

#[test]
fn raw_tree_two_bodies_one_model_association() {
    // The keying decision in one test: two DIFFERENT git trees (different
    // original SHAs, e.g. 100664 vs 100644 bytes) can normalize to the
    // SAME model tree id — both rows must coexist, keyed by their own
    // git SHA. Keying by model id would have silently conflated them.
    let store = MemoryStore::new();
    let mut body_a = Vec::new();
    body_a.extend_from_slice(b"100664 f\0");
    body_a.extend_from_slice(&[0x33u8; 20]);
    let mut body_b = Vec::new();
    body_b.extend_from_slice(b"100644 f\0");
    body_b.extend_from_slice(&[0x33u8; 20]);
    let key_a = git_tree_sha1(&body_a);
    let key_b = git_tree_sha1(&body_b);
    assert_ne!(key_a, key_b, "distinct raw trees have distinct SHAs");

    let model = id_of(7);
    store.put_raw_tree(key_a, Some(model), &body_a).expect("a");
    store.put_raw_tree(key_b, Some(model), &body_b).expect("b");

    assert_eq!(store.raw_tree(key_a).unwrap().unwrap(), body_a);
    assert_eq!(store.raw_tree(key_b).unwrap().unwrap(), body_b);
    assert_eq!(store.raw_tree_model_id(key_a).unwrap(), Some(Some(model)));
    assert_eq!(store.raw_tree_model_id(key_b).unwrap(), Some(Some(model)));
}

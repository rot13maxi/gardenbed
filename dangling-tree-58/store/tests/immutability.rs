//! M1 verification: immutability — putting an existing Id with different
//! bytes is an error, at the trait layer, the memory layer, and the fs layer
//! (by constructing the envelope path directly).

mod common;

use common::{envelope_for, random_bytes, sample_tree, shard_path};
use gardenbed_model::blob_id;
use gardenbed_store::{FsObjectStore, MemoryStore, ObjectStore, StoreError};
use std::fs;
use std::sync::Arc;
use std::thread;

#[test]
fn fs_put_same_id_with_different_bytes_is_violation() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    let content = b"original content";
    let id = store.put_blob(content).expect("first put");
    assert_eq!(id, blob_id(content));

    // Re-put identical bytes: idempotent.
    assert_eq!(store.put_blob(content).expect("identical re-put"), id);

    // At the fs layer, "put same id with different bytes" is simulated by
    // constructing the envelope path with a valid envelope of different
    // content, exactly as the brief prescribes.
    let path = root
        .path()
        .join("objects")
        .join(&id.to_hex()[..2])
        .join(&id.to_hex()[2..]);
    fs::write(&path, envelope_for(b"different bytes entirely")).expect("craft envelope");

    match store.put_blob(content) {
        Err(StoreError::ImmutabilityViolation(violated)) => {
            assert_eq!(violated, id)
        }
        other => panic!("expected ImmutabilityViolation, got {other:?}"),
    }
}

#[test]
fn fs_tree_conflicting_existing_bytes_are_violation() {
    let (_tree, tree_id, canonical) = sample_tree();
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    store.put_tree(tree_id, &canonical).expect("put_tree");

    // Overwrite the tree object's file with different (valid canonical-looking)
    // bytes, then re-put: the store must refuse.
    let path = root
        .path()
        .join("objects")
        .join(&tree_id.to_hex()[..2])
        .join(&tree_id.to_hex()[2..]);
    fs::write(&path, b"payload bytes that are not this tree").expect("craft tree file");

    match store.put_tree(tree_id, &canonical) {
        Err(StoreError::ImmutabilityViolation(violated)) => assert_eq!(violated, tree_id),
        other => panic!("expected ImmutabilityViolation, got {other:?}"),
    }
}

#[test]
fn fs_corrupt_existing_object_is_healed_not_violation() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = random_bytes(0xC0FFEE, 2048);
    let id = store.put_blob(&content).expect("put");

    // Externally damage the object (only possible by bypassing the store).
    let path = shard_path(root.path(), id);
    fs::write(&path, b"GBLO1garbage").expect("damage");

    // Re-put of the true content heals the corrupt object: the store holds
    // bytes that hash to the id, so overwriting restores the invariant.
    assert_eq!(store.put_blob(&content).expect("healing put"), id);
    assert_eq!(store.get_blob(id).expect("get").unwrap(), content);
}

#[test]
fn memory_put_same_id_with_different_bytes_is_violation() {
    let store = MemoryStore::new();
    let content = b"the real content";
    let id = store.put_blob(content).expect("put");

    // Seed a different payload under the same id (in-memory analogue of
    // crafting the envelope path).
    store.insert_raw(id, b"something else".to_vec());
    match store.put_blob(content) {
        Err(StoreError::ImmutabilityViolation(violated)) => assert_eq!(violated, id),
        other => panic!("expected ImmutabilityViolation, got {other:?}"),
    }
}

#[test]
fn memory_tree_conflicting_existing_bytes_are_violation() {
    let (_tree, tree_id, canonical) = sample_tree();
    let store = MemoryStore::new();
    store.put_tree(tree_id, &canonical).expect("put_tree");

    // Seed different bytes under the same tree id (in-memory analogue of
    // overwriting the object file).
    let tampered = b"not the canonical bytes".to_vec();
    store.insert_tree_raw(tree_id, tampered.to_vec());
    match store.put_tree(tree_id, &canonical) {
        Err(StoreError::ImmutabilityViolation(violated)) => assert_eq!(violated, tree_id),
        other => panic!("expected ImmutabilityViolation, got {other:?}"),
    }
}

#[test]
fn concurrent_identical_puts_are_idempotent() {
    // Same content from 32 threads: every put must succeed with the same id.
    let root = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(FsObjectStore::open(root.path()).expect("open"));
    let content: Arc<Vec<u8>> = Arc::new(random_bytes(0x1DE17, 8192));
    let id = blob_id(&content);

    let handles: Vec<_> = (0..32)
        .map(|_| {
            let store = Arc::clone(&store);
            let content = Arc::clone(&content);
            thread::spawn(move || store.put_blob(&content))
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().expect("join").expect("put"), id);
    }
}

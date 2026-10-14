//! M1 verification: byte-exactness of blob and tree roundtrips across both
//! object-store backends, plus §6 envelope layout on disk and Corrupt
//! behavior for damaged / mis-addressed objects.

mod common;

use common::{compressible_bytes, random_bytes, shard_path, sample_tree};
use gardenbed_model::{Id, Tree};
use gardenbed_store::{FsObjectStore, MemoryStore, ObjectStore};
use std::fs;
use std::str::FromStr;

fn assert_roundtrip(store: &dyn ObjectStore, content: &[u8]) {
    let id = store.put_blob(content).expect("put_blob");
    assert_eq!(id, gardenbed_model::blob_id(content), "id is SHA-256(content)");
    let got = store.get_blob(id).expect("get_blob");
    assert_eq!(got.as_deref(), Some(content), "byte-exact roundtrip");
    assert!(store.has(id).expect("has"), "has() true after put");
    // Idempotent re-put with identical bytes.
    assert_eq!(store.put_blob(content).expect("re-put"), id);
}

#[test]
fn blob_roundtrip_all_sizes_fs() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    assert_roundtrip(&store, b""); // size 0
    assert_roundtrip(&store, b"x"); // size 1
    assert_roundtrip(&store, &compressible_bytes(0x1001, 1024)); // 1 KiB
    assert_roundtrip(&store, &compressible_bytes(0x1002, 1 << 20)); // 1 MiB
    // Incompressible random data.
    assert_roundtrip(&store, &random_bytes(0x1003, 256 * 1024));
    assert_roundtrip(&store, &random_bytes(0x1004, 1 << 20));
}

#[test]
fn blob_roundtrip_all_sizes_memory() {
    let store = MemoryStore::new();
    assert_roundtrip(&store, b"");
    assert_roundtrip(&store, b"x");
    assert_roundtrip(&store, &compressible_bytes(0x2001, 1024));
    assert_roundtrip(&store, &compressible_bytes(0x2002, 1 << 20));
    assert_roundtrip(&store, &random_bytes(0x2003, 256 * 1024));
}

#[test]
fn fs_on_disk_envelope_matches_spec_section_6() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = b"envelope layout: magic, u64 LE raw_len, one zstd frame";
    let id = store.put_blob(content).expect("put");

    let path = shard_path(root.path(), id);
    assert!(path.exists(), "object at objects/<2hex>/<62hex> path");
    // Exactly the two-level shard directories exist.
    let parent = path.parent().unwrap();
    assert_eq!(parent.file_name().unwrap().to_str().unwrap(), &id.to_hex()[..2]);
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), &id.to_hex()[2..]);

    let raw = fs::read(&path).expect("read object file");
    assert_eq!(&raw[..5], b"GBLO1", "magic");
    let raw_len = u64::from_le_bytes(raw[5..13].try_into().unwrap());
    assert_eq!(raw_len, content.len() as u64, "raw_len little-endian");
    // The remainder is exactly one zstd frame of the content.
    let frame = &raw[13..];
    let mut decoded = Vec::new();
    zstd::stream::Decoder::new(frame)
        .and_then(|mut d| std::io::Read::read_to_end(&mut d, &mut decoded))
        .expect("single zstd frame decompresses");
    assert_eq!(decoded, content);
}

#[test]
fn tree_roundtrip_fs_and_memory() {
    let (tree, tree_id, canonical) = sample_tree();

    let root = tempfile::tempdir().expect("tempdir");
    let fs_store = FsObjectStore::open(root.path()).expect("open");
    fs_store.put_tree(tree_id, &canonical).expect("put_tree");
    let got = fs_store.get_tree(tree_id).expect("get_tree");
    assert_eq!(got.as_deref(), Some(canonical.as_slice()));
    assert_eq!(Tree::decode(&got.unwrap()).unwrap(), tree);
    assert!(fs_store.has(tree_id).unwrap());

    let mem = MemoryStore::new();
    mem.put_tree(tree_id, &canonical).expect("put_tree");
    assert_eq!(mem.get_tree(tree_id).unwrap().as_deref(), Some(canonical.as_slice()));
    assert!(mem.has(tree_id).unwrap());
}

#[test]
fn put_tree_rejects_bytes_that_do_not_hash_to_id() {
    let (_tree, tree_id, _canonical) = sample_tree();
    let root = tempfile::tempdir().expect("tempdir");
    let fs_store = FsObjectStore::open(root.path()).expect("open");
    let wrong = format!("not the canonical bytes of {tree_id}").into_bytes();
    assert!(matches!(
        fs_store.put_tree(tree_id, &wrong),
        Err(gardenbed_store::StoreError::Invalid(_))
    ));
    // Nothing was written.
    assert!(!fs_store.has(tree_id).unwrap());
}

#[test]
fn type_confusion_between_blob_and_tree_is_corrupt() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    let blob_id = store.put_blob(b"some blob").expect("put");
    // A blob id's on-disk form is an envelope; reading it as a tree fails
    // verification rather than returning the envelope as tree bytes.
    assert!(matches!(
        store.get_tree(blob_id),
        Err(gardenbed_store::StoreError::Corrupt(_))
    ));

    let (_tree, tree_id, canonical) = sample_tree();
    store.put_tree(tree_id, &canonical).expect("put_tree");
    // Tree bytes are raw canonical payloads: reading them as a blob fails
    // the envelope magic check.
    assert!(matches!(
        store.get_blob(tree_id),
        Err(gardenbed_store::StoreError::Corrupt(_))
    ));
}

#[test]
fn damaged_envelopes_are_corrupt_never_wrong_bytes() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = random_bytes(0xBEEF, 4096);
    let id = store.put_blob(&content).expect("put");
    let path = shard_path(root.path(), id);

    // Torn writes are impossible by construction (tmp + rename), so a
    // damaged file can only be external tampering — verify every mutation
    // is detected as Corrupt.
    let original = fs::read(&path).expect("read envelope");

    let variants: Vec<Vec<u8>> = vec![
        original[..original.len() - 1].to_vec(),        // truncated tail
        original[..13].to_vec(),                        // header only, no frame
        original[..5].to_vec(),                         // magic only
        { let mut v = original.clone(); v[0] = b'X'; v }, // bad magic
        { let mut v = original.clone(); v[5] ^= 0x01; v }, // corrupt raw_len
        { let mut v = original.clone(); v[20] ^= 0xFF; v }, // corrupt frame body
    ];
    for v in variants {
        fs::write(&path, &v).expect("write tampered envelope");
        match store.get_blob(id) {
            Err(gardenbed_store::StoreError::Corrupt(_)) => {}
            other => panic!(
                "expected Corrupt for damaged envelope, got {:?}",
                other.map(|o| o.map(|v| v.len()))
            ),
        }
    }
}

#[test]
fn absent_ids_read_as_none() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let ghost = Id::from_str(
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
    )
    .unwrap();
    assert_eq!(store.get_blob(ghost).unwrap(), None);
    assert_eq!(store.get_tree(ghost).unwrap(), None);
    assert!(!store.has(ghost).unwrap());

    let mem = MemoryStore::new();
    assert_eq!(mem.get_blob(ghost).unwrap(), None);
    assert_eq!(mem.get_tree(ghost).unwrap(), None);
    assert!(!mem.has(ghost).unwrap());
}

#[test]
fn empty_blob_roundtrips_through_the_envelope() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let id = store.put_blob(b"").expect("put empty");
    assert_eq!(
        id.to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "SHA-256 of empty content"
    );
    assert_eq!(store.get_blob(id).unwrap().unwrap(), Vec::<u8>::new());

    // The envelope on disk still carries magic + zero length + a valid frame.
    let raw = fs::read(shard_path(root.path(), id)).expect("envelope file");
    assert_eq!(&raw[..5], b"GBLO1");
    assert_eq!(&raw[5..13], &0u64.to_le_bytes());
    let mut decoded = Vec::new();
    zstd::stream::Decoder::new(&raw[13..])
        .and_then(|mut d| std::io::Read::read_to_end(&mut d, &mut decoded))
        .expect("empty-content frame");
    assert_eq!(decoded, Vec::<u8>::new());
}

#[test]
fn sharding_layout_is_two_levels() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = b"sharding check";
    let id = store.put_blob(content).expect("put");
    let expected = root
        .path()
        .join("objects")
        .join(&id.to_hex()[..2])
        .join(&id.to_hex()[2..]);
    assert_eq!(store.objects_dir().join(&id.to_hex()[..2]).join(&id.to_hex()[2..]), expected);
    assert!(expected.is_file());
    assert_eq!(store.get_blob(id).unwrap().unwrap(), content.to_vec());
}

//! M14 verification: GBLO2 — content-defined chunked storage above the
//! 1 MiB threshold (docs/v0.2.md §7 + §9 M14).
//!
//! 1. **Identity preserved**: 0 B, 1 KiB, exactly 1 MiB, 1 MiB + 1, 8 MiB,
//!    64 MiB, compressible and incompressible — roundtrip byte-exact,
//!    `id = SHA-256(raw)`, GBLO1 at or below the threshold, GBLO2 above.
//! 2. **FastCDC determinism**: identical content → identical chunk table
//!    across processes; a 1-byte tail append leaves every earlier boundary
//!    fixed (boundaries pinned, not frame bytes).
//! 3. **Torn/tampered multi-chunk blobs**: byte-exact or absent; one
//!    corrupted chunk ⇒ `Corrupt`, never partial bytes.
//!
//! The suite also pins the §7 envelope layout byte-for-byte (magic, header,
//! table, frames region) so the on-disk format is contractual, and the
//! GBLO2-aware immutability/heal rules.

mod common;

use common::{compressible_bytes, random_bytes, shard_path};
use gardenbed_model::blob_id;
use gardenbed_store::{FsObjectStore, ObjectStore, StoreError};
use std::fs;
use std::process::Command;

/// The chunking threshold (docs/v0.2.md §7): blobs above it store GBLO2.
const THRESHOLD: usize = 1 << 20;

/// Reads the on-disk envelope magic for a stored blob.
fn magic_of(root: &std::path::Path, id: gardenbed_model::Id) -> [u8; 5] {
    let raw = fs::read(shard_path(root, id)).expect("envelope exists");
    raw[..5].try_into().expect("5 magic bytes")
}

/// Parses a GBLO2 envelope's chunk table (raw, comp) pairs.
fn parse_table(raw: &[u8]) -> Vec<(u32, u32)> {
    assert_eq!(&raw[..5], b"GBLO2", "GBLO2 magic");
    let count = u32::from_le_bytes(raw[13..17].try_into().unwrap()) as usize;
    let mut table = Vec::with_capacity(count);
    for i in 0..count {
        let e = 17 + i * 8;
        let rl = u32::from_le_bytes(raw[e..e + 4].try_into().unwrap());
        let cl = u32::from_le_bytes(raw[e + 4..e + 8].try_into().unwrap());
        table.push((rl, cl));
    }
    table
}

// ----------------------------------------------------------- criterion 1

/// Roundtrip + envelope-class + identity at every pinned size, both
/// compressible and incompressible.
#[test]
fn identity_preserved_across_sizes_and_compressibility() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    let small: Vec<(usize, bool)> = vec![
        (0, true),
        (1024, true),
        (THRESHOLD, true),     // exactly 1 MiB: GBLO1
        (THRESHOLD, false),    // exactly 1 MiB incompressible: GBLO1
        (THRESHOLD + 1, true), // 1 MiB + 1: GBLO2
        (THRESHOLD + 1, false),
        (8 << 20, true),
        (8 << 20, false),
    ];
    for (len, compressible) in small {
        let content = if compressible {
            compressible_bytes(0x11D4 ^ len as u64, len)
        } else {
            random_bytes(0x11D4u64.wrapping_add(len as u64), len)
        };
        let id = store.put_blob(&content).expect("put");
        assert_eq!(
            id,
            blob_id(&content),
            "id is SHA-256(raw) at len={len} compressible={compressible}"
        );
        let got = store.get_blob(id).expect("get").expect("present");
        assert_eq!(
            got, content,
            "byte-exact roundtrip at len={len} compressible={compressible}"
        );
        let want_magic: &[u8; 5] = if len > THRESHOLD { b"GBLO2" } else { b"GBLO1" };
        assert_eq!(
            &magic_of(root.path(), id),
            want_magic,
            "envelope class at len={len} compressible={compressible}"
        );
        // Idempotent re-put.
        assert_eq!(store.put_blob(&content).expect("re-put"), id);
    }
}

/// 64 MiB (compressible — the incompressible variant would need 64 MiB of
/// entropy and buys no extra coverage on the same code path; the 8 MiB
/// incompressible case above covers random data).
#[test]
fn identity_preserved_at_64_mib() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let len = 64 << 20;
    let content = compressible_bytes(0x64B1B, len);
    let id = store.put_blob(&content).expect("put");
    assert_eq!(id, blob_id(&content), "id is SHA-256(raw) at 64 MiB");
    assert_eq!(&magic_of(root.path(), id), b"GBLO2");
    let got = store.get_blob(id).expect("get").expect("present");
    assert_eq!(got.len(), len);
    assert_eq!(got, content, "byte-exact roundtrip at 64 MiB");
}

/// The GBLO1 path keeps its §6 layout byte-identically at the threshold:
/// magic, u64 LE raw_len, one zstd frame of exactly the content.
#[test]
fn gblo1_layout_unchanged_at_threshold() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = compressible_bytes(0xED6E, THRESHOLD);
    let id = store.put_blob(&content).expect("put");
    let raw = fs::read(shard_path(root.path(), id)).expect("read envelope");
    assert_eq!(&raw[..5], b"GBLO1");
    assert_eq!(raw[5..13], (THRESHOLD as u64).to_le_bytes());
    let mut decoded = Vec::new();
    zstd::stream::Decoder::new(&raw[13..])
        .and_then(|mut d| std::io::Read::read_to_end(&mut d, &mut decoded))
        .expect("one zstd frame");
    assert_eq!(decoded, content);
}

/// The GBLO2 envelope matches the §7 layout exactly.
#[test]
fn gblo2_envelope_layout_matches_spec() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    // 5 MiB incompressible: several FastCDC chunks, frames ~ chunk size.
    let content = random_bytes(0x5B11, 5 << 20);
    let id = store.put_blob(&content).expect("put");
    let raw = fs::read(shard_path(root.path(), id)).expect("read envelope");

    assert_eq!(&raw[..5], b"GBLO2", "magic");
    let raw_len = u64::from_le_bytes(raw[5..13].try_into().unwrap());
    assert_eq!(raw_len as usize, content.len(), "raw_len LE");
    let count = u32::from_le_bytes(raw[13..17].try_into().unwrap()) as usize;
    assert!(count >= 4, "5 MiB must span several chunks (got {count})");

    let table = parse_table(&raw);
    assert_eq!(table.len(), count);
    // Table raw lengths sum to raw_len; each respects the FastCDC bounds;
    // frames region = declared comp lengths exactly.
    let sum: u64 = table.iter().map(|&(r, _)| r as u64).sum();
    assert_eq!(sum as usize, content.len(), "table sums to raw_len");
    for (i, &(r, _)) in table.iter().enumerate() {
        assert!(r as usize >= 1, "chunk {i} nonempty");
        // Interior chunks respect MIN; the final chunk may be a short tail.
        if i + 1 < table.len() {
            assert!(r as usize >= 64 * 1024, "chunk {i} ≥ min");
        }
        assert!(r as usize <= 1024 * 1024, "chunk {i} ≤ max");
    }
    let comp_total: usize = table.iter().map(|&(_, c)| c as usize).sum();
    let frames_len = raw.len() - 17 - count * 8;
    assert_eq!(
        comp_total, frames_len,
        "frames region is exactly the table's comp lengths"
    );
    // And the reassembled content still verifies against the id.
    assert_eq!(store.get_blob(id).unwrap().unwrap(), content);
}

// ----------------------------------------------------------- criterion 2

/// Identical content → identical chunk table **across processes**: the
/// child process (this same test binary, `--exact`-filtered) puts the same
/// content into a fresh store and prints its chunk table; the parent
/// compares.
#[test]
fn fastcdc_table_identical_across_processes() {
    let root = tempfile::tempdir().expect("tempdir");
    let content = random_bytes(0xDE7E, 6 << 20);

    // Parent process: table via the store.
    let store = FsObjectStore::open(root.path()).expect("open");
    let id = store.put_blob(&content).expect("put");
    let table_parent = parse_table(&fs::read(shard_path(root.path(), id)).expect("read"));

    // Child process: fresh store, same content.
    let exe = std::env::current_exe().expect("test binary");
    let out = Command::new(exe)
        .arg("fastcdc_table_child_entry")
        .arg("--exact")
        .arg("--ignored")
        .arg("--nocapture")
        .env("GBLO2_CHILD_ROOT", root.path())
        .env("GBLO2_CHILD_SEED", "0xDE7E")
        .env("GBLO2_CHILD_LEN", format!("{}", 6 << 20))
        .output()
        .expect("child spawns");
    assert!(
        out.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let table_line = stdout
        .lines()
        .find(|l| l.starts_with("CHUNKTABLE "))
        .expect("child printed its chunk table");
    let child: Vec<(u32, u32)> = table_line
        .strip_prefix("CHUNKTABLE ")
        .unwrap()
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|pair| {
            let mut it = pair.split(',');
            (
                it.next().unwrap().parse().unwrap(),
                it.next().unwrap().parse().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        table_parent, child,
        "identical content must yield the identical chunk table in another process"
    );
}

/// The child entry point: computes the table for the same content in a
/// fresh process and prints `CHUNKTABLE r,c;r,c;...`.
#[test]
#[ignore = "child entry; driven by fastcdc_table_identical_across_processes"]
fn fastcdc_table_child_entry() {
    let root = std::env::var("GBLO2_CHILD_ROOT").expect("root env");
    let seed: u64 = u64::from_str_radix(
        std::env::var("GBLO2_CHILD_SEED")
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )
    .expect("seed");
    let len: usize = std::env::var("GBLO2_CHILD_LEN")
        .unwrap()
        .parse()
        .expect("len");
    let content = random_bytes(seed, len);
    let store = FsObjectStore::open(std::path::Path::new(&root)).expect("open");
    let id = store.put_blob(&content).expect("put");
    let raw = fs::read(shard_path(std::path::Path::new(&root), id)).expect("read");
    let table = parse_table(&raw);
    let rendered = table
        .iter()
        .map(|(r, c)| format!("{r},{c}"))
        .collect::<Vec<_>>()
        .join(";");
    println!("CHUNKTABLE {rendered}");
}

/// A 1-byte tail append leaves every earlier boundary fixed — boundaries
/// are pinned by content, not by frame bytes. (The boundary table of the
/// original must be a prefix of the appended one's, except that the
/// original's last chunk boundary may merge into the appended file's next
/// chunk.)
#[test]
fn one_byte_tail_append_pins_earlier_boundaries() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    let base = random_bytes(0xA0B0, 8 << 20);
    let mut appended = base.clone();
    appended.push(0xAB);

    let id_b = store.put_blob(&base).expect("put base");
    let id_a = store.put_blob(&appended).expect("put appended");
    assert_ne!(id_b, id_a, "different content, different ids");

    let t_b = parse_table(&fs::read(shard_path(root.path(), id_b)).expect("read"));
    let t_a = parse_table(&fs::read(shard_path(root.path(), id_a)).expect("read"));

    // Every boundary of the original except its last must appear at the
    // same offset in the appended file's boundary set.
    let boundaries = |t: &[(u32, u32)]| -> Vec<u64> {
        let mut v = Vec::with_capacity(t.len());
        let mut acc = 0u64;
        for &(r, _) in t {
            acc += r as u64;
            v.push(acc);
        }
        v
    };
    let boundaries_b = boundaries(&t_b);
    let boundaries_a = boundaries(&t_a);
    for (i, &off_b) in boundaries_b.iter().enumerate().take(boundaries_b.len() - 1) {
        assert!(
            boundaries_a.contains(&off_b),
            "boundary {i} of the original (offset {off_b}) must survive the tail append"
        );
    }
}

// ----------------------------------------------------------- criterion 3

/// One corrupted chunk (a flipped byte inside a single zstd frame) ⇒
/// `Corrupt`, never partial bytes. Also: a chunk table entry lying about
/// its frame length, a truncated frames region, and a chunk whose
/// decompressed size disagrees — all `Corrupt`.
#[test]
fn corrupted_chunk_is_corrupt_never_partial() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");
    let content = random_bytes(0xC0DE, 4 << 20);
    let id = store.put_blob(&content).expect("put");
    let path = shard_path(root.path(), id);
    let raw = fs::read(&path).expect("read envelope");
    let count = u32::from_le_bytes(raw[13..17].try_into().unwrap()) as usize;
    assert!(count >= 3, "multi-chunk blob (got {count})");
    let frames_start = 17 + count * 8;

    // 1. Flip one byte inside the FIRST chunk's frame.
    let mut damaged = raw.clone();
    damaged[frames_start + 100] ^= 0xFF;
    fs::write(&path, &damaged).expect("damage frame");
    match store.get_blob(id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("corrupted first chunk must be Corrupt, got {other:?}"),
    }

    // 2. Flip one byte inside the LAST chunk's frame.
    let mut damaged = raw.clone();
    let last_comp = u32::from_le_bytes(
        raw[17 + (count - 1) * 8 + 4..17 + (count - 1) * 8 + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    let last_start = raw.len() - last_comp;
    damaged[last_start + (last_comp / 2).max(1)] ^= 0xFF;
    fs::write(&path, &damaged).expect("damage last frame");
    match store.get_blob(id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("corrupted last chunk must be Corrupt, got {other:?}"),
    }

    // 3. A chunk table entry claiming a frame longer than the region.
    let mut damaged = raw.clone();
    damaged[17 + 4..17 + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, &damaged).expect("lie comp_len");
    match store.get_blob(id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("lying comp_len must be Corrupt, got {other:?}"),
    }

    // 4. Truncated frames region (torn write).
    let mut damaged = raw[..raw.len() - 10].to_vec();
    damaged.truncate(raw.len() - 10);
    fs::write(&path, &damaged).expect("truncate");
    match store.get_blob(id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("truncated envelope must be Corrupt, got {other:?}"),
    }

    // 5. A chunk raw_len above the FastCDC max (hostile table).
    let mut damaged = raw.clone();
    damaged[17..17 + 4].copy_from_slice(&(1024u32 * 1024 + 1).to_le_bytes());
    fs::write(&path, &damaged).expect("oversize raw_len");
    match store.get_blob(id) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("oversize raw_len must be Corrupt, got {other:?}"),
    }

    // 6. Restore the true envelope: reads fine again.
    fs::write(&path, &raw).expect("restore");
    assert_eq!(store.get_blob(id).unwrap().unwrap(), content);
}

/// GBLO2-aware immutability: a structurally valid GBLO2 of *different*
/// content under this id is a violation; a damaged GBLO2 is healed by
/// re-putting the true content.
#[test]
fn gblo2_immutability_and_heal() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = FsObjectStore::open(root.path()).expect("open");

    let content = random_bytes(0x1AA0, 3 << 20);
    let id = store.put_blob(&content).expect("put");
    let path = shard_path(root.path(), id);

    // A valid GBLO2 envelope of different content, same id: violation.
    let other = random_bytes(0x0FB0, 3 << 20);
    assert_ne!(blob_id(&other), id);
    let forged = {
        // Build a GBLO2 envelope for `other` but store it at this id's path.
        let tmp_root = tempfile::tempdir().expect("tempdir");
        let tmp_store = FsObjectStore::open(tmp_root.path()).expect("open");
        let other_id = tmp_store.put_blob(&other).expect("put other");
        fs::read(shard_path(tmp_root.path(), other_id)).expect("read other envelope")
    };
    fs::write(&path, &forged).expect("forge");
    match store.put_blob(&content) {
        Err(StoreError::ImmutabilityViolation(_)) => {}
        other => panic!("different-content GBLO2 must be a violation, got {other:?}"),
    }

    // A damaged GBLO2 (bad magic region) is healed by the true re-put.
    fs::write(&path, b"GBLO2garbage-and-nothing-else").expect("damage");
    assert_eq!(store.put_blob(&content).expect("heal"), id);
    assert_eq!(store.get_blob(id).unwrap().unwrap(), content);
}

/// Concurrent identical puts of a GBLO2 blob stay idempotent (one id, one
/// byte-identical object).
#[test]
fn concurrent_identical_gblo2_puts_are_idempotent() {
    let root = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(FsObjectStore::open(root.path()).expect("open"));
    let content = std::sync::Arc::new(random_bytes(0xC0C0, 4 << 20));
    let id = blob_id(&content);

    let mut handles = Vec::new();
    for _ in 0..8 {
        let (store, content) = (store.clone(), content.clone());
        handles.push(std::thread::spawn(move || {
            assert_eq!(store.put_blob(&content).expect("put"), id);
        }));
    }
    for h in handles {
        h.join().expect("no panic");
    }
    assert_eq!(store.get_blob(id).unwrap().unwrap(), *content);
    // Exactly one object file exists at the shard path.
    assert!(shard_path(root.path(), id).is_file());
}

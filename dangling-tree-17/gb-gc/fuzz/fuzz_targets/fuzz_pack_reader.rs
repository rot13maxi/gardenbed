//! `fuzz_pack_reader` — the pack/idx reader fed random bytes (v0.2.md
//! M17.6), plus the writer→reader roundtrip on streams built from the
//! input.
//!
//! Two contracts:
//!
//! * **Robustness:** any byte string produces `Ok` (structurally valid:
//!   ascending ids, monotonic fanout, matching checksums) or a
//!   descriptive `Err` — never a panic, never a trusted declared length.
//! * **Roundtrip:** objects sliced from the input, written through
//!   `write_pack`/`write_idx` and read back, decode to exactly the
//!   written bytes and ids — the writer/reader pair is an identity on
//!   its own output.
//!
//! Nightly escalation of the in-CI deterministic property tests
//! (truncation, bit-flips, bad magic/version); run with
//! `cargo +nightly fuzz run fuzz_pack_reader`.

#![no_main]

use gardenbed_git::pack::{read_idx, read_pack, write_idx, write_pack, PackObject};
use libfuzzer_sys::fuzz_target;
use sha1::{Digest, Sha1};

fuzz_target!(|data: &[u8]| {
    // --- robustness: arbitrary bytes into the readers.
    if let Err(e) = read_pack(data) {
        assert!(!e.to_string().is_empty(), "read_pack rejected with an empty error");
    }
    if let Err(e) = read_idx(data) {
        assert!(!e.to_string().is_empty(), "read_idx rejected with an empty error");
    }

    // --- roundtrip: derive up to 4 objects from the input (kind cycles
    // through commit/tree/blob/tag; payload = successive input slices)
    // and push them through the writer/reader pair.
    if data.len() < 4 {
        return;
    }
    let n = (data[0] as usize % 4) + 1;
    let mut objects = Vec::with_capacity(n);
    let mut rest = &data[1..];
    for i in 0..n {
        let take = if rest.is_empty() {
            0
        } else {
            (rest[0] as usize).min(rest.len() - 1) + 1
        };
        let payload = &rest[..take];
        rest = &rest[take..];
        let kind = match i % 4 {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 4,
        };
        let mut sha = Sha1::new();
        let header = format!(
            "{} {}\0",
            gardenbed_git::pack::kind_name(kind),
            payload.len()
        );
        sha.update(header.as_bytes());
        sha.update(payload);
        objects.push(PackObject {
            kind,
            data: payload.to_vec(),
            sha1: sha.finalize().into(),
        });
    }

    let mut pack = Vec::new();
    let (pack_sha, entries) = write_pack(&mut pack, &objects).expect("writer never fails on Vec");
    let decoded = read_pack(&pack).expect("own output decodes");
    assert_eq!(decoded.len(), objects.len());
    for (d, o) in decoded.iter().zip(&objects) {
        assert_eq!(d.kind, o.kind);
        assert_eq!(d.data, o.data);
        assert_eq!(d.sha1, o.sha1);
    }
    assert_eq!(&pack[pack.len() - 20..], &pack_sha);

    let mut idx = Vec::new();
    let idx_sha = write_idx(&mut idx, entries, pack_sha).expect("idx writer never fails on Vec");
    let pairs = read_idx(&idx).expect("own idx decodes");
    // ids from the idx must match the pack's objects, sorted.
    let mut expect: Vec<[u8; 20]> = objects.iter().map(|o| o.sha1).collect();
    expect.sort_unstable();
    let got: Vec<[u8; 20]> = pairs.iter().map(|(id, _)| *id).collect();
    assert_eq!(got, expect);
    assert_eq!(&idx[idx.len() - 20..], &idx_sha);
});

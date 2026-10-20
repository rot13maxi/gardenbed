//! Mutation tests (M0 verification #3): truncations, bit flips, and the
//! committed rejection vectors must never panic and never accept
//! non-canonical bytes. Deterministic — runs in ordinary CI; the 1 CPU-hour
//! libfuzzer sweep (fuzz/) is the nightly escalation of the same property.

mod common;

use common::{
    change_add, from_hex, make_cs, make_tree, sample_changeset, sample_tree, small_id, to_hex,
};
use gardenbed_model::{Changeset, Mode, Tree};
use std::path::PathBuf;

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn check_accepts_only_canonical(bytes: &[u8]) {
    // Must not panic; if accepted, re-encode must reproduce the input
    // exactly (the decoder may only accept canonical payloads).
    if let Ok(cs) = Changeset::decode(bytes) {
        let re = cs.canonical_bytes().expect("decoded changeset re-encodes");
        assert_eq!(re, bytes, "accepted non-canonical changeset bytes");
    }
}

fn check_tree_accepts_only_canonical(bytes: &[u8]) {
    if let Ok(t) = Tree::decode(bytes) {
        let re = t.canonical_bytes().expect("decoded tree re-encodes");
        assert_eq!(re, bytes, "accepted non-canonical tree bytes");
    }
}

#[test]
fn truncations_never_panic_and_never_accept_non_canonical() {
    let cs = sample_changeset().canonical_bytes().unwrap();
    for l in 0..cs.len() {
        check_accepts_only_canonical(&cs[..l]);
    }
    let tree = sample_tree().canonical_bytes().unwrap();
    for l in 0..tree.len() {
        check_tree_accepts_only_canonical(&tree[..l]);
    }
}

#[test]
fn bit_flips_never_panic_and_never_accept_non_canonical() {
    let mut state = 0x67617264656e6265u64; // "gardenbe"
    let samples: Vec<Vec<u8>> = vec![
        sample_changeset().canonical_bytes().unwrap(),
        sample_tree().canonical_bytes().unwrap(),
        make_cs(vec![], vec![], "").canonical_bytes().unwrap(),
        make_tree(vec![]).canonical_bytes().unwrap(),
    ];
    for bytes in &samples {
        for _ in 0..2000 {
            let pos = (xorshift(&mut state) as usize) % bytes.len();
            let bit = (xorshift(&mut state) % 8) as u8;
            let mut flipped = bytes.clone();
            flipped[pos] ^= 1 << bit;
            check_accepts_only_canonical(&flipped);
            check_tree_accepts_only_canonical(&flipped);
        }
    }
}

#[test]
fn committed_rejection_vectors_are_refused() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut found = 0;
    for entry in std::fs::read_dir(&dir).expect("fixtures dir (run golden first)") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".hex") || !name.starts_with("invalid-") {
            continue;
        }
        let bytes = from_hex(std::fs::read_to_string(&path).unwrap().trim());
        let refused = if name.contains("tree") {
            Tree::decode(&bytes).is_err()
        } else {
            Changeset::decode(&bytes).is_err()
        };
        assert!(refused, "{name}: decoder accepted bytes it must reject");
        found += 1;
    }
    assert!(
        found >= 8,
        "expected the committed rejection vectors, found {found}"
    );
}

#[test]
fn valid_and_invalid_known_cases() {
    // valid: empty tree round-trips
    let t = make_tree(vec![]);
    assert!(Tree::decode(&t.canonical_bytes().unwrap()).is_ok());

    // invalid: absolute path, `..` component, empty path
    assert!(
        make_cs(
            vec![],
            vec![change_add("/etc/passwd", small_id(1), Mode::Regular)],
            ""
        )
        .canonical_bytes()
        .is_err()
    );
    assert!(
        make_cs(
            vec![],
            vec![change_add("a/../b", small_id(1), Mode::Regular)],
            ""
        )
        .canonical_bytes()
        .is_err()
    );
    assert!(
        make_cs(vec![], vec![change_add("", small_id(1), Mode::Regular)], "")
            .canonical_bytes()
            .is_err()
    );

    // invalid: rename with src_path == path
    assert!(
        make_cs(
            vec![],
            vec![gardenbed_model::Change {
                path: "x".into(),
                op: gardenbed_model::Op::Rename,
                blob: Some(small_id(1)),
                src_path: Some("x".into()),
                mode: Mode::Regular,
            }],
            ""
        )
        .canonical_bytes()
        .is_err()
    );

    // hex helpers agree with Id display
    assert_eq!(to_hex(&small_id(1).as_bytes()[..1]), "01");
}

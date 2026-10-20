//! Golden vectors (M0 verification #1): hand-built objects whose exact
//! canonical hex and SHA-256 Ids are committed under `tests/fixtures/`.
//!
//! First run blesses the fixtures; later runs must match byte-for-byte.
//! The Python cross-check (`tools/py-canonical/verify_fixtures.py`) reads
//! the same files and recomputes everything from the spec document alone.

mod common;

use common::*;
use gardenbed_model::{Changeset, Mode, Tree};
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Twelve hand-built objects (spec §8 requires ≥ 10).
fn samples() -> Vec<(String, Vec<u8>, String)> {
    let mut v: Vec<(String, Vec<u8>, String)> = Vec::new();
    let mut push = |name: &str, bytes: Vec<u8>, id_hex: String| {
        v.push((name.to_owned(), bytes, id_hex));
    };

    // -- changesets -----------------------------------------------------
    let cs = make_cs(vec![], vec![], "");
    push(
        "changeset-root-empty",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    let cs = make_cs(
        vec![],
        vec![change_add("src/main.rs", small_id(1), Mode::Regular)],
        "add main",
    );
    push(
        "changeset-single-add",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    let cs = make_cs(
        vec![small_id(1)],
        vec![change_modify("run.sh", small_id(2), Mode::Exec)],
        "make runnable",
    );
    push(
        "changeset-modify-exec",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    let cs = make_cs(
        vec![small_id(1)],
        vec![change_delete("docs/old.md")],
        "drop stale doc",
    );
    push(
        "changeset-delete",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    let cs = make_cs(
        vec![small_id(1)],
        vec![change_rename(
            "docs/new.md",
            "docs/old.md",
            small_id(3),
            Mode::Regular,
        )],
        "rename doc",
    );
    push(
        "changeset-rename",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    let cs = make_cs(
        vec![small_id(1)],
        vec![change_add("vendor/sub", small_id(4), Mode::Gitlink)],
        "pin submodule",
    );
    push(
        "changeset-gitlink",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    // merge with signature, distinct author/committer/timezones
    let cs = Changeset {
        parents: vec![small_id(1), small_id(2)],
        changes: vec![
            change_modify("src/lib.rs", small_id(3), Mode::Regular),
            change_add("src/bin/bed.rs", small_id(4), Mode::Exec),
            change_delete("docs/old.md"),
            change_rename("docs/new.md", "docs/renamed.md", small_id(5), Mode::Regular),
        ],
        message: "merge: feature branch with a rename".to_owned(),
        author: ident("ada", 60),
        committer: ident("grace", -420),
        signature: Some(vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]),
    };
    push(
        "changeset-merge-signed",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    // unicode paths, message, and negative tz
    let cs = make_cs(
        vec![small_id(1)],
        vec![change_add("données/café.txt", small_id(6), Mode::Regular)],
        "unicode: café ☕ — über",
    );
    let mut cs = cs;
    cs.author = ident("édith", -390);
    cs.committer = ident("björn", 330);
    push(
        "changeset-unicode",
        cs.canonical_bytes().unwrap(),
        cs.id().unwrap().to_hex(),
    );

    // -- trees ----------------------------------------------------------
    let tree = make_tree(vec![]);
    push(
        "tree-empty",
        tree.canonical_bytes().unwrap(),
        tree.id().unwrap().to_hex(),
    );

    let tree = make_tree(vec![
        file_entry("README.md", small_id(10), Mode::Regular),
        file_entry("run.sh", small_id(11), Mode::Exec),
        file_entry("link", small_id(12), Mode::Symlink),
        file_entry("sub", small_id(13), Mode::Gitlink),
    ]);
    push(
        "tree-flat-modes",
        tree.canonical_bytes().unwrap(),
        tree.id().unwrap().to_hex(),
    );

    // interleaved dirs and files; "src" sorts before "src.txt"
    let tree = make_tree(vec![
        file_entry("README.md", small_id(10), Mode::Regular),
        dir_entry("src", small_id(14)),
        file_entry("src.txt", small_id(15), Mode::Regular),
    ]);
    push(
        "tree-nested",
        tree.canonical_bytes().unwrap(),
        tree.id().unwrap().to_hex(),
    );

    let tree = make_tree(vec![
        file_entry("café.txt", small_id(16), Mode::Regular),
        file_entry("über.md", small_id(17), Mode::Regular),
    ]);
    push(
        "tree-unicode",
        tree.canonical_bytes().unwrap(),
        tree.id().unwrap().to_hex(),
    );

    v
}

/// Rejection vectors: raw bytes both implementations must refuse.
fn invalid_samples() -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<(String, Vec<u8>)> = Vec::new();
    let mut push = |name: &str, bytes: Vec<u8>| v.push((name.to_owned(), bytes));

    // parents varint written non-minimally (0x80 0x00 encodes zero)
    push(
        "invalid-changeset-nonminimal-varint",
        vec![0x01, 0x01, 0x80, 0x00],
    );

    // parents varint that never terminates within bounds
    push(
        "invalid-changeset-overlong-varint",
        vec![
            0x01, 0x01, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
        ],
    );

    let root = make_cs(vec![], vec![], "").canonical_bytes().unwrap();
    // cut the final byte (the absent-signature marker)
    push(
        "invalid-changeset-truncated",
        root[..root.len() - 1].to_vec(),
    );
    // trailing garbage after a valid payload
    let mut trailing = root.clone();
    trailing.push(0xff);
    push("invalid-changeset-trailing", trailing);

    // op byte 0x04 is undefined (op lives at index 4 of a single-add payload)
    let add = make_cs(
        vec![],
        vec![change_add("a", small_id(9), Mode::Regular)],
        "",
    )
    .canonical_bytes()
    .unwrap();
    let mut bad_op = add.clone();
    bad_op[4] = 0x04;
    push("invalid-changeset-bad-op", bad_op);

    // two change records swapped: no longer strictly ascending by path
    let two = make_cs(
        vec![],
        vec![
            change_add("a", small_id(1), Mode::Regular),
            change_add("b", small_id(2), Mode::Regular),
        ],
        "",
    )
    .canonical_bytes()
    .unwrap();
    let rec_len = (two.len() - 4) / 2;
    let (rec_a, rec_b) = (&two[4..4 + rec_len], &two[4 + rec_len..]);
    let mut unsorted = two[..4].to_vec();
    unsorted.extend_from_slice(rec_b);
    unsorted.extend_from_slice(rec_a);
    push("invalid-changeset-unsorted", unsorted);

    // same path twice
    let mut dup = two[..3].to_vec();
    dup.push(0x02); // changes count = 2
    dup.extend_from_slice(&two[4..]);
    dup.extend_from_slice(&two[4..]);
    push("invalid-changeset-duplicate-path", dup);

    // delete carrying a blob: hand-assembled (encode refuses to build it)
    let mut v2: Vec<u8> = vec![0x01, 0x01, 0x00, 0x01, 0x02, 0x01, b'a', 0x01];
    v2.extend_from_slice(&[0u8; 32]); // blob the delete must not have
    v2.extend_from_slice(&[0x00, 0x00]); // src absent, mode regular
    v2.extend_from_slice(&[0x00]); // empty message
    v2.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // author name/email/secs/tz
    v2.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // committer
    v2.extend_from_slice(&[0x00]); // absent signature
    push("invalid-changeset-delete-with-blob", v2);

    // tree kind byte 0x02 is undefined
    push(
        "invalid-tree-bad-kind",
        vec![0x02, 0x01, 0x01, 0x01, b'a', 0x02],
    );

    // same entry name twice (hand-assembled)
    let mut dup_tree: Vec<u8> = vec![0x02, 0x01, 0x02, 0x01, b'a', 0x00, 0x00];
    dup_tree.extend_from_slice(&[0u8; 32]);
    dup_tree.extend_from_slice(&[0x01, b'a', 0x00, 0x00]);
    dup_tree.extend_from_slice(&[1u8; 32]);
    push("invalid-tree-duplicate-entry", dup_tree);

    // two file entries swapped: not strictly ascending by name
    let ok_tree = make_tree(vec![
        file_entry("a", small_id(1), Mode::Regular),
        file_entry("b", small_id(2), Mode::Regular),
    ])
    .canonical_bytes()
    .unwrap();
    let trec_len = (ok_tree.len() - 3) / 2; // header is tag, ver, count
    let (ta, tb) = (&ok_tree[3..3 + trec_len], &ok_tree[3 + trec_len..]);
    let mut unsorted_tree = ok_tree[..3].to_vec();
    unsorted_tree.extend_from_slice(tb);
    unsorted_tree.extend_from_slice(ta);
    push("invalid-tree-unsorted", unsorted_tree);

    v
}

#[test]
fn golden_vectors() {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();

    for (name, bytes, id_hex) in samples() {
        let hex = to_hex(&bytes);
        let hp = dir.join(format!("{name}.hex"));
        let ip = dir.join(format!("{name}.id"));
        if !hp.exists() {
            std::fs::write(&hp, &hex).unwrap();
            std::fs::write(&ip, &id_hex).unwrap();
            eprintln!("blessed {name}");
            continue;
        }
        assert_eq!(
            std::fs::read_to_string(&hp).unwrap(),
            hex,
            "{name}: canonical bytes drifted from the committed fixture"
        );
        assert_eq!(
            std::fs::read_to_string(&ip).unwrap(),
            id_hex,
            "{name}: id drifted from the committed fixture"
        );

        // decode → re-encode → id must reproduce the fixture exactly
        if name.starts_with("changeset-") {
            let cs = Changeset::decode(&bytes).unwrap();
            assert_eq!(cs.canonical_bytes().unwrap(), bytes);
            assert_eq!(cs.id().unwrap().to_hex(), id_hex);
        } else {
            let t = Tree::decode(&bytes).unwrap();
            assert_eq!(t.canonical_bytes().unwrap(), bytes);
            assert_eq!(t.id().unwrap().to_hex(), id_hex);
        }
    }
}

#[test]
fn rejection_vectors() {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).unwrap();

    for (name, bytes) in invalid_samples() {
        let hp = dir.join(format!("{name}.hex"));
        if !hp.exists() {
            std::fs::write(&hp, to_hex(&bytes)).unwrap();
            eprintln!("blessed {name}");
        } else {
            assert_eq!(
                std::fs::read_to_string(&hp).unwrap(),
                to_hex(&bytes),
                "{name}: rejection vector drifted"
            );
        }

        let refused = if name.contains("tree") {
            Tree::decode(&bytes).is_err()
        } else {
            Changeset::decode(&bytes).is_err()
        };
        assert!(refused, "{name}: decoder accepted bytes it must reject");
    }
}

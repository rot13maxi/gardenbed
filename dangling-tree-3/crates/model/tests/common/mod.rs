//! Shared builders for model tests (golden vectors, mutations).

#![allow(dead_code)]

use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, Tree, TreeEntry, TreeKind};

pub const ZERO: &str = "0000000000000000000000000000000000000000000000000000000000000000";
pub const ONE: &str = "0000000000000000000000000000000000000000000000000000000000000001";
pub const TWO: &str = "0000000000000000000000000000000000000000000000000000000000000002";

/// Id whose first byte is `n` — readable in hex dumps.
pub fn small_id(n: u8) -> Id {
    let mut b = [0u8; 32];
    b[0] = n;
    Id::from_bytes(b)
}

pub fn id_from_hex(s: &str) -> Id {
    s.parse().expect("valid id hex")
}

pub fn ident(name: &str, tz_min: i32) -> Identity {
    Identity {
        name: name.to_owned(),
        email: format!("{name}@example.com"),
        time_secs: 1_700_000_000,
        tz_min,
    }
}

pub fn change_add(path: &str, blob: Id, mode: Mode) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Add,
        blob: Some(blob),
        src_path: None,
        mode,
    }
}

pub fn change_modify(path: &str, blob: Id, mode: Mode) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Modify,
        blob: Some(blob),
        src_path: None,
        mode,
    }
}

pub fn change_delete(path: &str) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Delete,
        blob: None,
        src_path: None,
        mode: Mode::Regular,
    }
}

pub fn change_rename(path: &str, src_path: &str, blob: Id, mode: Mode) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Rename,
        blob: Some(blob),
        src_path: Some(src_path.to_owned()),
        mode,
    }
}

pub fn make_cs(parents: Vec<Id>, changes: Vec<Change>, message: &str) -> Changeset {
    Changeset {
        parents,
        changes,
        message: message.to_owned(),
        author: ident("ada", 0),
        committer: ident("ada", 0),
        signature: None,
    }
}

pub fn file_entry(name: &str, blob: Id, mode: Mode) -> TreeEntry {
    TreeEntry {
        name: name.to_owned(),
        kind: TreeKind::File,
        mode,
        blob: Some(blob),
        tree: None,
    }
}

pub fn dir_entry(name: &str, tree: Id) -> TreeEntry {
    TreeEntry {
        name: name.to_owned(),
        kind: TreeKind::Dir,
        mode: Mode::Regular,
        blob: None,
        tree: Some(tree),
    }
}

pub fn make_tree(entries: Vec<TreeEntry>) -> Tree {
    Tree { entries }
}

/// A nontrivial changeset: merge with rename, exec bit, delete, signature,
/// distinct author/committer/tz — exercises most of the encoding surface.
pub fn sample_changeset() -> Changeset {
    Changeset {
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
    }
}

pub fn sample_tree() -> Tree {
    make_tree(vec![
        file_entry("README.md", small_id(10), Mode::Regular),
        file_entry("run.sh", small_id(11), Mode::Exec),
        dir_entry("src", small_id(12)),
        file_entry("link", small_id(13), Mode::Symlink),
    ])
}

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn from_hex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0, "odd hex length");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("valid hex"))
        .collect()
}

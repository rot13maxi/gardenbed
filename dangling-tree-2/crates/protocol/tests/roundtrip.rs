//! Proto ↔ model conversion roundtrips.
//!
//! Every fixture here is a hand-built canonical model value (a changeset or
//! tree that passes the codec's semantic rules and therefore has an id).
//! The roundtrip contract, per v0.1 decision D4 ("the wire shapes mirror the
//! canonical model exactly"):
//!
//! * model → wire → model reproduces the original value,
//! * the canonical bytes (and therefore `id()`) survive unchanged — the wire
//!   may carry the sender's order, the canonical bytes are what define
//!   identity,
//! * prost's own encode/decode roundtrip is byte-stable.

use gardenbed_model as model;
use gardenbed_model::codec::CodecError;
use gardenbed_protocol::ConversionError;
use gardenbed_protocol::pb;
use gardenbed_protocol::proto as pb2; // alias check: `proto` and `pb` are the same module
use prost::Message as _;
use std::str::FromStr;

// ------------------------------------------------------------- fixtures

fn ident(name: &str) -> model::Identity {
    model::Identity {
        name: name.to_owned(),
        email: format!("{name}@example.com"),
        time_secs: 1_700_000_123,
        tz_min: 60,
    }
}

/// An upstream git SHA-1, zero-padded to 32 bytes the way gitlinks carry it
/// (docs/canonical-encoding.md §3).
fn git_sha1_padded() -> model::Id {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(&[
        0x0d, 0xf0, 0x0d, 0xfe, 0xed, 0xbe, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
        0x42, 0x24, 0x24, 0x42, 0x42,
    ]);
    model::Id::from_bytes(bytes)
}

/// A changeset covering all four ops with deliberately unsorted changes:
/// the wire preserves the sender's order, the canonical bytes are sorted by
/// the codec, so ids must still match.
fn kitchen_sink_cs() -> model::Changeset {
    model::Changeset {
        parents: vec![model::blob_id(b"parent")],
        changes: vec![
            model::Change {
                path: "src/lib.rs".into(),
                op: model::Op::Add,
                blob: Some(model::blob_id(b"lib source")),
                src_path: None,
                mode: model::Mode::Regular,
            },
            model::Change {
                path: "old.txt".into(),
                op: model::Op::Delete,
                blob: None,
                src_path: None,
                mode: model::Mode::Regular,
            },
            model::Change {
                path: "docs/guide.md".into(),
                op: model::Op::Rename,
                blob: Some(model::blob_id(b"guide body")),
                src_path: Some("doc/guide.md".into()),
                mode: model::Mode::Regular,
            },
            model::Change {
                path: "README.md".into(),
                op: model::Op::Modify,
                blob: Some(model::blob_id(b"# gardenbed")),
                src_path: None,
                mode: model::Mode::Regular,
            },
        ],
        message: "rearrange the garden".into(),
        author: ident("ada"),
        committer: ident("grace"),
        signature: None,
    }
}

// ------------------------------------------------------------- helpers

fn assert_changeset_roundtrips(cs: &model::Changeset) {
    let canonical = cs
        .canonical_bytes()
        .expect("fixture must be canonically valid");
    let id = cs.id().expect("fixture must have an id");

    let wire = pb::ChangesetMsg::try_from(cs).expect("model → wire");
    // prost wire roundtrip is byte-stable.
    let encoded = wire.encode_to_vec();
    let decoded = pb::ChangesetMsg::decode(encoded.as_slice()).expect("prost decode");
    assert_eq!(decoded, wire, "prost encode/decode must be lossless");

    let back = model::Changeset::try_from(decoded).expect("wire → model");
    assert_eq!(&back, cs, "model survives the wire unchanged");
    assert_eq!(
        back.canonical_bytes().unwrap(),
        canonical,
        "canonical bytes survive the wire"
    );
    assert_eq!(back.id().unwrap(), id, "changeset id survives the wire");
}

fn assert_tree_roundtrips(tree: &model::Tree) {
    let canonical = tree
        .canonical_bytes()
        .expect("fixture must be canonically valid");
    let id = tree.id().expect("fixture must have an id");

    let wire = pb::TreeResp::try_from(tree).expect("model → wire");
    let encoded = wire.encode_to_vec();
    let decoded = pb::TreeResp::decode(encoded.as_slice()).expect("prost decode");
    assert_eq!(decoded, wire);

    let back = model::Tree::try_from(decoded).expect("wire → model");
    assert_eq!(&back, tree);
    assert_eq!(back.canonical_bytes().unwrap(), canonical);
    assert_eq!(back.id().unwrap(), id);
}

// ------------------------------------------------------------- changesets

#[test]
fn add_modify_delete_rename_roundtrip() {
    assert_changeset_roundtrips(&kitchen_sink_cs());
}

#[test]
fn wire_preserves_sender_order_but_canonical_bytes_still_match() {
    let cs = kitchen_sink_cs();
    let sorted_view = {
        let mut sorted = cs.clone();
        sorted
            .changes
            .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        sorted
    };
    assert_ne!(cs.changes, sorted_view.changes, "fixture must be unsorted");

    let wire = pb::ChangesetMsg::try_from(&cs).unwrap();
    let wire_names: Vec<&str> = wire.changes.iter().map(|c| c.path.as_str()).collect();
    let model_names: Vec<&str> = cs.changes.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        wire_names, model_names,
        "the wire preserves the sender's order"
    );

    // Ids are defined by the canonical (sorted) bytes, not the wire order.
    assert_eq!(cs.id().unwrap(), sorted_view.id().unwrap());
    assert_eq!(
        cs.canonical_bytes().unwrap(),
        sorted_view.canonical_bytes().unwrap()
    );
}

#[test]
fn modes_roundtrip() {
    let cs = model::Changeset {
        parents: vec![model::blob_id(b"p")],
        changes: vec![
            // Submodule pointer: git SHA-1 padded to 32 bytes.
            model::Change {
                path: "vendor/stdlib".into(),
                op: model::Op::Add,
                blob: Some(git_sha1_padded()),
                src_path: None,
                mode: model::Mode::Gitlink,
            },
            // Symlink: the blob carries the target path as content.
            model::Change {
                path: "bin/latest".into(),
                op: model::Op::Add,
                blob: Some(model::blob_id(b"/opt/gardenbed/bin/gardenbed")),
                src_path: None,
                mode: model::Mode::Symlink,
            },
            // Exec bit.
            model::Change {
                path: "scripts/sow.sh".into(),
                op: model::Op::Modify,
                blob: Some(model::blob_id(b"#!/bin/sh")),
                src_path: None,
                mode: model::Mode::Exec,
            },
        ],
        message: "modes".into(),
        author: ident("ada"),
        committer: ident("ada"),
        signature: None,
    };
    assert_changeset_roundtrips(&cs);

    // The modes survived as the same numeric discriminants.
    let wire = pb::ChangesetMsg::try_from(&cs).unwrap();
    assert_eq!(wire.changes[0].mode, pb::Mode::Gitlink as i32);
    assert_eq!(wire.changes[1].mode, pb::Mode::Symlink as i32);
    assert_eq!(wire.changes[2].mode, pb::Mode::Exec as i32);
}

#[test]
fn unicode_paths_roundtrip() {
    let cs = model::Changeset {
        parents: vec![model::blob_id(b"p")],
        changes: vec![
            model::Change {
                path: "文献/目录.md".into(),
                op: model::Op::Add,
                blob: Some(model::blob_id("内容".as_bytes())),
                src_path: None,
                mode: model::Mode::Regular,
            },
            model::Change {
                path: "emoji/🌱/notes.txt".into(),
                op: model::Op::Rename,
                blob: Some(model::blob_id(b"notes")),
                src_path: Some("emoji/🌿/notes.txt".into()),
                mode: model::Mode::Regular,
            },
        ],
        message: "ünïcødé 🌻".into(),
        author: ident("üñíçødé"),
        committer: ident("γράφων"),
        signature: None,
    };
    assert_changeset_roundtrips(&cs);
}

#[test]
fn signed_changeset_roundtrip() {
    let mut cs = kitchen_sink_cs();
    cs.signature =
        Some(b"-----BEGIN SSH SIGNATURE-----\nfake\n-----END SSH SIGNATURE-----".to_vec());
    assert_changeset_roundtrips(&cs);
    let wire = pb::ChangesetMsg::try_from(&cs).unwrap();
    assert_eq!(
        wire.signature, cs.signature,
        "signature rides on the wire verbatim"
    );
}

#[test]
fn multi_parent_changeset_roundtrip() {
    let cs = model::Changeset {
        parents: vec![
            model::blob_id(b"first parent"),
            model::blob_id(b"second parent"),
        ],
        changes: vec![model::Change {
            path: "merge.txt".into(),
            op: model::Op::Add,
            blob: Some(model::blob_id(b"merged")),
            src_path: None,
            mode: model::Mode::Regular,
        }],
        message: "octopus".into(),
        author: ident("ada"),
        committer: ident("grace"),
        signature: None,
    };
    assert_changeset_roundtrips(&cs);
    let wire = pb::ChangesetMsg::try_from(&cs).unwrap();
    assert_eq!(wire.parents.len(), 2);
    assert_eq!(
        gardenbed_protocol::convert::id_from_bytes(&wire.parents[1]).unwrap(),
        model::blob_id(b"second parent")
    );
}

#[test]
fn empty_root_changeset_roundtrip() {
    let cs = model::Changeset {
        parents: vec![],
        changes: vec![],
        message: "the garden is empty".into(),
        author: ident("ada"),
        committer: ident("ada"),
        signature: None,
    };
    assert_changeset_roundtrips(&cs);
    // Zero values do not ride on proto3: an empty changeset encodes to a
    // small payload with no author/committer bytes lost (they are messages,
    // so presence is explicit).
    let wire = pb::ChangesetMsg::try_from(&cs).unwrap();
    assert!(wire.author.is_some() && wire.committer.is_some());
}

// ------------------------------------------------------------------ trees

fn kitchen_sink_tree() -> model::Tree {
    model::Tree {
        entries: vec![
            model::TreeEntry {
                name: "README.md".into(),
                kind: model::TreeKind::File,
                mode: model::Mode::Regular,
                blob: Some(model::blob_id(b"# gardenbed")),
                tree: None,
            },
            model::TreeEntry {
                name: "src".into(),
                kind: model::TreeKind::Dir,
                mode: model::Mode::Regular,
                blob: None,
                tree: Some(model::blob_id(b"subtree")),
            },
            model::TreeEntry {
                name: "sow.sh".into(),
                kind: model::TreeKind::File,
                mode: model::Mode::Exec,
                blob: Some(model::blob_id(b"#!/bin/sh")),
                tree: None,
            },
            model::TreeEntry {
                name: "link".into(),
                kind: model::TreeKind::File,
                mode: model::Mode::Symlink,
                blob: Some(model::blob_id(b"target")),
                tree: None,
            },
            model::TreeEntry {
                name: "vendor".into(),
                kind: model::TreeKind::Dir,
                mode: model::Mode::Regular,
                blob: None,
                tree: Some(model::blob_id(b"vendored")),
            },
        ],
    }
}

#[test]
fn tree_roundtrip_files_dirs_and_modes() {
    assert_tree_roundtrips(&kitchen_sink_tree());
    let wire = pb::TreeResp::try_from(&kitchen_sink_tree()).unwrap();
    // The first entry is a file leaf (blob_id), the second a subtree.
    assert!(matches!(
        wire.entries[0].id,
        Some(pb::tree_entry::Id::BlobId(_))
    ));
    assert!(matches!(
        wire.entries[1].id,
        Some(pb::tree_entry::Id::TreeId(_))
    ));
}

#[test]
fn gitlink_tree_entry_roundtrip() {
    let tree = model::Tree {
        entries: vec![model::TreeEntry {
            name: "stdlib".into(),
            kind: model::TreeKind::File,
            mode: model::Mode::Gitlink,
            blob: Some(git_sha1_padded()),
            tree: None,
        }],
    };
    assert_tree_roundtrips(&tree);
}

#[test]
fn unicode_tree_names_roundtrip() {
    let tree = model::Tree {
        entries: vec![
            model::TreeEntry {
                name: "文献".into(),
                kind: model::TreeKind::Dir,
                mode: model::Mode::Regular,
                blob: None,
                tree: Some(model::blob_id(b"docs subtree")),
            },
            model::TreeEntry {
                name: "🌱.md".into(),
                kind: model::TreeKind::File,
                mode: model::Mode::Regular,
                blob: Some(model::blob_id(b"seed notes")),
                tree: None,
            },
        ],
    };
    assert_tree_roundtrips(&tree);
}

#[test]
fn wire_preserves_tree_entry_order() {
    let tree = kitchen_sink_tree(); // deliberately not ascending by name
    let wire = pb::TreeResp::try_from(&tree).unwrap();
    let wire_names: Vec<&str> = wire.entries.iter().map(|e| e.name.as_str()).collect();
    let model_names: Vec<&str> = tree.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(wire_names, model_names);
    // Canonical bytes are sorted by the codec regardless of entry order.
    let mut sorted = tree.clone();
    sorted
        .entries
        .sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    assert_eq!(
        tree.canonical_bytes().unwrap(),
        sorted.canonical_bytes().unwrap()
    );
    assert_eq!(tree.id().unwrap(), sorted.id().unwrap());
}

// -------------------------------------------------------------- negatives

fn minimal_valid_wire_changeset() -> pb::ChangesetMsg {
    pb::ChangesetMsg::try_from(&kitchen_sink_cs()).unwrap()
}

#[test]
fn short_parent_id_rejected() {
    let mut wire = minimal_valid_wire_changeset();
    wire.parents[0] = vec![0u8; 31];
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::IdLength(31))
    ));
}

#[test]
fn unknown_op_and_mode_discriminants_rejected() {
    // Proto3 enums are open: the wire may carry any i32.
    let mut wire = minimal_valid_wire_changeset();
    wire.changes[0].op = 9;
    assert!(matches!(
        model::Changeset::try_from(wire.clone()),
        Err(ConversionError::Op(9))
    ));

    let mut wire = minimal_valid_wire_changeset();
    wire.changes[0].mode = -1;
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Mode(-1))
    ));
}

#[test]
fn missing_author_or_committer_rejected() {
    let mut wire = minimal_valid_wire_changeset();
    wire.author = None;
    assert!(matches!(
        model::Changeset::try_from(wire.clone()),
        Err(ConversionError::Missing("ChangesetMsg.author"))
    ));

    let mut wire = minimal_valid_wire_changeset();
    wire.committer = None;
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Missing("ChangesetMsg.committer"))
    ));
}

#[test]
fn wire_changeset_semantic_violations_rejected() {
    // Path escaping the repo root.
    let mut wire = minimal_valid_wire_changeset();
    wire.changes[0].path = "../escape".into();
    let err = model::Changeset::try_from(wire).unwrap_err();
    assert!(matches!(
        err,
        ConversionError::Invalid(CodecError::Encode(_))
    ));

    // Add without a blob.
    let mut wire = minimal_valid_wire_changeset();
    wire.changes[0].blob_id = None;
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Invalid(_))
    ));

    // Delete carrying a blob.
    let mut wire = minimal_valid_wire_changeset();
    let del = wire
        .changes
        .iter_mut()
        .find(|c| c.op == pb::Op::Delete as i32)
        .unwrap();
    del.blob_id = Some(vec![0u8; 32]);
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Invalid(_))
    ));

    // Delete with a non-regular mode.
    let mut wire = minimal_valid_wire_changeset();
    let del = wire
        .changes
        .iter_mut()
        .find(|c| c.op == pb::Op::Delete as i32)
        .unwrap();
    del.mode = pb::Mode::Exec as i32;
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Invalid(_))
    ));

    // Rename without src_path.
    let mut wire = minimal_valid_wire_changeset();
    let ren = wire
        .changes
        .iter_mut()
        .find(|c| c.op == pb::Op::Rename as i32)
        .unwrap();
    ren.src_path = None;
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Invalid(_))
    ));

    // Duplicate paths.
    let mut wire = minimal_valid_wire_changeset();
    let first = wire.changes[0].clone();
    wire.changes.push(first);
    assert!(matches!(
        model::Changeset::try_from(wire),
        Err(ConversionError::Invalid(_))
    ));
}

#[test]
fn model_to_wire_rejects_invalid_values() {
    // Absolute path has no canonical form, hence no wire form.
    let bad = model::Changeset {
        parents: vec![],
        changes: vec![model::Change {
            path: "/etc/passwd".into(),
            op: model::Op::Add,
            blob: Some(model::blob_id(b"x")),
            src_path: None,
            mode: model::Mode::Regular,
        }],
        message: "nope".into(),
        author: ident("a"),
        committer: ident("a"),
        signature: None,
    };
    assert!(matches!(
        pb::ChangesetMsg::try_from(&bad),
        Err(ConversionError::Invalid(_))
    ));
}

#[test]
fn wire_tree_violations_rejected() {
    let tree = kitchen_sink_tree();
    let mut wire = pb::TreeResp::try_from(&tree).unwrap();

    // Entry with neither blob_id nor tree_id.
    wire.entries[0].id = None;
    assert!(matches!(
        model::Tree::try_from(wire.clone()),
        Err(ConversionError::TreeEntry {
            problem: "carries neither blob_id nor tree_id",
            ..
        })
    ));

    // Entry name with a slash.
    let mut wire = pb::TreeResp::try_from(&tree).unwrap();
    wire.entries[1].name = "has/slash".into();
    assert!(matches!(
        model::Tree::try_from(wire.clone()),
        Err(ConversionError::TreeEntry { name, .. }) if name == "has/slash"
    ));

    // Dir entry must carry mode regular.
    let mut wire = pb::TreeResp::try_from(&tree).unwrap();
    wire.entries[1].mode = pb::Mode::Exec as i32;
    assert!(matches!(
        model::Tree::try_from(wire.clone()),
        Err(ConversionError::Invalid(_))
    ));

    // Wrong id length in a tree entry.
    let mut wire = pb::TreeResp::try_from(&tree).unwrap();
    wire.entries[1].id = Some(pb::tree_entry::Id::TreeId(vec![1u8; 7]));
    assert!(matches!(
        model::Tree::try_from(wire),
        Err(ConversionError::IdLength(7))
    ));
}

#[test]
fn model_to_wire_rejects_mirrored_tree_entries() {
    // A file entry carrying a tree id cannot be mirrored onto the oneof.
    let entry = model::TreeEntry {
        name: "confused".into(),
        kind: model::TreeKind::File,
        mode: model::Mode::Regular,
        blob: Some(model::blob_id(b"a")),
        tree: Some(model::blob_id(b"b")),
    };
    assert!(matches!(
        pb::TreeEntry::try_from(&entry),
        Err(ConversionError::TreeEntry { name, .. }) if name == "confused"
    ));

    // A dir entry carrying a blob id likewise.
    let entry = model::TreeEntry {
        name: "also".into(),
        kind: model::TreeKind::Dir,
        mode: model::Mode::Regular,
        blob: Some(model::blob_id(b"a")),
        tree: Some(model::blob_id(b"b")),
    };
    assert!(pb::TreeEntry::try_from(&entry).is_err());

    // Dir entries must carry mode regular (codec rule — enforced at tree
    // level, since a lone entry has no canonical form of its own).
    let entry = model::TreeEntry {
        name: "dir".into(),
        kind: model::TreeKind::Dir,
        mode: model::Mode::Symlink,
        blob: None,
        tree: Some(model::blob_id(b"t")),
    };
    let tree = model::Tree {
        entries: vec![entry],
    };
    assert!(matches!(
        pb::TreeResp::try_from(&tree),
        Err(ConversionError::Invalid(_))
    ));
}

// ------------------------------------------------------------ id helpers

#[test]
fn blob_map_keys_are_hex_ids() {
    let id = model::blob_id(b"inline blob content");
    let key = gardenbed_protocol::convert::id_to_blob_key(id);
    assert_eq!(key.len(), 64);
    assert!(
        key.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(
        gardenbed_protocol::convert::blob_key_to_id(&key).unwrap(),
        id
    );
    // Parsers accept uppercase hex, never emit it.
    let upper: String = key.to_uppercase();
    assert_eq!(
        gardenbed_protocol::convert::blob_key_to_id(&upper).unwrap(),
        id
    );
    // Garbage keys are rejected.
    assert!(matches!(
        gardenbed_protocol::convert::blob_key_to_id("nothex"),
        Err(ConversionError::BlobKeyHex(_))
    ));
    assert!(matches!(
        gardenbed_protocol::convert::blob_key_to_id(&"z".repeat(64)),
        Err(ConversionError::BlobKeyHex(_))
    ));
}

// -------------------------------------------------------------- identity

#[test]
fn identity_conversion_is_infallible_both_ways() {
    let id = model::Identity {
        name: "ada lovelace".into(),
        email: "ada@example.com".into(),
        time_secs: 0,
        tz_min: -330, // negative offsets survive via sint32 zigzag
    };
    let wire = pb::IdentityMsg::from(&id);
    assert_eq!(wire.tz_min, -330);
    let back = model::Identity::from(wire);
    assert_eq!(back, id);
    // Ownership variant.
    let back2 = model::Identity::from(pb::IdentityMsg::from(&id));
    assert_eq!(back2, id);
}

#[test]
fn tz_offsets_zigzag_on_the_wire() {
    // sint32: +60 min encodes to 120, -330 min to 659 on the raw wire.
    let mut buf = Vec::new();
    pb::IdentityMsg::from(&model::Identity {
        name: "a".into(),
        email: "a".into(),
        time_secs: 0,
        tz_min: 60,
    })
    .encode(&mut buf)
    .unwrap();
    assert!(
        buf.windows(2).any(|w| w == [0x20, 0x78]),
        "tz_min=+60 zigzags to varint 120"
    );
}

// --------------------------------------------------------- alias sanity

#[test]
fn pb_and_proto_are_the_same_namespace() {
    let msg = pb2::BlobChunk {
        data: b"xyz".to_vec(),
    };
    assert_eq!(
        msg,
        pb::BlobChunk {
            data: b"xyz".to_vec()
        }
    );
    assert_eq!(
        model::blob_id(b"seed"),
        model::Id::from_str(&model::blob_id(b"seed").to_hex()).unwrap()
    );
}

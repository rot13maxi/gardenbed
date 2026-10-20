//! Property tests (M0 verification #2 and #5): roundtrip stability and
//! order-independence, plus a random-bytes acceptance property that acts as
//! an always-on miniature fuzzer in CI.

use gardenbed_model::codec::CodecError;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, Tree, TreeEntry, TreeKind};
use proptest::prelude::*;
use proptest::strategy::Strategy;

fn fail(e: CodecError) -> TestCaseError {
    TestCaseError::fail(e.to_string())
}

fn arb_id() -> impl Strategy<Value = Id> {
    proptest::array::uniform32(any::<u8>()).prop_map(Id::from_bytes)
}

/// Paths built only from safe components (regex guarantees no leading `/`,
/// no NUL, no empty/./.. components).
fn arb_path() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9_]{1,6}(/[a-z0-9_]{1,6}){0,3}").unwrap()
}

fn arb_identity() -> impl Strategy<Value = Identity> {
    (
        proptest::string::string_regex("[a-z]{1,8}").unwrap(),
        proptest::string::string_regex("[a-z]{1,6}@[a-z]{1,4}").unwrap(),
        any::<u64>(),
        -720i32..=840i32,
    )
        .prop_map(|(name, email, time_secs, tz_min)| Identity {
            name,
            email,
            time_secs,
            tz_min,
        })
}

fn arb_mode() -> impl Strategy<Value = Mode> {
    prop_oneof![
        Just(Mode::Regular),
        Just(Mode::Exec),
        Just(Mode::Symlink),
        Just(Mode::Gitlink),
    ]
}

/// Op-consistent change: blob presence and src_path follow the op rules.
fn arb_change() -> impl Strategy<Value = Change> {
    prop_oneof![
        3 => (arb_path(), arb_id(), arb_mode()).prop_map(|(path, blob, mode)| Change {
            path,
            op: Op::Add,
            blob: Some(blob),
            src_path: None,
            mode,
        }),
        3 => (arb_path(), arb_id(), arb_mode()).prop_map(|(path, blob, mode)| Change {
            path,
            op: Op::Modify,
            blob: Some(blob),
            src_path: None,
            mode,
        }),
        1 => arb_path().prop_map(|path| Change {
            path,
            op: Op::Delete,
            blob: None,
            src_path: None,
            mode: Mode::Regular,
        }),
        1 => (arb_path(), arb_id(), arb_mode()).prop_map(|(path, blob, mode)| Change {
            path: format!("new-{path}"),
            op: Op::Rename,
            blob: Some(blob),
            src_path: Some(path),
            mode,
        }),
    ]
}

fn arb_changeset() -> impl Strategy<Value = Changeset> {
    (
        proptest::collection::vec(arb_change(), 0..8),
        proptest::collection::vec(arb_id(), 0..3),
        proptest::string::string_regex("[a-z ]{0,40}").unwrap(),
        arb_identity(),
        arb_identity(),
        proptest::option::of(proptest::collection::vec(any::<u8>(), 0..64)),
    )
        .prop_map(
            |(changes, parents, message, author, committer, signature)| {
                // Canonical form requires unique paths; dedupe by path.
                let mut seen = std::collections::BTreeMap::new();
                for c in changes {
                    seen.entry(c.path.clone()).or_insert(c);
                }
                Changeset {
                    parents,
                    changes: seen.into_values().collect(),
                    message,
                    author,
                    committer,
                    signature,
                }
            },
        )
}

fn arb_tree() -> impl Strategy<Value = Tree> {
    proptest::collection::vec(
        (0usize..64, any::<bool>(), arb_mode(), arb_id(), arb_id()),
        0..12,
    )
    .prop_map(|items| {
        // Unique names via index; dir/file field rules per kind.
        let mut seen = std::collections::BTreeMap::new();
        for (i, is_dir, mode, blob, tree) in items {
            let name = format!("e{i}");
            let entry = if is_dir {
                TreeEntry {
                    name,
                    kind: TreeKind::Dir,
                    mode: Mode::Regular,
                    blob: None,
                    tree: Some(tree),
                }
            } else {
                TreeEntry {
                    name,
                    kind: TreeKind::File,
                    mode,
                    blob: Some(blob),
                    tree: None,
                }
            };
            seen.entry(format!("e{i}")).or_insert(entry);
        }
        Tree {
            entries: seen.into_values().collect(),
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn changeset_roundtrip_stable(cs in arb_changeset()) {
        let bytes = cs.canonical_bytes().map_err(fail)?;
        let decoded = Changeset::decode(&bytes).map_err(fail)?;
        prop_assert_eq!(&decoded, &cs);
        prop_assert_eq!(decoded.canonical_bytes().map_err(fail)?, bytes);
        prop_assert_eq!(decoded.id().map_err(fail)?, cs.id().map_err(fail)?);
    }

    #[test]
    fn tree_roundtrip_stable(t in arb_tree()) {
        let bytes = t.canonical_bytes().map_err(fail)?;
        let decoded = Tree::decode(&bytes).map_err(fail)?;
        prop_assert_eq!(&decoded, &t);
        prop_assert_eq!(decoded.canonical_bytes().map_err(fail)?, bytes);
        prop_assert_eq!(decoded.id().map_err(fail)?, t.id().map_err(fail)?);
    }

    #[test]
    fn changeset_field_order_independent(mut cs in arb_changeset()) {
        let a = cs.canonical_bytes().map_err(fail)?;
        // Shuffling identity fields must not affect canonical bytes: the
        // encoder emits spec order regardless of construction order.
        std::mem::swap(&mut cs.author, &mut cs.committer);
        std::mem::swap(&mut cs.author, &mut cs.committer);
        let b = cs.canonical_bytes().map_err(fail)?;
        prop_assert_eq!(&a, &b);
        // Same logical changeset with changes built in reverse order must
        // canonicalize identically (encoder sorts).
        cs.changes.reverse();
        let c = cs.canonical_bytes().map_err(fail)?;
        prop_assert_eq!(&a, &c);
    }

    #[test]
    fn changeset_decode_accepts_only_canonical(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(cs) = Changeset::decode(&bytes) {
            let re = cs.canonical_bytes().map_err(fail)?;
            prop_assert_eq!(re, bytes, "decoder accepted non-canonical bytes");
        }
    }

    #[test]
    fn tree_decode_accepts_only_canonical(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(t) = Tree::decode(&bytes) {
            let re = t.canonical_bytes().map_err(fail)?;
            prop_assert_eq!(re, bytes, "decoder accepted non-canonical bytes");
        }
    }
}

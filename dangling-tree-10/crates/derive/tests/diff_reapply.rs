//! Regression tests for the fuzz-found diff re-apply gap (M8): a diff
//! between two valid trees, replayed onto the older side as a child
//! changeset, must derive exactly the newer side's tree. File↔directory
//! swaps are the hard shape — canonical path order sorts the write of
//! `g` (which swallows the directory) before the delete of `g/h`.
//!
//! Each case pins the diff's exact output (git's `--no-renames` shape:
//! a Delete for everything only in `a`, an Add/Modify for everything
//! only in or changed in `b`) and then replays it.

use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::{MemoryStore, MetadataStore as _};

fn ident() -> Identity {
    Identity {
        name: "Diff Test".into(),
        email: "diff@bed.example".into(),
        time_secs: 1_700_000_000,
        tz_min: 0,
    }
}

fn add(path: &str, mode: Mode, content: &[u8]) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Add,
        blob: Some(blob_id(content)),
        src_path: None,
        mode,
    }
}

fn modify(path: &str, mode: Mode, content: &[u8]) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Modify,
        blob: Some(blob_id(content)),
        src_path: None,
        mode,
    }
}

fn delete(path: &str) -> Change {
    Change {
        path: path.to_owned(),
        op: Op::Delete,
        blob: None,
        src_path: None,
        mode: Mode::Regular,
    }
}

/// Root changeset `a` from `fa`, child `b` carrying `delta` (a consistent
/// `a → b` changeset), then: `diff(a, b)` must equal `delta` exactly, and
/// replaying `delta` onto `a` must derive `b`'s tree.
fn check(fa: &[Change], delta: &[Change]) {
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());

    // Trees only reference blob ids (content addressing); no blob objects
    // are needed to derive, diff, or re-apply.
    let cs_a = Changeset {
        parents: vec![],
        changes: fa.to_vec(),
        message: String::new(),
        author: ident(),
        committer: ident(),
        signature: None,
    };
    let id_a = store.put_changeset(&cs_a).expect("a ingests");

    let cs_b = Changeset {
        parents: vec![id_a],
        changes: delta.to_vec(),
        message: String::new(),
        author: ident(),
        committer: ident(),
        signature: None,
    };
    let id_b = store.put_changeset(&cs_b).expect("b ingests");

    let diff = engine.diff(id_a, id_b).expect("diff works");
    assert_eq!(diff, delta, "diff(a → b) must be exactly the delta");

    let cs_c = Changeset {
        parents: vec![id_a],
        changes: diff,
        message: "re-apply".into(),
        author: ident(),
        committer: ident(),
        signature: None,
    };
    let id_c = store
        .put_changeset(&cs_c)
        .expect("diff output is canonical");
    assert_eq!(
        engine.derive_tree(id_b).expect("b derives"),
        engine
            .derive_tree(id_c)
            .expect("re-applied diff must derive"),
        "applying diff(a, b) onto a did not converge on b"
    );
}

#[test]
fn directory_replaced_by_file_reapplies() {
    // A: `g` is a directory holding one file; B: `g` is a regular file.
    // Canonical order applies Add("g") first, so Delete("g/h") lands under
    // a path the write just swallowed — the fuzz-found crash shape.
    check(
        &[add("g/h", Mode::Regular, b"under the dir\n")],
        &[add("g", Mode::Regular, b"now a file\n"), delete("g/h")],
    );
}

#[test]
fn file_replaced_by_directory_reapplies() {
    // The mirror shape: Delete("g") sorts before Add("g/h") and already
    // worked — pinned here so both swap directions stay covered.
    check(
        &[add("g", Mode::Regular, b"was a file\n")],
        &[delete("g"), add("g/h", Mode::Regular, b"under the dir\n")],
    );
}

#[test]
fn deep_directory_replaced_by_file_reapplies() {
    check(
        &[add("g/h/i.txt", Mode::Regular, b"deep\n")],
        &[
            add("g", Mode::Regular, b"now a file\n"),
            delete("g/h/i.txt"),
        ],
    );
}

#[test]
fn file_replaced_by_deep_directory_reapplies() {
    check(
        &[add("g", Mode::Regular, b"was a file\n")],
        &[delete("g"), add("g/h/i.txt", Mode::Regular, b"deep\n")],
    );
}

#[test]
fn swap_with_sibling_traffic_reapplies() {
    // The swap rides along with unrelated modifies — the shape the random
    // fuzzer actually finds.
    check(
        &[
            add("a/f.txt", Mode::Regular, b"alpha\n"),
            add("g/h", Mode::Exec, b"#!/bin/sh\n"),
            add("j.bin", Mode::Regular, b"0000\n"),
        ],
        &[
            modify("a/f.txt", Mode::Regular, b"alpha!\n"),
            add("g", Mode::Regular, b"flattened\n"),
            delete("g/h"),
        ],
    );
}

#[test]
fn reverse_swap_with_sibling_traffic_reapplies() {
    check(
        &[
            add("a/f.txt", Mode::Regular, b"alpha\n"),
            add("g", Mode::Regular, b"will be a dir\n"),
            add("j.bin", Mode::Regular, b"0000\n"),
        ],
        &[
            modify("a/f.txt", Mode::Regular, b"alpha!\n"),
            delete("g"),
            add("g/h", Mode::Exec, b"#!/bin/sh\n"),
        ],
    );
}

#[test]
fn deleting_under_a_pre_existing_file_still_rejected() {
    // The no-op rule is scoped to paths the *same changeset* wrote: a
    // delete beneath a file the base tree already has is inconsistent and
    // must still be refused.
    let store = Arc::new(MemoryStore::new());
    let engine = Engine::new(store.clone(), store.clone());
    let fa = vec![add("g", Mode::Regular, b"occupies g\n")];
    let cs_a = Changeset {
        parents: vec![],
        changes: fa,
        message: String::new(),
        author: ident(),
        committer: ident(),
        signature: None,
    };
    let id_a: Id = store.put_changeset(&cs_a).expect("a ingests");

    let cs_c = Changeset {
        parents: vec![id_a],
        changes: vec![delete("g/h")],
        message: String::new(),
        author: ident(),
        committer: ident(),
        signature: None,
    };
    let id_c = store.put_changeset(&cs_c).expect("c ingests");
    assert!(
        engine.derive_tree(id_c).is_err(),
        "deleting under a pre-existing file must stay an error"
    );
}

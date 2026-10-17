//! Line-level merge3 classification fixtures (M9): hand-written,
//! deterministic cases exercising xmerge's decision table —
//! both-equal, one-side-unchanged, non-overlap auto-combine in both
//! orders, overlap conflicts, identical-change coalescing, zealous
//! refinement, and the no-trailing-newline / empty-file edges.
//!
//! Every case was cross-checked against `git merge-file` (the same
//! `ll_xdl_merge` defaults: ZEALOUS, no favor, no markers).

use gardenbed_derive::{Merge3Outcome, merge3};

fn merged(base: &str, ours: &str, theirs: &str) -> Vec<u8> {
    match merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()) {
        Merge3Outcome::Merged(b) => b,
        Merge3Outcome::Conflict(c) => panic!("expected clean merge, got {c:?}"),
    }
}

fn conflicts(base: &str, ours: &str, theirs: &str) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>, std::ops::Range<usize>)> {
    match merge3(base.as_bytes(), ours.as_bytes(), theirs.as_bytes()) {
        Merge3Outcome::Conflict(c) => c
            .into_iter()
            .map(|r| (r.base, r.ours, r.theirs))
            .collect(),
        Merge3Outcome::Merged(b) => panic!("expected conflict, merged to {:?}", String::from_utf8_lossy(&b)),
    }
}

// ------------------------------------------------------------ trivial

/// Both sides changed identically: the change itself is the result.
#[test]
fn both_sides_equal() {
    assert_eq!(merged("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"), b"a\nX\nc\n");
}

/// Ours unchanged: theirs verbatim (xmerge's empty-script early-out).
#[test]
fn ours_unchanged_takes_theirs() {
    assert_eq!(merged("a\nb\nc\n", "a\nb\nc\n", "a\nY\nc\n"), b"a\nY\nc\n");
}

/// Theirs unchanged: ours verbatim.
#[test]
fn theirs_unchanged_takes_ours() {
    assert_eq!(merged("a\nb\nc\n", "a\nB\nc\n", "a\nb\nc\n"), b"a\nB\nc\n");
}

/// Byte-equal inputs (not just record-equal).
#[test]
fn all_three_equal() {
    assert_eq!(merged("same\n", "same\n", "same\n"), b"same\n");
}

// --------------------------------------------------- non-overlap combine

/// Ours edits the head, theirs edits the tail: both land.
#[test]
fn non_overlap_ours_first() {
    assert_eq!(
        merged("1\n2\n3\n4\n5\n", "A\n2\n3\n4\n5\n", "1\n2\n3\n4\nE\n"),
        b"A\n2\n3\n4\nE\n"
    );
}

/// The same with the sides swapped.
#[test]
fn non_overlap_theirs_first() {
    assert_eq!(
        merged("1\n2\n3\n4\n5\n", "1\n2\n3\n4\nE\n", "A\n2\n3\n4\n5\n"),
        b"A\n2\n3\n4\nE\n"
    );
}

/// Non-overlapping pure insertions on both sides combine.
#[test]
fn non_overlap_insertions() {
    assert_eq!(
        merged("a\nz\n", "a\n[ours]\nz\n", "a\nz\n[theirs]\n"),
        b"a\n[ours]\nz\n[theirs]\n"
    );
}

/// Deletion on one side, disjoint edit on the other: both apply.
#[test]
fn non_overlap_delete_and_edit() {
    assert_eq!(
        merged("1\n2\n3\n4\n5\n", "1\n3\n4\n5\n", "1\n2\n3\n4\nFIVE\n"),
        b"1\n3\n4\nFIVE\n"
    );
}

// ------------------------------------------------------- overlap cases

/// Overlapping non-identical edits conflict, spanning both hunks.
#[test]
fn overlap_same_line_conflicts() {
    let c = conflicts("1\n2\n3\n", "1\nOURS\n3\n", "1\nTHEIRS\n3\n");
    assert_eq!(c.len(), 1, "one conflict region: {c:?}");
    // Base lines 1..2 (the "2"), ours 1..2, theirs 1..2 in records.
    assert_eq!(c[0].0, 1..2);
    assert_eq!(c[0].1, 1..2);
    assert_eq!(c[0].2, 1..2);
}

/// Adjacent (touching, not strictly overlapping) edits also conflict —
/// xmerge cannot order two inserts at the same point (git agrees: both
/// hunks touch the shared "b" record).
#[test]
fn adjacent_edits_conflict() {
    assert_eq!(conflicts("a\nb", "A\nb", "a\nb!").len(), 1);
    assert_eq!(conflicts("a\nb\n", "a\nX\nb\n", "a\nY\nb\n").len(), 1);
}

/// A missing trailing newline is preserved as part of the last record.
#[test]
fn no_trailing_newline() {
    assert_eq!(merged("a\nb", "a\nB", "a\nb"), b"a\nB");
    assert_eq!(merged("a\nb", "a\nb", "a\nB"), b"a\nB");
}

/// Zealous refinement: overlapping edits that agree on a common core
/// shrink to a smaller conflict.
#[test]
fn zealous_refinement_resolves_partial_overlap() {
    // Both sides insert different lines at the same spot but share the
    // surrounding changes; the re-diff of the postimages finds them
    // identical except for one line pair.
    let c = conflicts(
        "a\nb\nc\nd\ne\n",
        "a\nb\nOURS\nd\ne\n",
        "a\nb\nTHEIRS\nd\ne\n",
    );
    assert_eq!(c.len(), 1, "{c:?}");
    // The refined conflict is exactly the differing postimage records.
    assert_eq!(c[0].1, 2..3);
    assert_eq!(c[0].2, 2..3);
}

/// Two conflicts ≤ 3 unchanged records apart merge into one
/// (`xdl_simplify_non_conflicts` at ZEALOUS); farther apart they stay
/// separate (verified against `git merge-file`: exit 2 for the pair).
#[test]
fn close_conflicts_simplify_into_one() {
    // Both sides change lines 0 and 7 (6 unchanged records between the
    // conflicting hunks): two separate conflicts.
    let c = conflicts(
        "1\n2\n3\n4\n5\n6\n7\n8\n9\n",
        "1\nO1\n3\n4\n5\n6\n7\nO2\n9\n",
        "1\nT1\n3\n4\n5\n6\n7\nT2\n9\n",
    );
    assert_eq!(c.len(), 2, "{c:?}");

    // Same shape with only 2 unchanged records between: the conflicts
    // merge into one (git merge-file exits 1 with a single marker pair).
    let c = conflicts(
        "1\n2\n3\n4\n5\n",
        "1\nO1\n3\nO2\n5\n",
        "1\nT1\n3\nT2\n5\n",
    );
    assert_eq!(c.len(), 1, "{c:?}");
}

// ------------------------------------------------------------- edges


/// Empty sides: merge3 degenerates to the changed side / insertion.
#[test]
fn empty_files() {
    assert_eq!(merged("", "", ""), b"");
    assert_eq!(merged("", "ours\n", ""), b"ours\n");
    assert_eq!(merged("", "", "theirs\n"), b"theirs\n");
    // Both insert into an empty base at the same point: conflict.
    assert_eq!(conflicts("", "a\n", "b\n").len(), 1);
    // Identical inserts coalesce.
    assert_eq!(merged("", "a\n", "a\n"), b"a\n");
}

/// Empty base is the add/add shape the tree walk feeds merge3.
#[test]
fn empty_base_is_add_add() {
    // Disjoint-looking content still conflicts: with an empty base both
    // sides are one insertion hunk at position 0 — they overlap.
    let c = conflicts("", "1\n2\n", "3\n4\n");
    assert_eq!(c.len(), 1, "{c:?}");
}

/// Binary content (NUL in the first 8 KiB): `ll_xdl_merge` routes to the
/// binary driver *before* the empty-script early-outs, so a binary merge
/// with favor=0 conflicts whenever both sides are present — even when
/// one side is unchanged relative to base. (Verified against git: at the
/// *tree* level identical binary changes on both sides resolve by id
/// equality before merge3 is consulted, which is why this only surfaces
/// when both sides genuinely differ from base.)
#[test]
fn binary_conflicts() {
    let base = b"\x00\x01\x02";
    let ours = b"\x00\x01\x03";
    let theirs = b"\x00\x01\x04";
    match merge3(base, ours, theirs) {
        Merge3Outcome::Conflict(_) => {}
        Merge3Outcome::Merged(b) => panic!("binary merge must conflict, got {b:?}"),
    }
    match merge3(base, base, theirs) {
        Merge3Outcome::Conflict(_) => {}
        Merge3Outcome::Merged(b) => panic!("binary one-side merge must conflict, got {b:?}"),
    }
}

/// Larger random-ish smoke: non-overlapping edits on a many-line file
/// merge to the obvious interleaving.
#[test]
fn many_lines_interleave() {
    let base: String = (0..60).map(|i| format!("line{i}\n")).collect();
    let ours: String = (0..60)
        .map(|i| if i == 5 { "OURS\n".into() } else { format!("line{i}\n") })
        .collect();
    let theirs: String = (0..60)
        .map(|i| if i == 55 { "THEIRS\n".into() } else { format!("line{i}\n") })
        .collect();
    let want: String = (0..60)
        .map(|i| match i {
            5 => "OURS\n".to_owned(),
            55 => "THEIRS\n".to_owned(),
            _ => format!("line{i}\n"),
        })
        .collect();
    assert_eq!(merged(&base, &ours, &theirs), want.as_bytes());
}

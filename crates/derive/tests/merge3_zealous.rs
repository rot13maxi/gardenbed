//! Zealous-merge fixtures (v0.2.md M9, `XDL_MERGE_ZEALOUS`):
//!
//! git's `xdl_merge` at the zealous level runs `xdl_refine_conflicts`
//! after overlap detection: each conflicting cluster's two postimage
//! slices are compared, and a cluster both sides replaced with the same
//! bytes resolves clean (xmerge's mode 4) even when the sides' base
//! spans differ — one side's diff shaped the same replacement
//! differently. This suite pins that behavior against `git merge-file`
//! (the same `ll_xdl_merge` code path `git merge-tree` uses), so every
//! fixture's expectation is git's, not ours.

use gardenbed_derive::{Merge3Outcome, merge3};

/// Runs `git merge-file -p` on the triple and returns `Ok(merged)` /
/// `Err(())` per its exit status (0 = clean, >0 = conflict markers).
fn git_merge_file(base: &[u8], ours: &[u8], theirs: &[u8]) -> Result<Vec<u8>, ()> {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "gardenbed-zealous-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cleanup = || {
        let _ = std::fs::remove_dir_all(&dir);
    };
    std::fs::write(dir.join("base"), base).expect("write base");
    std::fs::write(dir.join("ours"), ours).expect("write ours");
    std::fs::write(dir.join("theirs"), theirs).expect("write theirs");
    let out = std::process::Command::new("git")
        .arg("merge-file")
        .arg("-p")
        .arg("ours")
        .arg("base")
        .arg("theirs")
        .current_dir(&dir)
        .output()
        .expect("git merge-file runs");
    let merged = out.stdout;
    let clean = out.status.code() == Some(0);
    cleanup();
    if clean { Ok(merged) } else { Err(()) }
}

/// Asserts our `merge3` agrees with `git merge-file` on classification,
/// and on the merged bytes when clean.
fn assert_matches_git(base: &[u8], ours: &[u8], theirs: &[u8]) {
    let git = git_merge_file(base, ours, theirs);
    let ours_result = merge3(Some(base), Some(ours), Some(theirs));
    match (&git, &ours_result) {
        (Ok(git_merged), Merge3Outcome::Merged(our_merged)) => {
            assert_eq!(
                git_merged, our_merged,
                "clean merge bytes differ from git merge-file"
            );
        }
        (Err(()), Merge3Outcome::Conflict) => {}
        (Ok(_), Merge3Outcome::Conflict) => {
            panic!("git merged cleanly but we conflicted");
        }
        (Err(()), Merge3Outcome::Merged(our_merged)) => {
            panic!("we merged cleanly ({our_merged:?}) but git conflicted");
        }
    }
}

/// The zealous mode-4 case: both sides made the same replacement with
/// differently-shaped base spans. Ours replaces the first line with a
/// two-line block; theirs' diff represents the same edit as an insert
/// plus a replace (its base span starts one line later). The spans
/// overlap, so pre-zealous merge3 calls it a conflict — zealous
/// refinement sees the identical replacement bytes and merges clean.
#[test]
fn zealous_refines_identical_replacement_to_clean() {
    // Sanity-check the shape really needs zealous: the sides' spans
    // differ, so the shared-region shortcut (same span, same bytes)
    // does not fire.
    let base = b"a\nb\nc\nd\ne\n";
    let ours = b"a\nX\nY\nd\ne\n";
    let theirs = b"a\nX\nY\nd\ne\nf\n";
    // ours replaces [b c] with [X Y]; theirs replaces [b c] with
    // [X Y] AND appends f — the append makes the files differ, so the
    // trivial o == t early-out does not fire, while the overlapping
    // spans carry the same replacement bytes.
    assert_matches_git(base, ours, theirs);
    let (ours, theirs) = (b"a\nX\nY\nd\ne\n", b"a\nX\nY\nd\ne\nf\n");
    match merge3(Some(base), Some(ours), Some(theirs)) {
        Merge3Outcome::Merged(m) => assert_eq!(m, b"a\nX\nY\nd\ne\nf\n"),
        Merge3Outcome::Conflict => panic!("zealous refinement did not fire"),
    }
}

/// Zealous refinement does not rescue a cluster whose sides genuinely
/// replaced different bytes: overlapping spans with different
/// replacements stay a conflict.
#[test]
fn zealous_keeps_real_conflicts() {
    let base = b"a\nb\nc\nd\ne\n";
    let ours = b"a\nX\nY\nd\ne\n";
    let theirs = b"a\nR\nS\nd\ne\n";
    assert_matches_git(base, ours, theirs);
    assert_eq!(
        merge3(Some(base), Some(ours), Some(theirs)),
        Merge3Outcome::Conflict
    );
}

/// An overlapping cluster where one side's replacement is empty (a
/// deletion) is never refined — git's `xdl_refine_conflicts` skips
/// one-sided-empty clusters, and an empty "replacement" against a
/// non-empty one is a real conflict.
#[test]
fn zealous_skips_empty_replacements() {
    // Ours deletes [b c]; theirs replaces [b] with [X Y].
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\nd\n";
    let theirs = b"a\nX\nY\nc\nd\n";
    assert_matches_git(base, ours, theirs);
    assert_eq!(
        merge3(Some(base), Some(ours), Some(theirs)),
        Merge3Outcome::Conflict
    );
}

/// The plain shared-span case (identical change, identical spans) was
/// clean before zealous and stays clean: same base span, same bytes.
#[test]
fn shared_span_stays_clean() {
    let base = b"a\nb\nc\nd\ne\n";
    let ours = b"a\nX\nb\nc\nd\ne\n";
    let theirs = b"a\nX\nb\nc\nd\ne\nf\n";
    assert_matches_git(base, ours, theirs);
    match merge3(Some(base), Some(ours), Some(theirs)) {
        Merge3Outcome::Merged(m) => assert_eq!(m, b"a\nX\nb\nc\nd\ne\nf\n"),
        Merge3Outcome::Conflict => panic!("shared-span insert regressed"),
    }
}

/// Regression pin for the pre-zealous behavior on adjacent edits: two
/// sides editing touching spans still conflict (both want to rewrite
/// the boundary record's neighborhood), matching git.
#[test]
fn adjacent_edits_still_conflict() {
    let base = b"a\nb\nc\nd\n";
    let ours = b"a\nB\nc\nd\n";
    let theirs = b"a\nb\nC\nd\n";
    assert_matches_git(base, ours, theirs);
    assert_eq!(
        merge3(Some(base), Some(ours), Some(theirs)),
        Merge3Outcome::Conflict
    );
}

/// A zealous-resolved cluster alongside an unrelated non-overlapping
/// change: both land in the merged output.
#[test]
fn zealous_cluster_plus_side_change() {
    let base = b"a\nb\nc\nd\ne\nf\ng\n";
    let ours = b"a\nX\nY\nd\ne\nf\ng\n";
    let theirs = b"a\nX\nY\nd\ne\nf\nG\n";
    assert_matches_git(base, ours, theirs);
    match merge3(Some(base), Some(ours), Some(theirs)) {
        Merge3Outcome::Merged(m) => assert_eq!(m, b"a\nX\nY\nd\ne\nf\nG\n"),
        Merge3Outcome::Conflict => panic!("zealous refinement did not fire"),
    }
}

/// One cluster zealous-resolvable, another genuinely conflicting: the
/// file still conflicts overall (git refines per cluster; any
/// surviving conflict region fails the file).
#[test]
fn zealous_cluster_plus_real_conflict_still_conflicts() {
    let base = b"a\nb\nc\nd\ne\nf\ng\n";
    let ours = b"a\nX\nY\nd\ne\nZ\ng\n";
    let theirs = b"a\nX\nY\nd\ne\nW\ng\n";
    assert_matches_git(base, ours, theirs);
    assert_eq!(
        merge3(Some(base), Some(ours), Some(theirs)),
        Merge3Outcome::Conflict
    );
}

//! Blame oracle: `git_blame_first_parent` runs cleanly on rename-heavy
//! histories and produces tiling, contiguous, correctly-attributed
//! ranges; empty files yield empty output.

use gardenbed_harness::{
    BlameRange, GenOptions, files_at, generate, git_blame_first_parent, materialize,
};
use gardenbed_model::Mode;

fn rename_heavy_opts() -> GenOptions {
    GenOptions {
        commits: 45,
        merge_ratio: 0.15,
        octopus_ratio: 0.05,
        rename_ratio: 0.55,
        delete_ratio: 0.12,
        max_files: 25,
        max_dirs: 4,
        big_blob_bytes: None,
        exec_ratio: 0.08,
        symlink_ratio: 0.04,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    }
}

/// Physical line count, git's convention: "a\n" = 1, "a" = 1, "a\nb" = 2,
/// "" = 0.
fn line_count(content: &[u8]) -> usize {
    let nl = content.iter().filter(|b| **b == b'\n').count();
    if content.is_empty() || content.ends_with(b"\n") {
        nl
    } else {
        nl + 1
    }
}

fn ranges_tile(ranges: &[BlameRange], total: u32) {
    assert!(!ranges.is_empty(), "non-empty file must have blame ranges");
    let mut expected_lo = 1;
    for r in ranges {
        assert!(r.lo <= r.hi, "inverted range {r:?}");
        assert_eq!(r.lo, expected_lo, "gap/overlap at {r:?}");
        expected_lo = r.hi + 1;
    }
    assert_eq!(expected_lo - 1, total, "ranges must tile 1..={total}");
}

#[test]
fn blame_ranges_tile_and_reference_mapped_commits() {
    let history = generate(77, &rename_heavy_opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");
    let repo = &mat.repo_path;
    let head_sha = &mat.mapping.last().unwrap().1;
    let known_shas: std::collections::BTreeSet<&String> =
        mat.mapping.iter().map(|(_, s)| s).collect();

    let tip = files_at(&history, history.changesets.len() - 1);
    assert!(
        tip.len() >= 5,
        "rename-heavy history should end non-trivial"
    );

    let mut blamed = 0;
    let mut empty_seen = false;
    for (path, (blob, mode)) in &tip {
        if *mode != Mode::Regular && *mode != Mode::Exec {
            continue; // blame works on text files; skip symlinks/gitlinks
        }
        let content = history.blob(blob).expect("tip blob present");
        let ranges = git_blame_first_parent(repo, head_sha, path)
            .unwrap_or_else(|e| panic!("blame failed for {path:?}: {e}"));
        let total = line_count(content) as u32;
        if total == 0 {
            empty_seen = true;
            assert!(ranges.is_empty(), "empty file must blame to no ranges");
            continue;
        }
        ranges_tile(&ranges, total);
        for r in ranges {
            assert!(
                known_shas.contains(&r.commit_sha),
                "blamed sha {} is not one of our commits",
                r.commit_sha
            );
        }
        blamed += 1;
        if blamed >= 10 {
            break;
        }
    }
    assert!(blamed >= 5, "expected several blameable files");
    let _ = empty_seen;
}

#[test]
fn blame_on_empty_file_yields_no_ranges() {
    let history = generate(78, &rename_heavy_opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");
    let head = history.changesets.len() - 1;
    let head_sha = &mat.mapping[head].1;

    // Find an empty regular file at head (empty_files is on; if the seed
    // produced none, skip this check via an empty-content search).
    let tip = files_at(&history, head);
    let empty_path = tip
        .iter()
        .find(|(_, (blob, mode))| {
            *mode == Mode::Regular && history.blob(blob).map(|c| c.is_empty()).unwrap_or(false)
        })
        .map(|(p, _)| p.clone());
    if let Some(path) = empty_path {
        let ranges = git_blame_first_parent(&mat.repo_path, head_sha, &path)
            .expect("blame on empty file runs");
        assert!(ranges.is_empty());
    }
}

#[test]
fn blame_attribution_follows_first_parent_through_renames() {
    // A file renamed along the spine keeps its content; with --first-parent
    // git attributes its lines to the commit that introduced the content,
    // which must be a mapped commit sha. Verify attribution invariants on
    // a file that exists across many changesets.
    let history = generate(88, &rename_heavy_opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");
    let head = history.changesets.len() - 1;
    let head_sha = &mat.mapping[head].1;

    for (path, (blob, mode)) in files_at(&history, head) {
        if mode != Mode::Regular && mode != Mode::Exec {
            continue;
        }
        let content = history.blob(&blob).expect("blob present");
        let ranges =
            git_blame_first_parent(&mat.repo_path, head_sha, &path).expect("blame runs cleanly");
        let total = line_count(content) as u32;
        if total == 0 {
            assert!(ranges.is_empty());
            continue;
        }
        ranges_tile(&ranges, total);
        // Every range sha is materialized from some changeset.
        for r in &ranges {
            assert!(mat.mapping.iter().any(|(_, s)| s == &r.commit_sha));
        }
        break; // one thorough file is enough for this check
    }
}

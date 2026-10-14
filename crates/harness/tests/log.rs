//! Log oracle: structure of `git_log` matches the changeset history —
//! order, parent counts, spine lineage, identities, messages, limits.

use gardenbed_harness::{GenOptions, generate, git_log, materialize};

fn opts() -> GenOptions {
    GenOptions {
        commits: 30,
        merge_ratio: 0.25,
        octopus_ratio: 0.1,
        rename_ratio: 0.2,
        delete_ratio: 0.1,
        max_files: 15,
        max_dirs: 3,
        big_blob_bytes: None,
        exec_ratio: 0.1,
        symlink_ratio: 0.05,
        unicode_paths: true,
        crlf_content: false,
        empty_files: true,
    }
}

#[test]
fn log_matches_changeset_order_and_structure() {
    let history = generate(41, &opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");

    let log = git_log(&mat.repo_path, "refs/heads/main", 10_000).expect("git log");
    assert_eq!(log.len(), history.changesets.len());

    for (k, entry) in log.iter().enumerate() {
        let idx = history.changesets.len() - 1 - k;
        let cs = &history.changesets[idx];
        assert_eq!(entry.sha, mat.mapping[idx].1, "position {k}");
        assert_eq!(
            entry.parents.len(),
            cs.parents.len(),
            "cs {idx} parent count"
        );
        // Spine: parent[0] is the previous changeset's commit.
        if idx > 0 {
            assert_eq!(entry.parents[0], mat.mapping[idx - 1].1);
        } else {
            assert!(entry.parents.is_empty());
        }
        assert_eq!(entry.author_name, cs.author.name);
        assert_eq!(entry.author_email, cs.author.email);
        assert_eq!(entry.message, cs.message);
    }
}

#[test]
fn log_respects_limit() {
    let history = generate(42, &opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");

    let limited = git_log(&mat.repo_path, "refs/heads/main", 5).expect("git log");
    assert_eq!(limited.len(), 5);
    // The limited prefix equals the head of the full log.
    let full = git_log(&mat.repo_path, "refs/heads/main", 10_000).expect("git log");
    assert_eq!(&limited[..], &full[..5]);

    let zero = git_log(&mat.repo_path, "refs/heads/main", 0).expect("git log");
    assert!(zero.is_empty());

    // A limit larger than the history returns everything.
    let all = git_log(&mat.repo_path, "refs/heads/main", 1_000).expect("git log");
    assert_eq!(all.len(), history.changesets.len());
}

#[test]
fn log_of_specific_rev_matches_that_changeset() {
    let history = generate(43, &opts());
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");

    // log at an arbitrary middle commit reports exactly that commit.
    let mid = history.changesets.len() / 2;
    let log = git_log(&mat.repo_path, &mat.mapping[mid].1, 10_000).expect("git log");
    assert_eq!(log.len(), mid + 1, "history below mid is fully reachable");
    assert_eq!(log[0].sha, mat.mapping[mid].1);
}

//! Materialization: fsck-clean repo, index-aligned mapping, identical
//! SHAs across runs, and the git DAG matches the changeset DAG.

use std::collections::BTreeSet;

use gardenbed_harness::{GenOptions, generate, git_fsck, git_log, materialize};
use gardenbed_model::Id;

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn rich_opts(commits: u32) -> GenOptions {
    GenOptions {
        commits,
        merge_ratio: 0.2,
        octopus_ratio: 0.08,
        rename_ratio: 0.2,
        delete_ratio: 0.15,
        max_files: 24,
        max_dirs: 4,
        big_blob_bytes: Some(96 * 1024),
        exec_ratio: 0.08,
        symlink_ratio: 0.06,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    }
}

#[test]
fn materialize_produces_fsck_clean_repo() {
    let history = generate(11, &rich_opts(30));
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let mat = materialize(&history, &repo).expect("materialize");

    assert_eq!(mat.mapping.len(), history.changesets.len());
    for (i, (id, sha)) in mat.mapping.iter().enumerate() {
        assert_eq!(id, &history.changesets[i].id().unwrap());
        assert!(is_sha(sha), "not a sha: {sha}");
    }
    let shas: BTreeSet<&String> = mat.mapping.iter().map(|(_, s)| s).collect();
    assert_eq!(shas.len(), mat.mapping.len(), "git shas must be unique");

    git_fsck(&repo).expect("git fsck --strict clean");

    // refs/heads/main points at the last commit.
    let log = git_log(&repo, "refs/heads/main", 1).expect("git log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].sha, mat.mapping.last().unwrap().1);
}

#[test]
fn materialize_is_deterministic_across_repos() {
    let history = generate(23, &rich_opts(25));
    let dir = tempfile::tempdir().expect("tempdir");
    let a = materialize(&history, &dir.path().join("a")).expect("materialize a");
    let b = materialize(&history, &dir.path().join("b")).expect("materialize b");
    assert_eq!(a.mapping, b.mapping, "same history must give same git SHAs");

    // The tip commit's structure matches the head changeset.
    let tip = git_log(&a.repo_path, "refs/heads/main", 1).unwrap();
    assert_eq!(tip[0].sha, a.mapping.last().unwrap().1);
    let head = history.changesets.last().unwrap();
    assert_eq!(tip[0].parents.len(), head.parents.len());
}

#[test]
fn materialize_preserves_graph_structure() {
    // The changeset DAG materializes to the identical git DAG: git log
    // (newest first) must be exactly the reversed mapping, because our
    // commit times strictly increase with changeset index.
    let opts = GenOptions {
        merge_ratio: 0.3,
        octopus_ratio: 0.1,
        ..rich_opts(25)
    };
    let history = generate(31, &opts);
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");

    let log = git_log(&mat.repo_path, "refs/heads/main", 10_000).expect("git log");
    assert_eq!(
        log.len(),
        history.changesets.len(),
        "one git commit per changeset"
    );

    for (k, entry) in log.iter().enumerate() {
        let cs_index = history.changesets.len() - 1 - k;
        assert_eq!(entry.sha, mat.mapping[cs_index].1, "log position {k}");
        let cs = &history.changesets[cs_index];
        assert_eq!(
            entry.parents.len(),
            cs.parents.len(),
            "changeset {cs_index}"
        );
        assert_eq!(entry.author_name, cs.author.name);
        assert_eq!(entry.author_email, cs.author.email);
        // First-parent lineage: parent[0] of cs[i] is cs[i-1].
        if cs_index > 0 {
            let expected = &mat.mapping[cs_index - 1].1;
            assert_eq!(&entry.parents[0], expected, "spine at {cs_index}");
        } else {
            assert!(entry.parents.is_empty(), "root has no parents");
        }
        // Extra merge parents map back to earlier changesets.
        for p in cs.parents.iter().skip(1) {
            let sha = sha_of_mapping(&mat.mapping, p);
            assert!(entry.parents.contains(&sha));
        }
    }
}

fn sha_of_mapping(mapping: &[(Id, String)], id: &Id) -> String {
    mapping
        .iter()
        .find(|(k, _)| k == id)
        .map(|(_, s)| s.clone())
        .expect("parent present in mapping")
}

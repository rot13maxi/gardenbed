//! Differential test: for seeded histories with merges and renames,
//! `files_at(i)` must equal `git ls-tree -r` at the mapped commit sha for
//! every path — same path set, same modes, same contents (compared via
//! git's own blob hashing, since git blob ids differ from ours by design).

use std::collections::HashMap;

use gardenbed_harness::{
    files_at, generate, git_hash_object, git_ls_tree_r, git_show, materialize, GenOptions,
};
use gardenbed_model::Mode;

fn opts_for(seed: u64, commits: u32) -> GenOptions {
    let _ = seed;
    GenOptions {
        commits,
        merge_ratio: 0.2,
        octopus_ratio: 0.07,
        rename_ratio: 0.25,
        delete_ratio: 0.15,
        max_files: 30,
        max_dirs: 4,
        big_blob_bytes: Some(64 * 1024),
        exec_ratio: 0.1,
        symlink_ratio: 0.06,
        unicode_paths: true,
        crlf_content: true,
        empty_files: true,
    }
}

fn git_mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// Lowercase hex of raw bytes (ls-tree shas arrive as 20 raw bytes).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Compares `files_at(i)` against git's recursive tree listing at the
/// mapped sha, for every changeset of the history.
fn compare_history(seed: u64) {
    let history = generate(seed, &opts_for(seed, 35));
    let dir = tempfile::tempdir().expect("tempdir");
    let mat = materialize(&history, dir.path()).expect("materialize");
    let repo = &mat.repo_path;

    // Cache: our blob id -> git blob sha (git's own hashing, no -w).
    let mut git_sha_of_blob: HashMap<gardenbed_model::Id, String> = HashMap::new();
    for (id, content) in &history.blob_contents {
        // Gitlink "blobs" are submodule pointers, not real blobs.
        if id.as_bytes()[..12].iter().all(|b| *b == 0) && content.as_slice() == id.as_bytes() {
            continue;
        }
        let sha = git_hash_object(repo, content).expect("hash-object");
        git_sha_of_blob.insert(*id, sha);
    }

    for (i, (_, sha)) in mat.mapping.iter().enumerate() {
        let expected = files_at(&history, i);
        let got = git_ls_tree_r(repo, sha).expect("ls-tree");

        let got_map: HashMap<&str, (&str, &[u8])> =
            got.iter().map(|(p, m, s)| (p.as_str(), (m.as_str(), s.as_slice()))).collect();
        assert_eq!(got_map.len(), got.len(), "ls-tree paths must be unique at {i}");
        assert_eq!(
            got_map.len(),
            expected.len(),
            "path count mismatch at changeset {i}"
        );
        for (path, (blob, mode)) in &expected {
            let Some((gmode, gsha)) = got_map.get(path.as_str()) else {
                panic!("path {path:?} missing from git tree at changeset {i}");
            };
            assert_eq!(git_mode_str(*mode), *gmode, "mode mismatch at {path:?} cs {i}");
            if *mode == Mode::Gitlink {
                // Submodule pointer: git stores the upstream sha verbatim.
                let padded = blob.as_bytes();
                let want = {
                    let mut s = String::new();
                    for b in &padded[12..] {
                        s.push_str(&format!("{b:02x}"));
                    }
                    s
                };
                assert_eq!(
                    want,
                    to_hex(gsha),
                    "gitlink pointer mismatch at {path:?} cs {i}"
                );
            } else {
                let want = git_sha_of_blob
                    .get(blob)
                    .unwrap_or_else(|| panic!("blob for {path:?} missing from cache"));
                assert_eq!(want, &to_hex(gsha), "content mismatch at {path:?} cs {i}");
            }
        }
    }

    // Direct `git show` spot-checks (every 8th changeset, all paths):
    // raw bytes round-trip, including empty files and CRLF content.
    for i in (0..mat.mapping.len()).step_by(8) {
        let sha = &mat.mapping[i].1;
        for (path, (blob, mode)) in files_at(&history, i) {
            if mode == Mode::Gitlink {
                continue; // git cannot `show` a submodule pointer
            }
            let shown = git_show(repo, sha, &path).expect("git show");
            let want = history.blob(&blob).expect("blob present");
            assert_eq!(shown, want, "git show mismatch at {path:?} cs {i}");
        }
    }
}

#[test]
fn files_at_matches_git_ls_tree_for_three_seeded_histories() {
    compare_history(101);
    compare_history(202);
    compare_history(303);
}

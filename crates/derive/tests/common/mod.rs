//! Shared test helpers for the derive crate's differential suites.
//!
//! Each test binary includes this module, so not every helper is used by
//! every binary.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_harness::GeneratedHistory;
use gardenbed_model::{Id, Mode, Op};
use gardenbed_store::{MetadataStore, ObjectStore};

/// Ingests a generated history into the stores: blobs first, then
/// changesets in order (parents before children), optionally calling
/// `on_changeset_created` per changeset (the eager D3 write path). Returns
/// the changeset ids, index-aligned with `h.changesets`.
pub fn ingest(
    objects: &Arc<dyn ObjectStore>,
    metadata: &Arc<dyn MetadataStore>,
    h: &GeneratedHistory,
    eager: bool,
) -> Vec<Id> {
    for (id, content) in &h.blob_contents {
        // Gitlink pointers are *not* content-addressed blobs: their id is
        // the upstream git sha left-padded with zeros to 32 bytes, and the
        // harness's "content" is exactly those id bytes (materialize.rs
        // detects pointers the same way). Loam stores only SHA-256-addressed
        // content, so pointers are never written there; tree derivation
        // carries them opaquely.
        let is_pointer =
            id.as_bytes()[..12].iter().all(|b| *b == 0) && content.as_slice() == id.as_bytes();
        if is_pointer {
            continue;
        }
        let got = objects.put_blob(content).expect("put blob");
        assert_eq!(got, *id, "blob id mismatch during ingest");
    }
    let engine = Engine::new(objects.clone(), metadata.clone());
    let mut ids = Vec::with_capacity(h.changesets.len());
    for cs in &h.changesets {
        let id = metadata.put_changeset(cs).expect("put changeset");
        ids.push(id);
        if eager {
            engine
                .on_changeset_created(id)
                .expect("on_changeset_created");
        }
    }
    ids
}

/// A hermetic `git` command (no inherited config, C locale) — mirrors the
/// harness's own environment pinning.
pub fn git_cmd(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("LC_ALL", "C");
    cmd.env("LANG", "C");
    cmd.args(args);
    cmd
}

/// Runs git, returning stdout; non-zero exit with empty stdout maps to
/// `None` (e.g. `git merge-base` on disjoint histories exits 1 silently).
pub fn git_out(repo: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = git_cmd(repo, args).output().expect("git runs");
    if out.status.success() {
        Some(out.stdout)
    } else {
        assert!(
            out.stdout.is_empty(),
            "git {} failed with unexpected output: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout)
        );
        None
    }
}

/// Trims a git stdout into a lowercase 40-hex sha, panicking on anything
/// else (tests fail loudly on oracle surprises).
pub fn git_sha(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let line = s.trim();
    assert_eq!(line.len(), 40, "expected a 40-hex sha, got {line:?}");
    assert!(
        line.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected hex, got {line:?}"
    );
    line.to_ascii_lowercase()
}

/// `git merge-base <a> <b>`: `None` when the histories share no ancestor.
pub fn git_merge_base(repo: &Path, a: &str, b: &str) -> Option<String> {
    git_out(repo, &["merge-base", a, b]).map(|o| git_sha(&o))
}

/// `git diff-tree -r -z --name-status --no-renames --no-commit-id <a> <b>`:
/// the tree-to-tree diff between two commits with rename detection off.
/// Returns `(status, path)` per entry, in git's tree order.
pub fn git_diff_two(repo: &Path, a: &str, b: &str) -> Vec<(char, String)> {
    let out = git_out(
        repo,
        &[
            "diff-tree",
            "-r",
            "-z",
            "--name-status",
            "--no-renames",
            "--no-commit-id",
            a,
            b,
        ],
    )
    .unwrap_or_default();
    let mut entries = Vec::new();
    let mut tokens = out.split(|c| *c == 0).filter(|t| !t.is_empty());
    while let Some(status) = tokens.next() {
        let status = std::str::from_utf8(status).expect("status utf8");
        let first = status.bytes().next().expect("non-empty status") as char;
        let path = tokens.next().expect("diff entry path");
        // Rename/copy entries carry (src, dst); --no-renames must not emit them.
        assert!(
            first != 'R' && first != 'C',
            "rename detection must be off on both sides"
        );
        let path = std::str::from_utf8(path).expect("path utf8").to_owned();
        entries.push((first, path));
    }
    entries
}

/// Maps a git status letter onto the Gardenbed op classification.
///
/// git's `T` (typechange: the same path flips mode class, e.g. regular ↔
/// symlink, as when a rename lands on an existing path of another type) is
/// a gardenbed `Modify`: the path exists on both sides with a different
/// blob and/or mode. Gardenbed's diff has no separate type-change class.
pub fn git_status_op(status: char) -> Op {
    match status {
        'A' => Op::Add,
        'M' | 'T' => Op::Modify,
        'D' => Op::Delete,
        other => panic!("unexpected git status {other:?} (no rename detection)"),
    }
}

/// Maps a gardenbed mode onto git's octal string.
pub fn mode_to_git(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

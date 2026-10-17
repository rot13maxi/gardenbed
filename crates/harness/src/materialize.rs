//! Renders generated changesets into a real git repository.
//!
//! Uses git plumbing only, incrementally: blobs are written with
//! `hash-object -w --stdin` (deduplicated per blob id), the index is kept
//! across changesets via `update-index --index-info` (upserts as
//! ls-tree-style lines, deletions as mode-0 lines) plus `read-tree --empty`
//! once up front, and each changeset becomes
//! `write-tree` → `commit-tree -p …` with `GIT_AUTHOR_*`/`GIT_COMMITTER_*`
//! taken verbatim from the changeset's [`gardenbed_model::Identity`] in git's raw
//! `"<secs> +hhmm"` date form. A final `update-ref` points `refs/heads/main`
//! at the last commit.
//!
//! Cost scales with change size, not repo size: exactly one
//! `update-index`/`write-tree`/`commit-tree` round per changeset plus one
//! `hash-object` per *new* blob.
//!
//! Determinism: the same history materialized twice (into different
//! directories) produces byte-identical git objects and therefore
//! identical commit SHAs — no wall-clock, locale, or config is consulted
//! (inherited `GIT_*` variables are stripped and `GIT_CONFIG_GLOBAL` /
//! `GIT_CONFIG_NOSYSTEM` pin the config away).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gardenbed_model::{Id, Mode, Op};

use crate::generator::{GeneratedHistory, format_tz};

/// The result of materializing a history into a git repo.
#[derive(Clone, Debug)]
pub struct Materialized {
    /// The repository directory (non-bare, object-format sha1).
    pub repo_path: PathBuf,
    /// `(changeset id, git commit sha)` — index-aligned with
    /// [`GeneratedHistory::changesets`] and in the same order. Shas are
    /// lowercase 40-hex.
    pub mapping: Vec<(Id, String)>,
}

/// Git's octal mode strings, one per gardenbed mode.
fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// A git command with a hermetic environment: inherited `GIT_*` vars are
/// removed, global/system config is pinned away, and a few knobs are
/// forced for byte-determinism.
fn git_cmd(repo: &Path, args: &[&str]) -> Command {
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
    cmd.args([
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.safecrlf=false",
        "-c",
        "gc.auto=0",
    ]);
    cmd.args(args);
    cmd
}

/// Runs git, returning stdout; non-zero exits become descriptive errors.
fn run_git(repo: &Path, args: &[&str]) -> std::io::Result<Vec<u8>> {
    let out = git_cmd(repo, args).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim_end()
        )));
    }
    Ok(out.stdout)
}

/// Runs git with bytes on stdin (used for `--index-info`, `commit-tree`
/// messages and `hash-object --stdin`).
fn run_git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut child = git_cmd(repo, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let stdin = child.stdin.as_mut().expect("stdin piped");
        stdin.write_all(input)?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(std::io::Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim_end()
        )));
    }
    Ok(out.stdout)
}

/// Validates a 40-hex git object id (zero-assumption parsing of git
/// output: anything else is an error, never silently accepted).
fn parse_sha40(s: &str) -> std::io::Result<String> {
    let s = s.trim();
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(std::io::Error::other(format!(
            "git printed a non-40-hex object id: {s:?}"
        )))
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The git SHA of a gardenbed blob id. Gitlinks carry the upstream git
/// SHA-1 padded to 32 bytes (spec §3), so their git sha is bytes 12..32 in
/// hex; everything else is hashed by git from the raw content.
fn blob_git_sha(
    repo: &Path,
    cache: &mut HashMap<Id, String>,
    id: &Id,
    history: &GeneratedHistory,
) -> std::io::Result<String> {
    if let Some(sha) = cache.get(id) {
        return Ok(sha.clone());
    }
    // A gitlink pointer's stored "content" is the padded git sha itself
    // (bytes 0..12 zero), so its git sha is bytes 12..32 in hex.
    let is_pointer = id.as_bytes()[..12].iter().all(|b| *b == 0)
        && history.blob(id) == Some(id.as_bytes().as_slice());
    let sha = if is_pointer {
        hex(&id.as_bytes()[12..])
    } else {
        let content = history.blob(id).ok_or_else(|| {
            std::io::Error::other(format!(
                "blob {} missing from GeneratedHistory contents",
                id
            ))
        })?;
        let out = run_git_stdin(repo, &["hash-object", "-w", "--stdin"], content)?;
        let line = String::from_utf8_lossy(&out);
        parse_sha40(line.lines().next().unwrap_or(""))?
    };
    cache.insert(*id, sha.clone());
    Ok(sha)
}

/// Materializes `history` into a real git repository at `dest` (created
/// if missing) and returns the changeset→git-sha mapping.
///
/// The mapping is index-aligned with [`GeneratedHistory::changesets`].
/// `refs/heads/main` points at the final commit. Running this twice on
/// the same history yields identical SHAs; running it into an existing
/// repo reuses it (the index is reset; unrelated refs are left alone).
///
/// In debug builds a `git fsck --strict` is run at the end (see
/// [`crate::oracle::git_fsck`] for the explicit-test variant).
pub fn materialize(history: &GeneratedHistory, dest: &Path) -> std::io::Result<Materialized> {
    std::fs::create_dir_all(dest)?;
    run_git(
        dest,
        &[
            "init",
            "-q",
            "--initial-branch=main",
            "--object-format=sha1",
        ],
    )?;
    run_git(dest, &["read-tree", "--empty"])?;

    let mut cache: HashMap<Id, String> = HashMap::new();
    let mut mapping: Vec<(Id, String)> = Vec::with_capacity(history.changesets.len());
    let mut sha_by_cs: HashMap<Id, String> = HashMap::new();

    for cs in &history.changesets {
        // Build the update-index stream for this changeset only.
        let mut index_info: Vec<u8> = Vec::new();
        for c in &cs.changes {
            match c.op {
                Op::Delete => {
                    index_info.extend_from_slice(
                        format!("0 0000000000000000000000000000000000000000\t{}\n", c.path)
                            .as_bytes(),
                    );
                }
                Op::Add | Op::Modify | Op::Rename => {
                    if let Some(src) = &c.src_path {
                        index_info.extend_from_slice(
                            format!("0 0000000000000000000000000000000000000000\t{}\n", src)
                                .as_bytes(),
                        );
                    }
                    let Some(blob) = c.blob else {
                        return Err(std::io::Error::other(format!(
                            "changeset {} op {:?} on {} lacks a blob",
                            cs.id().map(|i| i.to_hex()).unwrap_or_default(),
                            c.op,
                            c.path
                        )));
                    };
                    let sha = blob_git_sha(dest, &mut cache, &blob, history)?;
                    index_info.extend_from_slice(
                        format!(
                            "{} {} {}\t{}\n",
                            git_mode(c.mode),
                            if c.mode == Mode::Gitlink {
                                "commit"
                            } else {
                                "blob"
                            },
                            sha,
                            c.path
                        )
                        .as_bytes(),
                    );
                }
            }
        }
        if !index_info.is_empty() {
            run_git_stdin(dest, &["update-index", "--index-info"], &index_info)?;
        }

        let tree = parse_sha40(&String::from_utf8_lossy(&run_git(dest, &["write-tree"])?))?;

        // commit-tree: parents in order (first parent = delta base), the
        // message verbatim on stdin, identities via env in raw date form.
        let mut args: Vec<String> = vec!["commit-tree".into(), tree];
        for p in &cs.parents {
            let sha = sha_by_cs.get(p).ok_or_else(|| {
                std::io::Error::other(format!(
                    "changeset references parent {} that is not earlier in the history",
                    p
                ))
            })?;
            args.push("-p".into());
            args.push(sha.clone());
        }
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut cmd = git_cmd(dest, &arg_refs);
        cmd.env("GIT_AUTHOR_NAME", &cs.author.name);
        cmd.env("GIT_AUTHOR_EMAIL", &cs.author.email);
        cmd.env(
            "GIT_AUTHOR_DATE",
            format!("{} {}", cs.author.time_secs, format_tz(cs.author.tz_min)),
        );
        cmd.env("GIT_COMMITTER_NAME", &cs.committer.name);
        cmd.env("GIT_COMMITTER_EMAIL", &cs.committer.email);
        cmd.env(
            "GIT_COMMITTER_DATE",
            format!(
                "{} {}",
                cs.committer.time_secs,
                format_tz(cs.committer.tz_min)
            ),
        );
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .as_mut()
            .expect("stdin piped")
            .write_all(cs.message.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "git commit-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim_end()
            )));
        }
        let sha = parse_sha40(&String::from_utf8_lossy(&out.stdout))?;
        let cs_id = cs
            .id()
            .map_err(|e| std::io::Error::other(format!("changeset does not encode: {e}")))?;
        sha_by_cs.insert(cs_id, sha.clone());
        mapping.push((cs_id, sha));
    }

    if let Some((_, head)) = mapping.last() {
        let head = head.clone();
        run_git(dest, &["update-ref", "refs/heads/main", &head])?;
    }

    #[cfg(debug_assertions)]
    {
        use crate::oracle::git_fsck;

        // Debug builds (cargo test/dev) prove the repo is well-formed;
        // release builds skip the extra subprocess.
        git_fsck(dest).map_err(|e| {
            std::io::Error::other(format!("materialize produced a broken repo: {e}"))
        })?;
    }

    Ok(Materialized {
        repo_path: dest.to_path_buf(),
        mapping,
    })
}

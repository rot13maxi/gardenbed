//! Batched `git diff-tree --stdin` runs: status classification and rename
//! detection (decision D8).
//!
//! Rename detection is the one piece of import semantics git oxide does not
//! give us in v0.1, so the classification of each commit's first-parent
//! diff comes from a `git` subprocess:
//!
//! ```text
//! git diff-tree --stdin --root -r -z --name-status --diff-merges=first-parent [-M | --no-renames]
//! ```
//!
//! Commits are fed in batches (256 ids, ~10 KiB of stdin — comfortably
//! below the pipe buffer, so a write-then-read never deadlocks) and the
//! NUL-separated output is attributed back by 40-hex commit-id tokens.
//!
//! One git quirk this module owns: **a commit whose diff is empty produces
//! no output at all — not even its id token** (verified: redundant merges
//! print zero bytes). Absence of a token therefore means "empty diff",
//! which is safe *and verified by the caller*: the caller cross-checks
//! that every token-less commit's tree equals its first parent's tree —
//! except for commits git's diff machinery cannot see (mode-only
//! normalizations of pre-canonical tree entry modes, which `canon_mode`
//! collapses before comparing; early-2005 `100664` → `100644` fixups are
//! the real-world case), which the caller classifies with a raw,
//! mode-aware tree comparison (`import` owns it), and, for root
//! commits, that its tree is empty. The token stream itself stays
//! unambiguous because status tokens are never 40-hex and paths never
//! directly follow an id.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::ImportError;
use crate::oid::GitOid;

/// Commits per `git diff-tree --stdin` spawn.
pub(crate) const DIFF_BATCH: usize = 256;

/// One entry of a commit's first-parent diff (`--name-status`).
///
/// Public for the fuzz target that feeds random bytes to [`parse_batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffRecord {
    /// The status letter's meaning.
    pub status: DiffStatus,
    /// Destination path (the path in the new tree).
    pub path: String,
    /// Source path — present exactly for renames/copies.
    pub src: Option<String>,
}

/// The status letters git_bridge understands.
///
/// Public for the fuzz target that feeds random bytes to [`parse_batch`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    Add,
    Modify,
    /// Type change (`T`): the path changed kind between blob-like entries
    /// (file/symlink/gitlink). Imported as a [`DiffStatus::Modify`] — the
    /// new mode and content are read from the tree either way.
    TypeChange,
    Delete,
    Rename,
    /// Copy (`C`): mapped to a rename. Never occurs with `-M` alone (no
    /// `-C`), but handled for robustness.
    Copy,
}

impl DiffStatus {
    fn from_token(token: &str) -> Option<Self> {
        match token.as_bytes().first()? {
            b'A' => Some(DiffStatus::Add),
            b'M' => Some(DiffStatus::Modify),
            b'D' => Some(DiffStatus::Delete),
            b'T' => Some(DiffStatus::TypeChange),
            b'R' => Some(DiffStatus::Rename),
            b'C' => Some(DiffStatus::Copy),
            _ => None,
        }
    }
}

/// Runs one batch: returns each commit's diff records. Commits with an
/// empty diff are absent from the map.
pub(crate) fn run_batch(
    cwd: &Path,
    shas: &[GitOid],
    detect_renames: bool,
) -> Result<HashMap<GitOid, Vec<DiffRecord>>, ImportError> {
    assert!(!shas.is_empty(), "caller never spawns for empty batches");
    let mut cmd = git_cmd(cwd);
    cmd.args([
        "diff-tree",
        "--stdin",
        "--root",
        "-r",
        "-z",
        "--name-status",
        "--diff-merges=first-parent",
    ]);
    cmd.arg(if detect_renames { "-M" } else { "--no-renames" });
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| ImportError::Git(format!("cannot spawn git diff-tree: {e}")))?;
    let mut stdin = child.stdin.take().expect("stdin piped");
    let mut input = Vec::with_capacity(shas.len() * 41);
    for sha in shas {
        input.extend_from_slice(sha.to_hex().as_bytes());
        input.push(b'\n');
    }
    // ~10 KiB per batch fits the pipe buffer: no writer/reader deadlock.
    stdin
        .write_all(&input)
        .map_err(|e| ImportError::Git(format!("writing to git diff-tree stdin: {e}")))?;
    drop(stdin);
    let out = child
        .wait_with_output()
        .map_err(|e| ImportError::Git(format!("waiting for git diff-tree: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(batch_failure(cwd, shas, detect_renames, stderr.trim_end()));
    }
    match parse_batch(&out.stdout, shas) {
        Ok(map) => Ok(map),
        // A malformed batch is re-run commit-by-commit so the error names
        // the offending commit (a batch parse failure names none).
        Err(_) => {
            for sha in shas {
                let mut cmd = git_cmd(cwd);
                cmd.args([
                    "diff-tree",
                    "--root",
                    "-r",
                    "-z",
                    "--name-status",
                    "--diff-merges=first-parent",
                ]);
                cmd.arg(if detect_renames { "-M" } else { "--no-renames" });
                cmd.arg(sha.to_hex());
                let out = cmd
                    .output()
                    .map_err(|e| ImportError::Git(format!("cannot spawn git diff-tree: {e}")))?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    return Err(ImportError::Diff {
                        sha: sha.to_hex(),
                        detail: stderr.trim_end().to_owned(),
                    });
                }
                parse_batch(&out.stdout, std::slice::from_ref(sha))
                    .map_err(|e| e.with_commit_sha(sha))?;
            }
            Err(ImportError::Diff {
                sha: String::new(),
                detail: "diff-tree output could not be parsed (re-runs all succeeded)".to_owned(),
            })
        }
    }
}

/// When a batch fails, re-runs it commit-by-commit so the error names the
/// offending commit (the batch stderr names none).
fn batch_failure(
    cwd: &Path,
    shas: &[GitOid],
    detect_renames: bool,
    batch_stderr: &str,
) -> ImportError {
    for sha in shas {
        let mut cmd = git_cmd(cwd);
        cmd.args([
            "diff-tree",
            "--root",
            "-r",
            "-z",
            "--name-status",
            "--diff-merges=first-parent",
        ]);
        cmd.arg(if detect_renames { "-M" } else { "--no-renames" });
        cmd.arg(sha.to_hex());
        if let Ok(out) = cmd.output() {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return ImportError::Diff {
                    sha: sha.to_hex(),
                    detail: stderr.trim_end().to_owned(),
                };
            }
        } else {
            break;
        }
    }
    // Every commit individually succeeded (or re-running failed differently):
    // report the batch failure with its stderr.
    ImportError::Diff {
        sha: shas.first().map(GitOid::to_hex).unwrap_or_default(),
        detail: batch_stderr.to_owned(),
    }
}

/// Builds a git command with a hermetic environment (no inherited `GIT_*`,
/// no system/global config, C locale) — mirroring the harness oracle.
fn git_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
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
    cmd
}

/// Parses the batch's NUL-separated output into per-commit records.
///
/// Token stream shape: `<40-hex id> ( <status> <path> [<src>] )*`, records
/// back to back; commits with an empty diff contribute nothing (see the
/// module docs). The parse has exactly one reachable state — "id or
/// status" — because a status token is never 40-hex and a path token never
/// directly follows an id, so the stream is unambiguous despite commits
/// being interleaved freely.
///
/// Public for the fuzz target (`fuzz_difftree_parse`): random bytes must
/// always parse to `Ok` or a descriptive `Err`, never panic.
pub fn parse_batch(
    out: &[u8],
    expected: &[GitOid],
) -> Result<HashMap<GitOid, Vec<DiffRecord>>, ImportError> {
    let wanted: std::collections::HashSet<GitOid> = expected.iter().copied().collect();
    let mut map: HashMap<GitOid, Vec<DiffRecord>> = HashMap::with_capacity(expected.len());
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    // `None` until the first id token: an entry before any id is malformed.
    let mut current: Option<GitOid> = None;
    while let Some(tok) = tokens.next() {
        let text = std::str::from_utf8(tok).map_err(|_| diff_parse("token is not UTF-8"))?;
        if let Some(oid) = GitOid::from_hex(text) {
            // A commit id begins a record. Empty-diff commits contribute
            // exactly this (an empty record list).
            if map.insert(oid, Vec::new()).is_some() {
                return Err(diff_parse(format!("duplicate commit id {text} in output")));
            }
            if !wanted.contains(&oid) {
                return Err(diff_parse(format!(
                    "commit {text} in output was not requested"
                )));
            }
            current = Some(oid);
            continue;
        }
        // Otherwise the token must be a status for the current commit.
        let Some(oid) = current else {
            return Err(diff_parse("diff entry appears before any commit id"));
        };
        let status = DiffStatus::from_token(text)
            .ok_or_else(|| diff_parse(format!("unsupported diff status {text:?}")))?;
        let record = match status {
            DiffStatus::Rename | DiffStatus::Copy => {
                let src = tokens
                    .next()
                    .ok_or_else(|| diff_parse("rename entry is missing its source path"))?;
                let dst = tokens
                    .next()
                    .ok_or_else(|| diff_parse("rename entry is missing its destination path"))?;
                DiffRecord {
                    status,
                    path: utf8(dst, "rename destination")?,
                    src: Some(utf8(src, "rename source")?),
                }
            }
            _ => {
                let path = tokens.next().ok_or_else(|| {
                    diff_parse(format!("status {text:?} entry is missing its path"))
                })?;
                DiffRecord {
                    status,
                    path: utf8(path, "path")?,
                    src: None,
                }
            }
        };
        map.entry(oid).or_default().push(record);
    }
    Ok(map)
}

fn utf8(tok: &[u8], what: &str) -> Result<String, ImportError> {
    std::str::from_utf8(tok)
        .map(str::to_owned)
        .map_err(|_| diff_parse(format!("{what} is not UTF-8")))
}

fn diff_parse(detail: impl std::fmt::Display) -> ImportError {
    ImportError::Diff {
        sha: String::new(),
        detail: format!("malformed diff-tree output: {detail}"),
    }
}

/// The error for a batch parse failure names the batch, not one commit;
/// callers that have a commit in hand wrap the error with its sha.
impl ImportError {
    pub(crate) fn with_commit_sha(self, sha: &GitOid) -> Self {
        match self {
            ImportError::Diff { detail, .. } => ImportError::Diff {
                sha: sha.to_hex(),
                detail,
            },
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DiffRecord, DiffStatus, parse_batch};
    use crate::oid::GitOid;

    fn oid(n: u8) -> GitOid {
        GitOid::from_hex(&format!("{:02x}", n).repeat(20)).expect("40 hex")
    }

    fn hex(n: u8) -> String {
        format!("{:02x}", n).repeat(20)
    }

    fn stream(parts: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in parts {
            out.extend_from_slice(p);
            out.push(0);
        }
        out
    }

    #[test]
    fn parses_records_and_empty_commits() {
        // Commit 1: A + M; commit 2: empty diff (id token only — the
        // observed git behavior for a *non-empty* record followed by an
        // empty one would be just the id, but even that id may be absent;
        // both shapes parse identically).
        let out = stream(&[
            hex(1).as_bytes(),
            b"A",
            b"a.txt",
            b"M",
            b"b.txt",
            hex(2).as_bytes(),
        ]);
        let map = parse_batch(&out, &[oid(1), oid(2)]).expect("parses");
        assert_eq!(map[&oid(1)].len(), 2);
        assert_eq!(
            map[&oid(1)][0],
            DiffRecord {
                status: DiffStatus::Add,
                path: "a.txt".into(),
                src: None,
            }
        );
        assert_eq!(map[&oid(1)][1].status, DiffStatus::Modify);
        assert!(map[&oid(2)].is_empty());
    }

    #[test]
    fn absent_id_means_empty_diff() {
        // git emits NOTHING for an empty-diff commit — not even its id.
        let out = stream(&[hex(1).as_bytes(), b"A", b"a.txt"]);
        let map = parse_batch(&out, &[oid(1), oid(2)]).expect("parses");
        assert!(!map.contains_key(&oid(2)));
    }

    #[test]
    fn rename_consumes_two_path_tokens() {
        let out = stream(&[
            hex(1).as_bytes(),
            b"R100",
            b"old.txt",
            b"new.txt",
            b"D",
            b"gone.txt",
        ]);
        let map = parse_batch(&out, &[oid(1)]).expect("parses");
        let recs = &map[&oid(1)];
        assert_eq!(recs.len(), 2);
        assert_eq!(
            recs[0],
            DiffRecord {
                status: DiffStatus::Rename,
                path: "new.txt".into(),
                src: Some("old.txt".into()),
            }
        );
        assert_eq!(recs[1].status, DiffStatus::Delete);
    }

    #[test]
    fn a_40hex_path_does_not_confuse_the_parser() {
        let path40 = hex(9); // a path that happens to look like a git id
        let out = stream(&[
            hex(1).as_bytes(),
            b"M",
            path40.as_bytes(),
            hex(2).as_bytes(),
            b"A",
            b"x",
        ]);
        let map = parse_batch(&out, &[oid(1), oid(2), oid(9)]).expect("parses");
        assert_eq!(map[&oid(1)].len(), 1);
        assert_eq!(map[&oid(1)][0].path, path40);
        assert_eq!(map[&oid(2)][0].path, "x");
    }

    #[test]
    fn rejects_malformed_streams() {
        // Entry before any id.
        let out = stream(&[b"A", b"a.txt"]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
        // Unknown status.
        let out = stream(&[hex(1).as_bytes(), b"Q", b"a.txt"]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
        // Rename missing its destination.
        let out = stream(&[hex(1).as_bytes(), b"R100", b"old.txt"]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
        // Truncated entry (status without path).
        let out = stream(&[hex(1).as_bytes(), b"A"]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
        // Duplicate id.
        let out = stream(&[hex(1).as_bytes(), hex(1).as_bytes()]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
        // Unrequested id.
        let out = stream(&[hex(7).as_bytes()]);
        assert!(parse_batch(&out, &[oid(1)]).is_err());
    }
}

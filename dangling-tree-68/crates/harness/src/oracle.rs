//! Subprocess-git oracle wrappers.
//!
//! All output parsing is zero-assumption: `-z`/porcelain formats are used
//! wherever git provides them, paths are handled byte-exactly (spaces,
//! non-ASCII, leading dashes), and anything that does not match the
//! documented format is an `Err`, never a silently mangled value. The
//! parsers are factored out of the subprocess calls and unit-tested
//! against synthetic byte streams offline.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// One entry of `git diff-tree -r --name-status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffEntry {
    /// Status letter: `A` (added), `M` (modified), `D` (deleted),
    /// `R` (renamed), `C` (copied), `T` (type change), … For rename/copy
    /// entries this is the first character of the status token; the
    /// similarity score (e.g. `R100`) is not carried.
    pub status: char,
    /// Destination path (for renames: the new path).
    pub path: String,
    /// Source path, present exactly for `R`/`C` statuses.
    pub src_path: Option<String>,
}

/// One coalesced run of lines attributed to the same commit in
/// first-parent blame. Line numbers are 1-based inclusive.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameRange {
    /// First line of the range (1-based, inclusive).
    pub lo: u32,
    /// Last line of the range (1-based, inclusive).
    pub hi: u32,
    /// Git commit sha attributed to these lines.
    pub commit_sha: String,
}

/// One entry of `git log`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// Commit sha (40 lowercase hex).
    pub sha: String,
    /// Parent shas in order; empty for the root commit. The first parent
    /// is the mainline.
    pub parents: Vec<String>,
    /// Raw commit message (`%B`), verbatim as stored (may end with a
    /// newline if the commit's message did).
    pub message: String,
    /// Author name (`%an`).
    pub author_name: String,
    /// Author email (`%ae`).
    pub author_email: String,
}

/// Builds a git command with a hermetic environment (no inherited
/// `GIT_*`, no system/global config, C locale).
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
    cmd.args(args);
    cmd
}

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

fn run_git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut child = git_cmd(repo, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .expect("stdin piped")
        .write_all(input)?;
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

/// Validates a 40-hex git object id and lowercases it.
pub(crate) fn parse_sha40(s: &str) -> std::io::Result<String> {
    if s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(s.to_ascii_lowercase())
    } else {
        Err(std::io::Error::other(format!(
            "git printed a non-40-hex object id: {s:?}"
        )))
    }
}

// ------------------------------------------------------------ ls-tree -z

/// Parses `git ls-tree -r -z <sha>` output: `(path, mode, git blob sha as
/// 20 raw bytes)` per record. Records are `<mode> SP <type> SP <sha40> TAB
/// <path>` NUL-terminated; paths may contain any non-NUL bytes.
pub(crate) fn parse_ls_tree_z(out: &[u8]) -> std::io::Result<Vec<(String, String, Vec<u8>)>> {
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let sp1 = record
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| std::io::Error::other("ls-tree record missing mode separator"))?;
        let mode = std::str::from_utf8(&record[..sp1])
            .map_err(|_| std::io::Error::other("ls-tree mode not UTF-8"))?
            .to_owned();
        let rest = &record[sp1 + 1..];
        let sp2 = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| std::io::Error::other("ls-tree record missing type separator"))?;
        let typ = std::str::from_utf8(&rest[..sp2])
            .map_err(|_| std::io::Error::other("ls-tree type not UTF-8"))?;
        if typ != "blob" && typ != "commit" {
            return Err(std::io::Error::other(format!(
                "ls-tree -r produced unexpected entry type {typ:?}"
            )));
        }
        let after_type = &rest[sp2 + 1..];
        let tab = after_type
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| std::io::Error::other("ls-tree record missing tab separator"))?;
        let sha_hex = std::str::from_utf8(&after_type[..tab])
            .map_err(|_| std::io::Error::other("ls-tree sha not UTF-8"))?;
        if sha_hex.len() != 40 || !sha_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(std::io::Error::other(format!(
                "expected a 40-hex object id, got {sha_hex:?}"
            )));
        }
        let mut sha = Vec::with_capacity(20);
        for i in 0..20 {
            let hi = (sha_hex.as_bytes()[2 * i] as char)
                .to_digit(16)
                .unwrap_or(0) as u8;
            let lo = (sha_hex.as_bytes()[2 * i + 1] as char)
                .to_digit(16)
                .unwrap_or(0) as u8;
            sha.push((hi << 4) | lo);
        }
        let path = std::str::from_utf8(&after_type[tab + 1..])
            .map_err(|_| std::io::Error::other("ls-tree path not UTF-8"))?
            .to_owned();
        entries.push((path, mode, sha));
    }
    Ok(entries)
}

/// `git ls-tree -r -z <sha>` — every file entry of the tree, flattened.
/// Modes are git octal strings (`100644`, `100755`, `120000`, `160000`);
/// order is git's canonical tree order (ascending by raw path bytes).
/// Paths are UTF-8 (generated histories only use UTF-8 paths); a
/// non-UTF-8 path is an error rather than a silent mangle.
pub fn git_ls_tree_r(repo: &Path, sha: &str) -> std::io::Result<Vec<(String, String, Vec<u8>)>> {
    let out = run_git(repo, &["ls-tree", "-r", "-z", sha])?;
    parse_ls_tree_z(&out)
}

// ------------------------------------------------------------ diff-tree

/// Parses `git diff-tree -r -z --name-status` output. Tokens are
/// NUL-terminated; rename/copy entries carry two path tokens (src, dst).
pub(crate) fn parse_diff_tree_z(out: &[u8]) -> std::io::Result<Vec<DiffEntry>> {
    let mut entries = Vec::new();
    let mut tokens = out.split(|b| *b == 0).filter(|t| !t.is_empty());
    while let Some(status_tok) = tokens.next() {
        let status = std::str::from_utf8(status_tok)
            .map_err(|_| std::io::Error::other("diff-tree status not UTF-8"))?;
        let first = status.bytes().next().unwrap_or(b'?');
        let path;
        let src_path;
        if first == b'R' || first == b'C' {
            let src = tokens
                .next()
                .ok_or_else(|| std::io::Error::other("diff-tree rename entry missing src"))?;
            let dst = tokens
                .next()
                .ok_or_else(|| std::io::Error::other("diff-tree rename entry missing dst"))?;
            src_path = Some(
                std::str::from_utf8(src)
                    .map_err(|_| std::io::Error::other("diff-tree src not UTF-8"))?
                    .to_owned(),
            );
            path = std::str::from_utf8(dst)
                .map_err(|_| std::io::Error::other("diff-tree dst not UTF-8"))?
                .to_owned();
        } else {
            src_path = None;
            path = std::str::from_utf8(
                tokens
                    .next()
                    .ok_or_else(|| std::io::Error::other("diff-tree entry missing path"))?,
            )
            .map_err(|_| std::io::Error::other("diff-tree path not UTF-8"))?
            .to_owned();
        }
        entries.push(DiffEntry {
            status: first as char,
            path,
            src_path,
        });
    }
    Ok(entries)
}

/// `git diff-tree` of one commit against its first parent (root commits
/// diff against the empty tree via `--root`; merges diff against the
/// first parent via `--diff-merges=first-parent`, matching Gardenbed's
/// changeset semantics). With `detect_renames`, rename/copy detection
/// (`-M`) is on and `R`/`C` entries carry `src_path`.
pub fn git_diff_tree(
    repo: &Path,
    sha: &str,
    detect_renames: bool,
) -> std::io::Result<Vec<DiffEntry>> {
    let rename_flag = if detect_renames { "-M" } else { "--no-renames" };
    let out = run_git(
        repo,
        &[
            "diff-tree",
            "--root",
            "-r",
            "-z",
            "--name-status",
            "--no-commit-id",
            "--diff-merges=first-parent",
            rename_flag,
            sha,
        ],
    )?;
    parse_diff_tree_z(&out)
}

// ---------------------------------------------------------------- blame

/// Coalesces a list of (line, sha) attributions into contiguous
/// (lo, hi, sha) ranges with 1-based inclusive bounds.
pub(crate) fn coalesce_lines(mut lines: Vec<(u32, String)>) -> Vec<BlameRange> {
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<BlameRange> = Vec::new();
    for (line, sha) in lines {
        match out.last_mut() {
            Some(r) if r.hi + 1 == line && r.commit_sha == sha => r.hi = line,
            _ => out.push(BlameRange {
                lo: line,
                hi: line,
                commit_sha: sha,
            }),
        }
    }
    out
}

/// Recognizes a blame porcelain record: `<40-hex sha> <orig> <final>
/// [<count>]`. Returns the sha, the first final-line number and the group
/// count (1 when absent — the short continuation form).
pub(crate) fn parse_porcelain_record(s: &str) -> Option<(String, u32, u32)> {
    let mut parts = s.split(' ');
    let sha = parts.next()?;
    if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let _orig: u32 = parts.next()?.parse().ok()?;
    let final_line: u32 = parts.next()?.parse().ok()?;
    let count: u32 = match parts.next() {
        Some(n) => n.parse().ok()?,
        None => 1,
    };
    if count == 0 {
        return None;
    }
    Some((sha.to_ascii_lowercase(), final_line, count))
}

/// Parses `git blame --porcelain` output into coalesced ranges. Content
/// lines are TAB-prefixed and skipped; all metadata lines are ignored
/// except the sha records. Git describes each group twice — via the
/// group-count expansion of the first record and via a short header for
/// every line — so identical attributions are deduped before coalescing.
pub(crate) fn parse_blame_porcelain(out: &[u8]) -> Vec<BlameRange> {
    let mut lines: Vec<(u32, String)> = Vec::new();
    for raw in out.split(|b| *b == b'\n') {
        if raw.is_empty() || raw[0] == b'\t' {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(raw)
            && let Some((sha, final_line, count)) = parse_porcelain_record(s)
        {
            let last = final_line.saturating_add(count - 1);
            lines.push((final_line, sha.clone()));
            for l in final_line + 1..=last {
                lines.push((l, sha.clone()));
            }
        }
    }
    lines.sort();
    lines.dedup();
    coalesce_lines(lines)
}

/// `git blame --first-parent --porcelain <rev> -- <path>`, coalesced into
/// (lo, hi, commit_sha) ranges, 1-based inclusive. An empty file yields
/// an empty vec (git prints nothing for it). `rev` must name a commit;
/// the path must exist at that rev, otherwise git errors and this
/// returns `Err`.
pub fn git_blame_first_parent(
    repo: &Path,
    rev: &str,
    path: &str,
) -> std::io::Result<Vec<BlameRange>> {
    let out = run_git(
        repo,
        &["blame", "--first-parent", "--porcelain", rev, "--", path],
    )?;
    Ok(parse_blame_porcelain(&out))
}

// ------------------------------------------------------------------ log

/// Parses `git log -z --format=%H%x1f%P%x1f%an%x1f%ae%x1f%B` output:
/// NUL-terminated records of five 0x1f-separated fields.
pub(crate) fn parse_log_z(out: &[u8]) -> std::io::Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let text = std::str::from_utf8(record)
            .map_err(|_| std::io::Error::other("log record not UTF-8"))?;
        let mut fields = text.splitn(5, '\x1f');
        let sha = fields
            .next()
            .ok_or_else(|| std::io::Error::other("log record missing sha"))?;
        let parents_raw = fields
            .next()
            .ok_or_else(|| std::io::Error::other("log record missing parents"))?;
        let name = fields
            .next()
            .ok_or_else(|| std::io::Error::other("log record missing author name"))?
            .to_owned();
        let email = fields
            .next()
            .ok_or_else(|| std::io::Error::other("log record missing author email"))?
            .to_owned();
        let message = fields
            .next()
            .ok_or_else(|| std::io::Error::other("log record missing message"))?
            .to_owned();
        entries.push(LogEntry {
            sha: parse_sha40(sha)?,
            parents: parents_raw.split_whitespace().map(str::to_owned).collect(),
            message,
            author_name: name,
            author_email: email,
        });
    }
    Ok(entries)
}

/// `git log <rev> --max-count=<limit>`, parsed structurally (sha,
/// parents, raw message, author name/email). Order is git's default
/// (reverse chronological by commit date). `limit == 0` yields an empty
/// vec.
pub fn git_log(repo: &Path, rev: &str, limit: usize) -> std::io::Result<Vec<LogEntry>> {
    let limit_arg = format!("--max-count={limit}");
    let out = run_git(
        repo,
        &[
            "log",
            "-z",
            "--no-color",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%B",
            &limit_arg,
            rev,
        ],
    )?;
    parse_log_z(&out)
}

// ----------------------------------------------------------------- misc

/// `git fsck --strict` — the repo-integrity oracle. Exits non-zero only
/// on real corruption; informational `dangling …` lines on stderr do not
/// affect the exit code.
pub fn git_fsck(repo: &Path) -> std::io::Result<()> {
    run_git(repo, &["fsck", "--strict"]).map(|_| ())
}

/// `git hash-object --stdin` (no `-w`: computes git's blob sha without
/// writing anything). Handy for milestone comparators: "compare content"
/// becomes "compare git blob shas" without touching the object store.
pub fn git_hash_object(repo: &Path, content: &[u8]) -> std::io::Result<String> {
    let out = run_git_stdin(repo, &["hash-object", "--stdin"], content)?;
    let line = std::str::from_utf8(&out)
        .map_err(|_| std::io::Error::other("hash-object output not UTF-8"))?;
    parse_sha40(line.lines().next().unwrap_or(""))
}

/// `git show <sha>:<path>` — the blob's raw bytes (an empty file yields
/// an empty vec; a symlink path yields the link-target bytes). Gitlink
/// paths fail with an error, exactly as git does (it cannot resolve the
/// foreign commit object).
pub fn git_show(repo: &Path, sha: &str, path: &str) -> std::io::Result<Vec<u8>> {
    run_git(repo, &["show", &format!("{sha}:{path}")])
}

// ------------------------------------------------------------ merge-tree

/// The outcome of `git merge-tree --write-tree <ours> <theirs>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeTreeOutcome {
    /// The merged tree's sha (present even on conflict: the first
    /// stdout line is always the best-effort tree).
    pub tree: String,
    /// False when the merge was clean (exit 0); true on conflict
    /// (exit 1).
    pub conflicted: bool,
    /// Every conflicted path, deduplicated, in first-appearance order
    /// (empty when clean). Parsed from the stage lines: conflicted
    /// paths appear with nonzero stage numbers before the message
    /// block, as `<mode> <sha> <stage>\t<path>`.
    pub conflicted_paths: Vec<String>,
}

/// `git merge-tree --write-tree <ours> <theirs>` — the merge oracle
/// (M9). The merge base is git's own auto-computed one, so the caller
/// must only compare cases where git's base equals gardenbed's (the
/// divergent-pair generator guarantees this: both sides branch off the
/// same mainline commit). Exit code 1 = conflict; anything other than
/// 0/1 is an error. `--name-only` lists the conflicted paths.
pub fn git_merge_tree(repo: &Path, ours: &str, theirs: &str) -> std::io::Result<MergeTreeOutcome> {
    let out = git_cmd(repo, &["merge-tree", "--write-tree", "--name-only", ours, theirs])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    match out.status.code() {
        Some(0) | Some(1) => {}
        other => {
            return Err(std::io::Error::other(format!(
                "git merge-tree failed (exit {:?}): {}",
                other,
                String::from_utf8_lossy(&out.stderr).trim_end()
            )))
        }
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut lines = text.lines();
    let tree = lines
        .next()
        .ok_or_else(|| std::io::Error::other("git merge-tree printed no tree sha"))?
        .to_owned();
    parse_sha40(&tree)?;
    let conflicted = out.status.code() == Some(1);
    // After the tree sha, `--name-only` lists each conflicted path on
    // its own line (a blank line may precede the messages block).
    let mut paths: Vec<String> = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if line.starts_with("CONFLICT") || line.starts_with("Auto-merging") {
            continue; // message block, not a path
        }
        if !paths.contains(&line.to_owned()) {
            paths.push(line.to_owned());
        }
    }
    Ok(MergeTreeOutcome {
        tree,
        conflicted,
        conflicted_paths: paths,
    })
}

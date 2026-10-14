//! rootstock export — rendering gardenbed's changeset stores as a real
//! git repository (the bridge's write direction).
//!
//! The whole export is **one** `git fast-import` stream. Marks live only
//! inside a single fast-import run (they do not survive between
//! invocations — verified), so blobs and commits share one mark namespace
//! and nothing may be split across runs.
//!
//! # Stream shape
//!
//! 1. **Blobs first.** Every blob referenced by a non-gitlink change is
//!    written as `blob\nmark :N\ndata <len>\n<bytes>\n`, deduplicated by
//!    blob id and marked deterministically in ascending id order. Gitlink
//!    pointers are *not* blobs: their content is the upstream git SHA-1
//!    (left-padded to 32 bytes in the model), written inline as
//!    `M 160000 <40-hex> <path>`.
//! 2. **Commits** in a deterministic topological order (iterative
//!    depth-first post-order from the ref tips, parents in stored order).
//!    Every commit record carries the *verbatim* identity bytes
//!    (`Name <email>`) and the raw `<secs> ±hhmm` dates — that is what
//!    makes unsigned histories reproduce byte-identical git SHAs. Parents
//!    become `from`/`merge` (after the message `data` block, per the
//!    fast-import grammar). A **root** changeset writes `deleteall` so its
//!    tree starts from nothing (a second root mid-stream would otherwise
//!    inherit the previous commit's tree on the streaming ref).
//!    Changes are emitted in canonical order; a rename is expressed as
//!    `M <mode> :mark <newpath>` plus `D <src>` — deliberately never
//!    fast-import's `R`, which would be tree-identical anyway and harder
//!    to keep deterministic.
//! 3. **Refs last.** Every ref is finalized with `reset <ref>` +
//!    `from :<tip mark>`, so the ref the `commit` lines streamed onto
//!    (lexicographically first) is irrelevant to the final result.
//!
//! # Divergences (see `docs/divergences.md`)
//!
//! * **Signed commits**: a changeset's `signature` cannot be expressed in
//!   a fast-import stream (fast-import never emits `gpgsig`), so signed
//!   changesets export *unsigned* and therefore get different git SHAs.
//!   Unsigned histories reproduce their SHAs exactly. The report counts
//!   the dropped signatures.
//! * Non-UTF-8 commit bytes were already transcoded lossily at import;
//!   export writes the stored `String`s verbatim.

use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Instant;

use gardenbed_model::{Changeset, Id, Identity, Mode, Op};
use gardenbed_store::{MetadataStore, ObjectStore};

use crate::error::ExportError;
use crate::options::glob_match;

/// Knobs for a rootstock export.
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    /// Glob patterns (`*`, `?`) matched against full ref names such as
    /// `refs/heads/main`; empty means every ref in the seedbed.
    pub refs: Vec<String>,
}

/// What an export produced.
///
/// `commits` is in export (topological) order — the same order the stream
/// was written in. `refs` pairs each exported ref with the git SHA it was
/// verified to point at afterwards.
#[derive(Clone, Debug)]
pub struct ExportReport {
    /// Commit records written to the stream.
    pub commits_exported: u64,
    /// Distinct blob records written (deduplicated by blob id).
    pub blobs_exported: u64,
    /// Signed changesets exported unsigned (`fast-import` cannot express
    /// a signature). Their git SHAs differ from any prior import; unsigned
    /// histories are unaffected. See `docs/divergences.md`.
    pub signed_dropped: u64,
    /// `(full ref name, git sha)` for every exported ref, sorted by name,
    /// each verified against the target repository after the import.
    pub refs: Vec<(String, String)>,
    /// `(changeset id, git sha)` for every exported commit, in export
    /// order. Shas come from fast-import's marks file.
    pub commits: Vec<(Id, String)>,
    /// Path of the marks file fast-import was told to write (`:N <sha>`
    /// per mark). It is deliberately left in place for inspection.
    pub marks_path: PathBuf,
    /// Wall-clock duration of the whole export.
    pub elapsed: std::time::Duration,
}

impl std::fmt::Display for ExportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} commits ({} signed dropped), {} blobs, {} ref(s), {:.2}s",
            self.commits_exported,
            self.signed_dropped,
            self.blobs_exported,
            self.refs.len(),
            self.elapsed.as_secs_f64(),
        )
    }
}

/// Exports the changeset graph reachable from the seedbed's refs into a
/// git repository at `git_dir` (created as a bare sha1 repository when
/// missing; an existing repository is accepted as-is, sha256 rejected).
///
/// Blobs are read from `objects`, changesets and refs from `metadata`.
/// Signed changesets are exported unsigned (see the module docs); the
/// returned report carries the changeset→git-SHA mapping, verified ref
/// tips, and the path of the marks file fast-import wrote.
pub fn export_to_path(
    git_dir: &Path,
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    opts: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    let started = Instant::now();

    // ---- refs: filter, validate, keep seedbed's sorted-by-name order.
    let all_refs = metadata.list_refs()?;
    let tips: Vec<(String, Id)> = all_refs
        .iter()
        .filter(|(name, _)| opts.refs.is_empty() || opts.refs.iter().any(|p| glob_match(p, name)))
        .cloned()
        .collect();
    for (name, _) in &tips {
        validate_ref_name(name)?;
    }

    // ---- the reachable graph, parents before children.
    let (order, cache) = topo_order(&tips, metadata.as_ref())?;
    let cache = Arc::new(cache);
    let blob_ids = collect_blob_ids(&order, &cache);

    // ---- the target repository (bare sha1 when fresh).
    prepare_target(git_dir)?;

    // ---- one fast-import run; the marks file is our commit→SHA channel.
    let marks_path = marks_file_path();
    let first_ref = tips.first().map(|(n, _)| n.as_str()).unwrap_or("");
    let commit_marks = run_fast_import(
        git_dir,
        &marks_path,
        objects,
        order.clone(),
        cache.clone(),
        blob_ids.clone(),
        first_ref,
        &tips,
    )?;
    let mark_to_sha = parse_marks(&marks_path)?;

    // ---- report: shas from the marks file, refs verified independently.
    let mut commits = Vec::with_capacity(order.len());
    for id in &order {
        let mark = commit_marks.get(id).copied().ok_or_else(|| {
            ExportError::Changeset {
                id: id.to_hex(),
                detail: "was exported but received no mark".into(),
            }
        })?;
        let sha = mark_to_sha.get(&mark).ok_or_else(|| {
            ExportError::Git(format!("the marks file has no entry for mark :{mark}"))
        })?;
        commits.push((*id, sha.clone()));
    }
    let refs = verify_refs(git_dir, &tips, &commit_marks, &mark_to_sha)?;

    // Point HEAD at the first exported branch so the bare repository is
    // immediately usable (`git log`, clones). Deterministic: the
    // lexicographically first ref.
    if let Some((first_ref, _)) = tips.first() {
        run_git(
            git_dir,
            &["symbolic-ref", "HEAD", first_ref],
            "pointing HEAD at the exported branch",
        )?;
    }

    let signed_dropped = order
        .iter()
        .filter(|id| cache[id].signature.is_some())
        .count() as u64;

    Ok(ExportReport {
        commits_exported: order.len() as u64,
        blobs_exported: blob_ids.len() as u64,
        signed_dropped,
        refs,
        commits,
        marks_path,
        elapsed: started.elapsed(),
    })
}

/// Depth-first post-order over every changeset reachable from `tips`,
/// deduplicating shared ancestry and refusing cycles. Deterministic:
/// tips in ref-name order, parents in stored order.
fn topo_order(
    tips: &[(String, Id)],
    metadata: &dyn MetadataStore,
) -> Result<(Vec<Id>, HashMap<Id, Changeset>), ExportError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        /// On the current DFS path (an ancestor of whatever is being
        /// expanded) — meeting it again is a cycle.
        Visiting,
        /// Fully emitted.
        Done,
    }

    enum Frame {
        Enter(Id),
        Emit(Id),
    }

    let mut state: HashMap<Id, State> = HashMap::new();
    let mut cache: HashMap<Id, Changeset> = HashMap::new();
    let mut order: Vec<Id> = Vec::new();

    for (_, tip) in tips {
        let mut stack = vec![Frame::Enter(*tip)];
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(id) => match state.get(&id) {
                    Some(State::Done) => {}
                    Some(State::Visiting) => {
                        return Err(ExportError::Changeset {
                            id: id.to_hex(),
                            detail: "the changeset graph has a cycle (a changeset is its own \
                                     ancestor)"
                                .into(),
                        })
                    }
                    None => {
                        state.insert(id, State::Visiting);
                        let cs = load_changeset(metadata, id, &mut cache)?;
                        stack.push(Frame::Emit(id));
                        // Push in reverse so the first parent's subtree is
                        // expanded (and finished) first. A parent listed
                        // twice by one changeset is walked once.
                        let mut pushed = HashSet::new();
                        for p in cs.parents.iter().rev() {
                            if !pushed.insert(*p) {
                                continue;
                            }
                            match state.get(p) {
                                None => stack.push(Frame::Enter(*p)),
                                Some(State::Done) => {}
                                Some(State::Visiting) => {
                                    return Err(ExportError::Changeset {
                                        id: p.to_hex(),
                                        detail: "the changeset graph has a cycle (a changeset \
                                                 is its own ancestor)"
                                            .into(),
                                    })
                                }
                            }
                        }
                    }
                },
                Frame::Emit(id) => {
                    state.insert(id, State::Done);
                    order.push(id);
                }
            }
        }
    }
    Ok((order, cache))
}

/// Fetches a changeset into the walk cache, refusing dangling refs/parents.
fn load_changeset<'a>(
    metadata: &dyn MetadataStore,
    id: Id,
    cache: &'a mut HashMap<Id, Changeset>,
) -> Result<&'a Changeset, ExportError> {
    if !cache.contains_key(&id) {
        let (cs, _) = metadata.get_changeset(id)?.ok_or_else(|| {
            ExportError::Changeset {
                id: id.to_hex(),
                detail: "missing from the seedbed (dangling ref or parent)".into(),
            }
        })?;
        cache.insert(id, cs);
    }
    Ok(&cache[&id])
}

/// Every blob a non-gitlink change references, sorted by id for
/// deterministic marks (deduplicated — several paths share one blob).
fn collect_blob_ids(order: &[Id], cache: &HashMap<Id, Changeset>) -> Vec<Id> {
    let mut set: HashSet<Id> = HashSet::new();
    for id in order {
        for c in &cache[id].changes {
            if let Some(blob) = c.blob {
                if c.mode != Mode::Gitlink {
                    set.insert(blob);
                }
            }
        }
    }
    let mut ids: Vec<Id> = set.into_iter().collect();
    ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    ids
}

/// Creates a bare sha1 repository when `git_dir` is missing; an existing
/// directory must be a sha1 git repository.
fn prepare_target(git_dir: &Path) -> Result<(), ExportError> {
    if git_dir.exists() {
        run_git(
            git_dir,
            &["rev-parse", "--git-dir"],
            "inspecting the target repository",
        )?;
        let format = String::from_utf8_lossy(
            &run_git(
                git_dir,
                &["rev-parse", "--show-object-format"],
                "inspecting the target repository",
            )?,
        )
        .trim()
        .to_owned();
        if format != "sha1" {
            return Err(ExportError::Git(format!(
                "the target repository uses the {format} object format; v0.1 exports sha1 only"
            )));
        }
    } else {
        // `git init` needs an existing cwd: run from the parent directory
        // and name the (not yet existing) repository directory explicitly.
        let parent = git_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent).map_err(|e| {
            ExportError::Git(format!("creating {}: {e}", parent.display()))
        })?;
        let name = git_dir.file_name().ok_or_else(|| {
            ExportError::Git(format!(
                "cannot derive a repository directory from {}",
                git_dir.display()
            ))
        })?;
        let args = vec![
            "init".to_owned(),
            "-q".to_owned(),
            "--bare".to_owned(),
            "--object-format=sha1".to_owned(),
            name.to_string_lossy().into_owned(),
        ];
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_git(
            parent,
            &arg_refs,
            "initializing the target repository",
        )?;
    }
    Ok(())
}

/// Spawns `git fast-import` and feeds it the export stream from a writer
/// thread, then waits. The stream is built *inside* the thread so the
/// store reads overlap with git's parsing; git's stderr is drained by its
/// own thread so a full pipe can never deadlock the writer.
fn run_fast_import(
    git_dir: &Path,
    marks_path: &Path,
    objects: Arc<dyn ObjectStore>,
    order: Vec<Id>,
    cache: Arc<HashMap<Id, Changeset>>,
    blob_ids: Vec<Id>,
    first_ref: &str,
    tips: &[(String, Id)],
) -> Result<HashMap<Id, u64>, ExportError> {
    let mut child = git_command(git_dir)
        .args([
            "fast-import",
            "--quiet",
            "--force",
            &format!("--export-marks={}", marks_path.display()),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ExportError::Git(format!("cannot spawn git fast-import: {e}")))?;
    let stdin = child
        .stdin
        .take()
        .expect("fast-import stdin was piped");
    let stderr = child
        .stderr
        .take()
        .expect("fast-import stderr was piped");

    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        let mut reader = std::io::BufReader::new(stderr);
        let _ = reader.read_to_string(&mut text);
        text
    });

    let tips = tips.to_vec();
    let first_ref = first_ref.to_owned();
    let writer = std::thread::spawn(move || {
        let mut out = BufWriter::with_capacity(1 << 16, stdin);
        let result = write_stream(
            &mut out,
            &order,
            &cache,
            &blob_ids,
            &first_ref,
            &tips,
            objects.as_ref(),
        );
        let _ = out.flush();
        drop(out);
        result
    });

    let stream_result = match writer.join() {
        Ok(result) => result,
        Err(_) => Err(ExportError::Other(
            "the fast-import stream writer panicked".into(),
        )),
    };
    let status = child
        .wait()
        .map_err(|e| ExportError::Git(format!("waiting for git fast-import: {e}")))?;
    let stderr_text = stderr_reader.join().unwrap_or_default();

    // A stream-generation failure is the root cause (git merely saw a
    // truncated stream); a healthy stream with a failed git is git's error.
    let commit_marks = stream_result?;
    if !status.success() {
        return Err(ExportError::Git(format!(
            "git fast-import failed: {}",
            stderr_text.trim()
        )));
    }
    Ok(commit_marks)
}

/// Writes the whole fast-import stream: blobs, commits, final ref resets.
/// Returns the mark assigned to each exported changeset.
fn write_stream<W: Write>(
    out: &mut W,
    order: &[Id],
    cache: &HashMap<Id, Changeset>,
    blob_ids: &[Id],
    first_ref: &str,
    tips: &[(String, Id)],
    objects: &dyn ObjectStore,
) -> Result<HashMap<Id, u64>, ExportError> {
    let mut next_mark: u64 = 1;
    let bump = |next_mark: &mut u64| {
        let m = *next_mark;
        *next_mark += 1;
        m
    };

    // ---- blobs first: marks must be declared before any commit uses them.
    let mut blob_marks: HashMap<Id, u64> = HashMap::with_capacity(blob_ids.len());
    for id in blob_ids {
        let content = objects
            .get_blob(*id)?
            .ok_or(ExportError::MissingBlob { id: *id })?;
        let mark = bump(&mut next_mark);
        write!(out, "blob\nmark :{mark}\ndata {}\n", content.len())?;
        out.write_all(&content)?;
        out.write_all(b"\n")?;
        blob_marks.insert(*id, mark);
    }

    // ---- commits in topological order, streamed onto the first ref.
    let mut commit_marks: HashMap<Id, u64> = HashMap::with_capacity(order.len());
    for id in order {
        let cs = &cache[id];
        let mark = bump(&mut next_mark);
        write!(out, "commit {first_ref}\nmark :{mark}\n")?;
        write_identity(out, id, "author", &cs.author)?;
        write_identity(out, id, "committer", &cs.committer)?;
        let message = cs.message.as_bytes();
        write!(out, "data {}\n", message.len())?;
        out.write_all(message)?;
        out.write_all(b"\n")?;
        for (i, parent) in cs.parents.iter().enumerate() {
            let parent_mark = commit_marks.get(parent).copied().ok_or_else(|| {
                ExportError::Changeset {
                    id: id.to_hex(),
                    detail: format!(
                        "parent {} has no mark — it was not exported before this commit",
                        parent
                    ),
                }
            })?;
            write!(
                out,
                "{} :{parent_mark}\n",
                if i == 0 { "from" } else { "merge" }
            )?;
        }
        if cs.parents.is_empty() {
            // A root changeset's tree is built from *nothing*; without
            // `deleteall` the commit would inherit whatever tree the
            // previous commit on this ref had accumulated (a second root
            // mid-stream would grow the wrong tree).
            write!(out, "deleteall\n")?;
        }
        for c in &cs.changes {
            match c.op {
                Op::Delete => write!(out, "D {}\n", c_quote(&c.path))?,
                Op::Add | Op::Modify | Op::Rename => {
                    if let Some(src) = &c.src_path {
                        write!(out, "D {}\n", c_quote(src))?;
                    }
                    let blob = c.blob.ok_or_else(|| ExportError::Changeset {
                        id: id.to_hex(),
                        detail: format!("change on {} carries no blob", c.path),
                    })?;
                    if c.mode == Mode::Gitlink {
                        write!(
                            out,
                            "M 160000 {} {}\n",
                            gitlink_sha(blob)?,
                            c_quote(&c.path)
                        )?;
                    } else {
                        let blob_mark = blob_marks[&blob];
                        write!(
                            out,
                            "M {} :{blob_mark} {}\n",
                            git_mode(c.mode),
                            c_quote(&c.path)
                        )?;
                    }
                }
            }
        }
        commit_marks.insert(*id, mark);
    }

    // ---- finalize every ref: the commit lines above streamed onto one
    //      ref; the resets are what actually point the branches.
    for (name, tip) in tips {
        let tip_mark = commit_marks.get(tip).copied().ok_or_else(|| {
            ExportError::Ref {
                name: name.clone(),
                detail: "its tip changeset was not exported".into(),
            }
        })?;
        write!(out, "reset {name}\nfrom :{tip_mark}\n")?;
    }
    Ok(commit_marks)
}

/// Writes one git ident line: `author Name <email> secs ±hhmm`. The name
/// and email are written **verbatim** — reproducing the original commit
/// bytes is what makes unsigned roundtrips SHA-exact.
fn write_identity(
    out: &mut impl Write,
    cs: &Id,
    role: &str,
    identity: &Identity,
) -> Result<(), ExportError> {
    for (field, value) in [("name", &identity.name), ("email", &identity.email)] {
        if value.chars().any(|c| matches!(c, '<' | '>' | '\n' | '\r')) {
            return Err(ExportError::Identity {
                cs: cs.to_hex(),
                field: format!("{role} {field}"),
                detail: format!(
                    "{value:?} carries '<', '>' or a newline; fast-import could not re-create \
                     such a commit"
                ),
            });
        }
    }
    write!(
        out,
        "{role} {} <{}> {} {}\n",
        identity.name,
        identity.email,
        identity.time_secs,
        format_tz(identity.tz_min)
    )?;
    Ok(())
}

/// Formats `tz_min` as git's raw `±hhmm` form (mirrors the harness:
/// -330 → "-0530", 90 → "+0130", 345 → "+0545").
fn format_tz(tz_min: i32) -> String {
    let sign = if tz_min < 0 { '-' } else { '+' };
    let abs = tz_min.unsigned_abs();
    format!("{}{:02}{:02}", sign, abs / 60, abs % 60)
}

/// The model's `Mode` → git's octal mode string.
fn git_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Regular => "100644",
        Mode::Exec => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// The 40-hex git SHA-1 of a gitlink pointer: the model stores the upstream
/// git SHA-1 as raw bytes left-padded with zeros to 32 bytes (spec §3);
/// the padding is stripped here. Anything not zero-padded is not a pointer
/// and is refused (zero assumptions about corrupt stores).
fn gitlink_sha(id: Id) -> Result<String, ExportError> {
    let bytes = id.as_bytes();
    if !bytes[..12].iter().all(|b| *b == 0) {
        return Err(ExportError::Changeset {
            id: id.to_hex(),
            detail: "gitlink blob id is not a zero-padded git SHA-1".into(),
        });
    }
    let mut hex = String::with_capacity(40);
    for b in &bytes[12..] {
        hex.push_str(&format!("{b:02x}"));
    }
    Ok(hex)
}

/// Quotes a path for fast-import iff it needs it (any byte ≤ 0x20 or a
/// `"`), using git's C-style escapes; all other bytes (UTF-8 included) are
/// written raw inside the quotes. Unquoted backslashes are literal, so a
/// path containing only a backslash stays unquoted.
fn c_quote(path: &str) -> String {
    let bytes = path.as_bytes();
    if !bytes.iter().any(|&b| b <= 0x20 || b == b'"') {
        return path.to_owned();
    }
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + 2);
    out.push(b'"');
    for &b in bytes {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            _ if b < 0x20 => out.extend_from_slice(format!("\\{b:03o}").as_bytes()),
            _ => out.push(b),
        }
    }
    out.push(b'"');
    // The input was a &str, so the quoted form is UTF-8 too.
    String::from_utf8(out).expect("quoted path stays UTF-8")
}

/// Validates a full ref name against git's `check-ref-format` rules;
/// anything git would reject is refused before it can break the stream.
fn validate_ref_name(name: &str) -> Result<(), ExportError> {
    let fail = |detail: String| ExportError::Ref {
        name: name.to_owned(),
        detail,
    };
    if !name.starts_with("refs/") {
        return Err(fail("the name must start with refs/".into()));
    }
    if name.ends_with('/') || name.ends_with('.') {
        return Err(fail("the name must not end with '/' or '.'".into()));
    }
    if name.contains("..") || name.contains("@{") || name == "@" {
        return Err(fail("the name contains '..', '@{' or is '@'".into()));
    }
    if name.contains("//") || name.contains('\\') {
        return Err(fail("the name contains '//' or a backslash".into()));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(fail("the name has an empty component".into()));
        }
        if component.starts_with('.') {
            return Err(fail(format!("the component {component:?} starts with '.'")));
        }
        if component.ends_with(".lock") {
            return Err(fail(format!("the component {component:?} ends with .lock")));
        }
    }
    for &b in name.as_bytes() {
        if b < 0x20 || b == 0x7f {
            return Err(fail("the name contains a control character".into()));
        }
        if matches!(
            b,
            b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'['
        ) {
            return Err(fail(format!(
                "the name contains {:?} (not allowed in ref names)",
                b as char
            )));
        }
    }
    Ok(())
}

/// Reads fast-import's marks file (`:N <sha>` per line) into a map.
fn parse_marks(path: &Path) -> Result<HashMap<u64, String>, ExportError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ExportError::Git(format!("reading the marks file {}: {e}", path.display())))?;
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let mark_raw = parts.next().unwrap_or("");
        let sha_raw = parts.next().unwrap_or("");
        if parts.next().is_some() {
            return Err(ExportError::Git(format!(
                "malformed line in {}: {line:?}",
                path.display()
            )));
        }
        let mark = mark_raw
            .strip_prefix(':')
            .ok_or_else(|| {
                ExportError::Git(format!(
                    "malformed mark {mark_raw:?} in {}",
                    path.display()
                ))
            })?
            .parse::<u64>()
            .map_err(|_| {
                ExportError::Git(format!(
                    "malformed mark {mark_raw:?} in {}",
                    path.display()
                ))
            })?;
        if sha_raw.len() != 40 || !sha_raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ExportError::Git(format!(
                "malformed object id {sha_raw:?} in {}",
                path.display()
            )));
        }
        map.insert(mark, sha_raw.to_ascii_lowercase());
    }
    Ok(map)
}

/// Verifies every exported ref with `git rev-parse` and returns the
/// `(name, sha)` pairs for the report.
fn verify_refs(
    git_dir: &Path,
    tips: &[(String, Id)],
    commit_marks: &HashMap<Id, u64>,
    mark_to_sha: &HashMap<u64, String>,
) -> Result<Vec<(String, String)>, ExportError> {
    let mut out = Vec::with_capacity(tips.len());
    for (name, tip) in tips {
        let stdout = run_git(
            git_dir,
            &["rev-parse", "--verify", name],
            "verifying an exported ref",
        )?;
        let sha = String::from_utf8_lossy(&stdout).trim().to_owned();
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ExportError::Git(format!(
                "git printed a non-40-hex object id for {name}: {sha:?}"
            )));
        }
        let tip_mark = commit_marks.get(tip).copied().ok_or_else(|| {
            ExportError::Ref {
                name: name.clone(),
                detail: "its tip changeset was not exported".into(),
            }
        })?;
        let expected = mark_to_sha.get(&tip_mark).ok_or_else(|| {
            ExportError::Git(format!("the marks file has no entry for mark :{tip_mark}"))
        })?;
        if &sha != expected {
            return Err(ExportError::Git(format!(
                "ref {name} points at {sha} but the exported commit is {expected}"
            )));
        }
        out.push((name.clone(), sha));
    }
    Ok(out)
}

/// A git command with a hermetic environment (inherited `GIT_*` variables
/// removed, global/system config pinned away).
fn git_command(cwd: &Path) -> Command {
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

/// Runs git to completion, mapping failures to [`ExportError::Git`].
fn run_git(cwd: &Path, args: &[&str], what: &str) -> Result<Vec<u8>, ExportError> {
    let out = git_command(cwd)
        .args(args)
        .output()
        .map_err(|e| ExportError::Git(format!("cannot spawn git ({what}): {e}")))?;
    if !out.status.success() {
        return Err(ExportError::Git(format!(
            "git {} failed ({}): {}",
            args.join(" "),
            what,
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(out.stdout)
}

/// The marks file lives outside the target repository, in the system
/// temporary directory, under a per-run unique name (pid + nanos); the
/// file stays on disk and its path travels in the report.
fn marks_file_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "gardenbed-export-marks-{}-{}",
        std::process::id(),
        nanos
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_quotes_only_when_needed() {
        assert_eq!(c_quote("plain.txt"), "plain.txt");
        assert_eq!(c_quote("with space.txt"), "\"with space.txt\"");
        assert_eq!(c_quote("tab\there"), "\"tab\\there\"");
        assert_eq!(c_quote("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(c_quote("carriage\rreturn"), "\"carriage\\rreturn\"");
        assert_eq!(c_quote("quote\"d"), "\"quote\\\"d\"");
        // A backslash alone is literal (unquoted); next to a control byte
        // it must be escaped.
        assert_eq!(c_quote("back\\slash"), "back\\slash");
        assert_eq!(c_quote("back\\slash\ttab"), "\"back\\\\slash\\ttab\"");
        assert_eq!(c_quote("ctl\u{1}x"), "\"ctl\\001x\"");
    }

    #[test]
    fn format_tz_matches_git_raw_dates() {
        assert_eq!(format_tz(0), "+0000");
        assert_eq!(format_tz(330), "+0530");
        assert_eq!(format_tz(-330), "-0530");
        assert_eq!(format_tz(90), "+0130");
        assert_eq!(format_tz(345), "+0545");
        assert_eq!(format_tz(-420), "-0700");
    }

    #[test]
    fn utf8_paths_stay_raw() {
        // No byte ≤ 0x20 and no quote: unquoted, UTF-8 verbatim.
        assert_eq!(c_quote("motoré.txt"), "motoré.txt");
        assert_eq!(c_quote("电机.rs"), "电机.rs");
        // Quoted for the space, but the UTF-8 bytes are not escaped.
        assert_eq!(c_quote("motor é.txt"), "\"motor é.txt\"");
    }

    #[test]
    fn gitlink_ids_strip_the_zero_padding() {
        let mut bytes = [0u8; 32];
        let sha = [0x01u8, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67,
            0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67];
        bytes[12..].copy_from_slice(&sha);
        let id = Id::from_bytes(bytes);
        assert_eq!(
            gitlink_sha(id).unwrap(),
            "0123456789abcdef0123456789abcdef01234567"
        );
        // Not zero-padded: not a pointer, refuse it.
        let mut bad = bytes;
        bad[0] = 1;
        assert!(gitlink_sha(Id::from_bytes(bad)).is_err());
    }

    #[test]
    fn ref_names_follow_gits_rules() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/heads/feature/x").is_ok());
        assert!(validate_ref_name("refs/tags/v1.0").is_ok());
        // Not under refs/.
        assert!(validate_ref_name("main").is_err());
        assert!(validate_ref_name("HEAD").is_err());
        // Invalid components and shapes.
        assert!(validate_ref_name("refs/.bed/tip").is_err(), "dot component");
        assert!(validate_ref_name("refs/heads/a.lock").is_err());
        assert!(validate_ref_name("refs/heads/a..b").is_err());
        assert!(validate_ref_name("refs/heads/a b").is_err(), "space");
        assert!(validate_ref_name("refs/heads/a~b").is_err());
        assert!(validate_ref_name("refs/heads/a^b").is_err());
        assert!(validate_ref_name("refs/heads/a:b").is_err());
        assert!(validate_ref_name("refs/heads/a?b").is_err());
        assert!(validate_ref_name("refs/heads/a*b").is_err());
        assert!(validate_ref_name("refs/heads/a[b").is_err());
        assert!(validate_ref_name("refs/heads/a\\b").is_err());
        assert!(validate_ref_name("refs/heads/@{x}").is_err());
        assert!(validate_ref_name("refs/heads//x").is_err());
        assert!(validate_ref_name("refs/heads/x/").is_err());
        assert!(validate_ref_name("refs/heads/x.").is_err());
    }
}

//! `bed sow` — the write path (v0.1.md §M5): turning the plot's
//! working-copy changes into a new changeset on the arboretum.
//!
//! The flow is a strict pipeline, and every stage before the final state
//! update is repeatable:
//!
//! 1. **scan** — the offline engine ([`crate::status::scan_paths`])
//!    classifies every path under the plot: modified content, mode-only
//!    changes, deletions, untracked files. Symlinks contribute their
//!    link-target bytes.
//! 2. **delta** — the classification becomes canonical changes
//!    (strictly ascending by path bytes, no duplicates). A *pure* rename —
//!    a deleted path and an added path carrying the exact same blob id —
//!    is paired into one `Rename`; moved-and-edited files deliberately sow
//!    as a delete+add pair (identical tree, simpler contract; see
//!    `docs/divergences.md`).
//! 3. **upload** — the two-phase protocol. Blobs at or under
//!    [`INLINE_BLOB_LIMIT`] ride on `CreateChangeset` inline; heavier ones
//!    go through `PutBlob` first. Each heavy blob is written into the
//!    local cache *before* the upload, so a crash between `PutBlob` and
//!    `CreateChangeset` is recovered by simply sowing again: the cached
//!    blob is assumed present server-side and the upload is not repeated.
//!    If that assumption is wrong (the arboretum reports the classic
//!    "neither in loam nor inline" hole), the blobs are re-uploaded and
//!    the changeset retried once — retry-safe, never a permanent failure.
//! 4. **commit the state** — only after `CreateChangeset` succeeds does
//!    anything local move: `config.current_cs`, a new heads tip, and
//!    treestate rows refreshed to the freshly observed blob ids, modes and
//!    stat metadata. A failure anywhere earlier leaves the bed exactly as
//!    it stood (cache writes aside — they are cache semantics, always
//!    safe).

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use gardenbed_model::codec::validate_changeset;
use gardenbed_model::{blob_id, Change, Changeset, Id, Identity, Mode, Op};

use crate::bed::Bed;
use crate::error::BedError;
use crate::rpc::Session;
use crate::status::{mtime_ns, scan_paths, ScanDelta};
use crate::treestate::{TreestateEntry, HINT_VERIFIED};
use gardenbed_store::ObjectStore as _;

/// Blobs at or under this many bytes ride on `CreateChangeset` inline;
/// heavier ones are uploaded with `PutBlob` first. Mirrors the arboretum's
/// `MAX_INLINE_BLOB_LEN` (a 1 MiB wire bound); the two constants must move
/// together.
pub const INLINE_BLOB_LIMIT: usize = 1 << 20;

/// Chunk size for streaming a heavy blob through `PutBlob`.
const PUT_BLOB_CHUNK: usize = 256 * 1024;

/// The author used when neither `--author` nor `GARDENBED_AUTHOR` is set.
const DEFAULT_AUTHOR: &str = "Bed User <bed@gardenbed.local>";

/// What one sow did.
#[derive(Clone, Debug)]
pub struct SowOutcome {
    /// The changeset the arboretum accepted.
    pub cs_id: Id,
    /// The changeset the bed grew from (`None`: this was the garden's
    /// first seed — a root changeset).
    pub parent: Option<Id>,
    /// How many paths were added, modified, deleted, and renamed.
    pub added: usize,
    /// See [`SowOutcome::added`].
    pub modified: usize,
    /// See [`SowOutcome::added`].
    pub deleted: usize,
    /// See [`SowOutcome::added`].
    pub renamed: usize,
    /// Blobs that rode on `CreateChangeset` inline (≤ [`INLINE_BLOB_LIMIT`]).
    pub blobs_inline: usize,
    /// Blobs sent through `PutBlob` (the heavy ones), including any the
    /// crash-recovery retry re-sent.
    pub blobs_uploaded: usize,
}

/// A working file the scan wants written into the changeset (an untracked
/// path becomes an Add, a modified path a Modify).
struct WriteEntry {
    path: String,
    blob: Id,
    /// §4 mode code observed on disk.
    mode: i64,
    content: Vec<u8>,
    /// Stat metadata captured at read time — the treestate hint triple.
    size: i64,
    mtime_ns: i64,
    /// `true` for untracked paths (→ `Add`), `false` for modified tracked
    /// ones (→ `Modify`).
    is_new: bool,
}

/// `bed sow -m <message> [--author "Name <email>"]` — scan the plot, build
/// the delta, upload the blobs, and plant a new changeset; then move the
/// bed (config, heads, treestate) onto it. After a successful sow, `bed
/// status` is clean.
///
/// The author comes from the `--author` argument, else the
/// `GARDENBED_AUTHOR` environment variable, else the bed's default
/// identity. Author and committer are the same person at the same instant
/// (v0.1 has no separate-committer workflow).
pub async fn sow(bed_dir: &Path, message: &str, author: Option<&str>) -> Result<SowOutcome, BedError> {
    if message.trim().is_empty() {
        return Err(BedError::ConfigInvalid(
            "the message is empty — sow with -m <message>".into(),
        ));
    }
    let (name, email) = resolve_author(author)?;

    let mut bed = Bed::open(bed_dir)?;
    let plot = bed.config().plot.clone();
    let treestate = bed.treestate()?;
    let root = bed.garden_root();

    // ---- scan + read: the whole delta is known before any network I/O.
    let items = scan_paths(&root, &plot, &treestate, false)?;
    let mut delete_blobs: Vec<(String, Option<Id>)> = Vec::new();
    let mut writes: Vec<WriteEntry> = Vec::new();
    for item in items {
        match item.delta {
            ScanDelta::Clean => {}
            ScanDelta::Deleted => {
                // The row existed when the walk saw it (that is what made
                // the path "tracked"); its blob id is what pairs pure
                // renames. A row that vanished since is a concurrent-goto
                // race — sow the plain delete (no pairing) and let the
                // arboretum's tree apply decide.
                let blob = treestate.lookup(&item.path)?.map(|row| row.blob_id);
                delete_blobs.push((item.path, blob));
            }
            ScanDelta::Modified { .. } | ScanDelta::Untracked { .. } => {
                let is_new = matches!(item.delta, ScanDelta::Untracked { .. });
                // Re-read the working file: the bytes that get uploaded are
                // the bytes present *now*, re-hashed here so the claimed
                // content address is the uploaded content's address, never
                // a stale scan-time one. A file that changed between scan
                // and read is sown as it now is; one that vanished is
                // sown as a delete.
                match read_working(&root, &item.path)? {
                    Some(entry) => writes.push(WriteEntry {
                        path: item.path,
                        blob: entry.blob_id,
                        mode: entry.mode,
                        content: entry.content,
                        size: entry.size,
                        mtime_ns: entry.mtime_ns,
                        is_new,
                    }),
                    None => delete_blobs.push((item.path, None)),
                }
            }
        }
    }

    let (changes, renamed) = build_changes(delete_blobs, &mut writes)?;
    if changes.is_empty() {
        return Err(BedError::NothingToSow);
    }

    // ---- the changeset itself: parent is the bed's current changeset (a
    // bed that has never grown anywhere sows a root changeset).
    let session = Session::open(&bed).await?;
    let repo = bed.config().repo.clone();
    let parent = match bed.config().current_cs.as_deref() {
        Some(hex) => {
            let id = Id::from_str(hex).map_err(|_| BedError::NoCurrentCs)?;
            // A parent the arboretum no longer knows (restarted server)
            // must fail before anything is uploaded.
            session.get_changeset(id, "sow").await?;
            Some(id)
        }
        None => None,
    };

    let (time_secs, tz_min) = now_stamp();
    let identity = Identity {
        name,
        email,
        time_secs,
        tz_min,
    };
    let cs = Changeset {
        parents: parent.into_iter().collect(),
        changes: changes.clone(),
        message: message.to_owned(),
        author: identity.clone(),
        committer: identity,
        signature: None,
    };
    // The scan only ever produces flat relative paths, but the codec is the
    // authority on changeset shape — fail here, before any upload, if
    // anything is off.
    validate_changeset(&cs, true)
        .map_err(|e| BedError::ConfigInvalid(format!("the sown changeset is not valid: {e}")))?;

    // ---- two-phase upload. `inline` covers every small blob (the server
    // skips inline content for blobs it already has); heavy blobs go first,
    // cache-write before wire so a killed sow never uploads twice.
    let objects = bed.objects()?;
    let mut inline: Vec<(Id, Vec<u8>)> = Vec::new();
    let mut heavy: Vec<(Id, Vec<u8>)> = Vec::new();
    let mut seen: HashMap<Id, ()> = HashMap::new();
    for w in &writes {
        if seen.insert(w.blob, ()).is_some() {
            continue; // same content at several paths uploads once
        }
        if w.content.len() <= INLINE_BLOB_LIMIT {
            inline.push((w.blob, w.content.clone()));
        } else {
            heavy.push((w.blob, w.content.clone()));
        }
    }
    let mut uploaded = 0usize;
    for (id, content) in &heavy {
        // Cache-first: once the bytes are in `.bed/objects`, a rerun after
        // a crash assumes the upload landed and never re-sends (the one
        // exception is caught by the retry below).
        if !objects.has(*id)? {
            objects.put_blob(content)?;
            put_blob_heavy(&session, &repo, *id, content).await?;
            uploaded += 1;
        }
    }

    let cs_id = match session.client.create_changeset(&repo, &cs, inline.clone()).await {
        Ok(id) => id,
        Err(status)
            if status.code() == tonic::Code::InvalidArgument
                && status.message().contains("neither in loam nor inline") =>
        {
            // A previous run died between its cache write and its upload:
            // the cache claims a blob the arboretum never received. Push
            // every heavy blob again and retry once — from here on the
            // server provably has them.
            for (id, content) in &heavy {
                put_blob_heavy(&session, &repo, *id, content).await?;
                uploaded += 1;
            }
            match session.client.create_changeset(&repo, &cs, inline.clone()).await {
                Ok(id) => id,
                Err(status) => {
                    return Err(BedError::Rpc {
                        command: "sow",
                        code: status.code(),
                        message: status.message().to_owned(),
                    })
                }
            }
        }
        Err(status) => {
            return Err(BedError::Rpc {
                command: "sow",
                code: status.code(),
                message: status.message().to_owned(),
            })
        }
    };
    // The inline blobs are now known-good on the arboretum; keep them in
    // the local cache too so future reads never re-fetch them.
    for (id, content) in &inline {
        if !objects.has(*id)? {
            objects.put_blob(content)?;
        }
    }

    // ---- commit the local state. From here on the sow has happened.
    bed.config_mut().current_cs = Some(cs_id.to_hex());
    bed.save_config()?;
    bed.heads().append(cs_id)?;
    for c in &changes {
        match c.op {
            Op::Delete => {
                treestate.remove(&c.path)?;
            }
            Op::Add | Op::Modify | Op::Rename => {
                if let Some(src) = &c.src_path {
                    treestate.remove(src)?;
                }
                let w = writes
                    .iter()
                    .find(|w| w.path == c.path)
                    .expect("every sown write came from a scanned working file");
                treestate.upsert(&TreestateEntry {
                    path: c.path.clone(),
                    blob_id: w.blob,
                    mode: w.mode,
                    size: w.size,
                    mtime_ns: w.mtime_ns,
                    hint: HINT_VERIFIED,
                })?;
            }
        }
    }

    Ok(SowOutcome {
        cs_id,
        parent,
        added: changes.iter().filter(|c| c.op == Op::Add).count(),
        modified: changes.iter().filter(|c| c.op == Op::Modify).count(),
        deleted: changes.iter().filter(|c| c.op == Op::Delete).count(),
        renamed,
        blobs_inline: inline.len(),
        blobs_uploaded: uploaded,
    })
}

/// Renders a [`SowOutcome`] in the garden voice the CLI prints.
pub fn render_outcome(outcome: &SowOutcome) -> String {
    let n = outcome.added + outcome.modified + outcome.deleted + outcome.renamed;
    let mut parts = Vec::new();
    if outcome.added > 0 {
        parts.push(format!("{} added", outcome.added));
    }
    if outcome.modified > 0 {
        parts.push(format!("{} modified", outcome.modified));
    }
    if outcome.deleted > 0 {
        parts.push(format!("{} deleted", outcome.deleted));
    }
    if outcome.renamed > 0 {
        parts.push(format!("{} renamed", outcome.renamed));
    }
    let was = match outcome.parent {
        Some(p) => format!(" (was {})", p.short()),
        None => " — the garden's first changeset".to_owned(),
    };
    let mut out = format!(
        "planted seed {}: {} change(s) ({})\nthe bed now grows from {}{}\n",
        outcome.cs_id.short(),
        n,
        parts.join(", "),
        outcome.cs_id.short(),
        was,
    );
    if outcome.blobs_uploaded > 0 {
        out.push_str(&format!(
            "{} heavy blob(s) went over the wire first, {} rode inline\n",
            outcome.blobs_uploaded, outcome.blobs_inline,
        ));
    }
    out
}

/// The `author` ident: `--author` wins, then `GARDENBED_AUTHOR`, then the
/// bed's default.
pub fn resolve_author(explicit: Option<&str>) -> Result<(String, String), BedError> {
    match explicit {
        Some(raw) => parse_author(raw),
        None => match std::env::var("GARDENBED_AUTHOR") {
            Ok(raw) if !raw.trim().is_empty() => parse_author(&raw),
            _ => parse_author(DEFAULT_AUTHOR),
        },
    }
}

/// Parses a `Name <email>` identity. The strictness is not pedantry: a
/// name or email carrying `<`, `>` or a newline would produce a changeset
/// `git fast-import` cannot re-create, breaking the export roundtrip (the
/// bed refuses them at birth instead).
pub fn parse_author(raw: &str) -> Result<(String, String), BedError> {
    let fail = |reason: String| BedError::Author {
        raw: raw.to_owned(),
        reason,
    };
    let trimmed = raw.trim();
    let (lt, gt) = match (trimmed.find('<'), trimmed.rfind('>')) {
        (Some(lt), Some(gt)) if gt > lt => (lt, gt),
        _ => return Err(fail("expected the form Name <email>".into())),
    };
    if gt != trimmed.len() - 1 {
        return Err(fail("unexpected text after the email's '>'".into()));
    }
    let name = trimmed[..lt].trim();
    let email = trimmed[lt + 1..gt].trim();
    if name.is_empty() {
        return Err(fail("the name is empty".into()));
    }
    if email.is_empty() {
        return Err(fail("the email is empty".into()));
    }
    for (field, value) in [("name", name), ("email", email)] {
        if value.chars().any(|c| matches!(c, '<' | '>' | '\n' | '\r')) {
            return Err(fail(format!(
                "the {field} carries '<', '>' or a newline — such a changeset \
                 could not be exported to git"
            )));
        }
    }
    Ok((name.to_owned(), email.to_owned()))
}

/// The wall-clock identity stamp: Unix seconds plus the local zone offset
/// in minutes east of UTC (git-native, never normalized).
fn now_stamp() -> (u64, i32) {
    let now = chrono::Local::now();
    let secs = now.timestamp().max(0) as u64;
    let tz_min = now.offset().local_minus_utc() / 60;
    (secs, tz_min)
}

/// One working file's bytes and stat state, read in a single pass.
struct WorkingFile {
    blob_id: Id,
    mode: i64,
    content: Vec<u8>,
    size: i64,
    mtime_ns: i64,
}

/// Reads the working entry at `root/path`: bytes hashed, mode decoded, stat
/// metadata captured. A symlink's content is its link-target bytes.
/// `Ok(None)` when the entry vanished (or was replaced by a directory)
/// after the scan.
fn read_working(root: &Path, rel: &str) -> Result<Option<WorkingFile>, BedError> {
    let meta = match std::fs::symlink_metadata(root.join(rel)) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.is_dir() {
        return Ok(None);
    }
    let is_symlink = meta.is_symlink();
    let content: Vec<u8> = if is_symlink {
        std::os::unix::ffi::OsStrExt::as_bytes(std::ffi::OsStr::new(
            &std::fs::read_link(root.join(rel))?,
        ))
        .to_vec()
    } else {
        std::fs::read(root.join(rel))?
    };
    Ok(Some(WorkingFile {
        blob_id: blob_id(&content),
        mode: if is_symlink {
            crate::treestate::MODE_SYMLINK
        } else {
            mode_of(&meta)
        },
        content,
        size: meta.len() as i64,
        mtime_ns: mtime_ns(&meta),
    }))
}

/// The §4 mode code of an on-disk regular file (exec bit → exec).
fn mode_of(meta: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        let bits = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
        if bits & 0o111 != 0 {
            crate::treestate::MODE_EXEC
        } else {
            crate::treestate::MODE_REGULAR
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        crate::treestate::MODE_REGULAR
    }
}

/// Streams one heavy blob to the arboretum's `PutBlob`.
async fn put_blob_heavy(
    session: &Session,
    repo: &str,
    id: Id,
    content: &[u8],
) -> Result<(), BedError> {
    let chunks: Vec<Vec<u8>> = content
        .chunks(PUT_BLOB_CHUNK)
        .map(<[u8]>::to_vec)
        .collect();
    match session.client.put_blob(repo, tokio_stream::iter(chunks)).await {
        Ok(received) if received == id => Ok(()),
        Ok(received) => Err(BedError::BlobHashMismatch(received)),
        Err(status) => Err(BedError::Rpc {
            command: "sow",
            code: status.code(),
            message: status.message().to_owned(),
        }),
    }
}

/// Turns the scan's classification into canonical changes.
///
/// * a deleted path and an added path carrying the **exact same blob id**
///   pair into one `Rename` (a pure move; no similarity heuristic — a
///   moved-and-edited file sows as delete + add, which builds the
///   identical tree);
/// * pairing is deterministic: both sides sorted by path bytes, greedily
///   matched in order;
/// * the result is sorted strictly ascending by path bytes (the codec's
///   canonical form; paths are disjoint across the sources by
///   construction, so no duplicates can arise).
///
/// Deletes whose treestate row was gone (`None` blob) never pair — there
/// is nothing to move from.
fn build_changes(
    mut delete_blobs: Vec<(String, Option<Id>)>,
    writes: &mut Vec<WriteEntry>,
) -> Result<(Vec<Change>, usize), BedError> {
    delete_blobs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    writes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut consumed = vec![false; delete_blobs.len()];
    let mut renames: Vec<(String, usize)> = Vec::new(); // (src, index into writes)
    let mut plain: Vec<usize> = Vec::new();
    for (wi, w) in writes.iter().enumerate() {
        let mut hit = None;
        for (di, (dp, db)) in delete_blobs.iter().enumerate() {
            if !consumed[di] && *dp != w.path {
                if let Some(db) = db {
                    if *db == w.blob {
                        hit = Some(di);
                        break;
                    }
                }
            }
        }
        match hit {
            Some(di) => {
                consumed[di] = true;
                renames.push((delete_blobs[di].0.clone(), wi));
            }
            None => plain.push(wi),
        }
    }
    let renamed = renames.len();

    let mut changes: Vec<Change> =
        Vec::with_capacity(plain.len() + renames.len() + delete_blobs.len());
    for (di, (path, _)) in delete_blobs.iter().enumerate() {
        if !consumed[di] {
            changes.push(Change {
                path: path.clone(),
                op: Op::Delete,
                blob: None,
                src_path: None,
                mode: Mode::Regular,
            });
        }
    }
    for wi in plain {
        let w = &writes[wi];
        changes.push(Change {
            path: w.path.clone(),
            op: if w.is_new { Op::Add } else { Op::Modify },
            blob: Some(w.blob),
            src_path: None,
            mode: mode_code_to_mode(w.mode),
        });
    }
    for (src, wi) in &renames {
        let w = &writes[*wi];
        changes.push(Change {
            path: w.path.clone(),
            op: Op::Rename,
            blob: Some(w.blob),
            src_path: Some(src.clone()),
            mode: mode_code_to_mode(w.mode),
        });
    }
    changes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok((changes, renamed))
}

/// The treestate's §4 mode code → the model's [`Mode`].
fn mode_code_to_mode(code: i64) -> Mode {
    match code {
        crate::treestate::MODE_EXEC => Mode::Exec,
        crate::treestate::MODE_SYMLINK => Mode::Symlink,
        crate::treestate::MODE_GITLINK => Mode::Gitlink,
        _ => Mode::Regular,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str, content: &[u8], is_new: bool) -> WriteEntry {
        WriteEntry {
            path: path.to_owned(),
            blob: blob_id(content),
            mode: 0,
            content: content.to_vec(),
            size: content.len() as i64,
            mtime_ns: 0,
            is_new,
        }
    }

    #[test]
    fn pure_rename_pairs_by_identical_blob() {
        let mut writes = vec![write("b.txt", b"same", true)];
        let deletes = vec![("a.txt".to_owned(), Some(blob_id(b"same")))];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 1);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].op, Op::Rename);
        assert_eq!(changes[0].path, "b.txt");
        assert_eq!(changes[0].src_path.as_deref(), Some("a.txt"));
        assert_eq!(changes[0].blob, Some(blob_id(b"same")));
    }

    #[test]
    fn moved_and_edited_sows_as_delete_plus_add() {
        let mut writes = vec![write("b.txt", b"edited!", true)];
        let deletes = vec![("a.txt".to_owned(), Some(blob_id(b"original")))];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Delete); // a.txt < b.txt
        assert_eq!(changes[1].op, Op::Add);
    }

    #[test]
    fn changes_are_canonical_and_disjoint() {
        let mut writes = vec![
            write("z.md", b"z", true),
            write("m.rs", b"m", false),
        ];
        let deletes = vec![
            ("k.txt".to_owned(), Some(blob_id(b"k"))),
            ("a.rs".to_owned(), Some(blob_id(b"m.rs-content"))),
        ];
        // The a.rs delete pairs with nothing (different blob), so it stays
        // a delete; z.md is an add; m.rs a modify.
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "k.txt", "m.rs", "z.md"]);
        let mut bytes = paths.iter().map(|p| p.as_bytes()).collect::<Vec<_>>();
        bytes.dedup();
        assert_eq!(bytes.len(), paths.len(), "no duplicate paths");
    }

    #[test]
    fn delete_without_a_row_never_pairs() {
        let mut writes = vec![write("b.txt", b"content", true)];
        let deletes = vec![("a.txt".to_owned(), None)];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Delete);
        assert_eq!(changes[1].op, Op::Add);
    }

    #[test]
    fn rename_keeps_the_new_mode() {
        let mut w = write("b.txt", b"same", true);
        w.mode = crate::treestate::MODE_EXEC;
        let mut writes = vec![w];
        let deletes = vec![("a.txt".to_owned(), Some(blob_id(b"same")))];
        let (changes, _) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(changes[0].mode, Mode::Exec);
    }

    #[test]
    fn author_parsing_rejects_unexportable_identities() {
        assert_eq!(
            parse_author("A. Gardener <a@bed.example>").unwrap(),
            ("A. Gardener".to_owned(), "a@bed.example".to_owned())
        );
        // Whitespace tolerance.
        assert_eq!(
            parse_author("  Spaced Name   <s@x> ").unwrap(),
            ("Spaced Name".to_owned(), "s@x".to_owned())
        );
        for bad in [
            "no email at all",
            "<only-email@example>",
            "Name <email> trailing",
            "Name <email> more",
            "Bad <e> <x>",
            "New\nline <e>",
            "Name <e\nmail>",
            "Name <e>extra",
        ] {
            assert!(parse_author(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(parse_author("Name <>").is_err(), "empty email");
        assert!(parse_author(" <e>").is_err(), "empty name");
    }

    #[test]
    fn resolve_author_prefers_argument_then_env_then_default() {
        // SAFETY: single-threaded test body; the variable is unset at the end.
        unsafe {
            std::env::remove_var("GARDENBED_AUTHOR");
        }
        let (name, email) = resolve_author(None).unwrap();
        assert_eq!(name, "Bed User");
        assert_eq!(email, "bed@gardenbed.local");

        unsafe {
            std::env::set_var("GARDENBED_AUTHOR", "Env User <env@x>");
        }
        let (name, _) = resolve_author(None).unwrap();
        assert_eq!(name, "Env User");
        // The argument beats the environment.
        let (name, _) = resolve_author(Some("Flag User <flag@x>")).unwrap();
        assert_eq!(name, "Flag User");

        // An empty env value falls through to the default.
        unsafe {
            std::env::set_var("GARDENBED_AUTHOR", "   ");
            std::env::remove_var("GARDENBED_AUTHOR");
        }
        let (name, _) = resolve_author(None).unwrap();
        assert_eq!(name, "Bed User");
    }
}

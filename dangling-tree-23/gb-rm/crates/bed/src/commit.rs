//! `bed commit` — the write path (v0.1.md §M5): turning the selected paths'
//! working-copy changes into a new changeset on the server.
//!
//! The flow is a strict pipeline, and every stage before the final state
//! update is repeatable:
//!
//! 1. **scan** — the offline engine ([`crate::status::scan_paths`])
//!    classifies every path under the selected paths: modified content, mode-only
//!    changes, deletions, untracked files. Symlinks contribute their
//!    link-target bytes.
//! 2. **delta** — the classification becomes canonical changes
//!    (strictly ascending by path bytes, no duplicates). A *pure* rename —
//!    a deleted path and an added path carrying the exact same blob id —
//!    is paired into one `Rename`; moved-and-edited files deliberately commit
//!    as a delete+add pair (identical tree, simpler contract; see
//!    `docs/divergences.md`).
//! 3. **upload** — the two-phase protocol. Blobs at or under
//!    [`INLINE_BLOB_LIMIT`] ride on `CreateChangeset` inline; heavier ones
//!    go through `PutBlob` first. Each heavy blob is written into the
//!    local cache *before* the upload, so a crash between `PutBlob` and
//!    `CreateChangeset` is recovered by simply committing again: the cached
//!    blob is assumed present server-side and the upload is not repeated.
//!    If that assumption is wrong (the server reports the classic
//!    "neither in objects nor inline" hole), the blobs are re-uploaded and
//!    the changeset retried once — retry-safe, never a permanent failure.
//! 4. **commit the state** — only after `CreateChangeset` succeeds does
//!    anything local move: `config.current_cs`, a new heads tip, and
//!    treestate rows refreshed to the freshly observed blob ids, modes and
//!    stat metadata. A failure anywhere earlier leaves the working copy exactly as
//!    it stood (cache writes aside — they are cache semantics, always
//!    safe).

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use gardenbed_model::codec::validate_changeset;
use gardenbed_model::{Change, Changeset, Id, Identity, Mode, Op, blob_id};
use gardenbed_store::ObjectStore as _;

use crate::error::ClientError;
use crate::queue::PendingFile;
use crate::render;
use crate::rpc::Session;
use crate::status::{ScanDelta, mtime_ns, scan_paths};
use crate::treestate::{HINT_VERIFIED, TreestateEntry};
use crate::workspace::WorkingCopy;

/// Blobs at or under this many bytes ride on `CreateChangeset` inline;
/// heavier ones are uploaded with `PutBlob` first. Mirrors the server's
/// `MAX_INLINE_BLOB_LEN` (a 1 MiB wire bound); the two constants must move
/// together.
pub const INLINE_BLOB_LIMIT: usize = 1 << 20;

/// Leave room for changeset metadata and protobuf map overhead beneath
/// tonic's default 4 MiB unary request limit. Additional small blobs take
/// the same streamed upload path as individually large blobs.
const INLINE_TOTAL_LIMIT: usize = 2 << 20;

/// A batch of distinct blobs, keyed by their content-addressed id.
pub(crate) type BlobList = Vec<(Id, Vec<u8>)>;

/// Chunk size for streaming a heavy blob through `PutBlob`.
const PUT_BLOB_CHUNK: usize = 256 * 1024;

/// The author used when neither `--author` nor `GARDENBED_AUTHOR` is set.
const DEFAULT_AUTHOR: &str = "Gardenbed User <bed@gardenbed.local>";

/// What one commit did.
#[derive(Clone, Debug)]
pub struct CommitOutcome {
    /// The changeset the server accepted.
    pub cs_id: Id,
    /// The changeset the working copy grew from (`None`: this was the repository's
    /// first seed — a root changeset).
    pub parent: Option<Id>,
    /// How many paths were added, modified, deleted, and renamed.
    pub added: usize,
    /// See [`CommitOutcome::added`].
    pub modified: usize,
    /// See [`CommitOutcome::added`].
    pub deleted: usize,
    /// See [`CommitOutcome::added`].
    pub renamed: usize,
    /// Blobs that rode on `CreateChangeset` inline (≤ [`INLINE_BLOB_LIMIT`]).
    pub blobs_inline: usize,
    /// Blobs sent through `PutBlob` (the heavy ones), including any the
    /// crash-recovery retry re-sent.
    pub blobs_uploaded: usize,
    /// `true`: the server was unreachable at connect time and the commit
    /// went to the offline queue instead (`bed sync` uploads it later).
    pub queued: bool,
    /// When queued: how many commits (this one included) now wait for
    /// `bed sync`.
    pub pending: usize,
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

/// `bed commit -m <message> [--author "Name <email>"]` — scan the sparse selection, build
/// the delta, upload the blobs, and plant a new changeset; then move the
/// bed (config, heads, treestate) onto it. After a successful commit, `bed
/// status` is clean.
///
/// When the server is unreachable *at connect time* — and only then; a
/// server that answers, even to refuse, is online and its error is
/// surfaced — the same three phases run offline: prepare (the changeset
/// is built and content-addressed locally, every changed blob staged
/// into `.bed/objects`), queue (one sqlite transaction records the
/// pending row and its treestate patch), and the exact online state
/// move. Chained offline commits parent onto the previous pending tip;
/// `bed sync` re-uploads later.
///
/// The author comes from the `--author` argument, else the
/// `GARDENBED_AUTHOR` environment variable, else the working copy's default
/// identity. Author and committer are the same person at the same instant
/// (v0.1 has no separate-committer workflow).
pub async fn commit(
    state_dir: &Path,
    message: &str,
    author: Option<&str>,
) -> Result<CommitOutcome, ClientError> {
    if message.trim().is_empty() {
        return Err(ClientError::EmptyMessage);
    }
    let (name, email) = resolve_author(author)?;

    let mut bed = WorkingCopy::open(state_dir)?;
    // Open-time recovery (v0.2 §6): a previous run that died between its
    // queue insert and its state move is completed before anything new
    // happens. Cheap when the queue is empty.
    crate::queue::recover(&mut bed)?;
    let sparse = bed.config().sparse.clone();
    let treestate = bed.treestate()?;
    let root = bed.workspace_root();

    // ---- prepare: scan → changes → changeset construction. The whole
    // delta is known, and the changeset content-addressed, before any
    // network I/O — this is exactly what an offline commit queues.
    let items = scan_paths(&root, &sparse, &treestate, false)?;
    let mut deletes: Vec<(String, Option<Id>, Option<i64>)> = Vec::new();
    let mut writes: Vec<WriteEntry> = Vec::new();
    for item in items {
        match item.delta {
            ScanDelta::Clean => {}
            ScanDelta::Deleted => {
                // The row existed when the walk saw it (that is what made
                // the path "tracked"); its blob id is what pairs pure
                // renames. A row that vanished since is a concurrent-switch
                // race — commit the plain delete (no pairing) and let the
                // server's tree apply decide.
                let row = treestate.lookup(&item.path)?;
                let blob = row.as_ref().map(|row| row.blob_id);
                let mode = row.as_ref().map(|row| row.mode);
                deletes.push((item.path, blob, mode));
            }
            ScanDelta::Modified { .. } | ScanDelta::Untracked { .. } => {
                let is_new = matches!(item.delta, ScanDelta::Untracked { .. });
                // Re-read the working file: the bytes that get uploaded are
                // the bytes present *now*, re-hashed here so the claimed
                // content address is the uploaded content's address, never
                // a stale scan-time one. A file that changed between scan
                // and read is created as it now is; one that vanished is
                // created as a delete.
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
                    None => deletes.push((item.path, None, None)),
                }
            }
        }
    }

    let (changes, renamed) = build_changes(deletes, &mut writes)?;
    if changes.is_empty() {
        return Err(ClientError::NothingToCommit);
    }

    // The parent: the previous pending tip when the queue is non-empty
    // (chained offline commits — the server does not have that tip yet),
    // else the working copy's current changeset (a bed that has never
    // grown anywhere commits a root changeset).
    let mut queue = bed.queue()?;
    let mut parent_from_config = false;
    let parent = match queue.tip()? {
        Some(tip) => Some(tip),
        None => match bed.config().current_cs.as_deref() {
            Some(hex) => {
                parent_from_config = true;
                Some(Id::from_str(hex).map_err(|_| ClientError::NoCurrentCs)?)
            }
            None => None,
        },
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
    let invalid = |e: gardenbed_model::codec::CodecError| {
        ClientError::ConfigInvalid(format!("the created changeset is not valid: {e}"))
    };
    validate_changeset(&cs, true).map_err(invalid)?;
    let canonical = cs.canonical_bytes().map_err(invalid)?;
    let cs_id = cs.id().map_err(invalid)?;

    // The treestate patch this commit's state move applies — also the
    // `pending_files` rows when the commit is queued.
    let files: Vec<PendingFile> = writes
        .iter()
        .map(|w| PendingFile {
            path: w.path.clone(),
            blob_id: w.blob,
            mode: w.mode,
            size: w.size,
            mtime_ns: w.mtime_ns,
        })
        .collect();

    // The distinct changed blobs, classified exactly as the upload will
    // send them (same content at several paths uploads once).
    let mut blobs: Vec<(Id, Vec<u8>)> = Vec::new();
    let mut seen: HashMap<Id, ()> = HashMap::new();
    for w in &writes {
        if seen.insert(w.blob, ()).is_some() {
            continue;
        }
        blobs.push((w.blob, w.content.clone()));
    }
    let (inline, heavy) = split_inline(blobs);

    // ---- connect. A connect-time failure is the one condition that
    // queues: the server is unreachable, not refusing.
    let session = match Session::open(&bed).await {
        Ok(session) => session,
        Err(e) if crate::queue::is_connect_error(&e) => {
            // ---- offline: stage every changed blob into the local cache
            // (sync re-uploads from there), record the queue row, and
            // perform the exact online state move.
            let objects = bed.objects()?;
            for (id, content) in inline.iter().chain(&heavy) {
                if !objects.has(*id)? {
                    objects.put_blob(content)?;
                }
            }
            let pending = queue.enqueue(cs_id, parent, &canonical, message, &files)?;
            move_state(&mut bed, &treestate, &changes, &files, cs_id, false)?;
            return Ok(CommitOutcome {
                cs_id,
                parent,
                added: changes.iter().filter(|c| c.op == Op::Add).count(),
                modified: changes.iter().filter(|c| c.op == Op::Modify).count(),
                deleted: changes.iter().filter(|c| c.op == Op::Delete).count(),
                renamed,
                blobs_inline: inline.len(),
                blobs_uploaded: 0,
                queued: true,
                pending,
            });
        }
        Err(e) => return Err(e),
    };

    // A parent the server no longer knows (restarted server) must fail
    // before anything is uploaded. Skipped for a pending tip — the server
    // legitimately does not have it yet.
    if parent_from_config && let Some(id) = parent {
        session.get_changeset(id, "commit").await?;
    }

    // ---- upload. `inline` covers small blobs within the total request
    // budget (the server skips content it already has); other blobs go first,
    // cache-write before wire so a killed commit never uploads twice.
    let repo = bed.config().repo.clone();
    let objects = bed.objects()?;
    let mut uploaded = 0usize;
    for (id, content) in &heavy {
        // Cache-first: once the bytes are in `.bed/objects`, a rerun after
        // a crash assumes the upload landed and never re-sends (the one
        // exception is caught by the retry below).
        if !objects.has(*id)? {
            objects.put_blob(content)?;
            put_blob_heavy(&session, &repo, *id, content, "commit").await?;
            uploaded += 1;
        }
    }

    let ack = match session
        .client
        .create_changeset(&repo, &cs, inline.clone())
        .await
    {
        Ok(id) => id,
        Err(status)
            if status.code() == tonic::Code::InvalidArgument
                && status.message().contains("neither in objects nor inline") =>
        {
            // A previous run died between its cache write and its upload:
            // the cache claims a blob the server never received. Push
            // every heavy blob again and retry once — from here on the
            // server provably has them.
            for (id, content) in &heavy {
                put_blob_heavy(&session, &repo, *id, content, "commit").await?;
                uploaded += 1;
            }
            match session
                .client
                .create_changeset(&repo, &cs, inline.clone())
                .await
            {
                Ok(id) => id,
                Err(status) => {
                    return Err(ClientError::Rpc {
                        command: "commit",
                        code: status.code(),
                        message: status.message().to_owned(),
                    });
                }
            }
        }
        Err(status) => {
            return Err(ClientError::Rpc {
                command: "commit",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    };
    // Content-addressed: the server's answer must be the id prepare
    // computed locally, byte-exactly.
    if ack != cs_id {
        return Err(ClientError::BlobHashMismatch(ack));
    }
    // The inline blobs are now known-good on the server; keep them in
    // the local cache too so future reads never re-fetch them.
    for (id, content) in &inline {
        if !objects.has(*id)? {
            objects.put_blob(content)?;
        }
    }

    // ---- commit the local state. From here on the commit has happened.
    move_state(&mut bed, &treestate, &changes, &files, cs_id, false)?;

    Ok(CommitOutcome {
        cs_id,
        parent,
        added: changes.iter().filter(|c| c.op == Op::Add).count(),
        modified: changes.iter().filter(|c| c.op == Op::Modify).count(),
        deleted: changes.iter().filter(|c| c.op == Op::Delete).count(),
        renamed,
        blobs_inline: inline.len(),
        blobs_uploaded: uploaded,
        queued: false,
        pending: 0,
    })
}

/// Moves the bed onto `cs_id` after the changeset exists (on the server,
/// or in the queue): `config.current_cs`, a new heads tip, and treestate
/// rows refreshed to the freshly observed blob ids, modes and stat
/// metadata. Every write is idempotent, so recovery replays it freely;
/// in recovery (`recovering`) the heads append becomes check-then-append,
/// the one write that is not naturally idempotent.
pub(crate) fn move_state(
    bed: &mut WorkingCopy,
    treestate: &crate::treestate::Treestate,
    changes: &[Change],
    files: &[PendingFile],
    cs_id: Id,
    recovering: bool,
) -> Result<(), ClientError> {
    bed.config_mut().current_cs = Some(cs_id.to_hex());
    bed.save_config()?;
    let heads = bed.heads();
    if recovering && heads.read()?.contains(&cs_id) {
        // The crashed run appended this tip already.
    } else {
        heads.append(cs_id)?;
    }
    for c in changes {
        match c.op {
            Op::Delete => {
                treestate.remove(&c.path)?;
            }
            Op::Add | Op::Modify | Op::Rename => {
                if let Some(src) = &c.src_path {
                    treestate.remove(src)?;
                }
                let f = files
                    .iter()
                    .find(|f| f.path == c.path)
                    .expect("every created write came from a scanned working file");
                treestate.upsert(&TreestateEntry {
                    path: c.path.clone(),
                    blob_id: f.blob_id,
                    mode: f.mode,
                    size: f.size,
                    mtime_ns: f.mtime_ns,
                    hint: HINT_VERIFIED,
                })?;
            }
        }
    }
    Ok(())
}

/// Classifies distinct blobs into the inline payload (each within
/// [`INLINE_BLOB_LIMIT`], together within [`INLINE_TOTAL_LIMIT`]) and the
/// heavy ones `PutBlob` must stream first — the same split the upload and
/// `bed sync` apply.
pub(crate) fn split_inline(blobs: BlobList) -> (BlobList, BlobList) {
    let mut inline = Vec::new();
    let mut heavy = Vec::new();
    let mut inline_bytes = 0usize;
    for (id, content) in blobs {
        if content.len() <= INLINE_BLOB_LIMIT && content.len() <= INLINE_TOTAL_LIMIT - inline_bytes
        {
            inline_bytes += content.len();
            inline.push((id, content));
        } else {
            heavy.push((id, content));
        }
    }
    (inline, heavy)
}

/// Renders a [`CommitOutcome`] in the working copy voice the CLI prints.
pub fn render_outcome(outcome: &CommitOutcome) -> String {
    let mut out = String::new();
    if outcome.queued {
        // The offline queue line comes first: it says where the commit
        // landed, before the working-copy details.
        out.push_str(&format!(
            "queued as {} ({} pending — run bed sync when online)\n",
            outcome.cs_id.short(),
            outcome.pending,
        ));
    }
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
        None => " — initial changeset".to_owned(),
    };
    out.push_str(&format!(
        "created changeset {}: {} change(s) ({})\nworking copy at {}{}\n",
        outcome.cs_id.short(),
        n,
        parts.join(", "),
        outcome.cs_id.short(),
        was,
    ));
    if outcome.blobs_uploaded > 0 {
        out.push_str(&format!(
            "{} blob(s) uploaded separately, {} sent inline\n",
            outcome.blobs_uploaded, outcome.blobs_inline,
        ));
    }
    out
}

/// The `author` ident: `--author` wins, then `GARDENBED_AUTHOR`, then the
/// working copy's default.
pub fn resolve_author(explicit: Option<&str>) -> Result<(String, String), ClientError> {
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
pub fn parse_author(raw: &str) -> Result<(String, String), ClientError> {
    let fail = |reason: String| ClientError::Author {
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
pub(crate) fn now_stamp() -> (u64, i32) {
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
fn read_working(root: &Path, rel: &str) -> Result<Option<WorkingFile>, ClientError> {
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
        std::os::unix::ffi::OsStrExt::as_bytes(std::ffi::OsStr::new(&std::fs::read_link(
            root.join(rel),
        )?))
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

/// Streams one heavy blob to the server's `PutBlob`. `command` names the
/// caller ("commit" or "sync") in RPC error messages.
pub(crate) async fn put_blob_heavy(
    session: &Session,
    repo: &str,
    id: Id,
    content: &[u8],
    command: &'static str,
) -> Result<(), ClientError> {
    let chunks: Vec<Vec<u8>> = content.chunks(PUT_BLOB_CHUNK).map(<[u8]>::to_vec).collect();
    match session
        .client
        .put_blob(repo, tokio_stream::iter(chunks))
        .await
    {
        Ok(received) if received == id => Ok(()),
        Ok(received) => Err(ClientError::BlobHashMismatch(received)),
        Err(status) => Err(ClientError::Rpc {
            command,
            code: status.code(),
            message: status.message().to_owned(),
        }),
    }
}

/// Turns the scan's classification into canonical changes.
///
/// * a deleted path and an added path carrying the **exact same blob id**
///   pair into one `Rename` (a pure move; no similarity heuristic — a
///   moved-and-edited file commits as delete + add, which builds the
///   identical tree);
/// * pairing is deterministic: both sides sorted by path bytes, greedily
///   matched in order;
/// * a rename is recorded only when the write's destination is a new path
///   (untracked — `is_new`) and its entry kind matches the deleted side's
///   (regular/exec are one kind): `bed diff` renders a recorded rename as
///   git's `R100`, and git apply refuses a rename onto a pre-existing
///   path or across entry kinds — the recorded fact must stay renderable;
/// * the result is sorted strictly ascending by path bytes (the codec's
///   canonical form; paths are disjoint across the sources by
///   construction, so no duplicates can arise).
///
/// Deletes whose treestate row was gone (`None` blob or mode) never pair —
/// there is nothing to move from.
fn build_changes(
    mut deletes: Vec<(String, Option<Id>, Option<i64>)>,
    writes: &mut [WriteEntry],
) -> Result<(Vec<Change>, usize), ClientError> {
    deletes.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    writes.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut consumed = vec![false; deletes.len()];
    let mut renames: Vec<(String, usize)> = Vec::new(); // (src, index into writes)
    let mut plain: Vec<usize> = Vec::new();
    for (wi, w) in writes.iter().enumerate() {
        // A rename is recorded only for a brand-new destination of the
        // same entry kind as the deleted source — the pairing `bed diff`
        // renders as git's `R100`, which git apply accepts. A rewrite of
        // a pre-existing path, or a cross-kind move, stays a write (and
        // the delete stays a delete).
        if !w.is_new {
            plain.push(wi);
            continue;
        }
        let write_kind = mode_code_to_mode(w.mode);
        let mut hit = None;
        for (di, (dp, db, dmode)) in deletes.iter().enumerate() {
            if !consumed[di]
                && *dp != w.path
                && let Some(db) = db
                && let Some(dmode) = dmode
                && *db == w.blob
                && render::same_entry_kind(mode_code_to_mode(*dmode), write_kind)
            {
                hit = Some(di);
                break;
            }
        }
        match hit {
            Some(di) => {
                consumed[di] = true;
                renames.push((deletes[di].0.clone(), wi));
            }
            None => plain.push(wi),
        }
    }
    let renamed = renames.len();

    let mut changes: Vec<Change> = Vec::with_capacity(plain.len() + renames.len() + deletes.len());
    for (di, (path, ..)) in deletes.iter().enumerate() {
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
pub(crate) fn mode_code_to_mode(code: i64) -> Mode {
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
        let deletes = vec![(
            "a.txt".to_owned(),
            Some(blob_id(b"same")),
            Some(crate::treestate::MODE_REGULAR),
        )];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 1);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].op, Op::Rename);
        assert_eq!(changes[0].path, "b.txt");
        assert_eq!(changes[0].src_path.as_deref(), Some("a.txt"));
        assert_eq!(changes[0].blob, Some(blob_id(b"same")));
    }

    #[test]
    fn moved_and_edited_commits_as_delete_plus_add() {
        let mut writes = vec![write("b.txt", b"edited!", true)];
        let deletes = vec![(
            "a.txt".to_owned(),
            Some(blob_id(b"original")),
            Some(crate::treestate::MODE_REGULAR),
        )];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Delete); // a.txt < b.txt
        assert_eq!(changes[1].op, Op::Add);
    }

    #[test]
    fn changes_are_canonical_and_disjoint() {
        let mut writes = vec![write("z.md", b"z", true), write("m.rs", b"m", false)];
        let deletes = vec![
            (
                "k.txt".to_owned(),
                Some(blob_id(b"k")),
                Some(crate::treestate::MODE_REGULAR),
            ),
            (
                "a.rs".to_owned(),
                Some(blob_id(b"m.rs-content")),
                Some(crate::treestate::MODE_REGULAR),
            ),
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
        let deletes = vec![("a.txt".to_owned(), None, None)];
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
        let deletes = vec![(
            "a.txt".to_owned(),
            Some(blob_id(b"same")),
            Some(crate::treestate::MODE_REGULAR),
        )];
        let (changes, _) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(changes[0].mode, Mode::Exec);
    }

    #[test]
    fn rewrite_of_preexisting_path_never_pairs() {
        // Delete src.txt + rewrite a *pre-existing* dst.txt to the same
        // bytes: recording a rename would claim dst.txt was created by
        // the move when it existed — git apply refuses that shape. The
        // delete stays a delete; the rewrite stays a modify.
        let mut writes = vec![write("dst.txt", b"same", false)];
        let deletes = vec![(
            "src.txt".to_owned(),
            Some(blob_id(b"same")),
            Some(crate::treestate::MODE_REGULAR),
        )];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Modify); // dst.txt < src.txt
        assert_eq!(changes[0].path, "dst.txt");
        assert_eq!(changes[1].op, Op::Delete);
        assert_eq!(changes[1].path, "src.txt");
    }

    #[test]
    fn cross_kind_move_never_pairs() {
        // Delete a regular file + create a symlink whose target equals
        // its content: identical blob, different entry kinds. A recorded
        // rename would render an `R100` with mismatched modes — git apply
        // refuses ("new mode does not match"). Stays delete + add.
        let mut w = write("link", b"target", true);
        w.mode = crate::treestate::MODE_SYMLINK;
        let mut writes = vec![w];
        let deletes = vec![(
            "file.txt".to_owned(),
            Some(blob_id(b"target")),
            Some(crate::treestate::MODE_REGULAR),
        )];
        let (changes, renamed) = build_changes(deletes, &mut writes).unwrap();
        assert_eq!(renamed, 0);
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].op, Op::Delete); // file.txt < link
        assert_eq!(changes[0].path, "file.txt");
        assert_eq!(changes[1].op, Op::Add);
        assert_eq!(changes[1].path, "link");
        assert_eq!(changes[1].mode, Mode::Symlink);
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
        assert_eq!(name, "Gardenbed User");
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
        assert_eq!(name, "Gardenbed User");
    }
}

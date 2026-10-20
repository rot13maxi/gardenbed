//! The server-backed commands (M4 read path): `clone`, `sparse add`, `switch`,
//! `file`, `log`, `diff`; M6/M7 add `blame`.
//!
//! Two invariants shape every function here:
//!
//! * **validate, connect, then write** — argument validation and the
//!   network round-trips happen before any local state changes, so a
//!   failed command leaves the working copy exactly as it stood (v0.1.md §M4.5:
//!   no half-states; offline = clean errors);
//! * **want-list + cache** — objects are fetched only when missing, into
//!   `.bed/objects`, with bounded parallelism, so every command is
//!   resumable and idempotent by construction.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use gardenbed_model::codec::validate_path;
use gardenbed_model::{Id, Mode, TreeKind};
use gardenbed_protocol::client::GardenbedClient;
use gardenbed_protocol::convert::{mode_from_wire, op_from_wire};
use gardenbed_protocol::pb::{ChangeMsg, ChangesetSummary};
use gardenbed_store::ObjectStore as _;
use tokio_stream::StreamExt as _;
use tonic::Code;

use crate::error::ClientError;
use crate::render::{self, DiffEntry, LogEntry};
use crate::rpc::{self, Session};
use crate::sync;
use crate::workspace::WorkingCopy;

/// The ref every v0.1 working copy starts at unless `--ref` says otherwise: the
/// first name the no-flag `clone` resolution probes, the tip `publish`
/// consults after a lost race. (v0.1 has no remote-head discovery beyond
/// `ListRefs`; see `docs/v0.1.md` §2.)
const DEFAULT_REF: &str = "main";

/// The refs a no-flag `clone` probes, in order: `main` first — the v0.1
/// default, and the deterministic winner when several common names exist —
/// then the common primary-branch names a server might grow instead.
/// When none resolve, the server's own ref listing (`ListRefs`)
/// enriches the error with what actually exists.
const STARTING_REF_PROBES: [&str; 3] = [DEFAULT_REF, "master", "trunk"];

/// `bed clone <server> [--repo name] [--ref name] [--sparse path...]` —
/// create a working copy.
///
/// Validates everything local (no bed already here, repo name, sparse
/// prefixes), then connects and resolves the starting ref **before any
/// state is written** — a server that is down, a repo that does not
/// exist, or a garden with none of the expected refs leaves zero state
/// behind. On success the full `.bed` layout is initialized with `current_cs`
/// = the ref's changeset and the tip recorded in `heads`.
/// **Materializes nothing** (v0.1.md §2): creating a working copy costs one ref
/// lookup, not a checkout. Sparse prefixes handed to `--sparse` are recorded
/// in the config; files come with `bed sparse add`.
///
/// The starting ref: `--ref` names it exactly (bare names resolve against
/// `refs/heads/` server-side; full ref names pass through verbatim). A
/// ref the server does not have is refused, nothing initialized. Without
/// the flag, `main` is tried first — the v0.1 default, and the
/// deterministic winner when several common names exist — then the other
/// common names (`STARTING_REF_PROBES`), so a server that only
/// has a `master` branch clones without a flag. When none resolve, the error
/// names what was tried and — via the server's ref listing — what does
/// exist, pointing at `--ref`.
///
/// With `--repo` omitted the working copy is named `garden` (v0.1 servers are
/// single-repo and offer no name discovery; a documented deviation).
pub async fn clone_with_ref(
    state_dir: &Path,
    server: &str,
    repo: Option<&str>,
    ref_name: Option<&str>,
    sparse: Vec<String>,
) -> Result<String, ClientError> {
    // ---- validation phase: nothing has been written yet ----
    let repo = match repo {
        Some(r) if !r.trim().is_empty() => r.trim().to_owned(),
        Some(_) => {
            return Err(ClientError::ConfigInvalid("repo name is empty".into()));
        }
        None => "default".to_owned(),
    };
    let mut prefixes = Vec::with_capacity(sparse.len());
    for raw in &sparse {
        let prefix = crate::sparse::normalize_prefix(raw)?;
        refuse_bed_state_prefix(raw, &prefix)?;
        prefixes.push(prefix);
    }
    if state_dir.join("config.toml").exists() {
        return Err(ClientError::AlreadyAWorkingCopy(state_dir.to_path_buf()));
    }

    // ---- connection and resolution: still no local state ----
    let client = GardenbedClient::connect(server)
        .await
        .map_err(|e| ClientError::Connect {
            server: server.to_owned(),
            message: e.to_string(),
        })?;
    let (ref_name, main) = match ref_name {
        Some(name) => match client.get_ref(&repo, name).await {
            Ok(Some(id)) => (name.to_owned(), id),
            Ok(None) => {
                return Err(ClientError::MissingRef {
                    name: name.to_owned(),
                });
            }
            Err(status) => {
                return Err(ClientError::Rpc {
                    command: "clone",
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        },
        None => resolve_starting_ref(&client, &repo).await?,
    };
    // The ref must name a real changeset (a ref to nowhere is refused here,
    // not discovered half-initialized).
    match client.get_changeset(&repo, main).await {
        Ok(_) => {}
        Err(status) if status.code() == Code::NotFound => {
            return Err(ClientError::MissingCs(main));
        }
        Err(status) => {
            return Err(ClientError::Rpc {
                command: "clone",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    }

    // ---- initialize local state ----
    let mut bed = WorkingCopy::init(state_dir, server, &repo)?;
    bed.config_mut().sparse = prefixes;
    bed.config_mut().current_cs = Some(main.to_hex());
    bed.save_config()?;
    bed.heads().append(main)?;

    Ok(format!(
        "created working copy for {repo} at {server}\n\
         current changeset: {} ({ref_name})\n\
         no files materialized — select paths with `bed sparse add <path>`\n",
        main.short()
    ))
}

/// [`clone_with_ref`] without `--ref`: the starting ref is left to the
/// no-flag fallback resolution. Kept as the narrow entry point the
/// benchmark (`crates/bench`) and pre-`--ref` callers use.
pub async fn clone(
    state_dir: &Path,
    server: &str,
    repo: Option<&str>,
    sparse: Vec<String>,
) -> Result<String, ClientError> {
    clone_with_ref(state_dir, server, repo, None, sparse).await
}

/// Resolves the ref a no-flag `clone` starts at: `main` first ([`DEFAULT_REF`],
/// the deterministic winner when several common names exist), then the
/// other common primary-branch names, so a server that only has
/// `master` clones without a flag. When nothing resolves, the server's
/// ref listing (best-effort: its failure cannot change the verdict already
/// earned from the probes) names what does exist, and the error points at
/// `--ref`.
async fn resolve_starting_ref(
    client: &GardenbedClient,
    repo: &str,
) -> Result<(String, Id), ClientError> {
    for name in STARTING_REF_PROBES {
        match client.get_ref(repo, name).await {
            Ok(Some(id)) => return Ok((name.to_owned(), id)),
            Ok(None) => {}
            Err(status) => {
                return Err(ClientError::Rpc {
                    command: "clone",
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        }
    }
    // Best-effort listing: it only ever enriches the error with the
    // server's own ref names (the full stored spellings `--ref`
    // accepts), never masks the all-probes-missed verdict.
    let mut names: Vec<String> = match client.list_refs(repo).await {
        Ok(stream) => {
            let mut stream = stream;
            let mut names = Vec::new();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(entry) => names.push(entry.name),
                    Err(_) => break,
                }
            }
            names
        }
        Err(_) => Vec::new(),
    };
    names.sort();
    Err(ClientError::NoStartingRef {
        primary: DEFAULT_REF.to_owned(),
        tried: STARTING_REF_PROBES.join(", "),
        available: if names.is_empty() {
            String::new()
        } else {
            format!("\n  the server has refs: {}", names.join(", "))
        },
    })
}

/// `bed sparse add <paths...> [--concurrency N]` — record the prefixes, then
/// want-list fetch and materialize them at the current changeset.
///
/// Re-adding an existing sparse is idempotent: the cache-first fetch pulls
/// nothing the working copy already has, and faithful files are kept untouched.
/// The config only gains the new prefixes once the fetch has succeeded, so
/// a server that is down leaves the sparse selection list (and everything else)
/// unchanged.
///
/// The state-dir rule: a prefix that is `.bed` or lies inside it is
/// refused outright (`refuse_bed_state_prefix`), and repo files that
/// merely shadow that name (they can arrive under a root prefix) are
/// excluded from materialization with a printed note
/// (`exclude_bed_state`) — no repo file is ever initialized inside the
/// working copy's own state directory.
pub async fn sparse_add(
    state_dir: &Path,
    paths: &[String],
    concurrency: usize,
) -> Result<String, ClientError> {
    let mut bed = WorkingCopy::open(state_dir)?;
    let mut added: Vec<String> = Vec::new();
    for raw in paths {
        let prefix = crate::sparse::normalize_prefix(raw)?;
        refuse_bed_state_prefix(raw, &prefix)?;
        if !bed.config().sparse.contains(&prefix) {
            bed.config_mut().sparse.push(prefix.clone());
            added.push(prefix);
        }
    }

    // Fetch (cache-first) before saving config or materializing: a failed
    // fetch leaves neither the sparse selection list nor the treestate half-changed.
    let session = Session::open_with(&bed, concurrency).await?;
    let cs = session.resolve_current_cs(&bed, "sparse add").await?;
    let root = session.root_tree(cs, "sparse add").await?;
    let prefixes: Vec<String> = bed.config().sparse.clone();
    let (plan_files, stats) = rpc::plan_sparse(&session, root, &prefixes).await?;

    // Which of the newly added prefixes actually matched something at the
    // current changeset? Computed before the plan moves into sync_sparse.
    let prefix_matches: Vec<bool> = added
        .iter()
        .map(|p| {
            plan_files
                .iter()
                .any(|f| crate::sparse::in_sparse(&f.path, std::slice::from_ref(p)))
        })
        .collect();

    // Repo files that name `.bed` must never land in the working copy's state
    // directory: exclude them from materialization and say so.
    let (plan_files, bed_state) = exclude_bed_state(plan_files);

    let treestate = bed.treestate()?;
    let outcome = sync::sync_sparse(
        &bed.workspace_root(),
        &prefixes,
        plan_files,
        session.fetcher(),
        &treestate,
    )
    .await?;
    if !added.is_empty() {
        bed.save_config()?;
    }

    let mut out = String::new();
    for (prefix, matched) in added.iter().zip(&prefix_matches) {
        if *matched {
            out.push_str(&format!("selected {prefix}\n"));
        } else {
            out.push_str(&format!(
                "selected {prefix} — note: nothing exists under it at the \
                 current changeset; it fills as history lands (check the \
                 spelling if that surprises you)\n"
            ));
        }
    }
    if added.is_empty() {
        out.push_str("those paths are already selected — refreshing them\n");
    }
    out.push_str(&bed_state_note(&bed_state));
    out.push_str(&format!(
        "fetched {} tree(s) and {} blob(s) from the server ({} file(s) in the selected paths)\n",
        stats.trees_fetched,
        stats.blobs_fetched,
        stats.files - bed_state.len()
    ));
    out.push_str(&outcome.summarize());
    out.push('\n');
    Ok(out)
}

/// The state-dir rule: `.bed` is the working copy's own machinery (config,
/// treestate, object cache, heads), never garden content. A prefix that
/// is `.bed` or lies inside it would point the materializer straight at
/// the state directory — a repo that ships `.bed/config.toml`,
/// `.bed/index.sqlite3`, or `.bed/heads` could hijack or corrupt the working copy —
/// so such a prefix is refused in the same voice as every other
/// impossible prefix. Any component matching counts: the name `.bed` is
/// reserved wherever it appears in a repo path.
fn refuse_bed_state_prefix(raw: &str, prefix: &str) -> Result<(), ClientError> {
    if prefix.split('/').any(|comp| comp == ".bed") {
        Err(ClientError::SparsePrefix {
            raw: raw.to_owned(),
            reason: format!(
                "refusing to sparse {prefix} — path is reserved for working-copy state"
            ),
        })
    } else {
        Ok(())
    }
}

/// Splits the `.bed`-shaded paths out of a materialization plan: any path
/// with a `.bed` component would be initialized under a `.bed` directory in
/// the repository — at the root that IS the live state directory, and a repo
/// shipping `.bed/config.toml`, `.bed/index.sqlite3`, or `.bed/heads` would
/// hijack or corrupt the working copy. Such paths are excluded from
/// materialization rather than refusing the whole command — `bed sparse
/// add .` continues tracking every other file in the repository — and the caller
/// reports what was skipped ([`bed_state_note`]). Returns the plan that
/// may be materialized plus the skipped paths, in plan order.
fn exclude_bed_state(plan: Vec<rpc::PlanFile>) -> (Vec<rpc::PlanFile>, Vec<String>) {
    let mut kept = Vec::with_capacity(plan.len());
    let mut skipped = Vec::new();
    for file in plan {
        if file.path.split('/').any(|comp| comp == ".bed") {
            skipped.push(file.path);
        } else {
            kept.push(file);
        }
    }
    (kept, skipped)
}

/// The note naming what `exclude_bed_state` kept out of the repository.
/// Empty when nothing was skipped; the list is capped so a repo shipping
/// a large `.bed/` tree cannot flood one line.
fn bed_state_note(skipped: &[String]) -> String {
    const LISTED: usize = 8;
    if skipped.is_empty() {
        return String::new();
    }
    let names = if skipped.len() <= LISTED {
        skipped.join(", ")
    } else {
        format!(
            "{}, and {} more",
            skipped[..LISTED].join(", "),
            skipped.len() - LISTED
        )
    };
    format!("note: skipped {names} — path is reserved for working-copy state\n")
}

/// `bed switch <cs-or-ref>` — move the working copy to a changeset and refresh the
/// sparse's working copy to match it (fetch missing, rewrite changed, remove
/// gone). Local edits survive when the target leaves their paths unchanged;
/// destructive collisions are refused before any materialization.
///
/// The config's `current_cs` moves only after successful materialization.
/// A fetch failure or preflight refusal leaves the working copy where it stood;
/// an interrupted refresh is repaired by simply running switch again.
pub async fn switch(state_dir: &Path, spec: &str) -> Result<String, ClientError> {
    let mut bed = WorkingCopy::open(state_dir)?;
    let parsed = crate::revspec::CsOrRef::parse(spec)?;
    let session = Session::open(&bed).await?;
    let cs = session.resolve(&parsed, "switch").await?;

    // Fetch everything the sparse selection needs at the new changeset before writing
    // anything locally.
    let root = session.root_tree(cs, "switch").await?;
    let prefixes = bed.config().sparse.clone();
    let (plan, stats) = rpc::plan_sparse(&session, root, &prefixes).await?;
    // `.bed`-shaded repo paths never reach the state directory; say what
    // was skipped.
    let (plan, bed_state) = exclude_bed_state(plan);

    let treestate = bed.treestate()?;
    let outcome = sync::sync_sparse(
        &bed.workspace_root(),
        &prefixes,
        plan,
        session.fetcher(),
        &treestate,
    )
    .await?;
    bed.config_mut().current_cs = Some(cs.to_hex());
    bed.save_config()?;

    let where_from = parsed.as_ref_name().unwrap_or("changeset").to_owned();
    Ok(format!(
        "working copy at {} ({}) — fetched {} tree(s), {} blob(s)\n{}{}\n",
        cs.short(),
        where_from,
        stats.trees_fetched,
        stats.blobs_fetched,
        bed_state_note(&bed_state),
        outcome.summarize()
    ))
}

/// `bed file <path> [--cs id-or-ref]` — print one file's raw bytes at a
/// changeset. Trees along the walk are cached; the blob is fetched
/// cache-first. Pure read: no treestate, config, or heads writes.
pub async fn file(
    state_dir: &Path,
    path: &str,
    cs: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), ClientError> {
    validate_path(path).map_err(|e| ClientError::Path {
        raw: path.to_owned(),
        reason: e.to_string(),
    })?;
    let bed = WorkingCopy::open(state_dir)?;
    if cs.is_none() {
        require_current_cs(&bed)?;
    }
    let session = Session::open(&bed).await?;
    let cs_id = resolve_target(&session, &bed, cs, "file").await?;
    let entry = session
        .locate_entry(cs_id, path, "file")
        .await?
        .ok_or_else(|| ClientError::FileNotAFile {
            path: path.to_owned(),
            cs: cs_id.short(),
        })?;
    match entry.kind {
        TreeKind::Dir => {
            return Err(ClientError::FileNotAFile {
                path: path.to_owned(),
                cs: cs_id.short(),
            });
        }
        TreeKind::File => {}
    }
    if entry.mode == Mode::Gitlink {
        return Err(ClientError::FileIsGitlink {
            path: path.to_owned(),
            cs: cs_id.short(),
        });
    }
    // "Present exactly when kind == File" is the tree invariant; a missing
    // blob id would mean the server handed us a malformed tree.
    let blob = entry.blob.ok_or(ClientError::BlobHashMismatch(cs_id))?;
    let content = session.fetcher().blob(blob).await?;
    // Raw bytes, exactly — no trailing newline added. A closed pipe (e.g.
    // `bed file x | head -1`) is a graceful end, not an error.
    if let Err(e) = out.write_all(&content) {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(e.into());
    }
    if let Err(e) = out.flush() {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Ok(());
        }
        return Err(e.into());
    }
    Ok(())
}

/// `bed rebase <new-parent>` — replay the current changeset onto a new
/// parent (the server-side linearized rebase, v0.1.md D5) and move the working copy
/// onto the replayed changeset.
///
/// The server returns the rebased changeset's id, or refuses with
/// `FAILED_PRECONDITION` naming the conflicting paths (same path written
/// on both sides, delete of a missing path, …) — surfaced as
/// [`ClientError::Rpc`], verbatim. On success the working copy treats the result like
/// a `switch`: the sparse selection is re-planned and re-synced at the rebased changeset
/// (which usually changes nothing — the replay carries this working copy's own
/// changes — but the new parent may differ elsewhere under the selected paths), the
/// config moves, and the heads stack is re-pointed from the old changeset
/// to the rebased one.
pub async fn rebase(state_dir: &Path, new_parent: &str) -> Result<String, ClientError> {
    let mut bed = WorkingCopy::open(state_dir)?;
    let parsed = crate::revspec::CsOrRef::parse(new_parent)?;
    let session = Session::open(&bed).await?;
    let current = session.resolve_current_cs(&bed, "rebase").await?;
    let parent = session.resolve(&parsed, "rebase").await?;

    // The replay is the server's decision; conflicts come back as a
    // FAILED_PRECONDITION whose message lists every conflicting path.
    let rebased = match session
        .client
        .rebase(&bed.config().repo, current, parent)
        .await
    {
        Ok(id) => id,
        Err(status) => {
            return Err(ClientError::Rpc {
                command: "rebase",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    };

    // Move the working copy like `switch` does: plan the sparse selection at the rebased
    // changeset, then fetch + sync. `.bed`-shaded repo paths never reach
    // the state directory; say what was skipped.
    let root = session.root_tree(rebased, "rebase").await?;
    let prefixes = bed.config().sparse.clone();
    let (plan, stats) = rpc::plan_sparse(&session, root, &prefixes).await?;
    let (plan, bed_state) = exclude_bed_state(plan);
    let treestate = bed.treestate()?;
    let outcome = sync::sync_sparse(
        &bed.workspace_root(),
        &prefixes,
        plan,
        session.fetcher(),
        &treestate,
    )
    .await?;
    bed.config_mut().current_cs = Some(rebased.to_hex());
    bed.save_config()?;
    bed.heads().repoint(current, rebased)?;

    Ok(format!(
        "rebased {} onto {} as {}\n\
         fetched {} tree(s), {} blob(s)\n{}{}\n",
        current.short(),
        parent.short(),
        rebased.short(),
        stats.trees_fetched,
        stats.blobs_fetched,
        bed_state_note(&bed_state),
        outcome.summarize(),
    ))
}

/// `bed merge <target> [-m MESSAGE] [--author …]` — merge a changeset or
/// ref into the working copy's current changeset (the server-side
/// two-parent merge, v0.2.md D9–D11), then move the working copy onto
/// the merge changeset rebase-shaped.
///
/// The server returns the merge changeset's id, or refuses with
/// `FAILED_PRECONDITION` naming every conflicting path — surfaced as
/// [`ClientError::Rpc`], verbatim (resolution belongs in the working
/// copy; nothing is stored on conflict). When the target is already an
/// ancestor of the current changeset, the answer is up to date: the
/// working copy stays where it is. On success the working copy treats
/// the result like a `rebase`/`switch`: the sparse selection is
/// re-planned and re-synced at the merged changeset, the config moves,
/// and the heads stack is re-pointed from the old changeset to the
/// merge.
pub async fn merge(
    state_dir: &Path,
    target: &str,
    message: Option<&str>,
    author: Option<&str>,
) -> Result<String, ClientError> {
    let mut bed = WorkingCopy::open(state_dir)?;
    let parsed = crate::revspec::CsOrRef::parse(target)?;
    let session = Session::open(&bed).await?;
    let current = session.resolve_current_cs(&bed, "merge").await?;
    let theirs = session.resolve(&parsed, "merge").await?;

    // The merge is the server's decision; conflicts come back as a
    // FAILED_PRECONDITION whose message lists every conflicting path.
    let message = message
        .map(|m| m.to_owned())
        .unwrap_or_else(|| format!("merge {target}"));
    let (name, email) = crate::commit::resolve_author(author)?;
    let (time_secs, tz_min) = crate::commit::now_stamp();
    let identity = gardenbed_model::Identity {
        name,
        email,
        time_secs,
        tz_min,
    };
    let (merged, up_to_date) = session
        .merge(current, theirs, &message, &identity, &identity, "merge")
        .await?;
    if up_to_date {
        return Ok(format!(
            "already up to date — {} is an ancestor of {}; nothing to merge\n",
            theirs.short(),
            current.short(),
        ));
    }

    // Move the working copy like `rebase` does: plan the sparse
    // selection at the merge changeset, then fetch + sync.
    // `.bed`-shaded repo paths never reach the state directory; say
    // what was skipped.
    let root = session.root_tree(merged, "merge").await?;
    let prefixes = bed.config().sparse.clone();
    let (plan, stats) = rpc::plan_sparse(&session, root, &prefixes).await?;
    let (plan, bed_state) = exclude_bed_state(plan);
    let treestate = bed.treestate()?;
    let outcome = sync::sync_sparse(
        &bed.workspace_root(),
        &prefixes,
        plan,
        session.fetcher(),
        &treestate,
    )
    .await?;
    bed.config_mut().current_cs = Some(merged.to_hex());
    bed.save_config()?;
    bed.heads().repoint(current, merged)?;

    Ok(format!(
        "merged {} into {} as {}\n\
         fetched {} tree(s), {} blob(s)\n{}{}\n",
        theirs.short(),
        current.short(),
        merged.short(),
        stats.trees_fetched,
        stats.blobs_fetched,
        bed_state_note(&bed_state),
        outcome.summarize(),
    ))
}

/// `bed publish` — fast-forward `main` to the working copy's current changeset.
///
/// The server enforces the ancestry precondition with a
/// compare-and-swap: exactly one publisher wins a race, everyone else
/// receives [`ClientError::PublishNotFastForward`] with the fix spelled out.
/// Success appends the published changeset to the heads stack; the working copy
/// stays where it is — publishing publishes, it does not move.
pub async fn publish(state_dir: &Path) -> Result<String, ClientError> {
    let bed = WorkingCopy::open(state_dir)?;
    let session = Session::open(&bed).await?;
    let repo = bed.config().repo.clone();
    let current = session.resolve_current_cs(&bed, "publish").await?;

    match session.client.publish(&repo, current).await {
        // The returned tip is `main`'s value after the fast-forward —
        // the changeset itself for a conforming server.
        Ok(_tip) => {
            bed.heads().append(current)?;
            Ok(format!(
                "published {} — `main` now points to it\n",
                current.short()
            ))
        }
        Err(status) if status.code() == Code::FailedPrecondition => {
            // The ref moved under us (a concurrent publish, or history
            // landing from elsewhere). Name the new tip when the server
            // will say it, so the fix is one command away.
            let tip = session
                .client
                .get_ref(&repo, DEFAULT_REF)
                .await
                .ok()
                .flatten()
                .map(|id| format!(", whose current tip is {}", id.short()))
                .unwrap_or_default();
            Err(ClientError::PublishNotFastForward {
                cs: current.short(),
                tip,
            })
        }
        Err(status) => Err(ClientError::Rpc {
            command: "publish",
            code: status.code(),
            message: status.message().to_owned(),
        }),
    }
}

/// Resolves the changeset a read command should answer at: an explicit
/// `<cs-or-ref>` when given, else the working copy's current changeset. A missing
/// current changeset is reported before any connection is attempted (the
/// working copy's own state is the actionable error there).
fn require_current_cs(bed: &WorkingCopy) -> Result<(), ClientError> {
    bed.config()
        .current_cs
        .as_ref()
        .map(|_| ())
        .ok_or(ClientError::NoCurrentCs)
}

/// Resolves the changeset a read command should answer at: an explicit
/// `<cs-or-ref>` when given, else the working copy's current changeset.
async fn resolve_target(
    session: &Session,
    bed: &WorkingCopy,
    cs: Option<&str>,
    command: &'static str,
) -> Result<Id, ClientError> {
    match cs {
        Some(spec) => {
            let parsed = crate::revspec::CsOrRef::parse(spec)?;
            session.resolve(&parsed, command).await
        }
        None => session.resolve_current_cs(bed, command).await,
    }
}

/// `bed log [--limit N] [--cs id-or-ref]` — the history from the server's
/// date-ordered walk (git's own algorithm), newest first, rendered
/// git-log-like (see [`crate::render`]). `--limit 0` or omitted: all.
pub async fn log(
    state_dir: &Path,
    limit: Option<u32>,
    cs: Option<&str>,
) -> Result<String, ClientError> {
    let bed = WorkingCopy::open(state_dir)?;
    if cs.is_none() {
        require_current_cs(&bed)?;
    }
    let session = Session::open(&bed).await?;
    let cs_id = resolve_target(&session, &bed, cs, "log").await?;
    let summaries = session.log(cs_id, limit.unwrap_or(0), "log").await?;
    let entries: Vec<LogEntry> = summaries.iter().map(log_entry_of).collect();
    Ok(render::render_log(&entries))
}

/// Maps a wire summary onto the renderer's entry shape.
fn log_entry_of(summary: &ChangesetSummary) -> LogEntry {
    let author = summary.author.clone().unwrap_or_default();
    LogEntry {
        id: Id::from_bytes(summary.cs_id.as_slice().try_into().unwrap_or([0u8; 32])),
        author_name: author.name,
        author_email: author.email,
        time_secs: author.time_secs,
        tz_min: author.tz_min,
        message: summary.message.clone(),
    }
}

/// `bed diff [<a> [<b>]]` — the tree diff between two changesets (rename
/// detection off, server-side), rendered git-diff-like.
///
/// With no arguments: the current changeset against its first parent (a
/// root changeset has nothing to diff against — empty output). With one
/// argument: that changeset against its first parent. With two: `a → b`.
pub async fn diff(
    state_dir: &Path,
    a: Option<&str>,
    b: Option<&str>,
) -> Result<String, ClientError> {
    let bed = WorkingCopy::open(state_dir)?;
    if a.is_none() {
        require_current_cs(&bed)?;
    }
    let session = Session::open(&bed).await?;

    // Resolve sides; fill in first parents client-side as needed.
    let (older, newer) = match (a, b) {
        (Some(a), Some(b)) => {
            let pa = crate::revspec::CsOrRef::parse(a)?;
            let pb = crate::revspec::CsOrRef::parse(b)?;
            (
                session.resolve(&pa, "diff").await?,
                session.resolve(&pb, "diff").await?,
            )
        }
        (Some(a), None) => {
            // One side: that changeset against its first parent. A root
            // changeset has no parent — nothing to diff, empty output.
            let newer = session
                .resolve(&crate::revspec::CsOrRef::parse(a)?, "diff")
                .await?;
            match session.first_parent(newer, "diff").await? {
                Some(older) => (older, newer),
                None => return Ok(String::new()),
            }
        }
        (None, Some(_)) => {
            return Err(ClientError::ConfigInvalid(
                "bed diff takes two changesets, or none for the current one vs its parent".into(),
            ));
        }
        (None, None) => {
            let newer = session.resolve_current_cs(&bed, "diff").await?;
            match session.first_parent(newer, "diff").await? {
                Some(older) => (older, newer),
                None => return Ok(String::new()), // a root: nothing to diff
            }
        }
    };
    if older == newer {
        return Ok(String::new());
    }

    let changes = session.diff(older, newer, "diff").await?;
    let entries = build_entries(&session, older, newer, changes).await?;

    // All blob contents the renderer will need are in the cache now (the
    // fetch above is cache-first); rendering reads them straight from the
    // local store, no async needed.
    let objects = bed.objects()?;
    let content_of = |id: Id| -> Result<Vec<u8>, ClientError> {
        objects
            .get_blob(id)?
            .ok_or(ClientError::BlobHashMismatch(id))
    };
    render::render_diff(&entries, &content_of)
}

/// Converts wire `ChangeMsg`s into render entries. The wire carries only
/// the *new* side's blob id, so the old side is resolved by walking the
/// `older` changeset's tree per path (cache-first; the shared trees cost
/// nothing after the first walk).
async fn build_entries(
    session: &Session,
    older: Id,
    newer: Id,
    changes: Vec<ChangeMsg>,
) -> Result<Vec<DiffEntry>, ClientError> {
    let mut entries = Vec::with_capacity(changes.len());
    let mut wanted: HashSet<Id> = HashSet::new();

    for change in &changes {
        let op = op_from_wire(change.op).map_err(|_| ClientError::BlobHashMismatch(newer))?;
        let path = change.path.clone();
        let new_side = match op {
            gardenbed_model::Op::Delete => None,
            _ => {
                let blob = change
                    .blob_id
                    .as_deref()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok().map(Id::from_bytes))
                    .ok_or(ClientError::BlobHashMismatch(newer))?;
                let mode = mode_from_wire(change.mode)
                    .map_err(|_| ClientError::BlobHashMismatch(newer))?;
                if mode != Mode::Gitlink {
                    wanted.insert(blob);
                }
                Some(render::DiffSide { blob, mode })
            }
        };
        // The old side comes from the `a` tree. A path that was a directory
        // on the old side (file↔directory swap) has no old *file* entry —
        // the flat per-path deletes ride alongside as their own changes.
        let old_side = match op {
            gardenbed_model::Op::Add => None,
            _ => match session.locate_entry(older, &path, "diff").await? {
                Some(e) if e.kind == TreeKind::File => {
                    let blob = e.blob.ok_or(ClientError::BlobHashMismatch(older))?;
                    if e.mode != Mode::Gitlink {
                        wanted.insert(blob);
                    }
                    Some(render::DiffSide { blob, mode: e.mode })
                }
                _ => None,
            },
        };
        entries.push(DiffEntry {
            path,
            op: render::classify(old_side, new_side),
            old: old_side,
            new: new_side,
        });
    }

    // Fetch everything the renderer will read, cache-first, before any
    // output is produced.
    session.fetcher().blobs(&wanted).await?;
    Ok(entries)
}

/// `bed blame <path> [--cs id-or-ref]` — first-parent lineage blame of one
/// path (v0.1.md D7), rendered git-blame-like (see [`crate::render`]):
///
/// ```text
/// <16-hex-short> (<author name> <YYYY-MM-DD> <lineno>) <line content>
/// ```
///
/// Pure read like `file`/`log`: the path is validated and the changeset
/// resolved first, the file content is fetched cache-first, and identity
/// lookups per attributed changeset are cached for the run, so a file
/// blamed to k changesets costs at most k `GetChangeset` RPCs however many
/// ranges they cover. A path that is missing or a directory at the target
/// changeset, and submodule pointers, are refused before the blame RPC —
/// the same rules `bed file` applies.
pub async fn blame(state_dir: &Path, path: &str, cs: Option<&str>) -> Result<String, ClientError> {
    validate_path(path).map_err(|e| ClientError::Path {
        raw: path.to_owned(),
        reason: e.to_string(),
    })?;
    let bed = WorkingCopy::open(state_dir)?;
    if cs.is_none() {
        require_current_cs(&bed)?;
    }
    let session = Session::open(&bed).await?;
    let cs_id = resolve_target(&session, &bed, cs, "blame").await?;

    // The path must exist as a file at that changeset: blame answers a
    // content lineage, and a precise "no file here" beats the server's
    // generic not_found (which cannot distinguish path from changeset).
    let entry = session
        .locate_entry(cs_id, path, "blame")
        .await?
        .ok_or_else(|| ClientError::FileNotAFile {
            path: path.to_owned(),
            cs: cs_id.short(),
        })?;
    match entry.kind {
        TreeKind::Dir => {
            return Err(ClientError::FileNotAFile {
                path: path.to_owned(),
                cs: cs_id.short(),
            });
        }
        TreeKind::File => {}
    }
    if entry.mode == Mode::Gitlink {
        return Err(ClientError::FileIsGitlink {
            path: path.to_owned(),
            cs: cs_id.short(),
        });
    }

    let ranges = session.blame(cs_id, path, "blame").await?;
    let blob = entry.blob.ok_or(ClientError::BlobHashMismatch(cs_id))?;
    let content = session.fetcher().blob(blob).await?;

    // Identity lookups per attributed changeset, cached for this run.
    let mut authors: HashMap<Id, (String, u64, i32)> = HashMap::new();
    let mut groups = Vec::with_capacity(ranges.len());
    for r in &ranges {
        let id = blame_cs_id(r)?;
        let (name, time_secs, tz_min) = match authors.get(&id) {
            Some(hit) => hit.clone(),
            None => {
                let cs = session.get_changeset(id, "blame").await?;
                let hit = (
                    cs.author.name.clone(),
                    cs.author.time_secs,
                    cs.author.tz_min,
                );
                authors.insert(id, hit.clone());
                hit
            }
        };
        groups.push(render::BlameGroup {
            lo: r.lo,
            hi: r.hi,
            id,
            author_name: name,
            time_secs,
            tz_min,
        });
    }
    Ok(render::render_blame(&groups, &content))
}

/// The changeset id off a wire blame range (a 32-byte content address).
fn blame_cs_id(r: &gardenbed_protocol::pb::BlameRange) -> Result<Id, ClientError> {
    <[u8; 32]>::try_from(r.cs_id.as_slice())
        .map(Id::from_bytes)
        .map_err(|_| {
            ClientError::MalformedRpc(format!(
                "blame range {}-{} carries a {}-byte changeset address",
                r.lo,
                r.hi,
                r.cs_id.len()
            ))
        })
}

//! The server-backed commands (M4 read path): `clone`, `plot add`, `goto`,
//! `file`, `log`, `diff`; M6/M7 add `blame`.
//!
//! Two invariants shape every function here:
//!
//! * **validate, connect, then write** — argument validation and the
//!   network round-trips happen before any local state changes, so a
//!   failed command leaves the bed exactly as it stood (v0.1.md §M4.5:
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

use crate::bed::Bed;
use crate::error::BedError;
use crate::render::{self, DiffEntry, LogEntry};
use crate::rpc::{self, Session};
use crate::sync;

/// The ref every v0.1 bed grows from unless `--ref` says otherwise: the
/// first name the no-flag `clone` resolution probes, the tip `harvest`
/// consults after a lost race. (v0.1 has no remote-head discovery beyond
/// `ListRefs`; see `docs/v0.1.md` §2.)
const DEFAULT_REF: &str = "main";

/// The refs a no-flag `clone` probes, in order: `main` first — the v0.1
/// default, and the deterministic winner when several common names exist —
/// then the common primary-branch names an arboretum might grow instead.
/// When none resolve, the arboretum's own ref listing (`ListRefs`)
/// enriches the error with what actually exists.
const STARTING_REF_PROBES: [&str; 3] = [DEFAULT_REF, "master", "trunk"];

/// `bed clone <server> [--repo name] [--ref name] [--plot path...]` —
/// plant a new bed.
///
/// Validates everything local (no bed already here, repo name, plot
/// prefixes), then connects and resolves the starting ref **before any
/// state is written** — a server that is down, a repo that does not
/// exist, or a garden with none of the expected refs leaves zero state
/// behind. On success the full `.bed` layout is planted with `current_cs`
/// = the ref's changeset and the tip recorded in `heads`.
/// **Materializes nothing** (v0.1.md §2): planting a bed costs one ref
/// lookup, not a checkout. Plot prefixes handed to `--plot` are recorded
/// in the config; files come with `bed plot add`.
///
/// The starting ref: `--ref` names it exactly (bare names resolve against
/// `refs/heads/` server-side; full ref names pass through verbatim). A
/// ref the arboretum does not have is refused, nothing planted. Without
/// the flag, `main` is tried first — the v0.1 default, and the
/// deterministic winner when several common names exist — then the other
/// common names ([`STARTING_REF_PROBES`]), so an arboretum that only
/// grows `master` clones without a flag. When none resolve, the error
/// names what was tried and — via the arboretum's ref listing — what does
/// exist, pointing at `--ref`.
///
/// With `--repo` omitted the bed is named `garden` (v0.1 arboreta are
/// single-repo and offer no name discovery; a documented deviation).
pub async fn clone_with_ref(
    bed_dir: &Path,
    server: &str,
    repo: Option<&str>,
    ref_name: Option<&str>,
    plot: Vec<String>,
) -> Result<String, BedError> {
    // ---- validation phase: nothing has been written yet ----
    let repo = match repo {
        Some(r) if !r.trim().is_empty() => r.trim().to_owned(),
        Some(_) => {
            return Err(BedError::ConfigInvalid("repo name is empty".into()));
        }
        None => "garden".to_owned(),
    };
    let mut prefixes = Vec::with_capacity(plot.len());
    for raw in &plot {
        prefixes.push(crate::plot::normalize_prefix(raw)?);
    }
    if bed_dir.join("config.toml").exists() {
        return Err(BedError::AlreadyABed(bed_dir.to_path_buf()));
    }

    // ---- connection and resolution: still no local state ----
    let client = GardenbedClient::connect(server)
        .await
        .map_err(|e| BedError::Connect {
            server: server.to_owned(),
            message: e.to_string(),
        })?;
    let (ref_name, main) = match ref_name {
        Some(name) => match client.get_ref(&repo, name).await {
            Ok(Some(id)) => (name.to_owned(), id),
            Ok(None) => {
                return Err(BedError::MissingRef {
                    name: name.to_owned(),
                });
            }
            Err(status) => {
                return Err(BedError::Rpc {
                    command: "clone",
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        },
        None => resolve_starting_ref(&client, &repo).await?,
    };
    // The ref must name a real changeset (a ref to nowhere is refused here,
    // not discovered half-planted).
    match client.get_changeset(&repo, main).await {
        Ok(_) => {}
        Err(status) if status.code() == Code::NotFound => {
            return Err(BedError::MissingCs(main));
        }
        Err(status) => {
            return Err(BedError::Rpc {
                command: "clone",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    }

    // ---- plant the layout ----
    let mut bed = Bed::init(bed_dir, server, &repo)?;
    bed.config_mut().plot = prefixes;
    bed.config_mut().current_cs = Some(main.to_hex());
    bed.save_config()?;
    bed.heads().append(main)?;

    Ok(format!(
        "planted a bed for {repo} at {server}\n\
         the garden grows from {} ({ref_name})\n\
         materialized nothing — plant files with `bed plot add <path>`\n",
        main.short()
    ))
}

/// [`clone_with_ref`] without `--ref`: the starting ref is left to the
/// no-flag fallback resolution. Kept as the narrow entry point the
/// benchmark (`crates/bench`) and pre-`--ref` callers use.
pub async fn clone(
    bed_dir: &Path,
    server: &str,
    repo: Option<&str>,
    plot: Vec<String>,
) -> Result<String, BedError> {
    clone_with_ref(bed_dir, server, repo, None, plot).await
}

/// Resolves the ref a no-flag `clone` grows from: `main` first ([`DEFAULT_REF`],
/// the deterministic winner when several common names exist), then the
/// other common primary-branch names, so an arboretum that only grows
/// `master` clones without a flag. When nothing resolves, the arboretum's
/// ref listing (best-effort: its failure cannot change the verdict already
/// earned from the probes) names what does exist, and the error points at
/// `--ref`.
async fn resolve_starting_ref(
    client: &GardenbedClient,
    repo: &str,
) -> Result<(String, Id), BedError> {
    for name in STARTING_REF_PROBES {
        match client.get_ref(repo, name).await {
            Ok(Some(id)) => return Ok((name.to_owned(), id)),
            Ok(None) => {}
            Err(status) => {
                return Err(BedError::Rpc {
                    command: "clone",
                    code: status.code(),
                    message: status.message().to_owned(),
                });
            }
        }
    }
    // Best-effort listing: it only ever enriches the error with the
    // arboretum's own ref names (the full stored spellings `--ref`
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
    Err(BedError::NoStartingRef {
        primary: DEFAULT_REF.to_owned(),
        tried: STARTING_REF_PROBES.join(", "),
        available: if names.is_empty() {
            String::new()
        } else {
            format!("\n  the arboretum has refs: {}", names.join(", "))
        },
    })
}

/// `bed plot add <paths...> [--concurrency N]` — record the prefixes, then
/// want-list fetch and materialize them at the current changeset.
///
/// Re-adding an existing plot is idempotent: the cache-first fetch pulls
/// nothing the bed already has, and faithful files are kept untouched.
/// The config only gains the new prefixes once the fetch has succeeded, so
/// a server that is down leaves the plot list (and everything else)
/// unchanged.
pub async fn plot_add(
    bed_dir: &Path,
    paths: &[String],
    concurrency: usize,
) -> Result<String, BedError> {
    let mut bed = Bed::open(bed_dir)?;
    let mut added: Vec<String> = Vec::new();
    for raw in paths {
        let prefix = crate::plot::normalize_prefix(raw)?;
        if !bed.config().plot.contains(&prefix) {
            bed.config_mut().plot.push(prefix.clone());
            added.push(prefix);
        }
    }

    // Fetch (cache-first) before saving config or materializing: a failed
    // fetch leaves neither the plot list nor the treestate half-changed.
    let session = Session::open_with(&bed, concurrency).await?;
    let cs = session.resolve_current_cs(&bed, "plot add").await?;
    let root = session.root_tree(cs, "plot add").await?;
    let prefixes: Vec<String> = bed.config().plot.clone();
    let (plan_files, stats) = rpc::plan_plot(&session, root, &prefixes).await?;

    // Which of the newly added prefixes actually matched something at the
    // current changeset? Computed before the plan moves into sync_plot.
    let prefix_matches: Vec<bool> = added
        .iter()
        .map(|p| {
            plan_files
                .iter()
                .any(|f| crate::plot::in_plot(&f.path, std::slice::from_ref(p)))
        })
        .collect();

    let treestate = bed.treestate()?;
    let outcome = sync::sync_plot(
        &bed.garden_root(),
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
            out.push_str(&format!("plotted {prefix}\n"));
        } else {
            out.push_str(&format!(
                "plotted {prefix} — note: nothing exists under it at the \
                 current changeset; it fills as history lands (check the \
                 spelling if that surprises you)\n"
            ));
        }
    }
    if added.is_empty() {
        out.push_str("those prefixes are already in the plot — re-tending them\n");
    }
    out.push_str(&format!(
        "fetched {} tree(s) and {} blob(s) from the arboretum ({} file(s) in the plot)\n",
        stats.trees_fetched, stats.blobs_fetched, stats.files
    ));
    out.push_str(&outcome.summarize());
    out.push('\n');
    Ok(out)
}

/// `bed goto <cs-or-ref>` — move the bed to a changeset and refresh the
/// plot's working copy to match it (fetch missing, rewrite changed, remove
/// gone). Local edits survive when the target leaves their paths unchanged;
/// destructive collisions are refused before any materialization.
///
/// The config's `current_cs` moves only after successful materialization.
/// A fetch failure or preflight refusal leaves the bed where it stood;
/// an interrupted refresh is repaired by simply running goto again.
pub async fn goto(bed_dir: &Path, spec: &str) -> Result<String, BedError> {
    let mut bed = Bed::open(bed_dir)?;
    let parsed = crate::revspec::CsOrRef::parse(spec)?;
    let session = Session::open(&bed).await?;
    let cs = session.resolve(&parsed, "goto").await?;

    // Fetch everything the plot needs at the new changeset before writing
    // anything locally.
    let root = session.root_tree(cs, "goto").await?;
    let prefixes = bed.config().plot.clone();
    let (plan, stats) = rpc::plan_plot(&session, root, &prefixes).await?;

    let treestate = bed.treestate()?;
    let outcome = sync::sync_plot(
        &bed.garden_root(),
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
        "the bed now grows from {} ({}) — fetched {} tree(s), {} blob(s)\n{}\n",
        cs.short(),
        where_from,
        stats.trees_fetched,
        stats.blobs_fetched,
        outcome.summarize()
    ))
}

/// `bed file <path> [--cs id-or-ref]` — print one file's raw bytes at a
/// changeset. Trees along the walk are cached; the blob is fetched
/// cache-first. Pure read: no treestate, config, or heads writes.
pub async fn file(
    bed_dir: &Path,
    path: &str,
    cs: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), BedError> {
    validate_path(path).map_err(|e| BedError::Path {
        raw: path.to_owned(),
        reason: e.to_string(),
    })?;
    let bed = Bed::open(bed_dir)?;
    if cs.is_none() {
        require_current_cs(&bed)?;
    }
    let session = Session::open(&bed).await?;
    let cs_id = resolve_target(&session, &bed, cs, "file").await?;
    let entry = session
        .locate_entry(cs_id, path, "file")
        .await?
        .ok_or_else(|| BedError::FileNotAFile {
            path: path.to_owned(),
            cs: cs_id.short(),
        })?;
    match entry.kind {
        TreeKind::Dir => {
            return Err(BedError::FileNotAFile {
                path: path.to_owned(),
                cs: cs_id.short(),
            });
        }
        TreeKind::File => {}
    }
    if entry.mode == Mode::Gitlink {
        return Err(BedError::FileIsGitlink {
            path: path.to_owned(),
            cs: cs_id.short(),
        });
    }
    // "Present exactly when kind == File" is the tree invariant; a missing
    // blob id would mean the arboretum handed us a malformed tree.
    let blob = entry.blob.ok_or(BedError::BlobHashMismatch(cs_id))?;
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

/// `bed graft <new-parent>` — replay the current changeset onto a new
/// parent (the server-side linearized rebase, v0.1.md D5) and move the bed
/// onto the replayed changeset.
///
/// The server returns the grafted changeset's id, or refuses with
/// `FAILED_PRECONDITION` naming the conflicting paths (same path written
/// on both sides, delete of a missing path, …) — surfaced as
/// [`BedError::Rpc`], verbatim. On success the bed treats the result like
/// a `goto`: the plot is re-planned and re-synced at the grafted changeset
/// (which usually changes nothing — the replay carries this bed's own
/// changes — but the new parent may differ elsewhere under the plot), the
/// config moves, and the heads stack is re-pointed from the old changeset
/// to the grafted one.
pub async fn graft(bed_dir: &Path, new_parent: &str) -> Result<String, BedError> {
    let mut bed = Bed::open(bed_dir)?;
    let parsed = crate::revspec::CsOrRef::parse(new_parent)?;
    let session = Session::open(&bed).await?;
    let current = session.resolve_current_cs(&bed, "graft").await?;
    let parent = session.resolve(&parsed, "graft").await?;

    // The replay is the arboretum's decision; conflicts come back as a
    // FAILED_PRECONDITION whose message lists every conflicting path.
    let grafted = match session
        .client
        .graft(&bed.config().repo, current, parent)
        .await
    {
        Ok(id) => id,
        Err(status) => {
            return Err(BedError::Rpc {
                command: "graft",
                code: status.code(),
                message: status.message().to_owned(),
            });
        }
    };

    // Move the bed like `goto` does: plan the plot at the grafted
    // changeset, then fetch + sync.
    let root = session.root_tree(grafted, "graft").await?;
    let prefixes = bed.config().plot.clone();
    let (plan, stats) = rpc::plan_plot(&session, root, &prefixes).await?;
    let treestate = bed.treestate()?;
    let outcome = sync::sync_plot(
        &bed.garden_root(),
        &prefixes,
        plan,
        session.fetcher(),
        &treestate,
    )
    .await?;
    bed.config_mut().current_cs = Some(grafted.to_hex());
    bed.save_config()?;
    bed.heads().repoint(current, grafted)?;

    Ok(format!(
        "grafted {} onto {} as {}\n\
         fetched {} tree(s), {} blob(s)\n{}\n",
        current.short(),
        parent.short(),
        grafted.short(),
        stats.trees_fetched,
        stats.blobs_fetched,
        outcome.summarize(),
    ))
}

/// `bed harvest` — fast-forward `main` to the bed's current changeset.
///
/// The arboretum enforces the ancestry precondition with a
/// compare-and-swap: exactly one harvester wins a race, everyone else
/// receives [`BedError::HarvestNotFastForward`] with the fix spelled out.
/// Success appends the harvested changeset to the heads stack; the bed
/// stays where it is — harvesting publishes, it does not move.
pub async fn harvest(bed_dir: &Path) -> Result<String, BedError> {
    let bed = Bed::open(bed_dir)?;
    let session = Session::open(&bed).await?;
    let repo = bed.config().repo.clone();
    let current = session.resolve_current_cs(&bed, "harvest").await?;

    match session.client.harvest(&repo, current).await {
        // The returned tip is `main`'s value after the fast-forward —
        // the changeset itself for a conforming arboretum.
        Ok(_tip) => {
            bed.heads().append(current)?;
            Ok(format!(
                "harvested {} — `main` now grows from it\n",
                current.short()
            ))
        }
        Err(status) if status.code() == Code::FailedPrecondition => {
            // The ref moved under us (a concurrent harvest, or history
            // landing from elsewhere). Name the new tip when the arboretum
            // will say it, so the fix is one command away.
            let tip = session
                .client
                .get_ref(&repo, DEFAULT_REF)
                .await
                .ok()
                .flatten()
                .map(|id| format!(", which now grows from {}", id.short()))
                .unwrap_or_default();
            Err(BedError::HarvestNotFastForward {
                cs: current.short(),
                tip,
            })
        }
        Err(status) => Err(BedError::Rpc {
            command: "harvest",
            code: status.code(),
            message: status.message().to_owned(),
        }),
    }
}

/// Resolves the changeset a read command should answer at: an explicit
/// `<cs-or-ref>` when given, else the bed's current changeset. A missing
/// current changeset is reported before any connection is attempted (the
/// bed's own state is the actionable error there).
fn require_current_cs(bed: &Bed) -> Result<(), BedError> {
    bed.config()
        .current_cs
        .as_ref()
        .map(|_| ())
        .ok_or(BedError::NoCurrentCs)
}

/// Resolves the changeset a read command should answer at: an explicit
/// `<cs-or-ref>` when given, else the bed's current changeset.
async fn resolve_target(
    session: &Session,
    bed: &Bed,
    cs: Option<&str>,
    command: &'static str,
) -> Result<Id, BedError> {
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
pub async fn log(bed_dir: &Path, limit: Option<u32>, cs: Option<&str>) -> Result<String, BedError> {
    let bed = Bed::open(bed_dir)?;
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
pub async fn diff(bed_dir: &Path, a: Option<&str>, b: Option<&str>) -> Result<String, BedError> {
    let bed = Bed::open(bed_dir)?;
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
            return Err(BedError::ConfigInvalid(
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
    let content_of = |id: Id| -> Result<Vec<u8>, BedError> {
        objects.get_blob(id)?.ok_or(BedError::BlobHashMismatch(id))
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
) -> Result<Vec<DiffEntry>, BedError> {
    let mut entries = Vec::with_capacity(changes.len());
    let mut wanted: HashSet<Id> = HashSet::new();

    for change in &changes {
        let op = op_from_wire(change.op).map_err(|_| BedError::BlobHashMismatch(newer))?;
        let path = change.path.clone();
        let new_side = match op {
            gardenbed_model::Op::Delete => None,
            _ => {
                let blob = change
                    .blob_id
                    .as_deref()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok().map(Id::from_bytes))
                    .ok_or(BedError::BlobHashMismatch(newer))?;
                let mode =
                    mode_from_wire(change.mode).map_err(|_| BedError::BlobHashMismatch(newer))?;
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
                    let blob = e.blob.ok_or(BedError::BlobHashMismatch(older))?;
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
pub async fn blame(bed_dir: &Path, path: &str, cs: Option<&str>) -> Result<String, BedError> {
    validate_path(path).map_err(|e| BedError::Path {
        raw: path.to_owned(),
        reason: e.to_string(),
    })?;
    let bed = Bed::open(bed_dir)?;
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
        .ok_or_else(|| BedError::FileNotAFile {
            path: path.to_owned(),
            cs: cs_id.short(),
        })?;
    match entry.kind {
        TreeKind::Dir => {
            return Err(BedError::FileNotAFile {
                path: path.to_owned(),
                cs: cs_id.short(),
            });
        }
        TreeKind::File => {}
    }
    if entry.mode == Mode::Gitlink {
        return Err(BedError::FileIsGitlink {
            path: path.to_owned(),
            cs: cs_id.short(),
        });
    }

    let ranges = session.blame(cs_id, path, "blame").await?;
    let blob = entry.blob.ok_or(BedError::BlobHashMismatch(cs_id))?;
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
fn blame_cs_id(r: &gardenbed_protocol::pb::BlameRange) -> Result<Id, BedError> {
    <[u8; 32]>::try_from(r.cs_id.as_slice())
        .map(Id::from_bytes)
        .map_err(|_| {
            BedError::MalformedRpc(format!(
                "blame range {}-{} carries a {}-byte changeset address",
                r.lo,
                r.hi,
                r.cs_id.len()
            ))
        })
}

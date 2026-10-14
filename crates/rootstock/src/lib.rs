//! rootstock — the git import/export bridge.
//!
//! The **import** direction mirrors a git repository into gardenbed's
//! changeset stores. The **export** direction ([`export_to_path`]) renders
//! those stores back into a real git repository via one `git fast-import`
//! stream. The trust anchor is differential verification: the harness
//! replays imported changesets against git's own answers, and the export
//! roundtrip reproduces byte-identical git SHAs for unsigned histories.
//!
//! # Semantics
//!
//! * **Ingestion order** is topological (Kahn) over the reachable commit
//!   graph with compact `u32` arena ids; parents are always ingested
//!   before children, octopus merges included. Changes are computed
//!   against the **first parent** only — matching gardenbed's changeset
//!   semantics; merge parents are recorded as additional parents.
//! * **Blob identity** is `SHA-256(raw content)` (model spec). Submodule
//!   pointers (`160000` gitlinks) are never dereferenced: their blob field
//!   carries the upstream git SHA-1 left-padded to 32 bytes and no blob is
//!   written. Symlink blobs are their (byte-exact) link target.
//! * **Object reads** go through gitoxide ([`gix`], decision D6).
//!   **Status classification and rename detection** go through batched
//!   `git diff-tree --stdin --root -r -z --name-status
//!   --diff-merges=first-parent [-M|--no-renames]` subprocesses (decision
//!   D8) — 256 commits per spawn, attributed by 40-hex commit-id tokens.
//!   A commit whose diff is empty emits no output at all (verified git
//!   behavior); absence of a token is treated as an empty diff and
//!   cross-checked against the trees. Rename detection is git's own
//!   (`-M`, default 50% similarity): a rename whose content changed
//!   below that threshold is imported as the add+delete pair git reports,
//!   which yields the identical tree under a different (valid) change
//!   shape.
//! * **Dir→file replacements** (git reports `A P` + `D P/…`) are repaired
//!   into a shape the derive applier can consume — see
//!   [`transform`](self) module docs. Type changes (`T`) import as
//!   modifies; copies (`C`, impossible with `-M` alone) as renames.
//! * **Refs** are stored as full names (`refs/heads/main`) and moved with
//!   a compare-and-swap; a lost race is a hard error. Symbolic refs are
//!   skipped; annotated tags are deliberately not read in v0.1 (only
//!   `refs/heads/*` is consulted).
//! * **Incrementality**: commits whose git SHA already has a mapping are
//!   skipped without touching any object; mapped history is not descended
//!   (sound because git ids are content-addressed). A re-import of an
//!   unchanged repository writes zero objects.
//! * **Force pushes** are a non-event: refs simply move (CAS), old
//!   objects and their mapping rows remain (valid, merely unreachable).
//! * **`limit`** imports the newest `N` commits as a suffix of the
//!   topological order; slice boundaries are re-rooted (full-tree root
//!   changesets) and out-of-slice extra parents are dropped. This is a
//!   verification convenience, **not** a fidelity mode: the slice mapping
//!   is not a bijection of the full history, and later parents that were
//!   re-rooted over cannot be completed by a subsequent full import (they
//!   are already mapped, so the walk prunes them) — re-import the full
//!   repository into a fresh store for full fidelity.
//!
//! # Non-goals / documented divergences (v0.1)
//!
//! * SHA-256 object-format repositories and shallow clones are rejected
//!   with [`ImportError::Unsupported`]; an exported repository in sha256
//!   format is rejected with [`ExportError::Git`].
//! * Non-UTF-8 commit bytes (names, emails, messages, tree entry names)
//!   are decoded lossily; gardenbed stores `String`s (and export writes
//!   those `String`s verbatim, so such commits do not round-trip).
//! * Annotated tags are never read; tags-reachable-only commits are not
//!   imported.
//! * Negative (pre-1970) commit timestamps are rejected (the model stores
//!   `u64` seconds).
//! * Concurrent imports of the same repository into the same stores race
//!   on the mapping table (a lost bijection insert aborts the import);
//!   ref updates themselves are CAS-atomic.
//! * **Signed commits** are exported unsigned — fast-import cannot emit a
//!   `gpgsig` header — so their git SHAs differ from the originals
//!   (counted in [`ExportReport::signed_dropped`]; see
//!   `docs/divergences.md`).
//! * Extra commit headers (`encoding`, `mergetag`, `gpgsig-sha256`, …)
//!   are not modeled on either side of the bridge.
//!
//! # Errors
//!
//! Nothing panics on bad input: malformed git data surfaces as
//! [`ImportError`] or [`ExportError`] with the offending object named.

mod commit;
mod counting;
mod difftree;
mod error;
mod export;
mod graph;
mod gtree;
mod import;
mod oid;
mod options;
mod report;
mod resolve;
mod transform;

pub use error::{ExportError, ImportError};
pub use export::{export_to_path, ExportOptions, ExportReport};
pub use import::import_from_path;
pub use options::ImportOptions;
pub use report::ImportReport;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use gardenbed_derive::Engine;
use gardenbed_store::{MetadataStore, ObjectStore};

/// Imports a git repository by URL: mirrors the remote into `mirror_dir`
/// (a bare `git clone --mirror`, created on first use and refreshed with
/// `git remote update` afterwards) and then imports it like
/// [`import_from_path`].
///
/// The network part deliberately uses the `git` CLI rather than gitoxide:
/// fetch/transport is exactly the piece D8 carves out for subprocesses in
/// v0.1, and git's fetch is the reference implementation. `url` is
/// anything `git clone` accepts.
pub fn import_from_url(
    url: &str,
    mirror_dir: &Path,
    objects: Arc<dyn ObjectStore>,
    metadata: Arc<dyn MetadataStore>,
    engine: Engine,
    opts: &ImportOptions,
) -> Result<ImportReport, ImportError> {
    if mirror_dir.exists() {
        run_git(
            mirror_dir,
            &["remote", "update", "--prune"],
            "refreshing the mirror",
        )?;
    } else {
        if let Some(parent) = mirror_dir.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ImportError::Git(format!("creating {}: {e}", parent.display())))?;
        }
        run_git(
            mirror_dir.parent().unwrap_or(Path::new(".")),
            &["clone", "--mirror", url, &mirror_dir.to_string_lossy()],
            "cloning the mirror",
        )?;
    }
    import_from_path(mirror_dir, objects, metadata, engine, opts)
}

/// Runs a git command with a hermetic environment and maps failures to
/// [`ImportError::Git`].
fn run_git(cwd: &Path, args: &[&str], what: &str) -> Result<Vec<u8>, ImportError> {
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
    cmd.args(args);
    let out = cmd
        .output()
        .map_err(|e| ImportError::Git(format!("cannot spawn git ({}): {e}", what)))?;
    if !out.status.success() {
        return Err(ImportError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(out.stdout)
}

/// Counts the object entries under a loam directory (`<root>/objects` and
/// everything below it). A test/diagnostic helper: for the filesystem
/// backend this must equal [`ImportReport::objects_written`] for an
/// import into a fresh store, and its delta for incremental imports.
pub fn count_loam_entries(objects_root: &Path) -> std::io::Result<u64> {
    let mut count = 0u64;
    let mut stack = vec![objects_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(entry.path());
            } else {
                count += 1;
            }
        }
    }
    Ok(count)
}

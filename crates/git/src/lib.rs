//! gardenbed-git — Git import and export.
//!
//! The **import** direction mirrors a git repository into Gardenbed's
//! changeset stores. The **export** direction ([`export_to_path`])
//! renders those stores back into a real git repository with a native
//! pack writer (v0.2 D20/D21): full objects, no deltas, SHA-1, every
//! computed commit SHA asserted against the git-mapping table. The trust
//! anchor is differential verification: the harness replays imported
//! changesets against git's own answers, and the export roundtrip
//! reproduces byte-identical git SHAs — including signed and
//! extra-header commits, via raw-commit capture at import.
//!
//! # Semantics
//!
//! * **Ingestion order** is topological (Kahn) over the reachable commit
//!   graph with compact `u32` arena ids; parents are always ingested
//!   before children, octopus merges included. Changes are computed
//!   against the **first parent** only — matching Gardenbed's changeset
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
//!   cross-checked against the trees. Commits the diff machinery cannot
//!   see — git canonicalizes tree entry modes before comparing
//!   (`canon_mode`), so pre-canonical-mode fixups such as the early-2005
//!   `100664` → `100644` normalizations report nothing although the raw
//!   trees differ — are classified by a raw, mode-aware tree comparison:
//!   pure normalizations become zero-change changesets (the derived tree
//!   is canonical and differs from git's raw tree sha by construction),
//!   never invented renames. Rename detection is git's own
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
//! * **Force pushes** move refs like any update (CAS); old objects and
//!   their mapping rows remain (valid, merely unreachable). But a ref
//!   move that would make previously-published changesets unreachable is
//!   never silent: the import reports it in
//!   [`ImportReport::refs_rewound`] (ref name, old and new tips, how many
//!   changesets fell off), and — unless [`ImportOptions::allow_rewind`]
//!   is set — fails with [`ImportError::Ref`] before touching the ref.
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
//! * v0.1 stores have no raw-commit rows: their signed changesets refuse
//!   loudly at export (the SHA assertion fires before anything ships —
//!   never a silent divergence); re-import retires the divergence with
//!   stable changeset ids.
//! * A `limit`-sliced store exports only when every raw commit's parents
//!   are inside the export (slice boundaries name below-slice parents;
//!   the export refuses loudly rather than emit a dangling pack).
//!
//! # Errors
//!
//! Nothing panics on bad input: malformed git data surfaces as
//! [`ImportError`] or [`ExportError`] with the offending object named.

mod commit;
mod counting;
// `difftree` and `oid` are public for the fuzz target
// (`fuzz/fuzz_targets/fuzz_difftree_parse.rs`), which feeds random bytes to
// the batch parser; they are not part of the stabilized surface.
pub mod difftree;
mod error;
mod export;
mod graph;
mod gtree;
mod import;
pub mod oid;
mod options;
// `pack`'s read side (`read_pack`/`read_idx`) is public for the fuzz
// target (`fuzz/fuzz_targets/fuzz_pack_reader.rs`) exactly like
// `difftree` above: random bytes into the reader must never panic and
// never trust a declared length. Not part of the stabilized surface.
pub mod pack;
mod report;
mod resolve;
mod transform;

pub use error::{ExportError, ImportError};
pub use export::{ExportOptions, ExportReport, export_to_path};
pub use import::import_from_path;
pub use options::ImportOptions;
pub use report::{ImportReport, RefRewind};

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

/// Counts the object entries under a objects directory (`<root>/objects` and
/// everything below it). A test/diagnostic helper: for the filesystem
/// backend this must equal [`ImportReport::objects_written`] for an
/// import into a fresh store, and its delta for incremental imports.
pub fn count_objects_entries(objects_root: &Path) -> std::io::Result<u64> {
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

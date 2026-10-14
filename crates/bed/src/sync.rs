//! Materializing the plot: writing the want-list plan onto the garden
//! (files, modes, symlinks) and the treestate, exactly once per path.
//!
//! Shared by `bed plot add` (sync the added prefixes at the current
//! changeset) and `bed goto` (sync the whole plot at the new changeset).
//! Both converge: every step is idempotent, so an interrupted run that is
//! repeated finishes the job — files whose content already matches the
//! plan are kept (one hash read, no rewrite), missing or drifted files are
//! written, rows the plan no longer covers are removed together with their
//! files.
//!
//! Materialization rules (v0.1.md §2, harness contract):
//! * regular/exec files — bytes under `path`, parents created; exec mode
//!   chmods `0o755`, regular mode normalizes to `0o644`;
//! * symlinks — the blob's bytes are the link target; an existing entry at
//!   the path (file or symlink) is replaced;
//! * gitlinks — submodule pointers are never materialized as files; only
//!   the treestate row is recorded (mode [`MODE_GITLINK`]), so `status`
//!   stays clean;
//! * file↔directory swaps are handled by removing the not-in-plan entries
//!   first, then writing (a directory that a plan file replaces is removed
//!   with its contents — the plot's space is bed's to tend).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;

use gardenbed_model::Mode;
use gardenbed_model::blob_id;

use crate::error::BedError;
use crate::plot::in_plot;
use crate::rpc::PlanFile;
use crate::status::mtime_ns;
use crate::treestate::HINT_UNVERIFIED;
use crate::treestate::HINT_VERIFIED;
use crate::treestate::MODE_GITLINK;
use crate::treestate::MODE_SYMLINK;
use crate::treestate::Treestate;
use crate::treestate::TreestateEntry;

/// What one sync run did to the garden.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Files written (new content or mode) or created.
    pub written: usize,
    /// Files already correct (content and mode) — kept, hints refreshed.
    pub kept: usize,
    /// Tracked entries the plan no longer covers: file removed, row
    /// forgotten (gitlink rows just lose their row).
    pub removed: usize,
}

impl SyncOutcome {
    /// A one-line garden-voice summary of the materialization.
    pub fn summarize(&self) -> String {
        let mut parts = Vec::new();
        if self.written > 0 {
            parts.push(format!("{} file(s) materialized", self.written));
        }
        if self.kept > 0 {
            parts.push(format!("{} already faithful", self.kept));
        }
        if self.removed > 0 {
            parts.push(format!("{} removed", self.removed));
        }
        if parts.is_empty() {
            "nothing to materialize".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// Brings the working copy under `plot` in line with `plan`, relative to
/// the garden root. Blobs must already be in the local cache (the plan
/// fetch guarantees it); this function never touches the network.
///
/// Only paths inside `plot` are touched. Treestate rows are upserted with
/// freshly observed size/mtime and the verified hint; rows the plan no
/// longer covers (inside the plot) are deleted along with their files.
pub async fn sync_plot(
    garden_root: &std::path::Path,
    plot: &[String],
    plan: Vec<PlanFile>,
    fetcher: &crate::rpc::Fetcher,
    treestate: &Treestate,
) -> Result<SyncOutcome, BedError> {
    // Target set: path → plan file. Plans are deduplicated by path already,
    // but a map keeps the lookups below cheap and ordered.
    let mut target: BTreeMap<String, PlanFile> = BTreeMap::new();
    for file in plan {
        target.insert(file.path.clone(), file);
    }

    // Pass 1 — entries the plan no longer covers (inside the plot): remove
    // the file (any shape) and forget the row.
    let mut outcome = SyncOutcome::default();
    for path in treestate.all_paths()? {
        if target.contains_key(&path) || !in_plot(&path, plot) {
            continue;
        }
        let abs = garden_root.join(&path);
        // Remove whatever stands at the path: file, symlink, or a
        // directory left over from a previous shape of the garden.
        match fs::symlink_metadata(&abs) {
            Ok(m) if m.is_dir() => {
                fs::remove_dir_all(&abs)?;
            }
            Ok(_) => {
                fs::remove_file(&abs)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        treestate.remove(&path)?;
        outcome.removed += 1;
    }

    // Pass 2 — materialize every planned entry. Gitlink pointers never
    // become files; everything else is written or verified.
    for (path, file) in &target {
        let abs = garden_root.join(path);
        if file.mode == Mode::Gitlink {
            // The pointer's "content" never exists as bytes to plant; the
            // row records the upstream id so status stays clean and the
            // pointer round-trips.
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            remove_if_present(&abs)?;
            treestate.upsert(&TreestateEntry {
                path: path.clone(),
                blob_id: file.blob,
                mode: MODE_GITLINK,
                size: 32, // the upstream git SHA-1, zero-padded to 32
                mtime_ns: 0,
                hint: HINT_UNVERIFIED,
            })?;
            outcome.kept += 1;
            continue;
        }

        let content = fetcher.blob(file.blob).await?;
        let is_symlink = file.mode == Mode::Symlink;
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }

        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_symlink() => {
                let target = fs::read_link(&abs)?;
                let matches = is_symlink && target.as_os_str().as_bytes() == content.as_slice();
                if matches {
                    record_verified(treestate, path, file.blob, file.mode_code(), &meta)?;
                    outcome.kept += 1;
                } else {
                    fs::remove_file(&abs)?;
                    write_entry(&abs, &content, file.mode)?;
                    record_written(treestate, garden_root, path, file)?;
                    outcome.written += 1;
                }
            }
            Ok(meta) if meta.is_file() => {
                let current = fs::read(&abs)?;
                let same_bytes = !is_symlink && blob_id(&current) == file.blob;
                let mode_code = if is_symlink {
                    MODE_SYMLINK
                } else {
                    mode_code_of(&meta)
                };
                if same_bytes && mode_code == file.mode_code() {
                    record_verified(treestate, path, file.blob, file.mode_code(), &meta)?;
                    outcome.kept += 1;
                } else {
                    // Drifted content, a file where a symlink belongs, or a
                    // mode that moved: rewrite faithfully.
                    fs::remove_file(&abs)?;
                    write_entry(&abs, &content, file.mode)?;
                    record_written(treestate, garden_root, path, file)?;
                    outcome.written += 1;
                }
            }
            Ok(_) => {
                // A directory stands at the path. The plot's space is bed's
                // to tend: clear it and plant the entry.
                fs::remove_dir_all(&abs)?;
                write_entry(&abs, &content, file.mode)?;
                record_written(treestate, garden_root, path, file)?;
                outcome.written += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_entry(&abs, &content, file.mode)?;
                record_written(treestate, garden_root, path, file)?;
                outcome.written += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(outcome)
}

/// Removes a file or symlink at `path` if one stands there.
fn remove_if_present(path: &std::path::Path) -> Result<(), BedError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path)?,
        Ok(_) => {
            fs::remove_file(path)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// Writes one entry: a symlink whose target is `content`, or a file whose
/// mode comes from the plan.
fn write_entry(path: &std::path::Path, content: &[u8], mode: Mode) -> Result<(), BedError> {
    if mode == Mode::Symlink {
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(content), path)?;
    } else {
        fs::write(path, content)?;
        apply_mode(path, mode)?;
    }
    Ok(())
}

/// Applies the plan's mode to a materialized file: exec → `0o755`,
/// regular → `0o644` (normalized, so a stale exec bit cannot linger).
fn apply_mode(path: &std::path::Path, mode: Mode) -> Result<(), BedError> {
    let bits = match mode {
        Mode::Exec => 0o755,
        _ => 0o644,
    };
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() & !0o777 | bits);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// The §4 mode code of an on-disk regular file (exec bit → exec).
fn mode_code_of(meta: &fs::Metadata) -> i64 {
    if meta.permissions().mode() & 0o111 != 0 {
        crate::treestate::MODE_EXEC
    } else {
        crate::treestate::MODE_REGULAR
    }
}

/// Records a freshly written entry with its just-observed metadata.
fn record_written(
    treestate: &Treestate,
    garden_root: &std::path::Path,
    path: &str,
    file: &PlanFile,
) -> Result<(), BedError> {
    let meta = fs::symlink_metadata(garden_root.join(path))?;
    let mode_code = if meta.is_symlink() {
        MODE_SYMLINK
    } else {
        mode_code_of(&meta)
    };
    record_verified(treestate, path, file.blob, mode_code, &meta)
}

/// Upserts a treestate row with freshly observed size/mtime; the content
/// was just written or hash-compared, so the row is verified.
fn record_verified(
    treestate: &Treestate,
    path: &str,
    blob: gardenbed_model::Id,
    mode_code: i64,
    meta: &fs::Metadata,
) -> Result<(), BedError> {
    treestate.upsert(&TreestateEntry {
        path: path.to_owned(),
        blob_id: blob,
        mode: mode_code,
        size: meta.len() as i64,
        mtime_ns: mtime_ns(meta),
        hint: HINT_VERIFIED,
    })
}

/// The set of paths a plan covers.
pub fn plan_paths(plan: &[PlanFile]) -> BTreeSet<String> {
    plan.iter().map(|f| f.path.clone()).collect()
}

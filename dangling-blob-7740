//! Materializing the plot: writing the want-list plan onto the garden
//! (files, modes, symlinks) and the treestate, exactly once per path.
//!
//! Shared by `bed plot add` (sync the added prefixes at the current
//! changeset) and `bed goto` (sync the whole plot at the new changeset).
//! A complete preflight rejects overwrites of unsown work and untracked
//! collisions before changing files or treestate. Local edits to paths
//! whose target still equals the indexed baseline are preserved. Repeated
//! runs accept already-materialized target entries after interruption.
//!
//! Filesystem operations are anchored to open directory descriptors and
//! refuse symlink ancestors. Symlink leaves store the blob's target bytes;
//! regular files normalize mode to 0o644, executable files to 0o755.
//! Gitlinks are recorded in treestate without materializing a file.
//! File↔directory swaps are allowed only when every removed descendant is
//! clean and tracked; untracked descendants, including empty directories,
//! are preserved by refusing the operation.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use gardenbed_model::Mode;

use crate::error::BedError;
use crate::plot::in_plot;
use crate::rpc::PlanFile;
use crate::treestate::HINT_UNVERIFIED;
use crate::treestate::HINT_VERIFIED;
use crate::treestate::MODE_GITLINK;
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
    let target: BTreeMap<String, PlanFile> = plan
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect();
<<<<<<< HEAD
    let disk = crate::checkout_fs::CheckoutFs::open(garden_root)?;
    // Validate the whole operation before changing any file or index row.
    let preserve = preflight(garden_root, plot, &target, treestate, &disk)?;
    let mut outcome = SyncOutcome::default();
=======
    let disk = crate::checkout_fs::CheckoutFs::open(
        garden_root,
        treestate.path().parent().expect("state directory"),
    )?;
    // Only identity-registered staging entries from dead writers are
    // reclaimed. Substituted names remain ordinary local/untracked work.
    disk.cleanup_staging()?;
    // Validate the whole operation before changing any file or index row.
    let preserve = preflight(garden_root, plot, &target, treestate, &disk)?;
    let mut outcome = SyncOutcome::default();
    let mut obsolete = Vec::new();
>>>>>>> codex/integrity-fixes

    for path in treestate.all_paths()? {
        if target.contains_key(&path) || !in_plot(&path, plot) {
            continue;
        }
        // An interrupted file→directory transition may already have the
        // new directory. Keep it; all of its leaves were preflighted.
        if !materialized_ancestor(&path, &target, &disk)?
            && disk.entry(&path)? != Some(crate::checkout_fs::Entry::Directory)
        {
            disk.remove(&path)?;
        }
        // Keep the baseline until replacement entries are in place, so
        // an interrupted shape transition can still be preflighted.
        obsolete.push(path);
        outcome.removed += 1;
    }

    for (path, file) in &target {
        if preserve.contains(path) {
            continue;
        }
        let expected = crate::checkout_fs::Entry::File {
            blob: file.blob,
            mode: file.mode_code(),
        };
        if file.mode == Mode::Gitlink {
            disk.remove(path)?;
            treestate.upsert(&TreestateEntry {
                path: path.clone(),
                blob_id: file.blob,
                mode: MODE_GITLINK,
                size: 32,
                mtime_ns: 0,
                hint: HINT_UNVERIFIED,
            })?;
            outcome.kept += 1;
            continue;
        }
        if disk.entry(path)? == Some(expected) {
            outcome.kept += 1;
        } else {
            let content = fetcher.blob(file.blob).await?;
<<<<<<< HEAD
            disk.remove(path)?;
=======
>>>>>>> codex/integrity-fixes
            disk.write(path, &content, file.mode)?;
            outcome.written += 1;
        }
        let meta = disk.stat(path)?;
        treestate.upsert(&TreestateEntry {
            path: path.clone(),
            blob_id: file.blob,
            mode: file.mode_code(),
<<<<<<< HEAD
            size: meta.st_size as i64,
            mtime_ns: meta.st_mtime as i64 * 1_000_000_000 + meta.st_mtime_nsec as i64,
            hint: HINT_VERIFIED,
        })?;
=======
            size: meta.st_size,
            mtime_ns: meta.st_mtime * 1_000_000_000 + meta.st_mtime_nsec as i64,
            hint: HINT_VERIFIED,
        })?;
    }
    for path in obsolete {
        treestate.remove(&path)?;
>>>>>>> codex/integrity-fixes
    }
    Ok(outcome)
}

fn blocked(path: &str, reason: &str) -> BedError {
    BedError::CheckoutBlocked {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Identify the active state directory from the selected treestate, so a
/// custom --bed-dir is protected by the same rule as the default .bed.
pub(crate) fn state_prefixes(
    root: &std::path::Path,
    treestate: &Treestate,
) -> Result<BTreeSet<String>, BedError> {
    let state = treestate.path().parent().expect("treestate has a parent");
    let mut prefixes = BTreeSet::new();
    if let Some(name) = state.file_name().and_then(|name| name.to_str()) {
        prefixes.insert(name.to_owned());
    }
    let root = std::fs::canonicalize(root)?;
    let state = std::fs::canonicalize(state)?;
    if let Ok(relative) = state.strip_prefix(root) {
        if relative.as_os_str().is_empty() {
            prefixes.insert(String::new());
        } else if let Some(relative) = relative.to_str() {
            prefixes.insert(relative.to_owned());
        }
    }
    Ok(prefixes)
}

pub(crate) fn is_state_path(path: &str, prefixes: &BTreeSet<String>) -> bool {
    prefixes.iter().any(|prefix| {
        prefix.is_empty() || path == prefix || path.starts_with(&format!("{prefix}/"))
    })
}

fn preflight(
    root: &std::path::Path,
    plot: &[String],
    target: &BTreeMap<String, PlanFile>,
    treestate: &Treestate,
    disk: &crate::checkout_fs::CheckoutFs,
) -> Result<BTreeSet<String>, BedError> {
    use crate::checkout_fs::Entry;
    let protected = state_prefixes(root, treestate)?;
    let tracked: BTreeMap<_, _> = treestate
        .all_paths()?
        .into_iter()
        .map(|path| Ok((path.clone(), treestate.lookup(&path)?.expect("listed row"))))
        .collect::<Result<_, BedError>>()?;
    let affected: BTreeSet<_> = target
        .keys()
        .chain(tracked.keys().filter(|path| in_plot(path, plot)))
        .cloned()
        .collect();
    let mut preserve = BTreeSet::new();
    for path in &affected {
        gardenbed_model::codec::validate_path(path)
            .map_err(|error| blocked(path, &error.to_string()))?;
        if is_state_path(path, &protected)
            || protected
                .iter()
                .any(|prefix| prefix.starts_with(&format!("{path}/")))
        {
            return Err(blocked(
                path,
                "path overlaps the active bed state directory",
            ));
        }
        // A resumed directory→file swap can leave stale descendant rows
        // after its replacement has already been written. Forget those
        // rows without ever walking through the replacement file/link.
        if !target.contains_key(path) && materialized_ancestor(path, target, disk)? {
            continue;
        }
        let mut ancestor_removed = false;
        for (position, _) in path.match_indices('/') {
            let ancestor = &path[..position];
            match disk.entry(ancestor)? {
                Some(Entry::Directory) => {}
                None => break,
                Some(current) => {
                    // Clean tracked files/symlinks may become directories,
                    // but a link that remains an ancestor is never followed.
                    let baseline = tracked.get(ancestor).map(|entry| Entry::File {
                        blob: entry.blob_id,
                        mode: entry.mode,
                    });
                    if !target.contains_key(ancestor)
                        && affected.contains(ancestor)
                        && Some(current) == baseline
                    {
                        ancestor_removed = true;
                        break;
                    }
                    return Err(blocked(path, "ancestor is a file or symlink"));
                }
            }
        }
        let current = if ancestor_removed {
            None
        } else {
            disk.entry(path)?
        };
        let baseline = tracked.get(path).and_then(|entry| {
            (entry.mode != MODE_GITLINK).then_some(Entry::File {
                blob: entry.blob_id,
                mode: entry.mode,
            })
        });
        let desired = target.get(path).and_then(|file| {
            (file.mode != Mode::Gitlink).then_some(Entry::File {
                blob: file.blob,
                mode: file.mode_code(),
            })
        });
        if current == desired {
            continue; // Already done, including after an interrupted run.
        }
        if current != baseline && baseline == desired {
            // This target leaves a tracked path unchanged. Keep its local
            // edit (including a local deletion) and its baseline index row.
            if tracked.contains_key(path) {
                preserve.insert(path.clone());
                continue;
            }
        }
        if current == Some(Entry::Directory) {
            let leaves = disk.leaves(path)?;
            let becomes_directory = target.keys().any(|p| p.starts_with(&format!("{path}/")));
            for leaf in &leaves {
<<<<<<< HEAD
                if leaf == path && becomes_directory {
                    continue;
                }
                if let Some(old) = tracked.get(leaf) {
                    if disk.entry(leaf)?
=======
                if leaf == path
                    && (becomes_directory
                        || tracked.keys().any(|p| p.starts_with(&format!("{path}/"))))
                {
                    // Empty replacement directories left after removing
                    // old tracked descendants are safe to resume.
                    continue;
                }
                if let Some(old) = tracked.get(leaf)
                    && disk.entry(leaf)?
>>>>>>> codex/integrity-fixes
                        == Some(Entry::File {
                            blob: old.blob_id,
                            mode: old.mode,
                        })
<<<<<<< HEAD
                    {
                        continue;
                    }
                }
                // Completed target descendants are safe during a resume;
                // unrelated additions and modified descendants are not.
                if becomes_directory {
                    if let Some(file) = target.get(leaf) {
                        if disk.entry(leaf)?
                            == Some(Entry::File {
                                blob: file.blob,
                                mode: file.mode_code(),
                            })
                        {
                            continue;
                        }
                    }
=======
                {
                    continue;
                }
                // Completed target descendants are safe during a resume;
                // unrelated additions and modified descendants are not.
                if becomes_directory
                    && let Some(file) = target.get(leaf)
                    && disk.entry(leaf)?
                        == Some(Entry::File {
                            blob: file.blob,
                            mode: file.mode_code(),
                        })
                {
                    continue;
>>>>>>> codex/integrity-fixes
                }
                return Err(blocked(leaf, "directory contains local or untracked work"));
            }
            continue;
        }
        if current != baseline {
            return Err(blocked(
                path,
                "local changes or an untracked entry would be overwritten",
            ));
        }
    }
    Ok(preserve)
}

fn materialized_ancestor(
    path: &str,
    target: &BTreeMap<String, PlanFile>,
    disk: &crate::checkout_fs::CheckoutFs,
) -> Result<bool, BedError> {
    for (position, _) in path.match_indices('/') {
        let ancestor = &path[..position];
<<<<<<< HEAD
        if let Some(file) = target.get(ancestor) {
            if file.mode != Mode::Gitlink
                && disk.entry(ancestor)? == Some(crate::checkout_fs::Entry::File {
                    blob: file.blob,
                    mode: file.mode_code(),
                })
            {
                return Ok(true);
            }
=======
        if let Some(file) = target.get(ancestor)
            && file.mode != Mode::Gitlink
            && disk.entry(ancestor)?
                == Some(crate::checkout_fs::Entry::File {
                    blob: file.blob,
                    mode: file.mode_code(),
                })
        {
            return Ok(true);
>>>>>>> codex/integrity-fixes
        }
    }
    Ok(false)
}

/// The set of paths a plan covers.
pub fn plan_paths(plan: &[PlanFile]) -> BTreeSet<String> {
    plan.iter().map(|f| f.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bed::Bed;
    use crate::checkout_fs::CheckoutFs;
    use gardenbed_model::blob_id;
    use std::fs;

    fn plan(files: &[(&str, &[u8])]) -> BTreeMap<String, PlanFile> {
<<<<<<< HEAD
        files.iter().map(|(path, bytes)| (path.to_string(), PlanFile {
            path: path.to_string(), mode: Mode::Regular, blob: blob_id(bytes),
        })).collect()
    }

    fn track(ts: &Treestate, path: &str, bytes: &[u8]) {
        ts.upsert(&TreestateEntry { path: path.into(), blob_id: blob_id(bytes), mode: 0,
            size: bytes.len() as i64, mtime_ns: 0, hint: HINT_UNVERIFIED }).unwrap();
=======
        files
            .iter()
            .map(|(path, bytes)| {
                (
                    path.to_string(),
                    PlanFile {
                        path: path.to_string(),
                        mode: Mode::Regular,
                        blob: blob_id(bytes),
                    },
                )
            })
            .collect()
    }

    fn track(ts: &Treestate, path: &str, bytes: &[u8]) {
        ts.upsert(&TreestateEntry {
            path: path.into(),
            blob_id: blob_id(bytes),
            mode: 0,
            size: bytes.len() as i64,
            mtime_ns: 0,
            hint: HINT_UNVERIFIED,
        })
        .unwrap();
>>>>>>> codex/integrity-fixes
    }

    #[test]
    fn preflight_preserves_unchanged_local_edits_and_refuses_actual_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let bed = Bed::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        track(&ts, "f", b"base");
        fs::write(dir.path().join("f"), b"local").unwrap();
<<<<<<< HEAD
        let disk = CheckoutFs::open(dir.path()).unwrap();
        let plot = vec![".".into()];
        assert_eq!(preflight(dir.path(), &plot, &plan(&[("f", b"base")]), &ts, &disk).unwrap(), BTreeSet::from(["f".into()]));
=======
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let plot = vec![".".into()];
        assert_eq!(
            preflight(dir.path(), &plot, &plan(&[("f", b"base")]), &ts, &disk).unwrap(),
            BTreeSet::from(["f".into()])
        );
>>>>>>> codex/integrity-fixes
        assert!(preflight(dir.path(), &plot, &plan(&[("f", b"target")]), &ts, &disk).is_err());
        assert!(preflight(dir.path(), &plot, &plan(&[]), &ts, &disk).is_err());
        fs::remove_file(dir.path().join("f")).unwrap();
        assert!(preflight(dir.path(), &plot, &plan(&[("f", b"target")]), &ts, &disk).is_err());
        fs::write(dir.path().join("untracked"), b"local").unwrap();
<<<<<<< HEAD
        assert!(preflight(dir.path(), &plot, &plan(&[("untracked", b"target")]), &ts, &disk).is_err());
=======
        assert!(
            preflight(
                dir.path(),
                &plot,
                &plan(&[("untracked", b"target")]),
                &ts,
                &disk
            )
            .is_err()
        );
>>>>>>> codex/integrity-fixes
    }

    #[test]
    fn directory_replacements_refuse_untracked_and_modified_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let bed = Bed::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        track(&ts, "d/f", b"base");
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/f"), b"base").unwrap();
<<<<<<< HEAD
        let disk = CheckoutFs::open(dir.path()).unwrap();
=======
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
>>>>>>> codex/integrity-fixes
        let plot = vec![".".into()];
        let target = plan(&[("d", b"replacement")]);
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_ok());
        fs::write(dir.path().join("d/extra"), b"local").unwrap();
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_err());
        fs::remove_file(dir.path().join("d/extra")).unwrap();
        fs::write(dir.path().join("d/f"), b"local").unwrap();
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_err());
    }

    #[test]
    fn interrupted_shape_transitions_and_partial_writes_can_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bed = Bed::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
<<<<<<< HEAD
        let disk = CheckoutFs::open(dir.path()).unwrap();
=======
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
>>>>>>> codex/integrity-fixes
        let plot = vec![".".into()];
        track(&ts, "d/f", b"base");
        fs::write(dir.path().join("d"), b"replacement").unwrap();
        let target = plan(&[("d", b"replacement")]);
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_ok());
        assert!(materialized_ancestor("d/f", &target, &disk).unwrap());
<<<<<<< HEAD
=======
        // A crash after removing the old descendants but before installing
        // the new file retains baseline rows and an empty directory.
        fs::remove_file(dir.path().join("d")).unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_ok());
        fs::remove_dir(dir.path().join("d")).unwrap();
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_ok());
        fs::write(dir.path().join("d"), b"replacement").unwrap();
>>>>>>> codex/integrity-fixes
        ts.remove("d/f").unwrap();
        track(&ts, "d", b"replacement");
        fs::remove_file(dir.path().join("d")).unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/new"), b"done").unwrap();
        let target = plan(&[("d/new", b"done"), ("d/remaining", b"next")]);
        assert!(preflight(dir.path(), &plot, &target, &ts, &disk).is_ok());
    }

    #[test]
    fn active_custom_state_and_symlink_ancestors_are_protected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let bed = Bed::init(&dir.path().join("custom"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
<<<<<<< HEAD
        let disk = CheckoutFs::open(dir.path()).unwrap();
        let plot = vec![".".into()];
        assert!(preflight(dir.path(), &plot, &plan(&[("custom/config.toml", b"bad")]), &ts, &disk).is_err());
=======
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let plot = vec![".".into()];
        assert!(
            preflight(
                dir.path(),
                &plot,
                &plan(&[("custom/config.toml", b"bad")]),
                &ts,
                &disk
            )
            .is_err()
        );
>>>>>>> codex/integrity-fixes
        assert!(preflight(dir.path(), &plot, &plan(&[("custom", b"bad")]), &ts, &disk).is_err());
        fs::write(outside.path().join("f"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        assert!(preflight(dir.path(), &plot, &plan(&[("d/f", b"bad")]), &ts, &disk).is_err());
        // Operations enforce the rule independently of preflight.
        assert!(disk.write("d/f", b"bad", Mode::Regular).is_err());
        assert!(disk.remove("d/f").is_err());
        assert_eq!(fs::read(outside.path().join("f")).unwrap(), b"outside");
    }
}

//! Materializing selected paths: writing the want-list plan onto the repository
//! (files, modes, symlinks) and the treestate, exactly once per path.
//!
//! Shared by `bed sparse add` (sync the added prefixes at the current
//! changeset) and `bed switch` (sync the whole sparse at the new changeset).
//! A complete preflight rejects overwrites of uncreated work and untracked
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

use crate::error::ClientError;
use crate::rpc::PlanFile;
use crate::sparse::in_sparse;
use crate::treestate::HINT_UNVERIFIED;
use crate::treestate::HINT_VERIFIED;
use crate::treestate::MODE_GITLINK;
use crate::treestate::Treestate;
use crate::treestate::TreestateEntry;

/// What one sync run did to the repository.
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
    /// A one-line plain-language summary of the materialization.
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

/// Brings the working copy under `sparse` in line with `plan`, relative to
/// the working-copy root. Blobs must already be in the local cache (the plan
/// fetch guarantees it); this function never touches the network.
///
/// Only paths inside `sparse` are touched. Treestate rows are upserted with
/// freshly observed size/mtime and the verified hint; rows the plan no
/// longer covers (inside the sparse selection) are deleted along with their files.
pub async fn sync_sparse(
    workspace_root: &std::path::Path,
    sparse: &[String],
    plan: Vec<PlanFile>,
    fetcher: &crate::rpc::Fetcher,
    treestate: &Treestate,
) -> Result<SyncOutcome, ClientError> {
    let target: BTreeMap<String, PlanFile> = plan
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect();
    let disk = crate::checkout_fs::CheckoutFs::open(
        workspace_root,
        treestate.path().parent().expect("state directory"),
    )?;
    // Only identity-registered staging entries from dead writers are
    // reclaimed. Substituted names remain ordinary local/untracked work.
    disk.cleanup_staging()?;
    // Load the index once. Every per-path decision below reads this map
    // instead of hopping back into SQLite: a checkout update over N files
    // must not cost N lookups.
    let tracked: BTreeMap<String, TreestateEntry> = treestate
        .all_entries()?
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    // Validate the whole operation before changing any file or index row.
    let preserve = preflight(
        workspace_root,
        sparse,
        &target,
        &tracked,
        treestate.path(),
        &disk,
    )?;
    let mut outcome = SyncOutcome::default();
    let mut obsolete = Vec::new();

    for path in tracked.keys() {
        if target.contains_key(path) || !in_sparse(path, sparse) {
            continue;
        }
        // An interrupted file→directory transition may already have the
        // new directory. Keep it; all of its leaves were preflighted.
        if !materialized_ancestor(path, &target, &disk)?
            && disk.entry(path)? != Some(crate::checkout_fs::Entry::Directory)
        {
            disk.remove(path)?;
        }
        // Keep the baseline until replacement entries are in place, so
        // an interrupted shape transition can still be preflighted.
        obsolete.push(path.clone());
        outcome.removed += 1;
    }

    // After the whole removal pass, prune directories the removals left
    // empty — deepest first, each ancestor at most once. Only directories
    // that contain nothing at all disappear; the working-copy root and
    // the state directory are never candidates, and untracked content
    // blocks pruning by making the directory non-empty.
    let mut pruned: Vec<String> = obsolete
        .iter()
        .filter_map(|path| path.rsplit_once('/').map(|(dir, _)| dir.to_owned()))
        .collect();
    // Deepest first: pruning a child may empty its parent, never the
    // reverse — parents must be visited after everything inside them.
    pruned.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then_with(|| a.cmp(b))
    });
    pruned.dedup();
    for dir in pruned {
        disk.prune_empty(&dir, sparse)?;
    }

    // Refresh hints in one transaction rather than one statement per file.
    let mut refreshed: Vec<TreestateEntry> = Vec::with_capacity(target.len());
    for (path, file) in &target {
        if preserve.contains(path) {
            continue;
        }
        // Already settled above: the row is verified and names this exact
        // content, so neither the file nor the row needs touching.
        if tracked.get(path).is_some_and(|row| {
            row.hint == HINT_VERIFIED && row.blob_id == file.blob && row.mode == file.mode_code()
        }) {
            outcome.kept += 1;
            continue;
        }
        let expected = crate::checkout_fs::Entry::File {
            blob: file.blob,
            mode: file.mode_code(),
        };
        if file.mode == Mode::Gitlink {
            disk.remove(path)?;
            refreshed.push(TreestateEntry {
                path: path.clone(),
                blob_id: file.blob,
                mode: MODE_GITLINK,
                size: 32,
                mtime_ns: 0,
                hint: HINT_UNVERIFIED,
            });
            outcome.kept += 1;
            continue;
        }
        // The row describes this path from an earlier sync; let a matching
        // stat settle it without reading the file.
        let known = tracked.get(path);
        let current = match known {
            Some(known) => disk.entry_verified(path, known)?,
            None => disk.entry(path)?,
        };
        if current == Some(expected) {
            outcome.kept += 1;
            // Already correct: the row is unchanged, so only a row that is
            // missing or unverified needs rewriting (an unverified row must
            // not stay unverified — the next sync would re-read it).
            if let Some(known) = known
                && known.blob_id == file.blob
                && known.mode == file.mode_code()
                && known.hint == HINT_VERIFIED
            {
                continue;
            }
        } else {
            let content = fetcher.blob(file.blob).await?;
            disk.write(path, &content, file.mode)?;
            outcome.written += 1;
        }
        let meta = disk.stat(path)?;
        refreshed.push(TreestateEntry {
            path: path.clone(),
            blob_id: file.blob,
            mode: file.mode_code(),
            size: meta.size,
            mtime_ns: meta.mtime_ns,
            hint: HINT_VERIFIED,
        });
    }
    treestate.upsert_all(&refreshed)?;
    treestate.remove_all(&obsolete)?;
    Ok(outcome)
}

fn blocked(path: &str, reason: &str) -> ClientError {
    ClientError::CheckoutBlocked {
        path: path.to_owned(),
        reason: reason.to_owned(),
    }
}

/// Identify the active state directory from the selected treestate, so a
/// custom --state-dir is protected by the same rule as the default .bed.
pub(crate) fn state_prefixes(
    root: &std::path::Path,
    treestate: &Treestate,
) -> Result<BTreeSet<String>, ClientError> {
    state_prefixes_of(root, treestate.path())
}

/// [`state_prefixes`] for a caller that already holds the state directory.
pub(crate) fn state_prefixes_of(
    root: &std::path::Path,
    treestate_path: &std::path::Path,
) -> Result<BTreeSet<String>, ClientError> {
    let state = treestate_path.parent().expect("treestate has a parent");
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

/// Validates a sync against `tracked` (the treestate, loaded once by the
/// caller) and returns the paths whose local edit must be preserved.
///
/// Every `disk.entry` here is a full read-and-hash of a regular file, so
/// the walk is deliberately shaped to ask for one per affected path at
/// most, and to ask with the verified-row hint when one exists.
fn preflight(
    root: &std::path::Path,
    sparse: &[String],
    target: &BTreeMap<String, PlanFile>,
    tracked: &BTreeMap<String, TreestateEntry>,
    treestate_path: &std::path::Path,
    disk: &crate::checkout_fs::CheckoutFs,
) -> Result<BTreeSet<String>, ClientError> {
    use crate::checkout_fs::Entry;
    let protected = state_prefixes_of(root, treestate_path)?;
    // Only paths this sync can actually act on need the disk. A path whose
    // target already equals its hash-verified row is left exactly as it is:
    // there is nothing to write and nothing that could be overwritten (an
    // unchanged target never refuses a local edit — it preserves it). This
    // is what keeps an update proportional to the change rather than to the
    // size of the selection.
    let settled = |path: &str| {
        let Some(file) = target.get(path) else {
            return false;
        };
        matches!(tracked.get(path), Some(row)
            if row.hint == HINT_VERIFIED
                && row.blob_id == file.blob
                && row.mode == file.mode_code())
    };
    let affected: BTreeSet<_> = target
        .keys()
        .filter(|path| !settled(path))
        .chain(
            tracked
                .keys()
                .filter(|path| in_sparse(path, sparse) && !target.contains_key(*path)),
        )
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
            match tracked.get(path) {
                Some(known) => disk.entry_verified(path, known)?,
                None => disk.entry(path)?,
            }
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
                        == Some(Entry::File {
                            blob: old.blob_id,
                            mode: old.mode,
                        })
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
) -> Result<bool, ClientError> {
    for (position, _) in path.match_indices('/') {
        let ancestor = &path[..position];
        if let Some(file) = target.get(ancestor)
            && file.mode != Mode::Gitlink
            && disk.entry(ancestor)?
                == Some(crate::checkout_fs::Entry::File {
                    blob: file.blob,
                    mode: file.mode_code(),
                })
        {
            return Ok(true);
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
    use crate::checkout_fs::CheckoutFs;
    use crate::workspace::WorkingCopy;
    use gardenbed_model::blob_id;
    use std::fs;

    fn loaded(ts: &Treestate) -> BTreeMap<String, TreestateEntry> {
        ts.all_entries()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect()
    }

    fn plan(files: &[(&str, &[u8])]) -> BTreeMap<String, PlanFile> {
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
    }

    #[test]
    fn preflight_preserves_unchanged_local_edits_and_refuses_actual_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        track(&ts, "f", b"base");
        fs::write(dir.path().join("f"), b"local").unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let sparse = vec![".".into()];
        assert_eq!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("f", b"base")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .unwrap(),
            BTreeSet::from(["f".into()])
        );
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("f", b"target")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        fs::remove_file(dir.path().join("f")).unwrap();
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("f", b"target")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        fs::write(dir.path().join("untracked"), b"local").unwrap();
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("untracked", b"target")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
    }

    #[test]
    fn directory_replacements_refuse_untracked_and_modified_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        track(&ts, "d/f", b"base");
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/f"), b"base").unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let sparse = vec![".".into()];
        let target = plan(&[("d", b"replacement")]);
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_ok());
        fs::write(dir.path().join("d/extra"), b"local").unwrap();
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_err());
        fs::remove_file(dir.path().join("d/extra")).unwrap();
        fs::write(dir.path().join("d/f"), b"local").unwrap();
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_err());
    }

    #[test]
    fn interrupted_shape_transitions_and_partial_writes_can_resume() {
        let dir = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let sparse = vec![".".into()];
        track(&ts, "d/f", b"base");
        fs::write(dir.path().join("d"), b"replacement").unwrap();
        let target = plan(&[("d", b"replacement")]);
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_ok());
        assert!(materialized_ancestor("d/f", &target, &disk).unwrap());
        // A crash after removing the old descendants but before installing
        // the new file retains baseline rows and an empty directory.
        fs::remove_file(dir.path().join("d")).unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_ok());
        fs::remove_dir(dir.path().join("d")).unwrap();
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_ok());
        fs::write(dir.path().join("d"), b"replacement").unwrap();
        ts.remove("d/f").unwrap();
        track(&ts, "d", b"replacement");
        fs::remove_file(dir.path().join("d")).unwrap();
        fs::create_dir(dir.path().join("d")).unwrap();
        fs::write(dir.path().join("d/new"), b"done").unwrap();
        let target = plan(&[("d/new", b"done"), ("d/remaining", b"next")]);
        assert!(preflight(dir.path(), &sparse, &target, &loaded(&ts), ts.path(), &disk).is_ok());
    }

    #[test]
    fn active_custom_state_and_symlink_ancestors_are_protected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join("custom"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let sparse = vec![".".into()];
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("custom/config.toml", b"bad")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("custom", b"bad")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        fs::write(outside.path().join("f"), b"outside").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("d/f", b"bad")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err()
        );
        // Operations enforce the rule independently of preflight.
        assert!(disk.write("d/f", b"bad", Mode::Regular).is_err());
        assert!(disk.remove("d/f").is_err());
        assert_eq!(fs::read(outside.path().join("f")).unwrap(), b"outside");
    }

    /// The hint path must not weaken safety on the paths it skips.
    ///
    /// A settled path (verified row naming the target content) is skipped
    /// entirely: no read, no write, no refusal. That is only sound because
    /// skipping cannot *destroy* anything — a local edit is left exactly as
    /// it is. The test pins the two halves that matter: a settled path
    /// yields `kept` and leaves the working file untouched, and any path
    /// whose target differs is still read and still refused when the
    /// overwrite would lose work.
    #[test]
    fn settled_paths_are_skipped_and_edits_are_never_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        let sparse = vec![".".into()];

        fs::write(dir.path().join("f"), b"base").unwrap();
        let meta = std::fs::metadata(dir.path().join("f")).unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        ts.upsert(&TreestateEntry {
            path: "f".into(),
            blob_id: blob_id(b"base"),
            mode: 0,
            size: 4,
            #[cfg(unix)]
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            #[cfg(not(unix))]
            mtime_ns: crate::status::mtime_ns(&meta),
            hint: HINT_VERIFIED,
        })
        .unwrap();

        // Settled: nothing to preserve, nothing refused, nothing read.
        let preserve = preflight(
            dir.path(),
            &sparse,
            &plan(&[("f", b"base")]),
            &loaded(&ts),
            ts.path(),
            &disk,
        )
        .unwrap();
        assert!(preserve.is_empty());
        let verified = disk
            .entry_verified("f", &ts.lookup("f").unwrap().unwrap())
            .unwrap();
        assert_eq!(
            verified,
            Some(crate::checkout_fs::Entry::File {
                blob: blob_id(b"base"),
                mode: 0
            })
        );

        // A local edit makes the target differ from the row: the path is no
        // longer settled, so preflight reads it and refuses an overwrite.
        fs::write(dir.path().join("f"), b"local edit").unwrap();
        assert!(
            preflight(
                dir.path(),
                &sparse,
                &plan(&[("f", b"target")]),
                &loaded(&ts),
                ts.path(),
                &disk
            )
            .is_err(),
            "an overwrite of a local edit must still be refused"
        );
        // ...while a plan that leaves the target alone skips the path, which
        // also leaves the edit in place (skipping never writes).
        let preserve = preflight(
            dir.path(),
            &sparse,
            &plan(&[("f", b"base")]),
            &loaded(&ts),
            ts.path(),
            &disk,
        )
        .unwrap();
        assert!(preserve.is_empty());
        assert_eq!(
            fs::read(dir.path().join("f")).unwrap(),
            b"local edit",
            "a skipped path must never rewrite working content"
        );
    }

    /// An UNVERIFIED row never takes the hint path: the file is read.
    #[test]
    fn unverified_rows_are_always_read() {
        let dir = tempfile::tempdir().unwrap();
        let bed = WorkingCopy::init(&dir.path().join(".bed"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        let disk = CheckoutFs::open(dir.path(), bed.dir()).unwrap();
        fs::write(dir.path().join("f"), b"base").unwrap();
        let meta = std::fs::metadata(dir.path().join("f")).unwrap();
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        ts.upsert(&TreestateEntry {
            path: "f".into(),
            blob_id: blob_id(b"base"),
            mode: 0,
            size: 4,
            #[cfg(unix)]
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            #[cfg(not(unix))]
            mtime_ns: crate::status::mtime_ns(&meta),
            hint: HINT_UNVERIFIED,
        })
        .unwrap();
        let preserve = preflight(
            dir.path(),
            &[".".into()],
            &plan(&[("f", b"base")]),
            &loaded(&ts),
            ts.path(),
            &disk,
        )
        .unwrap();
        assert!(preserve.is_empty());
    }
}

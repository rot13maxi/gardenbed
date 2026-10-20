//! `bed status` — the offline working-copy scanner (v0.1.md §6, M4
//! verification 1 and 5).
//!
//! The engine compares the working files under the plot prefixes against
//! the treestate, entirely offline: no server, no network.
//!
//! Like git's index racy-refresh, the scan is *guided by hints*: a tracked
//! file whose size and `mtime_ns` still match its treestate row (and whose
//! row is [`crate::treestate::HINT_VERIFIED`]) is assumed untouched and
//! never read. Everything else — and, under `--full-scan`, every tracked
//! file — is hashed (`SHA-256`, via [`gardenbed_model::blob_id`]) and
//! compared against the recorded blob id:
//!
//! - same hash → clean; the row's hint metadata is refreshed so the next
//!   scan can skip it (mtime-only changes, e.g. a touch or a rewrite of
//!   identical bytes, land here);
//! - different hash → **modified** (same-size different-content is caught
//!   exactly here);
//! - tracked but missing on disk → **deleted**;
//! - present in the plot but unknown to the treestate → **untracked**.
//!
//! [`scan_paths`] is the pure core: `(root, plot, treestate) → a per-path
//! classification carrying blob ids, modes and hint metadata` — the shape
//! `sow` consumes. [`scan_status`] wraps it, collapsing the classification
//! into the three-bucket report the CLI prints. Their only side effect is
//! the deliberate one: refreshing hint metadata on files that hash clean —
//! never their blob ids (content addresses change only through
//! `sow`/`goto`).

use std::collections::HashSet;
use std::fs;
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::UNIX_EPOCH;

use gardenbed_model::Id;
use gardenbed_model::blob_id;

use crate::error::BedError;
use crate::plot::dir_may_hold_plotted;
use crate::plot::in_plot;
use crate::treestate::HINT_UNVERIFIED;
use crate::treestate::MODE_GITLINK;
use crate::treestate::MODE_SYMLINK;
use crate::treestate::Treestate;

/// What the scan decided about one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanDelta {
    /// Tracked and unchanged — content and mode match the treestate row
    /// (hint metadata may have been refreshed).
    Clean,
    /// Tracked, on disk, but content or mode differs from the row. Carries
    /// the file's *current* blob id (recomputed from the working bytes —
    /// identical to the row's for a mode-only change), mode, and the
    /// stat metadata `sow` needs to refresh the row.
    Modified {
        /// SHA-256 of the working bytes.
        blob_id: Id,
        /// §4 mode code observed on disk (0 regular · 1 exec · 2 symlink).
        mode: i64,
        /// Working file's size in bytes.
        size: i64,
        /// Working file's mtime, ns since the epoch.
        mtime_ns: i64,
    },
    /// Tracked as a file but gone from disk — or replaced by a directory,
    /// which is the same outcome: the path no longer holds a file.
    Deleted,
    /// In the plot but unknown to the treestate. Carries the same metadata
    /// as [`ScanDelta::Modified`] (the path has no row yet).
    Untracked {
        /// SHA-256 of the working bytes.
        blob_id: Id,
        /// §4 mode code observed on disk.
        mode: i64,
        /// Working file's size in bytes.
        size: i64,
        /// Working file's mtime, ns since the epoch.
        mtime_ns: i64,
    },
}

/// One classified path of the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanItem {
    /// Repo-relative, forward-slash path.
    pub path: String,
    /// What the scan found.
    pub delta: ScanDelta,
}

/// What `bed status` found under the plot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusReport {
    /// Tracked files whose content or mode differs from the treestate.
    pub modified: Vec<String>,
    /// Tracked files missing from the working copy.
    pub deleted: Vec<String>,
    /// Files inside the plot that the treestate has never seen.
    pub untracked: Vec<String>,
}

impl StatusReport {
    /// True when nothing is modified, deleted, or untracked.
    pub fn is_clean(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.untracked.is_empty()
    }

    /// git-status-like rendering: one section per non-empty category,
    /// paths sorted, entries indented. Empty when the plot is clean (the
    /// CLI prints its own "clean" line).
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut section = |name: &str, paths: &[String]| {
            if paths.is_empty() {
                return;
            }
            out.push_str(name);
            out.push_str(":\n");
            for path in paths {
                out.push_str("    ");
                out.push_str(path);
                out.push('\n');
            }
        };
        section("modified", &self.modified);
        section("deleted", &self.deleted);
        section("untracked", &self.untracked);
        out
    }
}

/// Compares the working files under `plot` against `treestate`, relative to
/// the working-copy root `root`, classifying every path the scan touches.
/// See the module docs for the semantics; [`scan_status`] renders the same
/// walk as a `StatusReport`.
///
/// `full_scan` (the `--full-scan` flag) forces the correctness path: every
/// tracked file is hashed regardless of hints.
///
/// Only modified/untracked entries carry metadata; the walk order is
/// deterministic (sorted per directory) but the returned list is not sorted
/// globally — callers that print sort their own buckets.
pub fn scan_paths(
    root: &Path,
    plot: &[String],
    treestate: &Treestate,
    full_scan: bool,
) -> Result<Vec<ScanItem>, BedError> {
    let mut items: Vec<ScanItem> = Vec::new();
    let state_prefixes = crate::sync::state_prefixes(root, treestate)?;
<<<<<<< HEAD
=======
    let disk = crate::checkout_fs::CheckoutFs::open(
        root,
        treestate.path().parent().expect("state directory"),
    )?;
    let (_staging_scan_lock, staged_paths) = disk.staging_scan()?;
>>>>>>> codex/integrity-fixes

    let tracked_paths = treestate.all_paths()?;
    let tracked: HashSet<String> = tracked_paths.iter().cloned().collect();

    // Pass 1 — deleted: tracked, inside the plot, and gone from disk (or
    // replaced by something that is not a file or symlink, e.g. a directory
    // landed on the path). Gitlink rows are skipped: submodule pointers are
    // never materialized as files, so their absence is their normal state.
    for path in &tracked_paths {
        if !in_plot(path, plot) || crate::sync::is_state_path(path, &state_prefixes) {
            continue;
        }
        let entry = match treestate.lookup(path)? {
            Some(entry) => entry,
            None => continue,
        };
        if entry.mode == MODE_GITLINK {
            continue;
        }
        // Ancestor replacements hide tracked descendants. Never inspect
        // them through a symlink: the directory walk below reports the
        // replacement itself as a tracked edit or an untracked entry.
        let mut hidden = false;
        for (position, _) in path.match_indices('/') {
            match fs::symlink_metadata(root.join(&path[..position])) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    hidden = true;
                    break;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    hidden = true;
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
        if hidden {
            items.push(ScanItem {
                path: path.clone(),
                delta: ScanDelta::Deleted,
            });
            continue;
        }
        match fs::symlink_metadata(root.join(path)) {
            Ok(m) if m.is_file() || m.is_symlink() => {}
            Ok(_) => items.push(ScanItem {
                path: path.clone(),
                delta: ScanDelta::Deleted,
            }),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                items.push(ScanItem {
                    path: path.clone(),
                    delta: ScanDelta::Deleted,
                })
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Pass 2 — walk the garden within the plot: untracked files and
    // suspects needing a hash.
    let mut stack: Vec<String> = vec![String::new()];
    while let Some(dir) = stack.pop() {
        let abs_dir = if dir.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&dir)
        };
        let mut entries: Vec<_> = fs::read_dir(&abs_dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            // Repo paths are UTF-8 flat paths; entries whose names are not
            // valid UTF-8 cannot be tracked and are skipped.
            let name = match entry.file_name().into_string() {
                Ok(name) => name,
                Err(_) => continue,
            };
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            // Skip the selected state directory regardless of its name
            // or whether it is itself a symlink.
<<<<<<< HEAD
            if crate::sync::is_state_path(&rel, &state_prefixes) {
=======
            if crate::sync::is_state_path(&rel, &state_prefixes) || staged_paths.contains(&rel) {
>>>>>>> codex/integrity-fixes
                continue;
            }
            if entry.file_type()?.is_dir() {
                if dir_may_hold_plotted(&rel, plot) {
                    stack.push(rel);
                }
            } else if in_plot(&rel, plot) {
                if !tracked.contains(&rel) {
                    items.push(ScanItem {
                        path: rel.clone(),
                        delta: classify_untracked(root, &rel)?,
                    });
                } else {
                    let delta = classify_tracked(root, &rel, full_scan, treestate)?;
                    items.push(ScanItem { path: rel, delta });
                }
            }
        }
    }

    Ok(items)
}

/// `bed status`: the same walk as [`scan_paths`], collapsed into the three
/// buckets the CLI prints. `full_scan` (the `--full-scan` flag) forces the
/// correctness path: every tracked file is hashed regardless of hints.
pub fn scan_status(
    root: &Path,
    plot: &[String],
    treestate: &Treestate,
    full_scan: bool,
) -> Result<StatusReport, BedError> {
    let mut report = StatusReport::default();
    for item in scan_paths(root, plot, treestate, full_scan)? {
        match item.delta {
            ScanDelta::Clean => {}
            ScanDelta::Modified { .. } => report.modified.push(item.path),
            ScanDelta::Deleted => report.deleted.push(item.path),
            ScanDelta::Untracked { .. } => report.untracked.push(item.path),
        }
    }
    // Deterministic output regardless of walk order.
    report.modified.sort();
    report.deleted.sort();
    report.untracked.sort();
    Ok(report)
}

/// Classifies a path the treestate has never seen: stat it and hash its
/// bytes (for a symlink, the link-target bytes).
fn classify_untracked(root: &Path, rel: &str) -> Result<ScanDelta, BedError> {
    let observed = observe(root, rel)?.ok_or_else(|| {
        // The walk listed the entry moments ago; it cannot have vanished
        // through a race bed cares about. Classify it deleted-of-nothing:
        // there is no row to delete, so the path is simply absent.
        BedError::ConfigInvalid(format!(
            "{rel:?} appeared and vanished while scanning — the garden is \
             being changed concurrently"
        ))
    })?;
    Ok(ScanDelta::Untracked {
        blob_id: observed.blob_id,
        mode: observed.mode,
        size: observed.size,
        mtime_ns: observed.mtime_ns,
    })
}

/// Classifies one tracked, on-disk file: hint-pass clean, or hash and
/// compare against the recorded blob id.
///
/// Symlinks are checked as the link itself (`symlink_metadata`, never
/// following): the "content" is the link-target bytes, the mode is
/// [`MODE_SYMLINK`], and a broken target is still a faithful symlink.
fn classify_tracked(
    root: &Path,
    rel: &str,
    full_scan: bool,
    treestate: &Treestate,
) -> Result<ScanDelta, BedError> {
    let entry = match treestate.lookup(rel)? {
        Some(entry) => entry,
        // The tracked set was loaded a moment ago; a row vanishing mid-scan
        // is not a state bed reports on — treat it as untouched.
        None => return Ok(ScanDelta::Clean),
    };
    // A gitlink row never has a file on disk; nothing to check here.
    if entry.mode == MODE_GITLINK {
        return Ok(ScanDelta::Clean);
    }

    let observed = match observe(root, rel)? {
        Some(observed) => observed,
        // Vanished between pass 1 and here: pass 1 already called it.
        None => return Ok(ScanDelta::Clean),
    };

    // Hint pass: size+mtime+mode unchanged on a verified row → untouched,
    // without reading the file. `--full-scan` skips this entirely.
    if !full_scan
        && entry.hint != HINT_UNVERIFIED
        && observed.size == entry.size
        && observed.mtime_ns == entry.mtime_ns
        && observed.mode == entry.mode
    {
        return Ok(ScanDelta::Clean);
    }

    // Suspect: let content decide (the bytes are already read). A symlink's
    // content is its target; everything else is the file's bytes.
    if observed.blob_id == entry.blob_id {
        if observed.mode != entry.mode {
            // Content is unchanged but the mode moved (e.g. the exec bit or
            // a file↔symlink swap): a real modification until the
            // treestate catches up.
            Ok(ScanDelta::Modified {
                blob_id: observed.blob_id,
                mode: observed.mode,
                size: observed.size,
                mtime_ns: observed.mtime_ns,
            })
        } else {
            // Touch-without-change (mtime or hint metadata moved): refresh
            // the hints so the next scan skips this file again.
            treestate.refresh_hint(rel, observed.size, observed.mtime_ns)?;
            Ok(ScanDelta::Clean)
        }
    } else {
        Ok(ScanDelta::Modified {
            blob_id: observed.blob_id,
            mode: observed.mode,
            size: observed.size,
            mtime_ns: observed.mtime_ns,
        })
    }
}

/// One file's working-copy state as the scan sees it: bytes hashed, mode
/// decoded, stat metadata captured in one pass.
struct Observed {
    blob_id: Id,
    mode: i64,
    size: i64,
    mtime_ns: i64,
}

/// Reads and hashes the working entry at `root/rel`. A symlink's content is
/// its link-target bytes. `Ok(None)` when the entry vanished (or was
/// replaced by a directory) between the walk and this read.
fn observe(root: &Path, rel: &str) -> Result<Option<Observed>, BedError> {
    let meta = match fs::symlink_metadata(root.join(rel)) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if meta.is_dir() {
        return Ok(None); // replaced by a directory
    }
    let is_symlink = meta.is_symlink();
    let content: Vec<u8> = if is_symlink {
        fs::read_link(root.join(rel))?
            .as_os_str()
            .as_bytes()
            .to_vec()
    } else {
        fs::read(root.join(rel))?
    };
    Ok(Some(Observed {
        blob_id: blob_id(&content),
        mode: if is_symlink {
            MODE_SYMLINK
        } else {
            mode_of(&meta)
        },
        size: meta.len() as i64,
        mtime_ns: mtime_ns(&meta),
    }))
}

/// The treestate's clock: file mtime as nanoseconds since the Unix epoch
/// (negative before the epoch). When the filesystem does not expose an
/// mtime, returns 0 — which simply makes the file a suspect every scan.
pub fn mtime_ns(meta: &Metadata) -> i64 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i64,
            Err(e) => -(e.duration().as_nanos() as i64),
        },
        Err(_) => 0,
    }
}

/// The working file's mode as a §4 code: 1 (exec) when any execute bit is
/// set, else 0 (regular). Symlink (2) and gitlink (3) rows are classified
/// by their metadata shape in [`check_tracked`]; a scan never writes them.
fn mode_of(meta: &Metadata) -> i64 {
    #[cfg(unix)]
    {
        let bits = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
        if bits & 0o111 != 0 { 1 } else { 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::treestate::HINT_UNVERIFIED;
    use crate::treestate::HINT_VERIFIED;
    use crate::treestate::TreestateEntry;

    struct Garden {
        dir: tempfile::TempDir,
        bed: crate::bed::Bed,
    }

    impl Garden {
        /// A bed with `src/` plotted and the treestate seeded to match the
        /// files on disk (as a completed materialization would leave it).
        fn planted(files: &[(&str, &[u8])]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut bed =
                crate::bed::Bed::init(&dir.path().join(".bed"), "http://127.0.0.1:1", "garden")
                    .unwrap();
            bed.config_mut().plot.push("src".to_owned());
            bed.save_config().unwrap();

            let ts = bed.treestate().unwrap();
            for (path, content) in files {
                let abs = dir.path().join(path);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).unwrap();
                }
                fs::write(&abs, content).unwrap();
                let meta = fs::metadata(&abs).unwrap();
                ts.upsert(&TreestateEntry {
                    path: path.to_string(),
                    blob_id: blob_id(content),
                    mode: 0,
                    size: content.len() as i64,
                    mtime_ns: mtime_ns(&meta),
                    hint: HINT_VERIFIED,
                })
                .unwrap();
            }
            Garden { dir, bed }
        }

        fn scan(&self, full: bool) -> StatusReport {
            let plot = self.bed.config().plot.clone();
            scan_status(
                &self.bed.garden_root(),
                &plot,
                &self.bed.treestate().unwrap(),
                full,
            )
            .unwrap()
        }
    }

    #[test]
    fn clean_garden_reports_clean() {
        let g = Garden::planted(&[("src/a.rs", b"hello"), ("src/deep/b.rs", b"bye")]);
        assert!(g.scan(false).is_clean());
        assert!(g.scan(true).is_clean());
    }

    #[test]
    fn modified_content_is_caught_even_at_same_size() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        // Same length, different bytes — the classic hint escape.
        fs::write(g.dir.path().join("src/a.rs"), b"hell0").unwrap();
        let report = g.scan(false);
        assert_eq!(report.modified, vec!["src/a.rs".to_owned()]);
        assert!(report.deleted.is_empty() && report.untracked.is_empty());
    }

    #[test]
    fn mtime_only_change_hashes_and_comes_back_clean() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);

        // Rewriting identical bytes bumps the mtime but not the content —
        // the file must NOT count as modified.
        fs::write(g.dir.path().join("src/a.rs"), b"hello").unwrap();
        assert!(g.scan(false).is_clean());

        // And deterministically through the *hash* path, not luck: seed a
        // row whose recorded mtime is stale, forcing the suspect path.
        let ts = g.bed.treestate().unwrap();
        let real = mtime_ns(&fs::metadata(g.dir.path().join("src/a.rs")).unwrap());
        let mut entry = ts.lookup("src/a.rs").unwrap().unwrap();
        entry.mtime_ns = real - 1_000_000;
        ts.upsert(&entry).unwrap();
        assert!(g.scan(false).is_clean());

        // The scan refreshed the hint to the observed mtime.
        assert_eq!(ts.lookup("src/a.rs").unwrap().unwrap().mtime_ns, real);
    }

    #[test]
    fn hint_skip_is_real_a_matching_hint_never_hashes() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        let ts = g.bed.treestate().unwrap();

        // Plant a deliberately WRONG blob id but leave size/mtime/hint
        // matching the disk. If the scan hashed, it would report modified;
        // reporting clean proves the hint pass genuinely skips hashing.
        let mut entry = ts.lookup("src/a.rs").unwrap().unwrap();
        entry.blob_id = gardenbed_model::Id::from_bytes([0xEE; 32]);
        ts.upsert(&entry).unwrap();
        assert!(g.scan(false).is_clean());

        // --full-scan forces the correctness path and catches the lie.
        assert_eq!(g.scan(true).modified, vec!["src/a.rs".to_owned()]);
    }

    #[test]
    fn unverified_hint_always_hashes() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        let ts = g.bed.treestate().unwrap();
        let mut entry = ts.lookup("src/a.rs").unwrap().unwrap();
        entry.blob_id = gardenbed_model::Id::from_bytes([0xEE; 32]);
        entry.hint = HINT_UNVERIFIED;
        ts.upsert(&entry).unwrap();
        // No full scan needed: hint = 0 means "don't trust me".
        assert_eq!(g.scan(false).modified, vec!["src/a.rs".to_owned()]);
    }

    #[test]
    fn deleted_files_are_reported() {
        let g = Garden::planted(&[("src/a.rs", b"hello"), ("src/b.rs", b"b")]);
        fs::remove_file(g.dir.path().join("src/a.rs")).unwrap();
        let report = g.scan(false);
        assert_eq!(report.deleted, vec!["src/a.rs".to_owned()]);
        assert!(report.modified.is_empty());
    }

    #[test]
    fn untracked_files_are_reported() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        fs::create_dir_all(g.dir.path().join("src/new")).unwrap();
        fs::write(g.dir.path().join("src/new/idea.md"), "draft").unwrap();
        let report = g.scan(false);
        assert_eq!(report.untracked, vec!["src/new/idea.md".to_owned()]);
        assert!(report.modified.is_empty() && report.deleted.is_empty());
    }

    #[test]
    fn files_outside_the_plot_are_invisible() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        // Untracked-looking file outside the plot: ignored entirely.
        fs::create_dir_all(g.dir.path().join("elsewhere")).unwrap();
        fs::write(g.dir.path().join("elsewhere/x.rs"), "?").unwrap();
        // Modified-looking file outside the plot: ignored entirely.
        fs::write(g.dir.path().join("elsewhere/z.rs"), "?").unwrap();
        let report = g.scan(false);
        assert!(report.is_clean());
    }

    #[test]
    fn empty_plot_scans_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let bed = crate::bed::Bed::init(&dir.path().join(".bed"), "s", "r").unwrap();
        fs::write(dir.path().join("stray.rs"), "?").unwrap();
        let report =
            scan_status(&bed.garden_root(), &[], &bed.treestate().unwrap(), false).unwrap();
        assert!(report.is_clean());
    }

    #[test]
    fn mode_change_counts_as_modified() {
        let g = Garden::planted(&[("src/run.sh", b"#!/bin/sh\n")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = g.dir.path().join("src/run.sh");
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&path, perms).unwrap();
            assert_eq!(g.scan(false).modified, vec!["src/run.sh".to_owned()]);
        }
    }

    #[test]
    fn the_bed_directory_is_never_garden_content() {
        let g = Garden::planted(&[("src/a.rs", b"hello")]);
        // `.bed` at the root is machinery, not files to report.
        fs::write(g.dir.path().join(".bed/loose.txt"), "?").unwrap();
        assert!(g.scan(false).is_clean());
    }

    #[test]
    fn render_is_git_status_like() {
        let report = StatusReport {
            modified: vec!["src/b.rs".to_owned()],
            deleted: vec!["src/a.rs".to_owned()],
            untracked: vec!["src/c.rs".to_owned()],
        };
        assert_eq!(
            report.render(),
            "modified:\n    src/b.rs\ndeleted:\n    src/a.rs\nuntracked:\n    src/c.rs\n"
        );
        assert_eq!(StatusReport::default().render(), "");
    }

    /// The sow-facing core reports the same walk with metadata attached.
    #[test]
    fn scan_paths_carries_blob_ids_modes_and_stat_metadata() {
        let g = Garden::planted(&[("src/a.rs", b"hello"), ("src/gone.txt", b"bye")]);
        fs::write(g.dir.path().join("src/a.rs"), b"changed").unwrap();
        fs::remove_file(g.dir.path().join("src/gone.txt")).unwrap();
        fs::write(g.dir.path().join("src/new.txt"), b"fresh").unwrap();

        let items = scan_paths(
            &g.bed.garden_root(),
            &g.bed.config().plot,
            &g.bed.treestate().unwrap(),
            false,
        )
        .unwrap();
        let by_path: std::collections::BTreeMap<_, _> =
            items.into_iter().map(|i| (i.path, i.delta)).collect();

        let meta = fs::metadata(g.dir.path().join("src/a.rs")).unwrap();
        assert_eq!(
            by_path.get("src/a.rs"),
            Some(&ScanDelta::Modified {
                blob_id: blob_id(b"changed"),
                mode: 0,
                size: b"changed".len() as i64,
                mtime_ns: mtime_ns(&meta),
            })
        );
        assert_eq!(by_path.get("src/gone.txt"), Some(&ScanDelta::Deleted));
        let meta = fs::metadata(g.dir.path().join("src/new.txt")).unwrap();
        assert_eq!(
            by_path.get("src/new.txt"),
            Some(&ScanDelta::Untracked {
                blob_id: blob_id(b"fresh"),
                mode: 0,
                size: b"fresh".len() as i64,
                mtime_ns: mtime_ns(&meta),
            })
        );
        // Everything tracked and untouched is Clean — the planted fixture
        // has no other files.
        assert_eq!(by_path.len(), 3);
    }

    /// A mode-only change reports the SAME blob id (content re-hashed
    /// unchanged) with the new mode — the shape sow turns into a `Modify`
    /// that reuses the old blob.
    #[test]
    fn mode_only_change_keeps_the_blob_id() {
        let g = Garden::planted(&[("src/run.sh", b"#!/bin/sh\n")]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = g.dir.path().join("src/run.sh");
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&path, perms).unwrap();
            let items = scan_paths(
                &g.bed.garden_root(),
                &g.bed.config().plot,
                &g.bed.treestate().unwrap(),
                false,
            )
            .unwrap();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].path, "src/run.sh");
            match &items[0].delta {
                ScanDelta::Modified {
                    blob_id: got, mode, ..
                } => {
                    assert_eq!(*got, gardenbed_model::blob_id(b"#!/bin/sh\n"));
                    assert_eq!(*mode, crate::treestate::MODE_EXEC);
                }
                other => panic!("expected Modified, got {other:?}"),
            }
        }
    }
    #[test]
    fn directory_replacements_delete_tracked_descendants_without_following_links() {
        for link in [false, true] {
            let g = Garden::planted(&[("src/f", b"base")]);
            fs::remove_dir_all(g.dir.path().join("src")).unwrap();
            let outside = tempfile::tempdir().unwrap();
            if link {
                fs::write(outside.path().join("f"), b"external").unwrap();
                std::os::unix::fs::symlink(outside.path(), g.dir.path().join("src")).unwrap();
            } else {
                fs::write(g.dir.path().join("src"), b"replacement").unwrap();
            }
            let ts = g.bed.treestate().unwrap();
            let report = scan_status(g.dir.path(), &[".".into()], &ts, true).unwrap();
            assert_eq!(report.deleted, ["src/f"]);
            assert_eq!(report.untracked, ["src"]);
            assert!(report.modified.is_empty());
        }
    }

    #[test]
    fn custom_state_directory_is_excluded_from_status_and_sow_scan() {
        let dir = tempfile::tempdir().unwrap();
        let bed = crate::bed::Bed::init(&dir.path().join("state"), "s", "r").unwrap();
        let ts = bed.treestate().unwrap();
        fs::write(dir.path().join("normal"), b"garden").unwrap();
        let report = scan_status(dir.path(), &[".".into()], &ts, true).unwrap();
        assert_eq!(report.untracked, ["normal"]);
        assert!(report.modified.is_empty());
        assert!(report.deleted.is_empty());
    }
<<<<<<< HEAD

=======
>>>>>>> codex/integrity-fixes
}

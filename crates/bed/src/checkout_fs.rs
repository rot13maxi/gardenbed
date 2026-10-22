//! Checkout operations anchored to open directories. No path component is
//! followed through a symlink, including when another process swaps an
//! ancestor after the checkout preflight.
//!
//! # Platform structure
//!
//! The safety contracts (never follow a symlinked component; identity —
//! device+inode — grants staging cleanup ownership; writes stage outside
//! the destination and publish atomically) are identical on every
//! platform. The primitives are not: Unix uses fd-anchored `*at` syscalls
//! through rustix (`openat` with `O_NOFOLLOW`, `statat` with
//! `AT_SYMLINK_NOFOLLOW`, `flock`, `renameat`); Windows has no `*at`
//! family, so the same discipline is enforced by resolving each open
//! directory's true path through its handle
//! (`GetFinalPathNameByHandleW`) and composing children underneath it —
//! a reparse point at any component is detected by the
//! reparse-aware leaf stat and refused, never followed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use gardenbed_model::{Id, Mode, blob_id};

use crate::error::ClientError;
use crate::treestate::{MODE_EXEC, MODE_REGULAR, MODE_SYMLINK};

#[path = "checkout_fs_primitives.rs"]
mod primitives;

use primitives::{DirHandle, Stat};
use primitives as prim;

pub(crate) struct CheckoutFs {
    root: DirHandle,
    staging: DirHandle,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    File { blob: Id, mode: i64 },
    Directory,
    Other,
}

/// The treestate's mtime convention: nanoseconds since the Unix epoch.
fn stat_mtime_ns(stat: &Stat) -> i64 {
    stat.mtime_ns
}

const JOURNAL_PREFIX: &str = "checkout-stage-";
const TEMP_PREFIX: &str = ".bed-checkout-";

/// Identity, rather than a filename convention, grants cleanup ownership.
/// A substituted entry is never ignored or deleted.
#[derive(Clone, Serialize, Deserialize)]
struct StageRecord {
    version: u8,
    path: String,
    device: String,
    inode: String,
    symlink: bool,
}

impl StageRecord {
    fn new(path: String, stat: &Stat) -> Self {
        Self {
            version: 1,
            path,
            device: format!("{:x}", stat.file_id.device),
            inode: format!("{:x}", stat.file_id.inode),
            symlink: stat.is_symlink,
        }
    }

    fn matches(&self, stat: &Stat) -> bool {
        self.version == 1
            && self.device == format!("{:x}", stat.file_id.device)
            && self.inode == format!("{:x}", stat.file_id.inode)
            && stat.is_symlink == self.symlink
    }
}

pub(crate) fn io_err(error: impl Into<std::io::Error>) -> ClientError {
    io(error.into())
}

/// Windows path-aliasing guard: repo paths are Unix-shaped (forward
/// slashes, case-sensitive), but the NT namespace resolves names
/// case-insensitively and treats several characters and shapes as
/// aliases. A repo path that would alias — or silently collide with the
/// case-insensitive `.bed` state directory — must fail before any local
/// file is touched, never overwrite or corrupt.
///
/// On Unix this is a no-op (paths are taken literally).
#[cfg(windows)]
fn check_windows_safe(path: &str) -> Result<(), ClientError> {
    for comp in path.split('/') {
        // Backslash and colon are namespace separators / ADS markers in
        // Win32; a repo path carrying them would address a different
        // object than the one history names.
        if comp.contains('\\') || comp.contains(':') {
            return Err(ClientError::CheckoutBlocked {
                path: path.to_owned(),
                reason: "Windows resolves this name differently than history does \
                         (separator, ADS, or aliasing character)"
                    .into(),
            });
        }
        // Trailing dots and spaces are stripped by Win32, so "f." and
        // "f " alias "f".
        if comp.ends_with('.') || comp.ends_with(' ') {
            return Err(ClientError::CheckoutBlocked {
                path: path.to_owned(),
                reason: "Windows strips a trailing dot or space, so this name aliases \
                         another file"
                    .into(),
            });
        }
        // Reserved device names resolve to devices, not files.
        let stem = comp.split('.').next().unwrap_or("");
        if matches!(
            stem.to_ascii_uppercase().as_str(),
            "CON" | "PRN" | "AUX" | "NUL"
                | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
                | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
        ) {
            return Err(ClientError::CheckoutBlocked {
                path: path.to_owned(),
                reason: "Windows reserves this device name; it can never be a file"
                    .into(),
            });
        }
        // Any case spelling of `.bed` collides with the state directory.
        if comp.len() == 4 && comp.starts_with('.') && comp[1..].eq_ignore_ascii_case("bed") {
            return Err(ClientError::CheckoutBlocked {
                path: path.to_owned(),
                reason: "this name aliases the case-insensitive .bed state directory"
                    .into(),
            });
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn check_windows_safe(_path: &str) -> Result<(), ClientError> {
    Ok(())
}

struct StageJournal {
    name: String,
    file: File, // exclusive flock held until publication/cleanup completes
    record: StageRecord,
}

fn io(error: std::io::Error) -> ClientError {
    error.into()
}

impl CheckoutFs {
    pub(crate) fn open(root: &Path, state_dir: &Path) -> Result<Self, ClientError> {
        Ok(Self {
            root: prim::open_dir(root).map_err(io)?,
            staging: prim::open_dir(state_dir).map_err(io)?,
        })
    }

    fn parent(&self, path: &str, create: bool) -> Result<Option<(DirHandle, String)>, ClientError> {
        prim::walk_parent(&self.root, path, create)
    }

    pub(crate) fn entry(&self, path: &str) -> Result<Option<Entry>, ClientError> {
        Ok(self.entry_inner(path, None)?.map(|(entry, _)| entry))
    }

    /// Like [`CheckoutFs::entry`], but when `known` describes a
    /// hash-verified row, a `stat` that still matches it settles the
    /// question without opening or reading the file.
    ///
    /// The returned entry then repeats the row's blob id instead of
    /// re-hashing. That is the same trust `bed status` extends to hints: a
    /// row with [`HINT_VERIFIED`](crate::treestate::HINT_VERIFIED) whose
    /// size, mtime, and mode still match was hashed when it was written, and
    /// a content change that leaves all three identical is not something a
    /// re-read could distinguish after the fact either — it would have
    /// already been missed by every hint-guided scan.
    ///
    /// Checkout updates walk the whole selection; without this, every
    /// update reads and hashes every selected file, which makes refresh
    /// cost scale with the selection rather than with the change.
    pub(crate) fn entry_verified(
        &self,
        path: &str,
        known: &crate::treestate::TreestateEntry,
    ) -> Result<Option<Entry>, ClientError> {
        if known.hint != crate::treestate::HINT_VERIFIED {
            return self.entry(path);
        }
        Ok(self.entry_inner(path, Some(known))?.map(|(entry, _)| entry))
    }

    fn entry_inner(
        &self,
        path: &str,
        known: Option<&crate::treestate::TreestateEntry>,
    ) -> Result<Option<(Entry, Option<Stat>)>, ClientError> {
        let Some((dir, name)) = self.parent(path, false)? else {
            return Ok(None);
        };
        let stat = match prim::statat(&dir, &name) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(e)),
        };
        let entry = if stat.is_dir {
            Entry::Directory
        } else if stat.is_symlink {
            Entry::File {
                blob: blob_id(&prim::readlinkat(&dir, &name).map_err(io)?),
                mode: MODE_SYMLINK,
            }
        } else if stat.is_file {
            // Windows has no exec bit (stat.exec is always false there);
            // the tracked row's exec identity is preserved instead of
            // diverging — same contract as status's classify_tracked.
            let mode =
                if stat.exec || (cfg!(windows) && known.map(|k| k.mode) == Some(MODE_EXEC)) {
                    MODE_EXEC
                } else {
                    MODE_REGULAR
                };
            // The hint path: a verified row whose size, mtime, and mode
            // still match proves the content without reading it.
            if let Some(known) = known
                && known.hint == crate::treestate::HINT_VERIFIED
                && known.mode == mode
                && known.size == stat.size
                && known.mtime_ns == stat_mtime_ns(&stat)
            {
                Entry::File {
                    blob: known.blob_id,
                    mode,
                }
            } else {
                let bytes = prim::read_file_nofollow(&dir, &name).map_err(io)?;
                Entry::File {
                    blob: blob_id(&bytes),
                    mode,
                }
            }
        } else {
            Entry::Other
        };
        Ok(Some((entry, Some(stat))))
    }

    /// Every leaf below a directory, without following symlinks. Empty
    /// directories are leaves as well so untracked directories survive.
    pub(crate) fn leaves(&self, path: &str) -> Result<Vec<String>, ClientError> {
        let Some((parent, name)) = self.parent(path, false)? else {
            return Ok(Vec::new());
        };
        let dir = prim::open_dir_nofollow(&parent, &name).map_err(io)?;
        let mut leaves = Vec::new();
        collect_leaves(&dir, path, &mut leaves)?;
        Ok(leaves)
    }

    pub(crate) fn remove(&self, path: &str) -> Result<(), ClientError> {
        check_windows_safe(path)?;
        let Some((parent, name)) = self.parent(path, false)? else {
            return Ok(());
        };
        remove_at(&parent, &name)
    }

    /// Removes now-empty directory ancestors of `path`, after a removal
    /// pass deleted files during a `switch`. Safe by construction: a
    /// directory that still holds anything (tracked leftovers, untracked
    /// files, ignored content, other directories) is never removed —
    /// `REMOVEDIR` on it fails `ENOTEMPTY`, which stops pruning there.
    /// The working-copy root is never a candidate (only strict
    /// sub-directories are named), and `.bed` lives under a state-dir
    /// rule the checkout never touches. Sparse selection is unaffected:
    /// it is recorded in the config, not on disk.
    pub(crate) fn prune_empty(&self, path: &str, sparse: &[String]) -> Result<(), ClientError> {
        if path.is_empty() {
            return Ok(()); // the working-copy root is never a candidate
        }
        let mut current = path.to_owned();
        loop {
            // A directory that is itself part of the sparse selection
            // stays on disk even when empty — the selection describes
            // what the working copy materializes, not what history at
            // this changeset happens to fill.
            if sparse
                .iter()
                .any(|prefix| prefix.trim_end_matches('/') == current)
            {
                return Ok(());
            }
            let Some((parent, name)) = self.parent(&current, false)? else {
                return Ok(()); // an ancestor is already gone — nothing to do
            };
            match prim::remove_dir(&parent, &name) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::DirectoryNotEmpty
                            | std::io::ErrorKind::AlreadyExists
                            | std::io::ErrorKind::NotFound
                    ) =>
                {
                    return Ok(())
                }
                Err(error) => return Err(io(error)),
            }
            let Some((dir, _)) = current.rsplit_once('/') else {
                return Ok(()); // the parent is the root — never removed
            };
            current = dir.to_owned();
        }
    }

    pub(crate) fn write(&self, path: &str, content: &[u8], mode: Mode) -> Result<(), ClientError> {
        check_windows_safe(path)?;
        let (parent, name) = self.parent(path, true)?.expect("parents created");
        if !same_rename_domain(&parent, &self.staging) {
            return self.write_cross_device(path, &parent, &name, content, mode);
        }
        // Stage inside the working copy state directory: interrupted writes leave
        // harmless cache debris, never partially overwrite working files.
        // EXCL also prevents a racing link from redirecting the write.
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp;
        loop {
            let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = format!("checkout-tmp-{}-{sequence}", std::process::id());
            let result = if mode == Mode::Symlink {
                prim::symlinkat(&self.staging, &candidate, content).map(|()| None)
            } else {
                prim::create_exclusive(&self.staging, &candidate).map(Some)
            };
            match result {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io(error)),
                Ok(fd) => {
                    temp = candidate;
                    let result = (|| -> Result<(), ClientError> {
                        if let Some(fd) = fd {
                            let mut file = fd;
                            file.write_all(content)?;
                            let bits = if mode == Mode::Exec { 0o755 } else { 0o644 };
                            prim::set_mode(&file, bits).map_err(io)?;
                        }
                        if let Ok(stat) = prim::statat(&parent, &name)
                            && stat.is_dir
                        {
                            remove_at(&parent, &name)?;
                        }
                        prim::renameat(&self.staging, &temp, &parent, &name).map_err(io)?;
                        Ok(())
                    })();
                    if result.is_err() {
                        let _ = prim::unlink(&self.staging, &temp);
                    }
                    return result;
                }
            }
        }
    }

    /// The shared scan lock prevents a scanner from observing the brief
    /// destination-temp creation→identity-registration interval. Writers
    /// hold the exclusive lock until their destination entry is published.
    pub(crate) fn staging_scan(&self) -> Result<(File, BTreeSet<String>), ClientError> {
        let lock = self.staging_lock(false)?;
        let mut paths = BTreeSet::new();
        for (_, _, record) in self.journals()? {
            if self.owned_stage(&record)? {
                paths.insert(record.path);
            }
        }
        Ok((lock, paths))
    }

    /// Reclaim only registered, inactive stage entries whose identities
    /// still match. Locks held by surviving writers are never stolen.
    pub(crate) fn cleanup_staging(&self) -> Result<(), ClientError> {
        let _lock = self.staging_lock(true)?;
        for (name, file, record) in self.journals()? {
            if let Err(e) = prim::try_lock_file_exclusive(&file) {
                match e {
                    gardenbed_platform::LockError::Held => continue,
                    gardenbed_platform::LockError::Io(e) => return Err(io(e)),
                }
            }
            if let Ok(Some((parent, leaf))) = self.parent(&record.path, false) {
                unlink_owned(&parent, &leaf, &record)?;
            }
            // A missing or substituted destination no longer belongs to
            // this journal; leave the destination alone and retire metadata.
            self.remove_journal(&name, &file)?;
        }
        Ok(())
    }

    fn staging_lock(&self, exclusive: bool) -> Result<File, ClientError> {
        let fd = prim::open_lock_file(
            &self.staging,
            "checkout-staging.lock",
        )
        .map_err(io)?;
        let file = fd;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData).into());
        }
        let result = if exclusive {
            gardenbed_platform::lock_file_exclusive(&file)
        } else {
            gardenbed_platform::lock_file_shared(&file)
        };
        result.map_err(|e| match e {
            gardenbed_platform::LockError::Held => {
                std::io::Error::other("staging lock contention").into()
            }
            gardenbed_platform::LockError::Io(e) => io(e),
        })?;
        Ok(file)
    }

    fn register_stage(&self, record: StageRecord, name: &str) -> Result<StageJournal, ClientError> {
        let fd = prim::create_lock_file(&self.staging, name).map_err(io)?;
        let mut file = fd;
        gardenbed_platform::lock_file_exclusive(&file)
            .map_err(|e| match e {
                gardenbed_platform::LockError::Held => {
                    std::io::Error::other("journal lock contention")
                }
                gardenbed_platform::LockError::Io(e) => e,
            })
            .map_err(io)?;
        let bytes = toml::to_string(&record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        file.write_all(bytes.as_bytes())?;
        file.sync_all()?;
        prim::fsync_dir(&self.staging).map_err(io)?;
        Ok(StageJournal {
            name: name.into(),
            file,
            record,
        })
    }

    fn journals(&self) -> Result<Vec<(String, File, StageRecord)>, ClientError> {
        let mut journals = Vec::new();
        let mut entries = prim::read_dir(&self.staging).map_err(io)?;
        for entry in &mut entries {
            let entry = entry.map_err(io)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !name.starts_with(JOURNAL_PREFIX) {
                continue;
            }
            let Ok(mut file) = prim::open_existing_rw(&self.staging, &name) else {
                continue;
            };
            if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
                continue;
            }
            let mut bytes = String::new();
            if Read::by_ref(&mut file)
                .take(16 * 1024)
                .read_to_string(&mut bytes)
                .is_err()
            {
                continue;
            }
            let Ok(record) = toml::from_str::<StageRecord>(&bytes) else {
                continue;
            };
            if record.version != 1
                || gardenbed_model::codec::validate_path(&record.path).is_err()
                || !record
                    .path
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| name.starts_with(TEMP_PREFIX))
            {
                continue;
            }
            journals.push((name.to_owned(), file, record));
        }
        Ok(journals)
    }

    fn owned_stage(&self, record: &StageRecord) -> Result<bool, ClientError> {
        let Ok(Some((parent, name))) = self.parent(&record.path, false) else {
            return Ok(false);
        };
        match prim::statat(&parent, &name) {
            Ok(stat) => Ok(record.matches(&stat)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io(error)),
        }
    }

    fn remove_journal(&self, name: &str, file: &File) -> Result<(), ClientError> {
        let identity = StageRecord::new(String::new(), &prim::fstat(file).map_err(io)?);
        unlink_owned(&self.staging, name, &identity)
    }

    fn write_cross_device(
        &self,
        path: &str,
        parent: &DirHandle,
        name: &str,
        content: &[u8],
        mode: Mode,
    ) -> Result<(), ClientError> {
        let _lock = self.staging_lock(true)?;
        static NEXT_STAGE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let sequence = NEXT_STAGE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let token = format!("{}-{stamp}-{sequence}", std::process::id());
            let temp = format!("{TEMP_PREFIX}{token}");
            let journal_name = format!("{JOURNAL_PREFIX}{token}");
            let temp_path = match path.rsplit_once('/') {
                Some((dir, _)) => format!("{dir}/{temp}"),
                None => temp.clone(),
            };
            let result = if mode == Mode::Symlink {
                prim::symlinkat(parent, &temp, content).map(|()| (None, true))
            } else {
                create_destination_stage(parent, &temp).map(|(fd, named)| (Some(fd), named))
            };
            let (fd, named) = match result {
                Ok(stage) => stage,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io(error)),
            };
            let identity = if named {
                prim::statat(parent, &temp).map_err(io)?
            } else {
                prim::fstat(fd.as_ref().expect("unnamed regular file")).map_err(io)?
            };
            let record = StageRecord::new(temp_path, &identity);
            let journal = match self.register_stage(record.clone(), &journal_name) {
                Ok(journal) => journal,
                Err(error) => {
                    unlink_owned(parent, &temp, &record)?;
                    return Err(error);
                }
            };
            let result = (|| -> Result<(), ClientError> {
                if let Some(fd) = fd {
                    let mut file = fd;
                    file.write_all(content)?;
                    prim::set_mode(
                        &file,
                        if mode == Mode::Exec { 0o755 } else { 0o644 },
                    )
                    .map_err(io)?;
                    if !named {
                        link_destination_stage(&file, parent, &temp).map_err(io)?;
                    }
                }
                let current = prim::statat(parent, &temp).map_err(io)?;
                if !journal.record.matches(&current) {
                    return Err(ClientError::CheckoutBlocked {
                        path: path.into(),
                        reason: "staged entry was replaced while materializing".into(),
                    });
                }
                if let Ok(stat) = prim::statat(parent, name)
                    && stat.is_dir
                {
                    remove_at(parent, name)?;
                }
                prim::renameat(parent, &temp, parent, name).map_err(io)?;
                Ok(())
            })();
            // Identity guards make cleanup safe even when a user replaced
            // the reserved-looking name after the writer was interrupted.
            let cleanup = unlink_owned(parent, &temp, &journal.record);
            if cleanup.is_ok() {
                self.remove_journal(&journal.name, &journal.file)?;
            }
            return result.and(cleanup);
        }
    }

    pub(crate) fn stat(&self, path: &str) -> Result<Stat, ClientError> {
        let (parent, name) = self
            .parent(path, false)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        prim::statat(&parent, &name).map_err(io)
    }
}

/// Equal device IDs are insufficient: separate bind mounts of the same
/// filesystem still reject cross-mount rename. If mount identity cannot
/// be established, destination staging is always the safe fallback.
fn same_rename_domain(a: &DirHandle, b: &DirHandle) -> bool {
    prim::same_rename_domain(a, b)
}

/// Prefer unnamed files: a process killed before registration leaves no
/// destination debris. Filesystems/platforms without O_TMPFILE use an
/// exclusive named entry, registered before any bytes are written.
fn create_destination_stage(parent: &DirHandle, name: &str) -> std::io::Result<(File, bool)> {
    prim::create_destination_stage(parent, name)
}

fn link_destination_stage(file: &File, parent: &DirHandle, name: &str) -> std::io::Result<()> {
    prim::link_destination_stage(file, parent, name)
}

fn unlink_owned(parent: &DirHandle, name: &str, record: &StageRecord) -> Result<(), ClientError> {
    match prim::statat(parent, name) {
        Ok(stat) if record.matches(&stat) => prim::unlink(parent, name).map_err(io),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error)),
    }
}

fn collect_leaves(dir: &DirHandle, path: &str, leaves: &mut Vec<String>) -> Result<(), ClientError> {
    let mut entries = prim::read_dir(dir).map_err(io)?;
    let before = leaves.len();
    for entry in &mut entries {
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        // Validated repository paths are UTF-8 on every platform the
        // checkout serves; a non-UTF-8 leaf cannot have come from a
        // checkout and is not cleanup's to touch.
        let Some(name) = name.to_str() else { continue };
        let child_path = format!("{path}/{name}");
        let stat = prim::statat(dir, name).map_err(io)?;
        if stat.is_dir {
            let child = prim::open_dir_nofollow(dir, name).map_err(io)?;
            collect_leaves(&child, &child_path, leaves)?;
        } else {
            leaves.push(child_path.to_owned());
        }
    }
    if leaves.len() == before {
        leaves.push(path.to_owned());
    }
    Ok(())
}

fn remove_at(parent: &DirHandle, name: &str) -> Result<(), ClientError> {
    let stat = match prim::statat(parent, name) {
        Ok(stat) => stat,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    if stat.is_dir {
        let child = prim::open_dir_nofollow(parent, name).map_err(io)?;
        let mut entries = prim::read_dir(&child).map_err(io)?;
        for entry in &mut entries {
            let entry = entry.map_err(io)?;
            let name = entry.file_name();
            if name != "." && name != ".."
                && let Some(name) = name.to_str()
            {
                remove_at(&child, name)?;
            }
        }
        prim::remove_dir(parent, name).map_err(io)?;
    } else {
        prim::unlink(parent, name).map_err(io)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn recursive_removal_unlinks_symlinks_without_touching_their_targets() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f"), b"valuable").unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("d/link")).unwrap();
        let disk = CheckoutFs::open(root.path(), outside.path()).unwrap();
        disk.remove("d").unwrap();
        assert!(!root.path().join("d").exists());
        assert_eq!(
            std::fs::read(outside.path().join("f")).unwrap(),
            b"valuable"
        );
    }

    #[test]
    fn failed_staging_preserves_the_old_leaf_and_completed_writes_replace_it() {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join(".bed");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(root.path().join("f"), b"old").unwrap();
        let disk = CheckoutFs::open(root.path(), &state).unwrap();
        assert!(disk.write("f", b"invalid\0target", Mode::Symlink).is_err());
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"old");
        disk.write("f", b"new", Mode::Exec).unwrap();
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"new");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(root.path().join("f"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(std::fs::read_dir(&state).unwrap().count(), 0);
    }

    fn cross_device_dirs() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
        let root = tempfile::tempdir().unwrap();
        let state = match tempfile::tempdir_in("/dev/shm") {
            Ok(state) => state,
            Err(error) => {
                eprintln!("cross-device test unavailable: {error}");
                return None;
            }
        };
        use std::os::unix::fs::MetadataExt as _;
        if std::fs::metadata(root.path()).unwrap().dev()
            == std::fs::metadata(state.path()).unwrap().dev()
        {
            eprintln!("cross-device test unavailable: /tmp and /dev/shm share a device");
            return None;
        }
        Some((root, state))
    }

    #[test]
    fn cross_device_regular_and_symlink_publication_is_atomic_and_preserves_old_leaf_on_failure() {
        let Some((root, state)) = cross_device_dirs() else {
            return;
        };
        std::fs::write(root.path().join("f"), b"old").unwrap();
        let disk = CheckoutFs::open(root.path(), state.path()).unwrap();
        assert!(disk.write("f", b"bad\0target", Mode::Symlink).is_err());
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"old");
        disk.write("f", b"new", Mode::Exec).unwrap();
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"new");
        disk.write("f", b"relative-target", Mode::Symlink).unwrap();
        assert_eq!(
            std::fs::read_link(root.path().join("f")).unwrap(),
            Path::new("relative-target")
        );
        disk.write("f", b"regular again", Mode::Regular).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("f")).unwrap(),
            b"regular again"
        );
        assert!(disk.journals().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn registered_staging_is_hidden_but_substituted_and_unregistered_entries_are_preserved() {
        let Some((root, state)) = cross_device_dirs() else {
            return;
        };
        let disk = CheckoutFs::open(root.path(), state.path()).unwrap();
        let temp = ".bed-checkout-owned";
        let mut fd = prim::create_exclusive(&disk.root, temp).unwrap();
        let record = StageRecord::new(temp.to_owned(), &prim::fstat(&fd).unwrap());
        let journal = disk.register_stage(record, "checkout-stage-owned").unwrap();
        fd.write_all(b"partial stage").unwrap();
        {
            let (_lock, hidden) = disk.staging_scan().unwrap();
            assert!(hidden.contains(temp));
        }
        // A live writer keeps ownership; cleanup may not steal its stage.
        disk.cleanup_staging().unwrap();
        assert!(root.path().join(temp).exists());
        drop(journal);
        std::fs::rename(root.path().join(temp), root.path().join("held-inode")).unwrap();
        std::fs::write(root.path().join(temp), b"user replacement").unwrap();
        std::fs::write(root.path().join(".bed-checkout-unregistered"), b"user file").unwrap();
        {
            let (_lock, hidden) = disk.staging_scan().unwrap();
            assert!(!hidden.contains(temp));
            assert!(!hidden.contains(".bed-checkout-unregistered"));
        }
        disk.cleanup_staging().unwrap();
        assert_eq!(
            std::fs::read(root.path().join(temp)).unwrap(),
            b"user replacement"
        );
        assert_eq!(
            std::fs::read(root.path().join(".bed-checkout-unregistered")).unwrap(),
            b"user file"
        );
        assert!(disk.journals().unwrap().is_empty());
    }

    #[test]
    fn cross_device_stage_crash_child() {
        let Some(root) = std::env::var_os("GARDENBED_STAGE_CRASH_ROOT") else {
            return;
        };
        let state = std::env::var_os("GARDENBED_STAGE_CRASH_STATE").unwrap();
        let disk = CheckoutFs::open(Path::new(&root), Path::new(&state)).unwrap();
        let name = ".bed-checkout-killed";
        let mut fd = prim::create_exclusive(&disk.root, name).unwrap();
        let record = StageRecord::new(name.to_owned(), &prim::fstat(&fd).unwrap());
        let _journal = disk
            .register_stage(record, "checkout-stage-killed")
            .unwrap();
        fd.write_all(b"incomplete stage").unwrap();
        std::fs::write(Path::new(&state).join("child-ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn killed_cross_device_writer_is_recovered_without_publishing_partial_bytes() {
        let Some((root, state)) = cross_device_dirs() else {
            return;
        };
        std::fs::write(root.path().join("f"), b"old").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "checkout_fs::tests::cross_device_stage_crash_child",
                "--nocapture",
            ])
            .env("GARDENBED_STAGE_CRASH_ROOT", root.path())
            .env("GARDENBED_STAGE_CRASH_STATE", state.path())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !state.path().join("child-ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ready = state.path().join("child-ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready, "child registered its partial stage before kill");
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"old");
        let disk = CheckoutFs::open(root.path(), state.path()).unwrap();
        {
            let (_lock, hidden) = disk.staging_scan().unwrap();
            assert!(hidden.contains(".bed-checkout-killed"));
        }
        disk.cleanup_staging().unwrap();
        assert!(!root.path().join(".bed-checkout-killed").exists());
        disk.write("f", b"complete", Mode::Regular).unwrap();
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"complete");
        assert!(disk.journals().unwrap().is_empty());
    }

    #[test]
    fn cross_device_writer_waits_for_a_scan_before_creating_staging_entries() {
        let Some((root, state)) = cross_device_dirs() else {
            return;
        };
        let disk = CheckoutFs::open(root.path(), state.path()).unwrap();
        let (scan_lock, _) = disk.staging_scan().unwrap();
        let root_path = root.path().to_path_buf();
        let state_path = state.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let writer = CheckoutFs::open(&root_path, &state_path).unwrap();
            sender.send(false).unwrap();
            writer.write("f", b"complete", Mode::Regular).unwrap();
            sender.send(true).unwrap();
        });
        assert!(
            !receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        drop(scan_lock);
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        );
        thread.join().unwrap();
        assert_eq!(std::fs::read(root.path().join("f")).unwrap(), b"complete");
    }

    #[test]
    fn checkout_descriptors_and_locks_are_not_inherited_by_exec_children() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let disk = CheckoutFs::open(root.path(), state.path()).unwrap();
        let (parent, _) = disk.parent("d/f", true).unwrap().unwrap();
        let flags = rustix::io::FdFlags::CLOEXEC;
        use std::os::fd::AsFd as _;
        assert!(rustix::io::fcntl_getfd(disk.root.as_fd()).unwrap().contains(flags));
        assert!(
            rustix::io::fcntl_getfd(disk.staging.as_fd())
                .unwrap()
                .contains(flags)
        );
        assert!(rustix::io::fcntl_getfd(parent.as_fd()).unwrap().contains(flags));
        let lock = disk.staging_lock(false).unwrap();
        assert!(rustix::io::fcntl_getfd(lock.as_fd()).unwrap().contains(flags));
        let stage = prim::create_exclusive(&parent, ".bed-checkout-flags").unwrap();
        let record =
            StageRecord::new("d/.bed-checkout-flags".to_owned(), &prim::fstat(&stage).unwrap());
        let journal = disk.register_stage(record, "checkout-stage-flags").unwrap();
        assert!(
            rustix::io::fcntl_getfd(journal.file.as_fd())
                .unwrap()
                .contains(flags)
        );
    }
}

#[cfg(all(test, unix))]
mod prune_tests {
    use super::*;
    #[test]
    fn prune_removes_empty_chain_but_respects_sparse_and_content() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/web/src")).unwrap();
        std::fs::write(root.path().join("crates/web/src/main.rs"), b"x").unwrap();
        let disk = CheckoutFs::open(root.path(), outside.path()).unwrap();
        disk.remove("crates/web/src/main.rs").unwrap();
        // `crates` is sparse-selected and stays; the emptied chain below
        // it is pruned.
        disk.prune_empty("crates/web/src", &["crates".to_owned()])
            .unwrap();
        assert!(!root.path().join("crates/web/src").exists());
        assert!(!root.path().join("crates/web").exists());
        assert!(root.path().join("crates").exists());

        // Untracked content blocks pruning of its directory.
        std::fs::create_dir_all(root.path().join("crates/keep")).unwrap();
        std::fs::write(root.path().join("crates/keep/precious"), b"u").unwrap();
        disk.prune_empty("crates/keep", &[]).unwrap();
        assert!(root.path().join("crates/keep").exists());
        assert!(root.path().join("crates/keep/precious").exists());
    }
}

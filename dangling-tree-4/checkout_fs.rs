//! Checkout operations anchored to open directories. No path component is
//! followed through a symlink, including when another process swaps an
//! ancestor after the checkout preflight.

<<<<<<< HEAD
=======
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
>>>>>>> codex/integrity-fixes
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use gardenbed_model::{Id, Mode, blob_id};
use rustix::fs::{self, AtFlags, FileType, OFlags};

use crate::error::BedError;
use crate::treestate::{MODE_EXEC, MODE_REGULAR, MODE_SYMLINK};

pub(crate) struct CheckoutFs {
    root: OwnedFd,
<<<<<<< HEAD
=======
    staging: OwnedFd,
>>>>>>> codex/integrity-fixes
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    File { blob: Id, mode: i64 },
    Directory,
    Other,
}

<<<<<<< HEAD
=======
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
    fn new(path: String, stat: &fs::Stat) -> Self {
        Self {
            version: 1,
            path,
            device: format!("{:x}", stat.st_dev),
            inode: format!("{:x}", stat.st_ino),
            symlink: FileType::from_raw_mode(stat.st_mode) == FileType::Symlink,
        }
    }

    fn matches(&self, stat: &fs::Stat) -> bool {
        self.version == 1
            && self.device == format!("{:x}", stat.st_dev)
            && self.inode == format!("{:x}", stat.st_ino)
            && FileType::from_raw_mode(stat.st_mode)
                == if self.symlink {
                    FileType::Symlink
                } else {
                    FileType::RegularFile
                }
    }
}

struct StageJournal {
    name: String,
    file: File, // exclusive flock held until publication/cleanup completes
    record: StageRecord,
}

>>>>>>> codex/integrity-fixes
fn io(error: rustix::io::Errno) -> BedError {
    std::io::Error::from(error).into()
}

impl CheckoutFs {
<<<<<<< HEAD
    pub(crate) fn open(root: &Path) -> Result<Self, BedError> {
        Ok(Self {
            root: fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY, fs::Mode::empty())
                .map_err(io)?,
=======
    pub(crate) fn open(root: &Path, state_dir: &Path) -> Result<Self, BedError> {
        Ok(Self {
            root: fs::open(
                root,
                OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY,
                fs::Mode::empty(),
            )
            .map_err(io)?,
            staging: fs::open(
                state_dir,
                OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY,
                fs::Mode::empty(),
            )
            .map_err(io)?,
>>>>>>> codex/integrity-fixes
        })
    }

    fn parent(&self, path: &str, create: bool) -> Result<Option<(OwnedFd, String)>, BedError> {
        let mut parts = path.split('/').peekable();
<<<<<<< HEAD
        let mut dir = rustix::io::dup(&self.root).map_err(io)?;
=======
        let mut dir = rustix::io::fcntl_dupfd_cloexec(&self.root, 0).map_err(io)?;
>>>>>>> codex/integrity-fixes
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Some((dir, part.to_owned())));
            }
<<<<<<< HEAD
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
=======
            let flags = OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
>>>>>>> codex/integrity-fixes
            let next = match fs::openat(&dir, part, flags, fs::Mode::empty()) {
                Ok(next) => next,
                Err(rustix::io::Errno::NOENT) if create => {
                    match fs::mkdirat(&dir, part, fs::Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(io(e)),
                    }
                    fs::openat(&dir, part, flags, fs::Mode::empty()).map_err(io)?
                }
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(e) => {
                    return Err(BedError::CheckoutBlocked {
                        path: path.to_owned(),
                        reason: format!("ancestor {part:?} is not a real directory ({e})"),
                    });
                }
            };
            dir = next;
        }
        unreachable!("validated repository paths are nonempty")
    }

    pub(crate) fn entry(&self, path: &str) -> Result<Option<Entry>, BedError> {
        let Some((dir, name)) = self.parent(path, false)? else {
            return Ok(None);
        };
        let stat = match fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => return Err(io(e)),
        };
        Ok(Some(match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => Entry::Directory,
            FileType::Symlink => Entry::File {
                blob: blob_id(
                    fs::readlinkat(&dir, name.as_str(), Vec::new())
                        .map_err(io)?
                        .to_bytes(),
                ),
                mode: MODE_SYMLINK,
            },
            FileType::RegularFile => {
                let fd = fs::openat(
                    &dir,
                    name.as_str(),
<<<<<<< HEAD
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
=======
                    OFlags::CLOEXEC | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
>>>>>>> codex/integrity-fixes
                    fs::Mode::empty(),
                )
                .map_err(io)?;
                // Verify the opened object too: a racing replacement must
                // not turn this read into a device or FIFO operation.
                let mut file = File::from(fd);
                let meta = file.metadata()?;
                if !meta.is_file() {
                    return Ok(Some(Entry::Other));
                }
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                use std::os::unix::fs::PermissionsExt as _;
                Entry::File {
                    blob: blob_id(&bytes),
                    mode: if meta.permissions().mode() & 0o111 != 0 {
                        MODE_EXEC
                    } else {
                        MODE_REGULAR
                    },
                }
            }
            _ => Entry::Other,
        }))
    }

    /// Every leaf below a directory, without following symlinks. Empty
    /// directories are leaves as well so untracked directories survive.
    pub(crate) fn leaves(&self, path: &str) -> Result<Vec<String>, BedError> {
        let Some((parent, name)) = self.parent(path, false)? else {
            return Ok(Vec::new());
        };
        let dir = fs::openat(
            parent,
            name.as_str(),
<<<<<<< HEAD
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
=======
            OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
>>>>>>> codex/integrity-fixes
            fs::Mode::empty(),
        )
        .map_err(io)?;
        let mut leaves = Vec::new();
        collect_leaves(&dir, path, &mut leaves)?;
        Ok(leaves)
    }

    pub(crate) fn remove(&self, path: &str) -> Result<(), BedError> {
        let Some((parent, name)) = self.parent(path, false)? else {
            return Ok(());
        };
        remove_at(&parent, name.as_str())
    }

    pub(crate) fn write(&self, path: &str, content: &[u8], mode: Mode) -> Result<(), BedError> {
        let (parent, name) = self.parent(path, true)?.expect("parents created");
<<<<<<< HEAD
        if mode == Mode::Symlink {
            use std::os::unix::ffi::OsStrExt as _;
            fs::symlinkat(std::ffi::OsStr::from_bytes(content), &parent, name.as_str())
                .map_err(io)?;
        } else {
            // EXCL refuses even a racing leaf symlink. Parent descriptors
            // stay anchored if an ancestor is renamed or replaced.
            let fd = fs::openat(
                &parent,
                name.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                fs::Mode::from_raw_mode(0o600),
            )
            .map_err(io)?;
            let mut file = File::from(fd);
            file.write_all(content)?;
            let bits = if mode == Mode::Exec { 0o755 } else { 0o644 };
            fs::fchmod(file.as_fd(), fs::Mode::from_raw_mode(bits)).map_err(io)?;
=======
        if !same_rename_domain(&parent, &self.staging) {
            return self.write_cross_device(path, &parent, &name, content, mode);
        }
        // Stage inside the bed state directory: interrupted writes leave
        // harmless cache debris, never partially overwrite working files.
        // EXCL also prevents a racing link from redirecting the write.
        static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp;
        loop {
            let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let candidate = format!("checkout-tmp-{}-{sequence}", std::process::id());
            let result = if mode == Mode::Symlink {
                use std::os::unix::ffi::OsStrExt as _;
                fs::symlinkat(
                    std::ffi::OsStr::from_bytes(content),
                    &self.staging,
                    candidate.as_str(),
                )
                .map(|()| None)
            } else {
                fs::openat(
                    &self.staging,
                    candidate.as_str(),
                    OFlags::CLOEXEC
                        | OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW,
                    fs::Mode::from_raw_mode(0o600),
                )
                .map(Some)
            };
            match result {
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(io(error)),
                Ok(fd) => {
                    temp = candidate;
                    let result = (|| -> Result<(), BedError> {
                        if let Some(fd) = fd {
                            let mut file = File::from(fd);
                            file.write_all(content)?;
                            let bits = if mode == Mode::Exec { 0o755 } else { 0o644 };
                            fs::fchmod(file.as_fd(), fs::Mode::from_raw_mode(bits)).map_err(io)?;
                        }
                        if let Ok(stat) =
                            fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                            && FileType::from_raw_mode(stat.st_mode) == FileType::Directory
                        {
                            remove_at(&parent, name.as_str())?;
                        }
                        fs::renameat(&self.staging, temp.as_str(), &parent, name.as_str())
                            .map_err(io)?;
                        Ok(())
                    })();
                    if result.is_err() {
                        let _ = fs::unlinkat(&self.staging, temp.as_str(), AtFlags::empty());
                    }
                    return result;
                }
            }
        }
    }

    /// The shared scan lock prevents a scanner from observing the brief
    /// destination-temp creation→identity-registration interval. Writers
    /// hold the exclusive lock until their destination entry is published.
    pub(crate) fn staging_scan(&self) -> Result<(File, BTreeSet<String>), BedError> {
        let lock = self.staging_lock(fs::FlockOperation::LockShared)?;
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
    pub(crate) fn cleanup_staging(&self) -> Result<(), BedError> {
        let _lock = self.staging_lock(fs::FlockOperation::LockExclusive)?;
        for (name, file, record) in self.journals()? {
            match fs::flock(&file, fs::FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) => continue,
                Err(error) => return Err(io(error)),
            }
            if let Ok(Some((parent, leaf))) = self.parent(&record.path, false) {
                unlink_owned(&parent, &leaf, &record)?;
            }
            // A missing or substituted destination no longer belongs to
            // this journal; leave the destination alone and retire metadata.
            self.remove_journal(&name, &file)?;
>>>>>>> codex/integrity-fixes
        }
        Ok(())
    }

<<<<<<< HEAD
=======
    fn staging_lock(&self, operation: fs::FlockOperation) -> Result<File, BedError> {
        let fd = fs::openat(
            &self.staging,
            "checkout-staging.lock",
            OFlags::CLOEXEC | OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            fs::Mode::from_raw_mode(0o600),
        )
        .map_err(io)?;
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData).into());
        }
        fs::flock(&file, operation).map_err(io)?;
        Ok(file)
    }

    fn register_stage(&self, record: StageRecord, name: &str) -> Result<StageJournal, BedError> {
        let fd = fs::openat(
            &self.staging,
            name,
            OFlags::CLOEXEC | OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            fs::Mode::from_raw_mode(0o600),
        )
        .map_err(io)?;
        let mut file = File::from(fd);
        fs::flock(&file, fs::FlockOperation::LockExclusive).map_err(io)?;
        let bytes = toml::to_string(&record)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        file.write_all(bytes.as_bytes())?;
        file.sync_all()?;
        fs::fsync(&self.staging).map_err(io)?;
        Ok(StageJournal {
            name: name.into(),
            file,
            record,
        })
    }

    fn journals(&self) -> Result<Vec<(String, File, StageRecord)>, BedError> {
        let mut journals = Vec::new();
        let mut entries = fs::Dir::read_from(&self.staging).map_err(io)?;
        for entry in &mut entries {
            let entry = entry.map_err(io)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if !name.starts_with(JOURNAL_PREFIX) {
                continue;
            }
            let fd = match fs::openat(
                &self.staging,
                name,
                OFlags::CLOEXEC | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                fs::Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(_) => continue,
            };
            let mut file = File::from(fd);
            if !file.metadata()?.is_file() {
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

    fn owned_stage(&self, record: &StageRecord) -> Result<bool, BedError> {
        let Ok(Some((parent, name))) = self.parent(&record.path, false) else {
            return Ok(false);
        };
        match fs::statat(parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(record.matches(&stat)),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(io(error)),
        }
    }

    fn remove_journal(&self, name: &str, file: &File) -> Result<(), BedError> {
        let identity = StageRecord::new(String::new(), &fs::fstat(file).map_err(io)?);
        unlink_owned(&self.staging, name, &identity)
    }

    fn write_cross_device(
        &self,
        path: &str,
        parent: &OwnedFd,
        name: &str,
        content: &[u8],
        mode: Mode,
    ) -> Result<(), BedError> {
        let _lock = self.staging_lock(fs::FlockOperation::LockExclusive)?;
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
                use std::os::unix::ffi::OsStrExt as _;
                fs::symlinkat(std::ffi::OsStr::from_bytes(content), parent, temp.as_str())
                    .map(|()| (None, true))
            } else {
                create_destination_stage(parent, &temp).map(|(fd, named)| (Some(fd), named))
            };
            let (fd, named) = match result {
                Ok(stage) => stage,
                Err(rustix::io::Errno::EXIST) => continue,
                Err(error) => return Err(io(error)),
            };
            let identity = if named {
                fs::statat(parent, temp.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)?
            } else {
                fs::fstat(fd.as_ref().expect("unnamed regular file")).map_err(io)?
            };
            let record = StageRecord::new(temp_path, &identity);
            let journal = match self.register_stage(record.clone(), &journal_name) {
                Ok(journal) => journal,
                Err(error) => {
                    unlink_owned(parent, &temp, &record)?;
                    return Err(error);
                }
            };
            let result = (|| -> Result<(), BedError> {
                if let Some(fd) = fd {
                    let mut file = File::from(fd);
                    file.write_all(content)?;
                    fs::fchmod(
                        &file,
                        fs::Mode::from_raw_mode(if mode == Mode::Exec { 0o755 } else { 0o644 }),
                    )
                    .map_err(io)?;
                    if !named {
                        link_destination_stage(&file, parent, &temp).map_err(io)?;
                    }
                }
                let current =
                    fs::statat(parent, temp.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
                if !journal.record.matches(&current) {
                    return Err(BedError::CheckoutBlocked {
                        path: path.into(),
                        reason: "staged entry was replaced while materializing".into(),
                    });
                }
                if let Ok(stat) = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                    && FileType::from_raw_mode(stat.st_mode) == FileType::Directory
                {
                    remove_at(parent, name)?;
                }
                fs::renameat(parent, temp.as_str(), parent, name).map_err(io)?;
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

>>>>>>> codex/integrity-fixes
    pub(crate) fn stat(&self, path: &str) -> Result<fs::Stat, BedError> {
        let (parent, name) = self
            .parent(path, false)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        fs::statat(parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)
    }
}

<<<<<<< HEAD
=======
/// Equal device IDs are insufficient: separate bind mounts of the same
/// filesystem still reject cross-mount rename. If mount identity cannot
/// be established, destination staging is always the safe fallback.
fn same_rename_domain(a: &OwnedFd, b: &OwnedFd) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mask = fs::StatxFlags::MNT_ID;
        match (
            fs::statx(a, "", AtFlags::EMPTY_PATH, mask),
            fs::statx(b, "", AtFlags::EMPTY_PATH, mask),
        ) {
            (Ok(a), Ok(b)) if a.stx_mask & mask.bits() != 0 && b.stx_mask & mask.bits() != 0 => {
                a.stx_mnt_id == b.stx_mnt_id
            }
            _ => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (a, b);
        false
    }
}

/// Prefer unnamed files: a process killed before registration leaves no
/// destination debris. Filesystems/platforms without O_TMPFILE use an
/// exclusive named entry, registered before any bytes are written.
fn create_destination_stage(parent: &OwnedFd, name: &str) -> rustix::io::Result<(OwnedFd, bool)> {
    #[cfg(target_os = "linux")]
    if let Ok(fd) = fs::openat(
        parent,
        ".",
        OFlags::CLOEXEC | OFlags::WRONLY | OFlags::TMPFILE,
        fs::Mode::from_raw_mode(0o600),
    ) {
        return Ok((fd, false));
    }
    fs::openat(
        parent,
        name,
        OFlags::CLOEXEC | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
        fs::Mode::from_raw_mode(0o600),
    )
    .map(|fd| (fd, true))
}

fn link_destination_stage(file: &File, parent: &OwnedFd, name: &str) -> rustix::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        match fs::linkat(file, "", parent, name, AtFlags::EMPTY_PATH) {
            Ok(()) => Ok(()),
            // Unprivileged processes can link their O_TMPFILE through
            // their own live descriptor without CAP_DAC_READ_SEARCH.
            Err(rustix::io::Errno::PERM | rustix::io::Errno::NOENT) => {
                use std::os::fd::AsRawFd as _;
                let source = format!("/proc/self/fd/{}", file.as_raw_fd());
                fs::linkat(
                    fs::CWD,
                    source.as_str(),
                    parent,
                    name,
                    AtFlags::SYMLINK_FOLLOW,
                )
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, parent, name);
        unreachable!("only Linux uses unnamed stage files")
    }
}

fn unlink_owned(parent: &OwnedFd, name: &str, record: &StageRecord) -> Result<(), BedError> {
    match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if record.matches(&stat) => {
            fs::unlinkat(parent, name, AtFlags::empty()).map_err(io)
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => Ok(()),
        Err(error) => Err(io(error)),
    }
}

>>>>>>> codex/integrity-fixes
fn collect_leaves(dir: &OwnedFd, path: &str, leaves: &mut Vec<String>) -> Result<(), BedError> {
    let mut entries = fs::Dir::read_from(dir).map_err(io)?;
    let before = leaves.len();
    for entry in &mut entries {
        let entry = entry.map_err(io)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let child_path = format!("{path}/{}", name.to_string_lossy());
        let stat = fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = fs::openat(
                dir,
                name,
<<<<<<< HEAD
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
=======
                OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
>>>>>>> codex/integrity-fixes
                fs::Mode::empty(),
            )
            .map_err(io)?;
            collect_leaves(&child, &child_path, leaves)?;
        } else {
            leaves.push(child_path);
        }
    }
    if leaves.len() == before {
        leaves.push(path.to_owned());
    }
    Ok(())
}

fn remove_at(parent: &OwnedFd, name: impl rustix::path::Arg + Copy) -> Result<(), BedError> {
    let stat = match fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
        let child = fs::openat(
            parent,
            name,
<<<<<<< HEAD
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
=======
            OFlags::CLOEXEC | OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
>>>>>>> codex/integrity-fixes
            fs::Mode::empty(),
        )
        .map_err(io)?;
        let mut entries = fs::Dir::read_from(&child).map_err(io)?;
        for entry in &mut entries {
            let entry = entry.map_err(io)?;
            let name = entry.file_name();
            if name.to_bytes() != b"." && name.to_bytes() != b".." {
                remove_at(&child, name)?;
            }
        }
        fs::unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io)?;
    } else {
        fs::unlinkat(parent, name, AtFlags::empty()).map_err(io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_removal_unlinks_symlinks_without_touching_their_targets() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f"), b"valuable").unwrap();
        std::fs::create_dir(root.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("d/link")).unwrap();
<<<<<<< HEAD
        let disk = CheckoutFs::open(root.path()).unwrap();
        disk.remove("d").unwrap();
        assert!(!root.path().join("d").exists());
        assert_eq!(std::fs::read(outside.path().join("f")).unwrap(), b"valuable");
=======
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
        assert_eq!(std::fs::read_dir(state).unwrap().count(), 0);
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
        let fd = fs::openat(
            &disk.root,
            temp,
            OFlags::CLOEXEC | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        let record = StageRecord::new(temp.into(), &fs::fstat(&fd).unwrap());
        let journal = disk.register_stage(record, "checkout-stage-owned").unwrap();
        File::from(fd).write_all(b"partial stage").unwrap();
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
        let fd = fs::openat(
            &disk.root,
            name,
            OFlags::CLOEXEC | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        let record = StageRecord::new(name.into(), &fs::fstat(&fd).unwrap());
        let _journal = disk
            .register_stage(record, "checkout-stage-killed")
            .unwrap();
        File::from(fd).write_all(b"incomplete stage").unwrap();
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
        assert!(rustix::io::fcntl_getfd(&disk.root).unwrap().contains(flags));
        assert!(
            rustix::io::fcntl_getfd(&disk.staging)
                .unwrap()
                .contains(flags)
        );
        assert!(rustix::io::fcntl_getfd(&parent).unwrap().contains(flags));
        let lock = disk.staging_lock(fs::FlockOperation::LockShared).unwrap();
        assert!(rustix::io::fcntl_getfd(&lock).unwrap().contains(flags));
        let stage = fs::openat(
            &parent,
            ".bed-checkout-flags",
            OFlags::CLOEXEC | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            fs::Mode::from_raw_mode(0o600),
        )
        .unwrap();
        let record = StageRecord::new("d/.bed-checkout-flags".into(), &fs::fstat(&stage).unwrap());
        let journal = disk.register_stage(record, "checkout-stage-flags").unwrap();
        assert!(
            rustix::io::fcntl_getfd(&journal.file)
                .unwrap()
                .contains(flags)
        );
>>>>>>> codex/integrity-fixes
    }
}

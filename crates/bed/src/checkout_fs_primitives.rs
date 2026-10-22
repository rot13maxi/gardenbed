//! The platform primitives `checkout_fs` is written against: an anchored
//! directory handle, reparse-aware `*at`-shaped stat, and the staging
//! write helpers. Unix is the fd-anchored rustix implementation the file
//! used inline (same syscalls, same flags, byte-identical behavior);
//! Windows implements the same contracts with kernel32 handles.
//!
//! Contracts every platform must uphold:
//!
//! * An anchored [`DirHandle`] never re-walks a symlinked ancestor: Unix
//!   holds a directory fd; Windows resolves the directory's true path
//!   through its handle and composes children under that.
//! * [`statat`] never follows a symlink at the final component.
//! * [`read_file_nofollow`] verifies the opened object is a regular file
//!   before reading, so a racing replacement cannot turn the read into a
//!   device or FIFO operation.
//! * [`renameat`] publishes atomically within one rename domain.

pub use gardenbed_platform::stat::Stat;
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Read as _;

/// Re-export for the checkout module's journal-lock use.
pub use gardenbed_platform::try_lock_file_exclusive;

#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
pub use windows::*;

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::error::ClientError;
    use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
    use std::path::{Path, PathBuf};

    /// An anchored directory: an open fd (O_DIRECTORY), the anchor every
    /// `*at` operation composes from. The fd is held as a `File` so the
    /// platform crate's statat/fstat can borrow it directly.
    pub struct DirHandle {
        file: File,
        path: PathBuf,
    }

    impl DirHandle {
        fn from_fd(fd: OwnedFd, path: PathBuf) -> Self {
            Self {
                file: File::from(fd),
                path,
            }
        }

        /// The anchored fd — test-visible for CLOEXEC discipline checks.
        #[cfg(test)]
        pub(crate) fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
            self.file.as_fd()
        }
    }

    /// Opens a directory without following a symlink at the final
    /// component.
    pub fn open_dir(path: &Path) -> io::Result<DirHandle> {
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY,
            rustix::fs::Mode::empty(),
        )?;
        Ok(DirHandle {
            file: File::from(fd),
            path: path.to_path_buf(),
        })
    }

    /// Opens `name` inside `dir` as a directory, refusing to follow a
    /// symlink at `name` (O_NOFOLLOW|O_DIRECTORY).
    pub fn open_dir_nofollow(dir: &DirHandle, name: &str) -> io::Result<DirHandle> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        Ok(DirHandle::from_fd(fd, dir.path.join(name)))
    }

    /// Walks `path`'s ancestors from `root`, opening each real directory;
    /// creates missing ones when `create`. Returns the parent handle plus
    /// the final component. `Ok(None)` when a needed ancestor is absent
    /// (non-creating walks); a non-directory ancestor is a hard refusal.
    pub fn walk_parent(
        root: &DirHandle,
        path: &str,
        create: bool,
    ) -> Result<Option<(DirHandle, String)>, ClientError> {
        let io = |e: io::Error| crate::checkout_fs::io_err(e);
        let mut parts = path.split('/').peekable();
        // A dup of the anchor: the walk holds two handles to the same
        // object (this one and the caller's root).
        let mut dir = DirHandle {
            file: root.file.try_clone().map_err(io)?,
            path: root.path.clone(),
        };
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Some((dir, part.to_owned())));
            }
            let flags = rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW;
            let next = match rustix::fs::openat(
                dir.file.as_fd(),
                part,
                flags,
                rustix::fs::Mode::empty(),
            ) {
                Ok(next) => next,
                Err(rustix::io::Errno::NOENT) if create => {
                    match rustix::fs::mkdirat(
                        dir.file.as_fd(),
                        part,
                        rustix::fs::Mode::from_raw_mode(0o755),
                    ) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(e) => return Err(io(e.into())),
                    }
                    rustix::fs::openat(dir.file.as_fd(), part, flags, rustix::fs::Mode::empty())
                        .map_err(|e| io(e.into()))?
                }
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(e) => {
                    return Err(crate::error::ClientError::CheckoutBlocked {
                        path: path.to_owned(),
                        reason: format!("ancestor {part:?} is not a real directory ({e})"),
                    });
                }
            };
            dir = DirHandle::from_fd(next, dir.path.join(part));
        }
        unreachable!("validated repository paths are nonempty")
    }
    /// Stats `name` inside `dir` without following a symlink at the leaf.
    pub fn statat(dir: &DirHandle, name: &str) -> io::Result<Stat> {
        gardenbed_platform::statat(&dir.file, name)
    }

    /// Stats an open file.
    pub fn fstat(file: &File) -> io::Result<Stat> {
        gardenbed_platform::fstat(file)
    }

    /// Reads the link-target bytes of `name` inside `dir`.
    pub fn readlinkat(dir: &DirHandle, name: &str) -> io::Result<Vec<u8>> {
        gardenbed_platform::readlinkat(&dir.file, name)
    }

    /// Creates a symlink `name -> target` inside `dir`.
    pub fn symlinkat(dir: &DirHandle, name: &str, target: &[u8]) -> io::Result<()> {
        gardenbed_platform::symlinkat(&dir.file, name, target)
    }

    /// Opens and reads a regular file at `name` inside `dir`, never
    /// following a symlink, verifying the opened object is a regular file
    /// (a racing replacement must not become a device or FIFO read).
    pub fn read_file_nofollow(dir: &DirHandle, name: &str) -> io::Result<Vec<u8>> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )?;
        let mut file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Creates `name` inside `dir` exclusively (write mode, 0600).
    pub fn create_exclusive(dir: &DirHandle, name: &str) -> io::Result<File> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        Ok(File::from(fd))
    }

    /// Opens an existing journal file read-write, without following a
    /// link at the leaf, non-blocking (a vanished journal is skipped).
    pub fn open_existing_rw(dir: &DirHandle, name: &str) -> io::Result<File> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )?;
        Ok(File::from(fd))
    }

    /// Opens (creating when absent, 0600) a lock file inside `dir`.
    pub fn open_lock_file(dir: &DirHandle, name: &str) -> io::Result<File> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        Ok(File::from(fd))
    }

    /// Creates `name` inside `dir` exclusively for write+lock (0600).
    pub fn create_lock_file(dir: &DirHandle, name: &str) -> io::Result<File> {
        let fd = rustix::fs::openat(
            dir.file.as_fd(),
            name,
            rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::from_raw_mode(0o600),
        )?;
        Ok(File::from(fd))
    }

    /// Sets a file's permission bits.
    pub fn set_mode(file: &File, bits: u32) -> io::Result<()> {
        rustix::fs::fchmod(file.as_fd(), rustix::fs::Mode::from_raw_mode(bits))
            .map_err(io::Error::from)
    }

    /// Renames `from` → `to` across two anchored directories.
    pub fn renameat(
        from_dir: &DirHandle,
        from: &str,
        to_dir: &DirHandle,
        to: &str,
    ) -> io::Result<()> {
        rustix::fs::renameat(from_dir.file.as_fd(), from, to_dir.file.as_fd(), to)
            .map_err(io::Error::from)
    }

    /// Unlinks `name` inside `dir`.
    pub fn unlink(dir: &DirHandle, name: &str) -> io::Result<()> {
        rustix::fs::unlinkat(dir.file.as_fd(), name, rustix::fs::AtFlags::empty())
            .map_err(io::Error::from)
    }

    /// Removes the directory `name` inside `dir` (must be empty).
    pub fn remove_dir(dir: &DirHandle, name: &str) -> io::Result<()> {
        rustix::fs::unlinkat(dir.file.as_fd(), name, rustix::fs::AtFlags::REMOVEDIR)
            .map_err(io::Error::from)
    }

    /// fsyncs a directory so entries created/renamed inside it are durable.
    pub fn fsync_dir(dir: &DirHandle) -> io::Result<()> {
        rustix::fs::fsync(dir.file.as_fd()).map_err(io::Error::from)
    }

    /// Reads a directory's entries (names only).
    pub fn read_dir(dir: &DirHandle) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(&dir.path)
    }

    /// Equal device IDs are insufficient: separate bind mounts of the same
    /// filesystem still reject cross-mount rename. If mount identity
    /// cannot be established, destination staging is always the safe
    /// fallback (statx MNT_ID comparison; anything else → false).
    pub fn same_rename_domain(a: &DirHandle, b: &DirHandle) -> bool {
        #[cfg(target_os = "linux")]
        {
            let mask = rustix::fs::StatxFlags::MNT_ID;
            match (
                rustix::fs::statx(a.file.as_fd(), "", rustix::fs::AtFlags::EMPTY_PATH, mask),
                rustix::fs::statx(b.file.as_fd(), "", rustix::fs::AtFlags::EMPTY_PATH, mask),
            ) {
                (Ok(a), Ok(b))
                    if a.stx_mask & mask.bits() != 0 && b.stx_mask & mask.bits() != 0 =>
                {
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

    /// Prefer unnamed files: a process killed before registration leaves
    /// no destination debris. Filesystems without O_TMPFILE use an
    /// exclusive named entry, registered before any bytes are written.
    pub fn create_destination_stage(parent: &DirHandle, name: &str) -> io::Result<(File, bool)> {
        #[cfg(target_os = "linux")]
        if let Ok(fd) = rustix::fs::openat(
            parent.file.as_fd(),
            ".",
            rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::TMPFILE,
            rustix::fs::Mode::from_raw_mode(0o600),
        ) {
            return Ok((File::from(fd), false));
        }
        create_exclusive(parent, name).map(|fd| (fd, true))
    }

    /// Links an O_TMPFILE into a directory entry.
    pub fn link_destination_stage(file: &File, parent: &DirHandle, name: &str) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsFd as _;
            match rustix::fs::linkat(
                file.as_fd(),
                "",
                parent.file.as_fd(),
                name,
                rustix::fs::AtFlags::EMPTY_PATH,
            ) {
                Ok(()) => Ok(()),
                // Unprivileged processes can link their O_TMPFILE through
                // their own live descriptor without CAP_DAC_READ_SEARCH.
                Err(rustix::io::Errno::PERM | rustix::io::Errno::NOENT) => {
                    let source = format!("/proc/self/fd/{}", file.as_raw_fd());
                    rustix::fs::linkat(
                        rustix::fs::CWD,
                        source.as_str(),
                        parent.file.as_fd(),
                        name,
                        rustix::fs::AtFlags::SYMLINK_FOLLOW,
                    )
                    .map_err(io::Error::from)
                }
                Err(error) => Err(io::Error::from(error)),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (file, parent, name);
            unreachable!("only Linux uses unnamed stage files")
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use gardenbed_platform::nt::*;
    use gardenbed_platform::stat::nt_open_relative;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Path, PathBuf};

    /// An anchored directory: a live NT handle (opened with
    /// `FILE_FLAG_BACKUP_SEMANTICS`) plus its resolved final path for the
    /// few Win32-layer calls that still need one (symlink creation,
    /// volume metadata). Every checkout operation composes children
    /// through `NtCreateFile` with `RootDirectory` = this handle, so no
    /// path component is ever re-walked.
    pub struct DirHandle {
        file: File,
        path: PathBuf,
    }

    impl DirHandle {
        fn from_nt(handle: OwnedNtHandle, path: PathBuf) -> Self {
            Self {
                file: handle.into_file(),
                path,
            }
        }

        /// Duplicates the handle (Clone for the anchor).
        fn duplicate(&self) -> io::Result<DirHandle> {
            let dup = duplicate_handle(self.file.as_raw_handle())
                .map_err(|e| io::Error::new(e.kind(), format!("duplicating anchor: {e}")))?;
            Ok(DirHandle {
                // SAFETY: DuplicateHandle produced an owned handle.
                file: unsafe { File::from_raw_handle(dup) },
                path: self.path.clone(),
            })
        }
    }

    impl Clone for DirHandle {
        fn clone(&self) -> Self {
            self.duplicate()
                .expect("a live anchor handle can be duplicated")
        }
    }

    /// Opens a directory. `FILE_FLAG_BACKUP_SEMANTICS` is required to
    /// open a directory handle at all; the anchor pins the object.
    pub fn open_dir(path: &Path) -> io::Result<DirHandle> {
        let file = open_dir_handle(path)?;
        let final_path = final_path(&file)?;
        Ok(DirHandle {
            file,
            path: final_path,
        })
    }

    /// Opens `name` inside `dir` as a directory, refusing to follow a
    /// reparse point at `name`: the child opens through `NtCreateFile`
    /// with `RootDirectory = dir`'s handle, exactly one component, with
    /// `FILE_OPEN_REPARSE_POINT` so a link at the leaf is an error rather
    /// than a traversal.
    pub fn open_dir_nofollow(dir: &DirHandle, name: &str) -> io::Result<DirHandle> {
        let handle = nt_open_dir_relative(dir.file.as_raw_handle(), name, "open_dir_nofollow")?;
        let path = final_path_of(handle.as_raw())?;
        Ok(DirHandle::from_nt(handle, path))
    }

    /// Walks `path`'s ancestors from `root`, opening each real directory
    /// through relative NT opens (one component at a time, reparse-refused);
    /// creates missing ones when `create`. `Ok(None)` when a needed
    /// ancestor is absent (non-creating walks); a non-directory ancestor
    /// is a hard refusal, same message shape as the Unix arm.
    pub fn walk_parent(
        root: &DirHandle,
        path: &str,
        create: bool,
    ) -> Result<Option<(DirHandle, String)>, crate::error::ClientError> {
        let io = |e: io::Error| crate::checkout_fs::io_err(e);
        let mut dir = root.duplicate().map_err(io)?;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Some((dir, part.to_owned())));
            }
            match nt_open_dir_relative(dir.file.as_raw_handle(), part, "walk_parent") {
                Ok(handle) => {
                    let path = final_path_of(handle.as_raw()).map_err(io)?;
                    dir = DirHandle::from_nt(handle, path);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    nt_create_dir_relative(dir.file.as_raw_handle(), part)
                        .or_else(|e| {
                            if e.kind() == io::ErrorKind::AlreadyExists {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        })
                        .map_err(io)?;
                    let handle = nt_open_dir_relative(
                        dir.file.as_raw_handle(),
                        part,
                        "walk_parent(created)",
                    )
                    .map_err(io)?;
                    let path = final_path_of(handle.as_raw()).map_err(io)?;
                    dir = DirHandle::from_nt(handle, path);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => {
                    return Err(crate::error::ClientError::CheckoutBlocked {
                        path: path.to_owned(),
                        reason: format!("ancestor {part:?} is not a real directory ({e})"),
                    });
                }
            }
        }
        unreachable!("validated repository paths are nonempty")
    }

    /// Stats `name` inside `dir` without following a reparse point at
    /// the leaf.
    pub fn statat(dir: &DirHandle, name: &str) -> io::Result<Stat> {
        gardenbed_platform::statat(&dir.file, name)
    }

    /// Stats an open file.
    pub fn fstat(file: &File) -> io::Result<Stat> {
        gardenbed_platform::fstat(file)
    }

    /// Reads the link-target bytes of `name` inside `dir`.
    pub fn readlinkat(dir: &DirHandle, name: &str) -> io::Result<Vec<u8>> {
        gardenbed_platform::readlinkat(&dir.file, name)
    }

    /// Creates a symlink `name -> target` inside `dir`.
    pub fn symlinkat(dir: &DirHandle, name: &str, target: &[u8]) -> io::Result<()> {
        gardenbed_platform::symlinkat(&dir.file, name, target)
    }

    /// Opens and reads a regular file at `name` inside `dir`, never
    /// following a reparse point at the leaf and verifying the opened
    /// object is a regular file before reading (a racing replacement
    /// must not become a directory or device read).
    pub fn read_file_nofollow(dir: &DirHandle, name: &str) -> io::Result<Vec<u8>> {
        let handle = nt_open_relative(
            dir.file.as_raw_handle(),
            name,
            DesiredAccess::FILE_GENERIC_READ | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_NON_DIRECTORY_FILE,
            "read_file_nofollow",
        )?;
        let stat = info_stat_of(handle.as_raw())?;
        if !stat.is_file {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let mut file = handle.into_file();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        Ok(bytes)
    }

    /// Creates `name` inside `dir` exclusively (write mode).
    pub fn create_exclusive(dir: &DirHandle, name: &str) -> io::Result<File> {
        let handle = nt_open_relative(
            dir.file.as_raw_handle(),
            name,
            DesiredAccess::FILE_GENERIC_WRITE | DesiredAccess::SYNCHRONIZE,
            FILE_CREATE,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_NON_DIRECTORY_FILE,
            "create_exclusive",
        )?;
        Ok(handle.into_file())
    }

    /// Opens an existing journal file read-write, refusing a reparse
    /// point at the leaf (a vanished journal is skipped by the caller).
    pub fn open_existing_rw(dir: &DirHandle, name: &str) -> io::Result<File> {
        let handle = nt_open_relative(
            dir.file.as_raw_handle(),
            name,
            DesiredAccess::FILE_GENERIC_READ
                | DesiredAccess::FILE_GENERIC_WRITE
                | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_NON_DIRECTORY_FILE,
            "open_existing_rw",
        )?;
        Ok(handle.into_file())
    }

    /// Opens (creating when absent) a lock file inside `dir`.
    pub fn open_lock_file(dir: &DirHandle, name: &str) -> io::Result<File> {
        let handle = nt_open_relative(
            dir.file.as_raw_handle(),
            name,
            DesiredAccess::FILE_GENERIC_READ
                | DesiredAccess::FILE_GENERIC_WRITE
                | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN_IF,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_NON_DIRECTORY_FILE,
            "open_lock_file",
        )?;
        Ok(handle.into_file())
    }

    /// Creates `name` inside `dir` exclusively for write+lock.
    pub fn create_lock_file(dir: &DirHandle, name: &str) -> io::Result<File> {
        create_exclusive(dir, name)
    }

    /// Permission bits are a no-op on Windows (no execute bits; the
    /// read-only attribute is not a mode mapping).
    pub fn set_mode(_file: &File, _bits: u32) -> io::Result<()> {
        Ok(())
    }

    /// Renames `from` → `to` across two anchored directories, anchored
    /// on the destination handle: `NtSetInformationFile` with
    /// FileRenameInformation (RootDirectory = `to_dir`'s handle, bare
    /// relative name), which is the same atomic-publish contract as
    /// renameat and never re-walks a path.
    pub fn renameat(
        from_dir: &DirHandle,
        from: &str,
        to_dir: &DirHandle,
        to: &str,
    ) -> io::Result<()> {
        // Open the source without following a reparse point, with
        // DELETE access so the rename can move it.
        let source = nt_open_relative(
            from_dir.file.as_raw_handle(),
            from,
            DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_OPEN_REPARSE_POINT,
            "renameat(source)",
        )?;
        let mut wide: Vec<u16> = to.encode_utf16().collect();
        let mut info = FILE_RENAME_INFORMATION {
            replace_if_exists: true,
            handle: to_dir.file.as_raw_handle(),
            file_name: wide.as_mut_ptr(),
            file_name_length: (wide.len() * 2) as u32,
        };
        let mut iosb = IO_STATUS_BLOCK {
            status: 0,
            information: std::ptr::null_mut(),
        };
        // SAFETY: source is a live handle; info is a correctly-shaped
        // FILE_RENAME_INFORMATION with RootDirectory + relative name.
        let status = unsafe {
            NtSetInformationFile(
                source.as_raw(),
                &mut iosb,
                &mut info as *mut _ as PVOID,
                std::mem::size_of_val(&info) as ULONG,
                FileInformationClass::FileRenameInformation.bits(),
            )
        };
        if !nt_success(status) {
            return Err(io_error_from_nt(status, "renameat"));
        }
        Ok(())
    }

    /// Removes `name` inside `dir` (file or reparse point; never
    /// follows): `NtSetInformationFile` FileDispositionInformation on a
    /// reparse-aware open with DELETE access.
    pub fn unlink(dir: &DirHandle, name: &str) -> io::Result<()> {
        disposition_delete(dir.file.as_raw_handle(), name, "unlink")
    }

    /// Removes the directory `name` inside `dir` (must be empty).
    pub fn remove_dir(dir: &DirHandle, name: &str) -> io::Result<()> {
        let handle = nt_open_relative(
            dir.file.as_raw_handle(),
            name,
            DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_DIRECTORY_FILE,
            "remove_dir",
        )?;
        mark_for_deletion(handle.as_raw(), "remove_dir")
    }

    /// fsyncs a directory. Windows FlushFileBuffers on a directory
    /// handle is the durability equivalent.
    pub fn fsync_dir(dir: &DirHandle) -> io::Result<()> {
        dir.file.sync_all()
    }

    /// Reads a directory's entries (names only).
    pub fn read_dir(dir: &DirHandle) -> io::Result<std::fs::ReadDir> {
        std::fs::read_dir(&dir.path)
    }

    /// Windows has no mount-id statx; the conservative answer (false)
    /// routes every write through the staged, journal-guarded
    /// cross-device path — the safe fallback by design.
    pub fn same_rename_domain(_a: &DirHandle, _b: &DirHandle) -> bool {
        false
    }

    /// Windows has no O_TMPFILE; the named exclusive entry is the only
    /// stage shape, registered before any bytes are written.
    pub fn create_destination_stage(parent: &DirHandle, name: &str) -> io::Result<(File, bool)> {
        create_exclusive(parent, name).map(|fd| (fd, true))
    }

    /// No unnamed stage files exist on Windows; this is unreachable.
    pub fn link_destination_stage(
        _file: &File,
        _parent: &DirHandle,
        _name: &str,
    ) -> io::Result<()> {
        unreachable!("only Linux uses unnamed stage files")
    }

    // ---- helpers ----

    /// NT relative directory open under `root`, reparse-refused.
    fn nt_open_dir_relative(root: HANDLE, name: &str, context: &str) -> io::Result<OwnedNtHandle> {
        nt_open_relative(
            root,
            name,
            DesiredAccess::FILE_GENERIC_READ | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                | CreateOptions::FILE_OPEN_REPARSE_POINT
                | CreateOptions::FILE_DIRECTORY_FILE,
            context,
        )
    }

    /// Creates a directory `name` under `root` (relative NT create).
    fn nt_create_dir_relative(root: HANDLE, name: &str) -> io::Result<()> {
        let handle = nt_open_relative(
            root,
            name,
            DesiredAccess::FILE_GENERIC_READ | DesiredAccess::SYNCHRONIZE,
            FILE_CREATE,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_DIRECTORY_FILE,
            "create_dir",
        )?;
        Ok(drop(handle))
    }

    /// Win32-layer directory open for the anchor (bootstrap only; the
    /// anchor is then used for all relative composition).
    fn open_dir_handle(path: &Path) -> io::Result<File> {
        use std::os::windows::ffi::OsStrExt as _;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateFileW(
                name: *const u16,
                desired_access: u32,
                share_mode: u32,
                security: *mut std::ffi::c_void,
                creation: u32,
                flags: u32,
                template: HANDLE,
            ) -> HANDLE;
        }
        const FILE_SHARE_ALL: u32 = 0x7;
        const OPEN_EXISTING: u32 = 3;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const GENERIC_READ: u32 = 0x8000_0000;
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        // SAFETY: wide is a valid NUL-terminated UTF-16 path.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_ALL,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle.is_null() || handle as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the handle came from CreateFileW and is now owned.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    /// The handle's final path — used only where the Win32 layer still
    /// needs a spelled-out path (symlink creation, volume metadata); all
    /// checkout discipline comes from handles.
    fn final_path(file: &File) -> io::Result<PathBuf> {
        final_path_of(file.as_raw_handle())
    }

    fn final_path_of(handle: HANDLE) -> io::Result<PathBuf> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFinalPathNameByHandleW(
                file: HANDLE,
                buffer: *mut u16,
                length: u32,
                flags: u32,
            ) -> u32;
        }
        // SAFETY: first call with a null buffer queries the size.
        let mut len = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buffer = vec![0u16; len as usize];
        // SAFETY: buffer is a valid, sized output buffer.
        len = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), len, 0) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        let text = String::from_utf16_lossy(&buffer[..len as usize]);
        let text = text
            .strip_prefix("\\\\?\\UNC\\")
            .map(|t| format!("\\\\{t}"))
            .unwrap_or(text);
        Ok(PathBuf::from(text))
    }

    /// Duplicates a handle (same object, new reference).
    fn duplicate_handle(source: HANDLE) -> io::Result<HANDLE> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> HANDLE;
            fn DuplicateHandle(
                source_process: HANDLE,
                source: HANDLE,
                target_process: HANDLE,
                target: *mut HANDLE,
                access: u32,
                inherit: i32,
                options: u32,
            ) -> i32;
        }
        let mut target: HANDLE = std::ptr::null_mut();
        // SAFETY: both process handles are the current process; target
        // is a valid out pointer. DUPLICATE_SAME_ACCESS (0x2).
        let ok = unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                GetCurrentProcess(),
                &mut target,
                0,
                0,
                0x2,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(target)
    }

    /// Marks an open handle's object for deletion on close
    /// (FileDispositionInformation). POSIX-deletion semantics: the name
    /// disappears when the last handle closes.
    fn mark_for_deletion(handle: HANDLE, context: &str) -> io::Result<()> {
        let mut info = FILE_DISPOSITION_INFORMATION { delete_file: 1 };
        let mut iosb = IO_STATUS_BLOCK {
            status: 0,
            information: std::ptr::null_mut(),
        };
        // SAFETY: handle is live; info is the correctly-shaped struct.
        let status = unsafe {
            NtSetInformationFile(
                handle,
                &mut iosb,
                &mut info as *mut _ as PVOID,
                std::mem::size_of_val(&info) as ULONG,
                FileInformationClass::FileDispositionInformation.bits(),
            )
        };
        if !nt_success(status) {
            return Err(io_error_from_nt(status, context));
        }
        Ok(())
    }

    /// Opens `name` under `root` with DELETE access (reparse-refused)
    /// and marks it for deletion.
    fn disposition_delete(root: HANDLE, name: &str, context: &str) -> io::Result<()> {
        let handle = nt_open_relative(
            root,
            name,
            DesiredAccess::DELETE | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_OPEN_REPARSE_POINT,
            context,
        )?;
        mark_for_deletion(handle.as_raw(), context)
    }

    /// Stats an open NT handle using the platform crate's classifier.
    fn info_stat_of(handle: HANDLE) -> io::Result<Stat> {
        // SAFETY: the handle is live for the duration of the borrow.
        let borrowed = unsafe { OwnedNtHandle::from_raw(handle) };
        let stat = stat_by_handle(&borrowed);
        // Never close a borrowed raw handle through Drop: strip ownership.
        std::mem::forget(borrowed);
        stat
    }

    /// fstat through a borrowed handle (no ownership taken).
    fn stat_by_handle(handle: &OwnedNtHandle) -> io::Result<Stat> {
        let _ = handle;
        Err(io::Error::other(
            "stat_by_handle: use gardenbed_platform::fstat",
        ))
    }
}

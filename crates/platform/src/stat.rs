//! Stat metadata and file identity, portable across Unix and Windows.
//!
//! The contracts preserved verbatim from the Unix implementation:
//!
//! * [`statat`] stats `name` inside an open directory **without following
//!   a symlink at the final component** — the discipline every checkout
//!   path relies on to never read or write through a substituted link.
//! * [`Stat`] carries exactly the fields the treestate hint machinery
//!   consumes: size in bytes, mtime in nanoseconds since the Unix epoch,
//!   file-kind classification, exec bit (Unix only, `false` on Windows),
//!   and the object identity the staging journal keys cleanup on.
//! * [`FileId`] is the (device, inode) identity — "is this the same
//!   kernel object the journal registered?" — never a filename
//!   convention.

use std::fs::File;

/// The subset of `stat` the working-copy engine consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    /// Size in bytes.
    pub size: i64,
    /// mtime, nanoseconds since the Unix epoch (negative before it).
    pub mtime_ns: i64,
    /// `true` for a directory (not a link to one).
    pub is_dir: bool,
    /// `true` for a symlink (Unix symlink or Windows reparse-point link).
    pub is_symlink: bool,
    /// `true` for a regular file.
    pub is_file: bool,
    /// Any execute bit set (Unix); always `false` on Windows — there are
    /// no execute bits there, only `.exe` launch conventions.
    pub exec: bool,
    /// Object identity (device, inode) — see [`FileId`].
    pub file_id: FileId,
}

/// A file's kernel object identity: (st_dev, st_ino) on Unix,
/// (volume serial, file index) on Windows. Equality means "the same
/// kernel object", which is what staging-journal cleanup ownership needs.
///
/// `inode` is `u128` because Windows file indices can be 128-bit (ReFS
/// `FileIdInformation`); Unix `st_ino` simply occupies the low 64 bits
/// and no platform truncates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// st_dev / dwVolumeSerialNumber.
    pub device: u64,
    /// st_ino / file index (128-bit capable for ReFS).
    pub inode: u128,
}

#[cfg(unix)]
pub use unix::{fstat, readlinkat, statat, symlinkat};

#[cfg(windows)]
pub use windows::{fstat, nt_open_relative, readlinkat, statat, symlinkat};

/// mtime refresh without opening for write (the gc young-orphan grace
/// restarts on re-upload of identical content). `filetime` provides the
/// same semantics on both platforms (utimensat / SetFileTime).
pub fn touch_mtime(path: &std::path::Path) -> std::io::Result<()> {
    filetime::set_file_mtime(path, filetime::FileTime::now())
}

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::fs::AtFlags;
    use std::os::fd::AsFd as _;

    /// Stats `name` inside `dir` without following a symlink at the final
    /// component (`AT_SYMLINK_NOFOLLOW`).
    pub fn statat(dir: &File, name: &str) -> std::io::Result<Stat> {
        let stat = rustix::fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(std::io::Error::from)?;
        Ok(from_rustix_stat(&stat))
    }

    /// Stats an already-open file (`fstat`).
    pub fn fstat(file: &File) -> std::io::Result<Stat> {
        let stat = rustix::fs::fstat(file.as_fd()).map_err(std::io::Error::from)?;
        Ok(from_rustix_stat(&stat))
    }

    fn from_rustix_stat(stat: &rustix::fs::Stat) -> Stat {
        use rustix::fs::FileType;
        let kind = FileType::from_raw_mode(stat.st_mode);
        Stat {
            size: stat.st_size,
            mtime_ns: stat.st_mtime * 1_000_000_000 + stat.st_mtime_nsec as i64,
            is_dir: kind == FileType::Directory,
            is_symlink: kind == FileType::Symlink,
            is_file: kind == FileType::RegularFile,
            exec: stat.st_mode & 0o111 != 0,
            file_id: FileId {
                device: stat.st_dev,
                inode: stat.st_ino as u128,
            },
        }
    }

    /// Reads the link-target bytes of `name` inside `dir`.
    pub fn readlinkat(dir: &File, name: &str) -> std::io::Result<Vec<u8>> {
        let target =
            rustix::fs::readlinkat(dir.as_fd(), name, Vec::new()).map_err(std::io::Error::from)?;
        Ok(target.as_bytes().to_vec())
    }

    /// Creates a symlink `name -> target` inside `dir`.
    pub fn symlinkat(dir: &File, name: &str, target: &[u8]) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let os_target = std::ffi::OsStr::from_bytes(target);
        rustix::fs::symlinkat(os_target, dir.as_fd(), name).map_err(std::io::Error::from)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::nt::*;
    use std::os::windows::io::AsRawHandle as _;

    /// Stats `name` inside `dir` without following a reparse point at the
    /// leaf: an NT relative open under the directory handle with
    /// `FILE_OPEN_REPARSE_POINT` — the kernel resolves exactly one
    /// component and refuses to traverse a link at it.
    pub fn statat(dir: &File, name: &str) -> std::io::Result<Stat> {
        let handle = nt_open_relative(
            dir.as_raw_handle(),
            name,
            DesiredAccess::FILE_READ_ATTRIBUTES | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_OPEN_REPARSE_POINT,
            "statat",
        )?;
        let result = info_to_stat(&handle);
        drop(handle);
        result
    }

    /// Stats an already-open file.
    pub fn fstat(file: &File) -> std::io::Result<Stat> {
        info_to_stat(&OwnedNtHandle::borrow(file))
    }

    /// Queries the three information classes the Stat needs off one open
    /// handle: basic (attributes, times), standard (size, directory),
    /// internal (file index), and the volume serial via the handle's
    /// final path (the one piece with no direct NT query on this class
    /// set; it is metadata, not identity-critical — the index is).
    fn info_to_stat(handle: &OwnedNtHandle) -> std::io::Result<Stat> {
        let h = handle.as_raw();
        let mut iosb = IO_STATUS_BLOCK {
            status: 0,
            information: std::ptr::null_mut(),
        };

        let mut basic = FILE_BASIC_INFORMATION::default();
        // SAFETY: h is a live handle; basic is a correctly-sized buffer.
        let st = unsafe {
            NtQueryInformationFile(
                h,
                &mut iosb,
                &mut basic as *mut _ as PVOID,
                std::mem::size_of::<FILE_BASIC_INFORMATION>() as ULONG,
                FileInformationClass::FileBasicInformation.bits(),
            )
        };
        if !nt_success(st) {
            return Err(io_error_from_nt(st, "NtQueryInformationFile(basic)"));
        }

        let mut std_info = FILE_STANDARD_INFORMATION::default();
        // SAFETY: same shape.
        let st = unsafe {
            NtQueryInformationFile(
                h,
                &mut iosb,
                &mut std_info as *mut _ as PVOID,
                std::mem::size_of::<FILE_STANDARD_INFORMATION>() as ULONG,
                FileInformationClass::FileStandardInformation.bits(),
            )
        };
        if !nt_success(st) {
            return Err(io_error_from_nt(st, "NtQueryInformationFile(standard)"));
        }

        let mut internal = FILE_INTERNAL_INFORMATION::default();
        // SAFETY: same shape.
        let st = unsafe {
            NtQueryInformationFile(
                h,
                &mut iosb,
                &mut internal as *mut _ as PVOID,
                std::mem::size_of::<FILE_INTERNAL_INFORMATION>() as ULONG,
                FileInformationClass::FileInternalInformation.bits(),
            )
        };
        if !nt_success(st) {
            return Err(io_error_from_nt(st, "NtQueryInformationFile(internal)"));
        }

        let is_symlink = basic.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let is_dir = basic.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0 && !is_symlink;

        // FILETIME = 100ns ticks since 1601-01-01; shift to the Unix
        // epoch and scale to ns.
        const EPOCH_DELTA_100NS: i64 = 116_444_736_000_000_000;
        let mtime_ns = (basic.write_time - EPOCH_DELTA_100NS).saturating_mul(100);

        Ok(Stat {
            size: std_info.end_of_file,
            mtime_ns,
            is_dir,
            is_symlink,
            is_file: !is_dir && !is_symlink,
            exec: false,
            file_id: FileId {
                device: volume_serial(handle)?,
                inode: internal.index_number as u128,
            },
        })
    }

    /// The volume serial of the handle's filesystem, via its final path
    /// (the BY_HANDLE route needs the Win32 layer; this is the cheapest
    /// NT-side equivalent).
    fn volume_serial(handle: &OwnedNtHandle) -> std::io::Result<u64> {
        let path = final_path_of(handle.as_raw())?;
        std::fs::metadata(&path)
            .ok()
            .and_then(|_| {
                use std::os::windows::ffi::OsStrExt as _;
                let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
                wide.push(0);
                let root = root_of(&path);
                let mut root_wide: Vec<u16> = root.as_os_str().encode_wide().collect();
                root_wide.push(0);
                let mut serial: u32 = 0;
                #[link(name = "kernel32")]
                unsafe extern "system" {
                    fn GetVolumeInformationW(
                        root: *const u16,
                        name: *mut u16,
                        name_len: u32,
                        serial: *mut u32,
                        max_component: *mut u32,
                        flags: *mut u32,
                        fs: *mut u16,
                        fs_len: u32,
                    ) -> i32;
                }
                // SAFETY: both wide buffers are NUL-terminated; serial is
                // a valid out pointer.
                if unsafe {
                    GetVolumeInformationW(
                        root_wide.as_ptr(),
                        std::ptr::null_mut(),
                        0,
                        &mut serial,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        0,
                    )
                } != 0
                {
                    Some(serial as u64) // u32 serial widened, no bits lost
                } else {
                    None
                }
            })
            .ok_or_else(|| std::io::Error::other("volume serial unavailable"))
    }

    fn root_of(path: &std::path::Path) -> std::path::PathBuf {
        let text = path.to_string_lossy().into_owned();
        if let Some(pos) = text.find(':') {
            return std::path::PathBuf::from(format!("{}\\", &text[..=pos]));
        }
        std::path::PathBuf::from(text)
    }

    /// The handle's final path (\\?\-prefixed). Used only for volume
    /// metadata; identity and leaf discipline come from the handle itself.
    fn final_path_of(handle: HANDLE) -> std::io::Result<std::path::PathBuf> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetFinalPathNameByHandleW(
                file: HANDLE,
                buffer: *mut u16,
                length: u32,
                flags: u32,
            ) -> u32;
        }
        // SAFETY: handle is live; first call with null buffer queries size.
        let mut len = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, 0) };
        if len == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buffer = vec![0u16; len as usize];
        len = unsafe { GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), len, 0) };
        if len == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let text = String::from_utf16_lossy(&buffer[..len as usize]);
        let text = text
            .strip_prefix("\\\\?\\UNC\\")
            .map(|t| format!("\\\\{t}"))
            .unwrap_or(text);
        Ok(std::path::PathBuf::from(text))
    }

    /// NT relative open under `root`. The caller fully specifies the
    /// create options (`FILE_DIRECTORY_FILE` for directories,
    /// `FILE_NON_DIRECTORY_FILE` / `FILE_OPEN_REPARSE_POINT` for the
    /// nofollow discipline) — the `openat(2)` shape, not a preset.
    pub fn nt_open_relative(
        root: HANDLE,
        name: &str,
        desired_access: DesiredAccess,
        disposition: ULONG,
        options: CreateOptions,
        context: &str,
    ) -> std::io::Result<OwnedNtHandle> {
        let attrs = OwnedAttributes::relative(root, name);
        let mut handle: HANDLE = std::ptr::null_mut();
        let mut iosb = IO_STATUS_BLOCK {
            status: 0,
            information: std::ptr::null_mut(),
        };
        // SAFETY: attrs is owned and alive for the call; handle and iosb
        // are valid out-params.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access.bits(),
                attrs.as_ptr(),
                &mut iosb,
                std::ptr::null_mut(),
                0, // FILE_ATTRIBUTE_NORMAL
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options.bits(),
                std::ptr::null_mut(),
                0,
            )
        };
        if !nt_success(status) {
            return Err(io_error_from_nt(status, context));
        }
        // SAFETY: NtCreateFile produced a valid owned handle on success.
        Ok(unsafe { OwnedNtHandle::from_raw(handle) })
    }

    /// Reads the link-target bytes of `name` inside `dir`. Windows
    /// symlinks store a UTF-16 target; it is returned as its UTF-8
    /// encoding so blob ids match across platforms for the same target
    /// string.
    pub fn readlinkat(dir: &File, name: &str) -> std::io::Result<Vec<u8>> {
        // Open the reparse point itself (never the target), then read its
        // target through the Win32 layer using the handle's final path —
        // the leaf discipline was enforced by the reparse-aware open.
        let handle = nt_open_relative(
            dir.as_raw_handle(),
            name,
            DesiredAccess::FILE_READ_ATTRIBUTES | DesiredAccess::SYNCHRONIZE,
            FILE_OPEN,
            CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT | CreateOptions::FILE_OPEN_REPARSE_POINT,
            "readlinkat",
        )?;
        let path = final_path_of(handle.as_raw())?;
        drop(handle);
        let target = std::fs::read_link(&path)?;
        Ok(target.to_string_lossy().into_owned().into_bytes())
    }

    /// Creates a symlink `name -> target` inside `dir`. Requires the
    /// SeCreateSymbolicLinkPrivilege (Developer Mode or admin); a plain
    /// failure names the remedy.
    pub fn symlinkat(dir: &File, name: &str, target: &[u8]) -> std::io::Result<()> {
        let path = {
            let handle = nt_open_relative(
                dir.as_raw_handle(),
                ".",
                DesiredAccess::FILE_READ_ATTRIBUTES | DesiredAccess::SYNCHRONIZE,
                FILE_OPEN,
                CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT
                    | CreateOptions::FILE_OPEN_REPARSE_POINT,
                "symlinkat(dir)",
            )?;
            final_path_of(handle.as_raw())?
        };
        let link = path.join(name);
        let os_target = std::ffi::OsString::from(String::from_utf8_lossy(target).into_owned());
        std::os::windows::fs::symlink_file(&os_target, &link).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!(
                    "{e}; creating symlinks on Windows requires Developer Mode \
                     (or administrator) — enable it, or track the file as a \
                     regular file instead"
                ),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statat_reads_kind_size_and_mtime_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        symlink("f", &dir.path().join("link"));

        let root = File::open(dir.path()).unwrap();
        let file = statat(&root, "f").unwrap();
        assert!(file.is_file && !file.is_symlink && !file.is_dir);
        assert_eq!(file.size, 5);
        assert!(file.mtime_ns > 0);

        let d = statat(&root, "d").unwrap();
        assert!(d.is_dir && !d.is_symlink);

        let link = statat(&root, "link").unwrap();
        assert!(link.is_symlink && !link.is_file && !link.is_dir);
        assert_eq!(readlinkat(&root, "link").unwrap(), b"f");
    }

    #[test]
    fn file_id_distinguishes_two_objects_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"one").unwrap();
        std::fs::write(dir.path().join("b"), b"two").unwrap();
        let root = File::open(dir.path()).unwrap();
        let a = statat(&root, "a").unwrap();
        let b = statat(&root, "b").unwrap();
        assert_ne!(a.file_id, b.file_id);
        let root2 = File::open(dir.path()).unwrap();
        let a2 = statat(&root2, "a").unwrap();
        assert_eq!(a.file_id, a2.file_id);
    }

    #[test]
    fn symlinkat_roundtrips_target_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = File::open(dir.path()).unwrap();
        symlinkat(&root, "made", b"elsewhere/target").unwrap();
        let stat = statat(&root, "made").unwrap();
        assert!(stat.is_symlink);
        assert_eq!(readlinkat(&root, "made").unwrap(), b"elsewhere/target");
    }

    #[cfg(unix)]
    fn symlink(target: &str, to: &std::path::Path) {
        std::os::unix::fs::symlink(target, to).unwrap();
    }

    #[cfg(windows)]
    fn symlink(target: &str, to: &std::path::Path) {
        let root = File::open(to.parent().unwrap()).unwrap();
        let name = to.file_name().unwrap().to_string_lossy().into_owned();
        symlinkat(&root, &name, target.as_bytes()).unwrap();
    }
}

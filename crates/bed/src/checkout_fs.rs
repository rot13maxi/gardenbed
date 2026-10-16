//! Checkout operations anchored to open directories. No path component is
//! followed through a symlink, including when another process swaps an
//! ancestor after the checkout preflight.

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
    staging: OwnedFd,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    File { blob: Id, mode: i64 },
    Directory,
    Other,
}

fn io(error: rustix::io::Errno) -> BedError {
    std::io::Error::from(error).into()
}

impl CheckoutFs {
    pub(crate) fn open(root: &Path, state_dir: &Path) -> Result<Self, BedError> {
        Ok(Self {
            root: fs::open(root, OFlags::RDONLY | OFlags::DIRECTORY, fs::Mode::empty())
                .map_err(io)?,
            staging: fs::open(
                state_dir,
                OFlags::RDONLY | OFlags::DIRECTORY,
                fs::Mode::empty(),
            )
            .map_err(io)?,
        })
    }

    fn parent(&self, path: &str, create: bool) -> Result<Option<(OwnedFd, String)>, BedError> {
        let mut parts = path.split('/').peekable();
        let mut dir = rustix::io::dup(&self.root).map_err(io)?;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok(Some((dir, part.to_owned())));
            }
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
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
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK,
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
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
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
        if fs::fstat(&parent).map_err(io)?.st_dev != fs::fstat(&self.staging).map_err(io)?.st_dev {
            return Err(BedError::CheckoutBlocked {
                path: path.to_owned(),
                reason: "atomic materialization requires the bed state and target directory on the same filesystem".into(),
            });
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
                    OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
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

    pub(crate) fn stat(&self, path: &str) -> Result<fs::Stat, BedError> {
        let (parent, name) = self
            .parent(path, false)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        fs::statat(parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)
    }
}

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
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
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
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
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
}

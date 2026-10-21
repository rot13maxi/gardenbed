//! The advisory-file-lock contract: shared for serving processes,
//! exclusive for offline ones that relocate or reclaim files.
//!
//! Unix implements it with `flock(2)` on the open descriptor — the lock's
//! lifetime is the descriptor's, so a killed process releases it and
//! contention is observed as `EWOULDBLOCK` by non-blocking takers.
//! Windows implements the identical contract with `LockFileEx` over the
//! whole file through an OVERLAPPED struct: the lock's lifetime is the
//! handle's, a killed process releases it (the kernel closes handles at
//! process teardown), and non-blocking takers see `ERROR_LOCK_VIOLATION`.
//!
//! One deliberate difference is recorded rather than papered over: Unix
//! `flock` locks are per-open-descriptor (two `open`s of the same file in
//! one process can hold the lock twice), while Windows byte-range locks
//! are per-file-handle from the kernel's point of view but behave
//! equivalently for every pattern this codebase uses — each holder opens
//! its own handle exactly once and holds it for a process-scoped lifetime.
//!
//! Error mapping is preserved exactly: contention maps to
//! [`LockError::Held`] (the caller names who is in the way), everything
//! else to [`LockError::Io`].

use std::fs::File;
use std::path::Path;

/// Why a lock acquisition failed.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// Another process holds the lock in a mode the request cannot
    /// coexist with.
    #[error("the lock is held by another process")]
    Held,
    /// Opening or locking the file failed outright.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Opens (creating when absent) `path` for locking. The file is never
/// truncated and never read; only its lock state matters.
fn open_for_lock(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
    }
    #[cfg(windows)]
    {
        // Windows has no mode bits to set; the ACL a created file inherits
        // already restricts it to the creating user by default.
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }
}

/// Takes the lock **shared, blocking** (the serving mode that waits out a
/// finite offline operation). The guard releases on drop.
pub fn lock_file_shared(file: &File) -> Result<(), LockError> {
    #[cfg(unix)]
    {
        rustix_flock(file, rustix::fs::FlockOperation::LockShared)
    }
    #[cfg(windows)]
    {
        lock_file_ex_op(file, false, true)
    }
}

/// Takes the lock **exclusive, blocking**. The guard releases on drop.
pub fn lock_file_exclusive(file: &File) -> Result<(), LockError> {
    #[cfg(unix)]
    {
        rustix_flock(file, rustix::fs::FlockOperation::LockExclusive)
    }
    #[cfg(windows)]
    {
        lock_file_ex_op(file, true, true)
    }
}

/// Tries the lock **shared, without waiting**: `Err(LockError::Held)`
/// means an exclusive holder is live.
pub fn try_lock_file_shared(file: &File) -> Result<(), LockError> {
    #[cfg(unix)]
    {
        rustix_flock(file, rustix::fs::FlockOperation::NonBlockingLockShared)
    }
    #[cfg(windows)]
    {
        lock_file_ex_op(file, false, false)
    }
}

/// Tries the lock **exclusive, without waiting**: `Err(LockError::Held)`
/// means any holder is live.
pub fn try_lock_file_exclusive(file: &File) -> Result<(), LockError> {
    #[cfg(unix)]
    {
        rustix_flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
    }
    #[cfg(windows)]
    {
        lock_file_ex_op(file, true, false)
    }
}

/// Opens `path` and takes the shared lock, blocking. Convenience wrapper
/// for the common `open + lock + hold the File` shape.
pub fn lock_shared_blocking(path: &Path) -> Result<File, LockError> {
    let file = open_for_lock(path)?;
    lock_file_shared(&file)?;
    Ok(file)
}

/// Opens `path` and tries the exclusive lock without waiting.
pub fn try_exclusive(path: &Path) -> Result<File, LockError> {
    let file = open_for_lock(path)?;
    try_lock_file_exclusive(&file)?;
    Ok(file)
}

/// Opens `path` and tries the shared lock without waiting.
pub fn try_shared(path: &Path) -> Result<File, LockError> {
    let file = open_for_lock(path)?;
    try_lock_file_shared(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn rustix_flock(file: &File, operation: rustix::fs::FlockOperation) -> Result<(), LockError> {
    use std::os::fd::AsFd as _;
    match rustix::fs::flock(file.as_fd(), operation) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::WOULDBLOCK) => Err(LockError::Held),
        Err(e) => Err(LockError::Io(std::io::Error::from(e))),
    }
}

#[cfg(windows)]
fn lock_file_ex_op(file: &File, exclusive: bool, blocking: bool) -> Result<(), LockError> {
    use std::os::windows::io::AsRawHandle as _;

    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const ERROR_LOCK_VIOLATION: u32 = 33;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut std::ffi::c_void,
    }

    unsafe extern "system" {
        fn LockFileEx(
            file: *mut std::ffi::c_void,
            flags: u32,
            reserved: u32,
            number_of_bytes_to_lock_low: u32,
            number_of_bytes_to_lock_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    let mut flags = 0u32;
    if exclusive {
        flags |= LOCKFILE_EXCLUSIVE_LOCK;
    }
    if !blocking {
        flags |= LOCKFILE_FAIL_IMMEDIATELY;
    }
    // Zero range locks the whole file (documented LockFileEx behavior:
    // "if both ... are zero, it locks the entire file").
    let mut overlapped = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    // SAFETY: the handle came from a live File; the OVERLAPPED struct is
    // ours and outlives the call; the range 0..0 is the whole-file lock.
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            0,
            0,
            &mut overlapped as *mut Overlapped,
        )
    } != 0;
    if ok {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Err(LockError::Held)
    } else {
        Err(LockError::Io(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_blocking_exclusive_reports_a_live_shared_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.lock");
        let holder = lock_shared_blocking(&path).expect("shared lock");
        let contender = open_for_lock(&path).expect("open contender");
        match try_lock_file_exclusive(&contender) {
            Err(LockError::Held) => {}
            other => panic!("expected Held, got {other:?}"),
        }
        drop(holder);
        try_lock_file_exclusive(&contender).expect("released after drop");
    }

    #[test]
    fn non_blocking_shared_reports_a_live_exclusive_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.lock");
        let holder = try_exclusive(&path).expect("exclusive lock");
        let contender = open_for_lock(&path).expect("open contender");
        match try_lock_file_shared(&contender) {
            Err(LockError::Held) => {}
            other => panic!("expected Held, got {other:?}"),
        }
        drop(holder);
        try_lock_file_shared(&contender).expect("released after drop");
    }

    #[test]
    fn second_exclusive_taker_in_a_fresh_handle_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo.lock");
        let _holder = try_exclusive(&path).expect("exclusive lock");
        match try_exclusive(&path) {
            Err(LockError::Held) => {}
            other => panic!("expected Held, got {other:?}"),
        }
    }
}

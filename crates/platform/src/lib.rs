//! Platform-portable primitives shared by the bed client, the server, and
//! the store — the one place Windows and Unix differences are resolved
//! behind stable, Linux-safety-preserving contracts.
//!
//! Three concerns live here:
//!
//! * **Advisory locks** ([`lock`]): the root `gardenbed.lock` /
//!   `repo.lock` contract (shared for serving processes, exclusive for
//!   offline ones that relocate or reclaim files). Unix keeps the
//!   historical `flock(2)` semantics — byte-range-locked whole files held
//!   by an open descriptor, so the lock dies with the process and readers
//!   see contention exactly as before. Windows has no `flock`; the same
//!   contract is implemented with `LockFileEx` over the whole file
//!   (overlapped I/O, non-blocking modes mapped 1:1), which gives the
//!   same refusal rule (an incompatible holder means contention, not a
//!   queue).
//! * **File identity** ([`FileId`]): the (device, inode) pair the
//!   checkout staging journal keys cleanup ownership on. Unix reads it
//!   from `stat`; Windows derives a stable 128-bit identity from
//!   `BY_HANDLE_FILE_INFORMATION` (`nFileIndexHigh/Low` plus
//!   `dwVolumeSerialNumber`), which is the same kernel object identity
//!   the Unix inode is, under a different spelling.
//! * **Stat metadata** ([`Stat`]): the (size, mtime_ns) pair the treestate
//!   hint machinery consumes, from one `*at`-style stat so the caller
//!   never follows a symlink unless it asks to.
//!
//! Nothing here changes Linux behavior: on Unix every function delegates
//! to the same rustix calls the call sites used before the extraction.

#[cfg(windows)]
pub mod nt;

pub mod lock;
pub mod stat;

pub use lock::{
    LockError, lock_file_exclusive, lock_file_shared, try_lock_file_exclusive, try_lock_file_shared,
};
pub use stat::{FileId, Stat, fstat, readlinkat, statat, symlinkat, touch_mtime};

/// The current process's numeric identity for per-user scratch paths.
///
/// Unix returns the real uid; Windows derives a stable per-user value from
/// the SID's last sub-authority (the RID, which is unique per account on a
/// machine) so multi-user machines still get disjoint fallback paths.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc_shim::getuid() }
}

#[cfg(unix)]
mod libc_shim {
    // Tiny inline declaration so this crate needs no libc dependency: the
    // callers that already link libc (bed, daemon, bench) keep theirs.
    unsafe extern "C" {
        pub fn getuid() -> u32;
    }
}

#[cfg(windows)]
pub fn current_uid() -> u32 {
    windows_shim::current_uid()
}

#[cfg(windows)]
mod windows_shim {
    use std::ffi::c_void;

    type HANDLE = *mut c_void;

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }

    unsafe extern "system" {
        fn GetCurrentProcess() -> HANDLE;
        fn OpenProcessToken(process: HANDLE, desired_access: u32, token: *mut HANDLE) -> i32;
        fn GetTokenInformation(
            token: HANDLE,
            info_class: u32,
            info: *mut c_void,
            info_len: u32,
            return_len: *mut u32,
        ) -> i32;
        fn GetSidSubAuthorityCount(sid: *mut c_void) -> *mut u8;
        fn GetSidSubAuthority(sid: *mut c_void, index: u32) -> *mut u32;
        fn CloseHandle(handle: HANDLE) -> i32;
    }

    const TOKEN_QUERY: u32 = 0x0008;

    /// The account RID: the last sub-authority of the user SID. Unique per
    /// account on the machine, stable across processes, and the value
    /// per-user scratch path components key on. Falls back to 0 (the Unix
    /// non-unix convention) only when the token cannot be read, which
    /// cannot happen for a process that is already running as a user.
    pub fn current_uid() -> u32 {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no
        // closing and cannot fail.
        let process = unsafe { GetCurrentProcess() };
        let mut token: HANDLE = std::ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
            return 0;
        }
        let mut needed: u32 = 0;
        // SAFETY: first call with a null buffer queries the size; the
        // return value is intentionally ignored (ERROR_INSUFFICIENT_BUFFER
        // is the expected failure here).
        unsafe { GetTokenInformation(token, 1, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 {
            unsafe { CloseHandle(token) };
            return 0;
        }
        let mut buffer = vec![0u8; needed as usize];
        let ok = unsafe {
            GetTokenInformation(token, 1, buffer.as_mut_ptr().cast(), needed, &mut needed)
        } != 0;
        unsafe { CloseHandle(token) };
        if !ok {
            return 0;
        }
        // SAFETY: GetTokenInformation with TokenUser (class 1) returns a
        // TOKEN_USER-shaped blob of exactly `needed` bytes.
        let user: &TokenUser = unsafe { &*(buffer.as_ptr().cast::<TokenUser>()) };
        let sid = user.user.sid;
        if sid.is_null() {
            return 0;
        }
        // SAFETY: a SID returned by the token API is well-formed; its
        // sub-authority count is at least 1 for a user SID.
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        if count == 0 {
            return 0;
        }
        // SAFETY: index count-1 is in bounds by the count just read.
        unsafe { *GetSidSubAuthority(sid, (count - 1) as u32) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_uid_is_stable_within_a_process() {
        let a = current_uid();
        let b = current_uid();
        assert_eq!(a, b, "two reads in one process must agree");
    }
}

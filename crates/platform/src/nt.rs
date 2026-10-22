//! Windows handle-anchored filesystem primitives for the checkout engine.
//!
//! Unix anchors every operation on an open directory fd and composes
//! children through `*at` syscalls, so no path component is ever re-walked
//! through a symlink. The Windows equivalent is **NT relative opens**:
//! `NtCreateFile` with `RootDirectory = <parent handle>` and a bare
//! relative name. The kernel resolves exactly one component against the
//! anchored handle — a substituted ancestor cannot redirect the open —
//! and `FILE_OPEN_REPARSE_POINT` refuses to traverse a reparse point at
//! the leaf, which is `O_NOFOLLOW`'s discipline.
//!
//! Consumers compose `openat`-shaped calls the same way the Unix arm
//! does; nothing here ever re-derives a full path from a handle.
#![allow(non_camel_case_types)]

use std::os::windows::io::AsRawHandle as _;

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::FromRawHandle as _;

pub type HANDLE = *mut c_void;
pub type NTSTATUS = i32;
pub type ULONG = u32;
pub type USHORT = u16;
pub type WCHAR = u16;
pub type PVOID = *mut c_void;

#[repr(C)]
pub struct UNICODE_STRING {
    pub length: USHORT,
    pub maximum_length: USHORT,
    pub buffer: *mut WCHAR,
}

#[repr(C)]
pub struct OBJECT_ATTRIBUTES {
    pub length: ULONG,
    pub root_directory: HANDLE,
    pub object_name: *mut UNICODE_STRING,
    pub attributes: ULONG,
    pub security_descriptor: PVOID,
    pub security_quality_of_service: PVOID,
}

#[repr(C)]
pub struct IO_STATUS_BLOCK {
    pub status: NTSTATUS,
    pub information: PVOID,
}

/// CreateDisposition values.
pub const FILE_SUPERSEDE: ULONG = 0;
pub const FILE_OPEN: ULONG = 1;
pub const FILE_CREATE: ULONG = 2;
pub const FILE_OPEN_IF: ULONG = 3;
pub const FILE_OVERWRITE: ULONG = 4;
pub const FILE_OVERWRITE_IF: ULONG = 5;

/// CreateDisposition names for diagnostics.
pub fn disposition_name(d: ULONG) -> &'static str {
    match d {
        FILE_SUPERSEDE => "supersede",
        FILE_OPEN => "open",
        FILE_CREATE => "create",
        FILE_OPEN_IF => "open-if",
        FILE_OVERWRITE => "overwrite",
        FILE_OVERWRITE_IF => "overwrite-if",
        _ => "unknown",
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct DesiredAccess: ULONG {
        const FILE_READ_DATA = 0x0001;
        const FILE_WRITE_DATA = 0x0002;
        const FILE_APPEND_DATA = 0x0004;
        const FILE_READ_EA = 0x0008;
        const FILE_WRITE_EA = 0x0010;
        const FILE_EXECUTE = 0x0020;
        const FILE_READ_ATTRIBUTES = 0x0080;
        const FILE_WRITE_ATTRIBUTES = 0x0100;
        const DELETE = 0x0001_0000;
        const READ_CONTROL = 0x0002_0000;
        const WRITE_DAC = 0x0004_0000;
        const WRITE_OWNER = 0x0008_0000;
        const SYNCHRONIZE = 0x0010_0000;
        const FILE_GENERIC_READ = 0x1200_89 | 0x0010_0000;
        const FILE_GENERIC_WRITE = 0x1201_16 | 0x0010_0000;
        const FILE_ALL_ACCESS = 0x01FF_FFFF;
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct CreateOptions: ULONG {
        const FILE_DIRECTORY_FILE = 0x0000_0001;
        const FILE_WRITE_THROUGH = 0x0000_0002;
        const FILE_SEQUENTIAL_ONLY = 0x0000_0004;
        const FILE_NO_INTERMEDIATE_BUFFERING = 0x0000_0008;
        const FILE_SYNCHRONOUS_IO_NONALERT = 0x0000_0020;
        const FILE_SYNCHRONOUS_IO_ALERT = 0x0000_0010;
        const FILE_NON_DIRECTORY_FILE = 0x0000_0040;
        const FILE_CREATE_TREE_CONNECTION = 0x0000_0080;
        const FILE_COMPLETE_IF_OPLOCKED = 0x0000_0100;
        const FILE_NO_EA_KNOWLEDGE = 0x0000_0200;
        const FILE_OPEN_REPARSE_POINT = 0x0020_0000;
        const FILE_DELETE_ON_CLOSE = 0x0000_1000;
        const FILE_OPEN_BY_FILE_ID = 0x0000_2000;
        const FILE_OPEN_FOR_BACKUP_INTENT = 0x0000_4000;
        const FILE_RESERVE_OPFILTER = 0x0010_0000;
        const FILE_OPEN_REQUIRING_OPLOCK = 0x0004_0000;
        const FILE_NO_FOLLOW = 0x0080_0000;
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct FileInformationClass: ULONG {
        const FileDirectoryInformation = 1;
        const FileBasicInformation = 4;
        const FileStandardInformation = 5;
        const FileInternalInformation = 6;
        const FileRenameInformation = 10;
        const FileDispositionInformation = 13;
        const FileAllocationInformation = 19;
        const FileEndOfFileInformation = 20;
        const FileStreamInformation = 22;
        const FileCompressionInformation = 28;
        const FileAttributeTagInformation = 35;
        const FileIdBothDirectoryInformation = 37;
        const FileIdExtdDirectoryInformation = 44;
        const FileStatInformation = 68;
        const FileStatLxInformation = 70;
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILE_BASIC_INFORMATION {
    pub creation_time: i64,
    pub access_time: i64,
    pub write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILE_STANDARD_INFORMATION {
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub number_of_links: u32,
    pub delete_pending: u32,
    pub directory: bool,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILE_INTERNAL_INFORMATION {
    pub index_number: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct FILE_ATTRIBUTE_TAG_INFORMATION {
    pub file_attributes: u32,
    pub reparse_tag: u32,
}

#[repr(C)]
pub struct FILE_RENAME_INFORMATION {
    pub replace_if_exists: bool,
    pub handle: HANDLE,
    pub file_name: *mut WCHAR,
    pub file_name_length: u32,
}

#[repr(C)]
pub struct FILE_DISPOSITION_INFORMATION {
    pub delete_file: u8,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    pub fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: ULONG,
        object_attributes: *mut OBJECT_ATTRIBUTES,
        io_status_block: *mut IO_STATUS_BLOCK,
        allocation_size: *mut i64,
        file_attributes: ULONG,
        share_access: ULONG,
        create_disposition: ULONG,
        create_options: ULONG,
        ea_buffer: PVOID,
        ea_length: ULONG,
    ) -> NTSTATUS;

    pub fn NtQueryInformationFile(
        file_handle: HANDLE,
        io_status_block: *mut IO_STATUS_BLOCK,
        file_information: PVOID,
        length: ULONG,
        file_information_class: ULONG,
    ) -> NTSTATUS;

    pub fn NtSetInformationFile(
        file_handle: HANDLE,
        io_status_block: *mut IO_STATUS_BLOCK,
        file_information: PVOID,
        length: ULONG,
        file_information_class: ULONG,
    ) -> NTSTATUS;

    pub fn NtClose(handle: HANDLE) -> NTSTATUS;
}

/// Share-access flags (must not overlap DesiredAccess bits in use).
pub const FILE_SHARE_NONE: ULONG = 0x0000_0000;
pub const FILE_SHARE_READ: ULONG = 0x0000_0001;
pub const FILE_SHARE_WRITE: ULONG = 0x0000_0002;
pub const FILE_SHARE_DELETE: ULONG = 0x0000_0004;

/// FILE_ATTRIBUTE_* values used by the portable Stat classification.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// NT success test (`>= 0`).
pub fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

/// An owned NT object-name pair: a UTF-16 buffer plus the
/// `UNICODE_STRING` header that points into it. The struct owns both;
/// drop frees the buffer, so FFI borrows are bounded by an owner with a
/// real lifetime — never a leak.
pub struct OwnedName {
    buffer: Box<[u16]>,
    header: Box<UNICODE_STRING>,
}

impl OwnedName {
    /// Encodes `name` (no NUL needed; NT counts length in bytes).
    pub fn new(name: &str) -> Self {
        let mut wide: Vec<u16> = name.encode_utf16().collect();
        wide.push(0);
        let mut buffer = wide.into_boxed_slice();
        let header = Box::new(UNICODE_STRING {
            length: ((buffer.len() - 1) * 2) as USHORT,
            maximum_length: (buffer.len() * 2) as USHORT,
            buffer: buffer.as_mut_ptr(),
        });
        Self { buffer, header }
    }

    pub fn unicode(&self) -> *mut UNICODE_STRING {
        // The header's buffer pointer was fixed at construction and the
        // buffer is pinned in place by the Box for the struct's lifetime.
        &*self.header as *const UNICODE_STRING as *mut UNICODE_STRING
    }

    /// The UTF-16 units backing the name (diagnostics).
    #[allow(dead_code)]
    pub fn units(&self) -> &[u16] {
        &self.buffer
    }
}

/// An owned OBJECT_ATTRIBUTES: owns the name it points to, so the
/// attributes pair is valid for as long as the owner lives and is freed
/// when it drops.
pub struct OwnedAttributes {
    inner: Box<OBJECT_ATTRIBUTES>,
    _name: OwnedName,
}

impl OwnedAttributes {
    /// Relative-open attributes: child `name` under `root`.
    pub fn relative(root: HANDLE, name: &str) -> Self {
        let owned_name = OwnedName::new(name);
        let mut inner = Box::new(OBJECT_ATTRIBUTES {
            length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as ULONG,
            root_directory: root,
            object_name: owned_name.unicode(),
            attributes: 0, // OBJ_CASE_INSENSITIVE off: names stay exact
            security_descriptor: std::ptr::null_mut(),
            security_quality_of_service: std::ptr::null_mut(),
        });
        // Keep the header pointer stable under Box moves: the Box target
        // never moves once allocated.
        let _ = &mut inner;
        Self {
            inner,
            _name: owned_name,
        }
    }

    pub fn as_ptr(&self) -> *mut OBJECT_ATTRIBUTES {
        &*self.inner as *const OBJECT_ATTRIBUTES as *mut OBJECT_ATTRIBUTES
    }
}

/// Maps an NTSTATUS to a std io::Error. The mapping table covers the
/// statuses the checkout engine can meet; everything else becomes Other
/// with the hex status in the message.
pub fn io_error_from_nt(status: NTSTATUS, context: &str) -> io::Error {
    let kind = match status as u32 {
        0xC000_0034 => io::ErrorKind::NotFound, // STATUS_OBJECT_NAME_NOT_FOUND
        0xC000_0035 => io::ErrorKind::NotFound, // STATUS_OBJECT_PATH_NOT_FOUND
        0xC000_0033 => io::ErrorKind::InvalidInput, // STATUS_OBJECT_NAME_INVALID
        0xC000_003A => io::ErrorKind::NotFound, // STATUS_OBJECT_PATH_NOT_FOUND (alt)
        0xC000_0032 => io::ErrorKind::AlreadyExists, // STATUS_OBJECT_NAME_COLLISION
        0xC000_00BA => io::ErrorKind::IsADirectory, // STATUS_FILE_IS_A_DIRECTORY
        0xC000_0103 => io::ErrorKind::NotADirectory, // STATUS_NOT_A_DIRECTORY
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{context}: NTSTATUS {status:#010x}"))
}

/// An owned NT handle wrapper with std File integration.
pub struct OwnedNtHandle {
    handle: HANDLE,
    borrowed: bool,
}

impl OwnedNtHandle {
    /// Takes ownership of a raw NT handle.
    ///
    /// # SAFETY
    /// The handle must be a valid, owned kernel handle not owned elsewhere.
    pub unsafe fn from_raw(handle: HANDLE) -> Self {
        Self {
            handle,
            borrowed: false,
        }
    }

    pub fn as_raw(&self) -> HANDLE {
        self.handle
    }

    /// Converts into a std `File` (which closes the handle on drop).
    pub fn into_file(self) -> File {
        let handle = self.handle;
        std::mem::forget(self);
        // SAFETY: the handle is valid and now owned by the File.
        unsafe { File::from_raw_handle(handle as *mut c_void) }
    }

    /// Borrows a std File's handle without taking ownership (drop leaves
    /// the handle open).
    pub fn borrow(file: &File) -> Self {
        Self {
            handle: file.as_raw_handle(),
            borrowed: true,
        }
    }
}

impl Drop for OwnedNtHandle {
    fn drop(&mut self) {
        if self.borrowed {
            return;
        }
        // SAFETY: we own the handle.
        unsafe { NtClose(self.handle) };
    }
}

//! Handle-relative, reparse-point-refusing filesystem reads for the Host verified reader
//! (M9 E13 slice W1, Windows only). Counterpart of [`super::host_fs::read_file_under`].
//!
//! Every open is `NtCreateFile(RootDirectory = <retained/parent handle>, ObjectName = <one
//! component>)`: no absolute path, drive letter or `\\?\` prefix is ever built. Each component is
//! opened with `FILE_OPEN_REPARSE_POINT` (so a symlink / junction / mount point is opened as the
//! link itself, never traversed) plus `OBJ_DONT_REPARSE` where the OS knows it, and the opened
//! handle is then checked: a reparse point is refused. The final file must additionally be a
//! `FILE_TYPE_DISK` regular file with exactly one link and at most `max` bytes. Shares never
//! include `FILE_SHARE_DELETE`, so an opened directory cannot be renamed away mid-walk.
//!
//! The listed names are first filtered by [`super::verified_reader::windows_portable_components`]
//! (no ADS colon, no DOS device names, no trailing dot/space, bounded length/depth).
//!
//! Case: NTFS is case-insensitive, so a listed `A.js` may open an on-disk `a.js`. That is safe
//! because the caller compares the SRI of the bytes read against the signed listing; it only
//! matters that the bytes are the signed ones.

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_DONT_REPARSE, STATUS_ACCESS_DENIED, STATUS_FILE_IS_A_DIRECTORY, STATUS_INVALID_PARAMETER,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
    UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_TYPE_DISK, FILE_WRITE_DATA,
    FileAttributeTagInfo, FileStandardInfo, GetFileInformationByHandleEx, GetFileType, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::verified_reader::windows_portable_components;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Directory,
    RegularFile,
}

/// Read the regular file `rel` below the retained directory handle `root`.
pub(super) fn read_file_under(root: &File, rel: &str, max: u64) -> io::Result<Vec<u8>> {
    let parts = windows_portable_components(rel).map_err(|()| io::Error::from(io::ErrorKind::InvalidInput))?;
    let (last, dirs) = parts
        .split_last()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;

    // The retained root must itself be a directory handle that is not a reparse point.
    let root_std: FILE_STANDARD_INFO = file_info(root.as_raw_handle(), FileStandardInfo)?;
    if !root_std.Directory {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    refuse_reparse(root.as_raw_handle())?;

    let mut current = root.try_clone()?;
    for dir in dirs {
        current = open_relative(&current, dir, Want::Directory)?;
    }
    let file = open_relative(&current, last, Want::RegularFile)?;

    // SAFETY: GetFileType only inspects the handle we own.
    if unsafe { GetFileType(file.as_raw_handle() as HANDLE) } != FILE_TYPE_DISK {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let info: FILE_STANDARD_INFO = file_info(file.as_raw_handle(), FileStandardInfo)?;
    if info.Directory || info.NumberOfLinks != 1 || info.DeletePending {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let len = u64::try_from(info.EndOfFile).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    if len > max {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut bytes = Vec::new();
    Read::take(file, max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max || bytes.len() as u64 != len {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(bytes)
}

/// Open the single component `name` below `parent` for reading, never traversing a reparse point.
fn open_relative(parent: &File, name: &str, want: Want) -> io::Result<File> {
    let (access, extra_options) = match want {
        Want::Directory => (
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        ),
        Want::RegularFile => (
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_NON_DIRECTORY_FILE,
        ),
    };
    nt_open(
        parent,
        name,
        access,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        FILE_OPEN,
        extra_options,
        0,
    )
}

/// Handle-relative `NtCreateFile` of the single component `name` below `parent`.
///
/// `options` get `FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT` added, so an existing
/// link is opened as the link itself; the opened handle is then refused when it is a reparse
/// point. `OBJ_DONT_REPARSE` is preferred and dropped only if the OS rejects the flag itself.
fn nt_open(
    parent: &File,
    name: &str,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    file_attributes: u32,
) -> io::Result<File> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    let byte_len = u16::try_from(wide.len() * 2).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let options = options | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT;

    let mut status: NTSTATUS = STATUS_INVALID_PARAMETER;
    let mut handle: HANDLE = std::ptr::null_mut();
    for attributes in [OBJ_DONT_REPARSE, 0] {
        let unicode = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: wide.as_ptr().cast_mut(),
        };
        let object = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle() as HANDLE,
            ObjectName: &unicode,
            Attributes: attributes,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut io_status = IO_STATUS_BLOCK::default();
        handle = std::ptr::null_mut();
        // SAFETY: all pointers reference live locals for the duration of the call; `handle`
        // receives a new handle owned by us on success; no allocation size / EA buffer is passed.
        status = unsafe {
            NtCreateFile(
                &mut handle,
                access,
                &object,
                &mut io_status,
                std::ptr::null(),
                file_attributes,
                share,
                disposition,
                options,
                std::ptr::null::<c_void>(),
                0,
            )
        };
        if status != STATUS_INVALID_PARAMETER {
            break;
        }
    }
    if status < 0 {
        return Err(status_to_error(status));
    }
    // SAFETY: NtCreateFile succeeded, so `handle` is a valid handle that nothing else owns.
    let file = File::from(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
    refuse_reparse(file.as_raw_handle())?;
    Ok(file)
}

/// Access of a Host-owned directory handle: list, add entries, delete/rename itself.
const HOST_DIR_ACCESS: u32 = FILE_LIST_DIRECTORY
    | FILE_ADD_FILE
    | FILE_ADD_SUBDIRECTORY
    | FILE_TRAVERSE
    | FILE_READ_ATTRIBUTES
    | DELETE
    | SYNCHRONIZE;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Exclusively create the directory `name` below `parent` and return the handle of **that very
/// creation** (the analogue of `mkdirat` + `openat`): `STATUS_OBJECT_NAME_COLLISION` when anything
/// of that name exists, including a link. The handle carries `DELETE` so the Host can later
/// rename or remove the directory through it; it is Host-internal and never used to write
/// anything but verified bytes.
pub(super) fn create_dir_exclusive(parent: &File, name: &str) -> io::Result<File> {
    nt_open(
        parent,
        name,
        HOST_DIR_ACCESS,
        SHARE_ALL,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        FILE_ATTRIBUTE_NORMAL,
    )
}

/// Open the directory `name` below `parent`, creating it when absent. An existing file or link of
/// that name is refused (the analogue of `mkdirat(exist_ok)` + `openat(O_DIRECTORY|O_NOFOLLOW)`).
pub(super) fn open_or_create_dir(parent: &File, name: &str) -> io::Result<File> {
    nt_open(
        parent,
        name,
        HOST_DIR_ACCESS,
        SHARE_ALL,
        FILE_OPEN_IF,
        FILE_DIRECTORY_FILE,
        FILE_ATTRIBUTE_NORMAL,
    )
}

/// Exclusively create the regular file `name` below `parent`, write `bytes` and flush them. The
/// file is never shared while it is written; an existing entry of that name (including a link)
/// fails instead of being written through.
pub(super) fn create_file_exclusive(parent: &File, name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut file = nt_open(
        parent,
        name,
        FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        0,
        FILE_CREATE,
        FILE_NON_DIRECTORY_FILE,
        FILE_ATTRIBUTE_NORMAL,
    )?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Refuse a handle that is itself a reparse point (symlink, junction, mount point, …).
fn refuse_reparse(handle: RawHandle) -> io::Result<()> {
    let tag: FILE_ATTRIBUTE_TAG_INFO = file_info(handle, FileAttributeTagInfo)?;
    if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

fn file_info<T: Default>(handle: RawHandle, class: i32) -> io::Result<T> {
    let mut out = T::default();
    // SAFETY: `out` is a valid, properly sized out-buffer for the requested class `T`.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle as HANDLE,
            class,
            (&mut out as *mut T).cast::<c_void>(),
            std::mem::size_of::<T>() as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(out)
    }
}

fn status_to_error(status: NTSTATUS) -> io::Error {
    let kind = match status {
        STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => io::ErrorKind::NotFound,
        STATUS_ACCESS_DENIED => io::ErrorKind::PermissionDenied,
        STATUS_OBJECT_NAME_COLLISION => io::ErrorKind::AlreadyExists,
        STATUS_NOT_A_DIRECTORY | STATUS_FILE_IS_A_DIRECTORY => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("NTSTATUS {:#010x}", status as u32))
}

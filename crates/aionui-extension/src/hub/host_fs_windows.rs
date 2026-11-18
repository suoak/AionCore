//! Handle-relative, reparse-point-refusing filesystem reads for the Host verified reader
//! (M9 E13 slice W1, Windows only). Counterpart of [`super::host_fs::read_file_under`].
//!
//! Every open is `NtCreateFile(RootDirectory = <retained/parent handle>, ObjectName = <one
//! component>)`: no absolute path, drive letter or `\\?\` prefix is ever built. Each component is
//! opened with `FILE_OPEN_REPARSE_POINT` (so a symlink / junction / mount point is opened as the
//! link itself, never traversed) plus `OBJ_DONT_REPARSE` where the OS knows it, and the opened
//! handle is then checked: a reparse point is refused. The final file must additionally be a
//! `FILE_TYPE_DISK` regular file with exactly one link and at most `max` bytes. Handles share
//! read, write and delete so a concurrent install/upgrade can rename or delete the tree; a walk
//! is handle-relative, so a rename of an ancestor cannot redirect it.
//!
//! The listed names are first filtered by [`super::verified_reader::windows_portable_components`]
//! (no ADS colon, no DOS device names, no trailing dot/space, bounded length/depth).
//!
//! Defence in depth (design §W.3): after the walk, the normalized final path of the opened file
//! (`GetFinalPathNameByHandleW`, NT volume form) must be the final path of the retained root
//! followed by exactly the listed components
//! ([`super::verified_reader::final_path_is_listed_under`]); a failure of that query fails the
//! read. The path strings are only compared, never opened.
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
    FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, HANDLE, NTSTATUS, OBJ_DONT_REPARSE, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SHARING_VIOLATION, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FULL_DIR_INFO, FILE_LIST_DIRECTORY,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO, FILE_TRAVERSE, FILE_TYPE_DISK, FILE_WRITE_DATA, FileAttributeTagInfo, FileDispositionInfoEx,
    FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileStandardInfo, GetFileInformationByHandleEx, GetFileType,
    GetFinalPathNameByHandleW, SYNCHRONIZE, SetFileInformationByHandle, VOLUME_NAME_NT,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::verified_reader::{final_path_is_listed_under, windows_portable_components};

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
    // Defence in depth: the opened file must sit at exactly the listed names below the root.
    let root_final = final_path(root.as_raw_handle())?;
    let file_final = final_path(file.as_raw_handle())?;
    if !final_path_is_listed_under(&root_final, &file_final, &parts) {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
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
    nt_open(parent, name, access, SHARE_ALL, FILE_OPEN, extra_options, 0, true)
}

/// Handle-relative `NtCreateFile` of the single component `name` below `parent`.
///
/// `options` get `FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT` added, so an existing
/// link is opened as the link itself; the opened handle is then refused when it is a reparse
/// point. `OBJ_DONT_REPARSE` is preferred and dropped only if the OS rejects the flag itself.
#[allow(clippy::too_many_arguments)]
fn nt_open(
    parent: &File,
    name: &str,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    file_attributes: u32,
    refuse_links: bool,
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
    if refuse_links {
        refuse_reparse(file.as_raw_handle())?;
    }
    Ok(file)
}

/// Access of a Host-owned directory handle: list and add entries.
///
/// Deliberately **without `DELETE`**: a rename into a directory makes the kernel open that target
/// directory with `FILE_SHARE_READ | FILE_SHARE_WRITE` only, and on Windows 10 / 11 / Server 2022
/// kernels that open fails with `STATUS_SHARING_VIOLATION` while any handle to the directory holds
/// `DELETE`, so the built-in packs never placed on a fresh data dir (3.0.0 packaged acceptance;
/// Server 2025 no longer checks it). Renames and deletes always open their own short-lived
/// `DELETE` handle on the entry ([`open_for_delete`], [`remove_tree_at`]).
const HOST_DIR_ACCESS: u32 =
    FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Exclusively create the directory `name` below `parent` and return the handle of **that very
/// creation** (the analogue of `mkdirat` + `openat`): `STATUS_OBJECT_NAME_COLLISION` when anything
/// of that name exists, including a link. The handle carries no `DELETE` (see
/// [`HOST_DIR_ACCESS`]); it is Host-internal and never used to write anything but verified bytes.
pub(super) fn create_dir_exclusive(parent: &File, name: &str) -> io::Result<File> {
    nt_open(
        parent,
        name,
        HOST_DIR_ACCESS,
        SHARE_ALL,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        FILE_ATTRIBUTE_NORMAL,
        true,
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
        true,
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
        true,
    )?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Longest final path accepted (UTF-16 units, the NT path limit).
const MAX_FINAL_PATH_UNITS: usize = 32_768;

/// Normalized NT-volume final path of `handle` (`\Device\HarddiskVolumeN\…`). Only compared,
/// never used to open anything.
fn final_path(handle: RawHandle) -> io::Result<String> {
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: `buf` is a valid writable buffer of `buf.len()` UTF-16 units for the call.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                handle as HANDLE,
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NT,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return String::from_utf16(&buf).map_err(|_| io::Error::from(io::ErrorKind::InvalidData));
        }
        // Too small: `n` is the required size including the terminator.
        if n > MAX_FINAL_PATH_UNITS {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        buf.resize(n, 0);
    }
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
        STATUS_DIRECTORY_NOT_EMPTY => io::ErrorKind::DirectoryNotEmpty,
        STATUS_NOT_A_DIRECTORY | STATUS_FILE_IS_A_DIRECTORY => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("NTSTATUS {:#010x}", status as u32))
}

// ---- install / restore / sweep side (slice W2b): mirrors `host_fs` for unix ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Dir,
    Regular,
    Other,
}

/// Our own trees are shallow (zip names are bounded); anything deeper is refused, not followed.
pub(super) const MAX_REMOVE_DEPTH: u32 = 64;

/// Classify the entry `name` of `dir` without following a link: `Ok(None)` when absent. Anything
/// that is a reparse point (symlink, junction, mount point, …) is [`Kind::Other`].
pub(super) fn kind_nofollow(dir: &File, name: &str) -> io::Result<Option<Kind>> {
    let file = match nt_open(
        dir,
        name,
        FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        0,
        0,
        false,
    ) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let tag: FILE_ATTRIBUTE_TAG_INFO = file_info(file.as_raw_handle(), FileAttributeTagInfo)?;
    Ok(Some(if tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Other
    } else if tag.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Dir
    } else {
        Kind::Regular
    }))
}

/// Open the existing real directory `name` below `parent` for listing / reading (no write or
/// delete access); a link of that name is refused. Used for name directories and for the root
/// handle a restart re-retains.
pub(super) fn open_dir_existing(parent: &File, name: &str) -> io::Result<File> {
    nt_open(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_TRAVERSE | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
        0,
        true,
    )
}

/// Open the existing real directory `name` with the Host-directory access (list, add entries,
/// rename/delete) used for the per-extension name directory during an install.
pub(super) fn open_host_dir_existing(parent: &File, name: &str) -> io::Result<File> {
    nt_open(
        parent,
        name,
        HOST_DIR_ACCESS,
        SHARE_ALL,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
        0,
        true,
    )
}

/// UTF-8 entry names of an open directory (non-UTF-16-clean names are skipped; `.`/`..`
/// excluded). Always lists from the beginning.
pub(super) fn list_dir(dir: &File) -> io::Result<Vec<String>> {
    // 8-byte aligned scratch buffer (32 KiB); records are 8-byte aligned inside it.
    let mut buf = vec![0u64; 4096];
    let mut names = Vec::new();
    let mut class = FileFullDirectoryRestartInfo;
    loop {
        // SAFETY: `buf` is a valid writable buffer of the stated size for the whole call.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                dir.as_raw_handle() as HANDLE,
                class,
                buf.as_mut_ptr().cast::<c_void>(),
                (buf.len() * std::mem::size_of::<u64>()) as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                return Ok(names);
            }
            return Err(err);
        }
        class = FileFullDirectoryInfo;
        let base = buf.as_ptr().cast::<u8>();
        let mut offset = 0usize;
        loop {
            // SAFETY: the kernel filled a chain of `FILE_FULL_DIR_INFO` records inside `buf`;
            // `NextEntryOffset` stays within it and the file name lies within `FileNameLength`.
            let (next, name) = unsafe {
                let entry = base.add(offset).cast::<FILE_FULL_DIR_INFO>();
                let next = std::ptr::addr_of!((*entry).NextEntryOffset).read_unaligned() as usize;
                let len = std::ptr::addr_of!((*entry).FileNameLength).read_unaligned() as usize / 2;
                let name_ptr = base
                    .add(offset + std::mem::offset_of!(FILE_FULL_DIR_INFO, FileName))
                    .cast::<u16>();
                let units: Vec<u16> = (0..len).map(|i| name_ptr.add(i).read_unaligned()).collect();
                (next, String::from_utf16(&units).ok())
            };
            if let Some(name) = name
                && name != "."
                && name != ".."
            {
                names.push(name);
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

/// Mark the object behind `handle` for deletion with POSIX semantics (the name disappears at
/// once even while other handles are open). A non-empty directory fails.
fn mark_delete(handle: RawHandle) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `info` is a valid, properly sized buffer for `FileDispositionInfoEx`.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle as HANDLE,
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast::<c_void>(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open `name` (file, directory or link — never traversed) with delete access.
fn open_for_delete(parent: &File, name: &str, options: u32, refuse_links: bool) -> io::Result<File> {
    nt_open(
        parent,
        name,
        DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        options,
        0,
        refuse_links,
    )
}

/// Delete the non-directory entry `name` (regular file, or a link/junction **itself**).
pub(super) fn unlink_at(parent: &File, name: &str) -> io::Result<()> {
    let file = open_for_delete(parent, name, 0, false)?;
    mark_delete(file.as_raw_handle())
}

/// Delete the real directory `name`; fails unless it is empty.
pub(super) fn remove_empty_dir_at(parent: &File, name: &str) -> io::Result<()> {
    let dir = open_for_delete(parent, name, FILE_DIRECTORY_FILE, true)?;
    mark_delete(dir.as_raw_handle())
}

/// Delete the directory tree `name` under `parent`, handle-relative, never following a link: a
/// link or junction inside is deleted as a link and never entered.
pub(super) fn remove_tree_at(parent: &File, name: &str, depth: u32) -> io::Result<()> {
    if depth > MAX_REMOVE_DEPTH {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let dir = nt_open(
        parent,
        name,
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE,
        SHARE_ALL,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
        0,
        true,
    )?;
    for entry in list_dir(&dir)? {
        match kind_nofollow(&dir, &entry)? {
            Some(Kind::Dir) => remove_tree_at(&dir, &entry, depth + 1)?,
            Some(_) => unlink_at(&dir, &entry)?,
            None => {}
        }
    }
    mark_delete(dir.as_raw_handle())
}

/// Rename the entry `from` to `to` inside `dir` (both single components). `replace` lets a
/// regular file replace an existing regular file; without it an existing destination of any
/// kind fails (`AlreadyExists`) and nothing changes. The source is never a link.
///
/// A rename refused with `STATUS_ACCESS_DENIED` / `STATUS_SHARING_VIOLATION` is retried for a
/// bounded time ([`RENAME_RETRY_DELAYS_MS`]): NTFS refuses to rename a directory while any handle
/// to an entry below it is open, and on-access scanners (Defender) and the search indexer open
/// freshly written files for a moment right after the install closes them. The rename is atomic
/// and no-replace semantics are re-checked by the kernel on every attempt, so a retry can only
/// succeed or fail exactly as the first attempt would have without the transient opener.
fn rename_in_dir(dir: &File, from: &str, to: &str, replace: bool) -> io::Result<()> {
    let mut delays = RENAME_RETRY_DELAYS_MS.iter();
    loop {
        let status = rename_in_dir_once(dir, from, to, replace)?;
        if status >= 0 {
            return Ok(());
        }
        match delays.next() {
            Some(ms) if is_transient_rename_status(status) => {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
            _ => return Err(status_to_error(status)),
        }
    }
}

/// Back-off between rename attempts (about 3 s in total).
const RENAME_RETRY_DELAYS_MS: [u64; 10] = [5, 10, 20, 40, 80, 160, 320, 500, 800, 1000];

fn is_transient_rename_status(status: NTSTATUS) -> bool {
    status == STATUS_ACCESS_DENIED || status == STATUS_SHARING_VIOLATION
}

/// One rename attempt: an error opening the source is returned as is, otherwise the raw status.
fn rename_in_dir_once(dir: &File, from: &str, to: &str, replace: bool) -> io::Result<NTSTATUS> {
    let source = open_for_delete(dir, from, 0, true)?;
    let wide: Vec<u16> = to.encode_utf16().collect();
    let name_bytes = wide.len() * 2;
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    let total = header + name_bytes;
    // 8-byte aligned buffer for the variable-length record.
    let mut buf = vec![0u64; total.div_ceil(8)];
    // SAFETY: `buf` is large enough for the header plus the name and 8-byte aligned; the record
    // is fully initialised before it is passed to the kernel.
    unsafe {
        let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        (*info).Anonymous.ReplaceIfExists = replace;
        (*info).RootDirectory = dir.as_raw_handle() as HANDLE;
        (*info).FileNameLength = name_bytes as u32;
        let name_ptr = buf.as_mut_ptr().cast::<u8>().add(header).cast::<u16>();
        std::ptr::copy_nonoverlapping(wide.as_ptr(), name_ptr, wide.len());
    }
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: valid handle, valid initialised record of `total` bytes, valid status block.
    let status = unsafe {
        NtSetInformationFile(
            source.as_raw_handle() as HANDLE,
            &mut io_status,
            buf.as_ptr().cast::<c_void>(),
            total as u32,
            FileRenameInformation,
        )
    };
    Ok(status)
}

/// Rename `from` to `to` inside `dir`; an existing destination of any kind makes it fail.
pub(super) fn rename_noreplace(dir: &File, from: &str, to: &str) -> io::Result<()> {
    rename_in_dir(dir, from, to, false)
}

/// Rename the regular file `from` over the regular file (or absent) `to` inside `dir`.
pub(super) fn rename_replace(dir: &File, from: &str, to: &str) -> io::Result<()> {
    rename_in_dir(dir, from, to, true)
}

/// Atomically (re)write the small regular file `final_name` in `dir`: exclusive temp file,
/// flush, rename-over. Never writes through an existing path in place and never creates a link.
/// Returns without touching `final_name` on any failure.
pub(super) fn write_file_atomic(dir: &File, tmp_name: &str, final_name: &str, bytes: &[u8]) -> io::Result<()> {
    create_file_exclusive(dir, tmp_name, bytes).inspect_err(|_| {
        let _ = unlink_at(dir, tmp_name);
    })?;
    rename_replace(dir, tmp_name, final_name).inspect_err(|_| {
        let _ = unlink_at(dir, tmp_name);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::test_support::open_dir;

    /// The open the kernel performs on a rename's target directory (Windows 10/11/Server 2022:
    /// `FILE_SHARE_READ | FILE_SHARE_WRITE`, no `FILE_SHARE_DELETE`). Emulated so the regression is
    /// caught on every Windows version, including Server 2025 where the kernel no longer does it.
    fn open_like_rename_target(parent: &File, name: &str) -> io::Result<File> {
        nt_open(
            parent,
            name,
            FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            0,
            // No attribute query on this probe handle (it has no FILE_READ_ATTRIBUTES, like the
            // kernel's own open); only the share check matters.
            false,
        )
    }

    /// Packaged first-run flow (fresh data dir): the Host holds the parent, the name directory and
    /// the staged root while it renames the staged root into place and then renames the marker.
    /// None of those held handles may block the kernel's rename-target open.
    #[test]
    fn host_held_directory_handles_never_block_a_rename_into_them() {
        let d = tempfile::tempdir().unwrap();
        let parent = crate::hub::test_support::open_host_dir(d.path());
        let name_dir = create_dir_exclusive(&parent, "workmate.pack").unwrap();
        let staged = create_dir_exclusive(&name_dir, ".wip-1").unwrap();
        create_file_exclusive(&staged, "aion-extension.json", b"{}").unwrap();
        let reopened = open_or_create_dir(&parent, "workmate.pack").unwrap();
        let existing = open_host_dir_existing(&parent, "workmate.pack").unwrap();

        open_like_rename_target(&parent, "workmate.pack")
            .expect("a Host-held handle on the name directory must not hold DELETE");
        rename_noreplace(&name_dir, ".wip-1", "1.0.0").unwrap();
        write_file_atomic(&name_dir, ".current.tmp-1", "current", b"marker").unwrap();
        assert_eq!(
            read_file_under(&parent, "workmate.pack/1.0.0/aion-extension.json", 8).unwrap(),
            b"{}"
        );
        assert_eq!(read_file_under(&name_dir, "current", 8).unwrap(), b"marker");
        drop((reopened, existing, staged));
    }

    /// A handle held open below a staged directory (what an on-access scanner does for a moment
    /// right after the install writes a file) must not make the placement rename fail.
    #[test]
    fn directory_rename_waits_out_a_transient_open_handle_below_it() {
        let d = tempfile::tempdir().unwrap();
        let parent = open_dir(d.path());
        let staged = create_dir_exclusive(&parent, ".wip-test").unwrap();
        create_file_exclusive(&staged, "panel.html", b"<p>hi</p>").unwrap();
        drop(staged);
        // The "scanner": an ordinary open of the freshly written file.
        let scanner = std::fs::File::open(d.path().join(".wip-test").join("panel.html")).unwrap();

        let first = rename_in_dir_once(&parent, ".wip-test", "1.0.0", false).unwrap();
        eprintln!(
            "first rename attempt with an open child handle: NTSTATUS {:#010x}",
            first as u32
        );
        if first >= 0 {
            // This volume allowed it outright; put it back so the retry path is exercised too.
            rename_noreplace(&parent, "1.0.0", ".wip-test").unwrap();
        } else {
            assert!(is_transient_rename_status(first), "{:#010x}", first as u32);
        }

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(scanner);
        });
        rename_noreplace(&parent, ".wip-test", "1.0.0").unwrap();
        releaser.join().unwrap();
        assert_eq!(read_file_under(&parent, "1.0.0/panel.html", 64).unwrap(), b"<p>hi</p>");
        // No-replace is still enforced on the retried path.
        let other = create_dir_exclusive(&parent, ".wip-two").unwrap();
        drop(other);
        assert_eq!(
            rename_noreplace(&parent, ".wip-two", "1.0.0").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn final_path_cross_check_refuses_a_short_name_alias() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("panels")).unwrap();
        std::fs::write(d.path().join("panels").join("abcdefghij.html"), b"long").unwrap();
        let root = open_dir(d.path());

        // The listed long name reads, and its final path passes the cross-check.
        assert_eq!(read_file_under(&root, "panels/abcdefghij.html", 64).unwrap(), b"long");
        let root_final = final_path(root.as_raw_handle()).unwrap();
        assert!(root_final.starts_with(r"\Device\"), "NT volume form: {root_final}");

        // An 8.3-shaped name never reaches the filesystem through the reader…
        assert_eq!(
            read_file_under(&root, "panels/ABCDEF~1.HTM", 64).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // …and if the volume generates short names, the alias opened handle-relative is caught by
        // the final-path cross-check (it resolves to the long name, not the listed one).
        let panels = open_relative(&root, "panels", Want::Directory).unwrap();
        match open_relative(&panels, "ABCDEF~1.HTM", Want::RegularFile) {
            Ok(alias) => {
                let alias_final = final_path(alias.as_raw_handle()).unwrap();
                assert!(!final_path_is_listed_under(
                    &root_final,
                    &alias_final,
                    &["panels", "ABCDEF~1.HTM"]
                ));
                assert!(final_path_is_listed_under(
                    &root_final,
                    &alias_final,
                    &["panels", "abcdefghij.html"]
                ));
                eprintln!("short-name alias present on this volume: cross-check refused it");
            }
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
                eprintln!("no short-name alias on this volume: the name filter alone refuses it");
            }
        }
    }
}

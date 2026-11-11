//! Test-only helpers shared by the hub tests that must run on unix **and** Windows.

use std::fs::File;
use std::path::Path;

/// Open an existing directory as a handle (the Host-held parent / root in tests).
pub(crate) fn open_dir(path: &Path) -> File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_BACKUP_SEMANTICS is required to open a directory handle.
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0200_0000)
            .open(path)
            .unwrap()
    }
    #[cfg(not(windows))]
    {
        File::open(path).unwrap()
    }
}

/// Create a directory link `link -> target`: a junction on Windows (no privilege needed), a
/// symlink elsewhere.
pub(crate) fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    junction::create(target, link).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Create a file symlink; `false` when the OS refuses for lack of privilege (Windows).
pub(crate) fn link_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(target, link) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(1314) => {
                eprintln!("SKIPPED: SeCreateSymbolicLinkPrivilege not held");
                false
            }
            Err(e) => panic!("symlink_file: {e}"),
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
        true
    }
}

/// Open the Host-held *parent* directory handle the install code works relative to, with the same
/// access mask the Host app gives it (list / add entries / traverse / delete / synchronize).
pub(crate) fn open_host_dir(path: &Path) -> File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .access_mode(0x1 | 0x2 | 0x4 | 0x20 | 0x80 | 0x1_0000 | 0x10_0000)
            .custom_flags(0x0200_0000)
            .open(path)
            .unwrap()
    }
    #[cfg(not(windows))]
    {
        File::open(path).unwrap()
    }
}

/// Stable identity of the object a handle refers to (volume + file index / dev + inode).
pub(crate) fn identity(file: &File) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata().unwrap();
        (m.dev(), m.ino())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        // SAFETY: plain-old-data out parameter; the handle is valid for the duration of the call.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) };
        assert!(ok != 0, "GetFileInformationByHandle failed");
        (
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        )
    }
}

/// Identity of the directory at `path` (opened without following nothing special: tests only).
pub(crate) fn path_identity(path: &Path) -> (u64, u64) {
    identity(&open_dir(path))
}

/// Remove a link created by [`link_dir`] / [`link_file`] without touching its target (a directory
/// junction needs `remove_dir` on Windows, a symlink needs `remove_file`).
pub(crate) fn remove_link(link: &Path) {
    if std::fs::remove_file(link).is_err() {
        std::fs::remove_dir(link).unwrap();
    }
}

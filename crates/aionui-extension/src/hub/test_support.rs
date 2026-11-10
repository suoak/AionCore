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

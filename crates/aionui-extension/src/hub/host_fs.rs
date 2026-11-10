//! FD-relative filesystem helpers shared by the Host install / restore code (unix only).
//!
//! Every function takes an already-open directory handle plus a **single** component (or a
//! canonical relative path walked component by component) and never follows a symlink. No
//! function here accepts or builds an absolute path.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;

use super::local_package_install::sys as base;

/// Our own trees are shallow (zip names are bounded); anything deeper is refused, not followed.
pub(super) const MAX_REMOVE_DEPTH: u32 = 64;

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Dir,
    Regular,
    Other,
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)`: `Ok(None)` when absent.
pub(super) fn kind_nofollow(dir: &File, name: &str) -> io::Result<Option<Kind>> {
    let c = cstr(name)?;
    // SAFETY: zeroed `stat` is a valid out-parameter value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: dirfd borrowed for the call, `c` NUL-terminated, `st` a valid out pointer.
    let rc = unsafe { libc::fstatat(dir.as_raw_fd(), c.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::ENOENT) {
            Ok(None)
        } else {
            Err(err)
        };
    }
    let fmt = st.st_mode & libc::S_IFMT;
    Ok(Some(if fmt == libc::S_IFDIR {
        Kind::Dir
    } else if fmt == libc::S_IFREG {
        Kind::Regular
    } else {
        Kind::Other
    }))
}

pub(super) fn unlinkat(dir: &File, name: &str, flags: i32) -> io::Result<()> {
    let c = cstr(name)?;
    // SAFETY: dirfd borrowed for the call, `c` NUL-terminated.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// UTF-8 entry names of an open directory (non-UTF-8 names are skipped; `.`/`..` excluded).
pub(super) fn list_dir(dir: &File) -> io::Result<Vec<String>> {
    // SAFETY: duplicating a valid fd; the duplicate is owned by the DIR* on success.
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup` is a fresh directory fd we own; fdopendir takes ownership on success.
    let dirp = unsafe { libc::fdopendir(dup) };
    if dirp.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so `dup` is still ours to close.
        unsafe { libc::close(dup) };
        return Err(err);
    }
    // The duplicate shares its file offset with `dir`: a previous listing of the same long-lived
    // handle left it at end-of-directory, so always start from the beginning.
    // SAFETY: `dirp` is a valid open DIR*.
    unsafe { libc::rewinddir(dirp) };
    let mut names = Vec::new();
    loop {
        // SAFETY: `dirp` is a valid open DIR*; the returned entry is read before the next call.
        let ent = unsafe { libc::readdir(dirp) };
        if ent.is_null() {
            break;
        }
        // SAFETY: d_name is a NUL-terminated array inside the entry returned by readdir.
        let raw = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        if let Ok(name) = raw.to_str()
            && name != "."
            && name != ".."
        {
            names.push(name.to_owned());
        }
    }
    // SAFETY: closing the DIR* we opened (also closes `dup`).
    unsafe { libc::closedir(dirp) };
    Ok(names)
}

/// Delete the directory tree `name` under `parent`, FD-relative, never following symlinks.
pub(super) fn remove_tree_at(parent: &File, name: &str, depth: u32) -> io::Result<()> {
    if depth > MAX_REMOVE_DEPTH {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    // openat adds O_NOFOLLOW: a symlink (or non-directory) named `name` fails here.
    let dir = base::openat(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    for entry in list_dir(&dir)? {
        match kind_nofollow(&dir, &entry)? {
            Some(Kind::Dir) => remove_tree_at(&dir, &entry, depth + 1)?,
            Some(_) => unlinkat(&dir, &entry, 0)?,
            None => {}
        }
    }
    unlinkat(parent, name, libc::AT_REMOVEDIR)
}

/// `renameat` inside one directory that never replaces an existing destination.
pub(super) fn rename_noreplace(dir: &File, from: &str, to: &str) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    let fd = dir.as_raw_fd();
    #[cfg(target_os = "linux")]
    // SAFETY: valid dirfd (twice), NUL-terminated names.
    let rc = unsafe { libc::renameat2(fd, f.as_ptr(), fd, t.as_ptr(), libc::RENAME_NOREPLACE) };
    #[cfg(target_vendor = "apple")]
    // SAFETY: valid dirfd (twice), NUL-terminated names.
    let rc = unsafe { libc::renameatx_np(fd, f.as_ptr(), fd, t.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let rc = {
        // No atomic no-replace primitive: refuse when the destination exists (best effort).
        if kind_nofollow(dir, to)?.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EEXIST));
        }
        // SAFETY: valid dirfd (twice), NUL-terminated names.
        unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) }
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Atomically swap two existing entries of one directory (Linux `RENAME_EXCHANGE`, Apple
/// `RENAME_SWAP`). Elsewhere, or on a filesystem without support, this fails (`ENOSYS`/`EINVAL`).
pub(super) fn rename_exchange(dir: &File, a: &str, b: &str) -> io::Result<()> {
    let (ca, cb) = (cstr(a)?, cstr(b)?);
    let fd = dir.as_raw_fd();
    #[cfg(target_os = "linux")]
    // SAFETY: valid dirfd (twice), NUL-terminated names.
    let rc = unsafe { libc::renameat2(fd, ca.as_ptr(), fd, cb.as_ptr(), libc::RENAME_EXCHANGE) };
    #[cfg(target_vendor = "apple")]
    // SAFETY: valid dirfd (twice), NUL-terminated names.
    let rc = unsafe { libc::renameatx_np(fd, ca.as_ptr(), fd, cb.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    let rc = {
        let _ = (fd, &ca, &cb);
        return Err(io::Error::from_raw_os_error(libc::ENOSYS));
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Plain `renameat` within one directory that **replaces** an existing regular file / symlink
/// destination (never follows it). Used only for the `current` marker temp-file switch.
pub(super) fn rename_replace(dir: &File, from: &str, to: &str) -> io::Result<()> {
    let (f, t) = (cstr(from)?, cstr(to)?);
    let fd = dir.as_raw_fd();
    // SAFETY: valid dirfd (twice), NUL-terminated names.
    if unsafe { libc::renameat(fd, f.as_ptr(), fd, t.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Read a regular file at canonical relative path `rel` below `root`, walking each component with
/// `openat(O_NOFOLLOW)`. The final open is non-blocking so a FIFO cannot hang us; the result must be
/// a regular file of at most `max` bytes.
pub(super) fn read_file_under(root: &File, rel: &str, max: u64) -> io::Result<Vec<u8>> {
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, dirs) = parts
        .split_last()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut current = root.try_clone()?;
    for dir in dirs {
        current = base::openat(&current, dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    }
    let file = base::openat(&current, last, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > max {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    let mut bytes = Vec::new();
    Read::take(file, max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max || bytes.len() as u64 != meta.len() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    Ok(bytes)
}

/// Atomically (re)write the small regular file `final_name` in `dir`: exclusive temp file,
/// `fsync`, `renameat`, directory `fsync`. Never writes through an existing path in place and
/// never creates a symlink. Returns without touching `final_name` on any failure.
pub(super) fn write_file_atomic(dir: &File, tmp_name: &str, final_name: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut file = base::openat(dir, tmp_name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
    let write = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = write {
        let _ = unlinkat(dir, tmp_name, 0);
        return Err(e);
    }
    if let Err(e) = rename_replace(dir, tmp_name, final_name) {
        let _ = unlinkat(dir, tmp_name, 0);
        return Err(e);
    }
    let _ = dir.sync_all();
    Ok(())
}

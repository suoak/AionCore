//! Platform entry point for the Host **verified reader** (M9 E13 slice W1).
//!
//! `read_file_under(root, rel, max)` reads one regular file at the canonical `/`-separated
//! relative path `rel` below an already-open, Host-retained directory handle `root`:
//!
//! * unix: `openat(O_NOFOLLOW)` walked component by component ([`super::host_fs`]);
//! * windows: handle-relative `NtCreateFile` with `FILE_OPEN_REPARSE_POINT` (+ `OBJ_DONT_REPARSE`
//!   when supported), a reparse-point / hard-link / directory check on every opened handle, and a
//!   portable-name filter ([`super::host_fs_windows`]);
//! * anything else: `Unsupported` (fail closed, never a path read).
//!
//! No backend accepts or builds an absolute path or a path string for the *file*; the only
//! authority is the retained `root` handle. Callers compare the SRI of the **bytes returned**.
//! Callers map errors to fixed categories and never expose paths.

use std::fs::File;
use std::io;

/// Read the regular file `rel` below `root` (at most `max` bytes). See the module docs.
pub(crate) fn read_file_under(root: &File, rel: &str, max: u64) -> io::Result<Vec<u8>> {
    // Platform-independent canonical-form check: every backend sees only plain components.
    if rel.is_empty() || rel.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    #[cfg(unix)]
    {
        super::host_fs::read_file_under(root, rel, max)
    }
    #[cfg(windows)]
    {
        super::host_fs_windows::read_file_under(root, rel, max)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, rel, max);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// Maximum components of a verified path (matches the install-time depth bound).
pub(crate) const MAX_COMPONENTS: usize = 64;
/// Maximum UTF-16 code units of one NTFS path component.
pub(crate) const MAX_COMPONENT_UTF16: usize = 255;

/// Split `rel` into components and apply the **Windows portable-name rules**, fail closed.
///
/// Rejected: empty path/component, `.`/`..`, backslash or `/`-embedded oddities, drive/ADS
/// colons, the wildcard/reserved characters `* ? " < > |`, control characters, trailing dot or
/// space (Win32 silently strips them, so the opened name would differ from the listed one),
/// DOS device names (`CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`, `COM¹²³`, `LPT¹²³`,
/// `CONIN$`, `CONOUT$`, with or without an extension), components over 255 UTF-16 units and
/// paths deeper than [`MAX_COMPONENTS`]. Pure and platform independent so it is unit-tested
/// everywhere; only the Windows backend applies it.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn windows_portable_components(rel: &str) -> Result<Vec<&str>, ()> {
    if rel.is_empty() {
        return Err(());
    }
    let parts: Vec<&str> = rel.split('/').collect();
    if parts.len() > MAX_COMPONENTS {
        return Err(());
    }
    for part in &parts {
        if !windows_portable_component(part) {
            return Err(());
        }
    }
    Ok(parts)
}

fn windows_portable_component(part: &str) -> bool {
    if part.is_empty() || part == "." || part == ".." {
        return false;
    }
    if part.encode_utf16().count() > MAX_COMPONENT_UTF16 {
        return false;
    }
    if part
        .chars()
        .any(|c| c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        return false;
    }
    if part.ends_with('.') || part.ends_with(' ') {
        return false;
    }
    !is_dos_device_name(part)
}

/// DOS device name check: the stem before the first `.` (trailing spaces ignored), any case.
fn is_dos_device_name(part: &str) -> bool {
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    let upper = stem.to_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next())
                && (c.is_ascii_digit() || matches!(c, '\u{b9}' | '\u{b2}' | '\u{b3}'))
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ---- portable-name filter (runs on every platform) ----

    #[test]
    fn portable_names_accept_ordinary_listed_paths() {
        for ok in [
            "a",
            "a.txt",
            "services/digest.mjs",
            "a/b/c.d.e",
            "dir.with.dots/file",
            "console.js",
            "comx.js",
            "com10.js",
            "lpt.txt",
            "auxiliary",
            "nul_file",
            "中文/文件.txt",
            "a b/c d.txt",
        ] {
            assert_eq!(
                windows_portable_components(ok).map(|v| v.join("/")),
                Ok(ok.to_owned()),
                "{ok}"
            );
        }
    }

    #[test]
    fn portable_names_reject_unsafe_paths() {
        let long = "x".repeat(MAX_COMPONENT_UTF16 + 1);
        let deep = vec!["d"; MAX_COMPONENTS + 1].join("/");
        for bad in [
            "",
            "/",
            "a/",
            "/a",
            "a//b",
            ".",
            "..",
            "a/./b",
            "../a",
            "a/../b",
            "a\\b",
            "C:",
            "C:/x",
            "a.txt:stream",
            "a.txt::$DATA",
            "a:b/c",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a\u{0}b",
            "a\tb",
            "a\u{7f}b",
            "trailing.",
            "trailing ",
            "dir./file",
            "dir /file",
            "CON",
            "con",
            "Con.txt",
            "PRN",
            "AUX.js",
            "NUL",
            "nul.tar.gz",
            "COM1",
            "com9.txt",
            "COM0",
            "LPT1",
            "lpt7.log",
            "COM\u{b9}",
            "LPT\u{b3}.txt",
            "CONIN$",
            "CONOUT$",
            "CON .txt",
            "a/NUL/b",
            long.as_str(),
            deep.as_str(),
        ] {
            assert!(windows_portable_components(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn portable_names_count_utf16_units_not_chars() {
        // 128 astral characters = 256 UTF-16 units: over the NTFS component limit.
        let astral = "\u{1F600}".repeat(128);
        assert!(windows_portable_components(&astral).is_err());
        let ok = "\u{1F600}".repeat(127);
        assert!(windows_portable_components(&ok).is_ok());
    }

    // ---- reader conformance (runs against the real backend on unix AND windows) ----

    const MAX: u64 = 1024;

    fn open_root(path: &Path) -> File {
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

    fn fixture() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("services/deep")).unwrap();
        std::fs::write(p.join("top.txt"), b"top").unwrap();
        std::fs::write(p.join("empty.txt"), b"").unwrap();
        std::fs::write(p.join("services/digest.mjs"), b"export const a = 1;\n").unwrap();
        std::fs::write(p.join("services/deep/leaf.bin"), [0u8, 159, 146, 150, 255]).unwrap();
        let root = open_root(p);
        (dir, root)
    }

    fn kind_of(r: io::Result<Vec<u8>>) -> io::ErrorKind {
        r.expect_err("must be refused").kind()
    }

    /// Create a directory link `link -> target`: a junction on Windows (no privilege needed),
    /// a symlink elsewhere.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(windows)]
        junction::create(target, link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    /// Create a file symlink; `false` when the OS refuses for lack of privilege (Windows).
    fn link_file(target: &Path, link: &Path) -> bool {
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

    #[test]
    fn reads_top_level_nested_empty_and_binary_files_exactly() {
        let (_d, root) = fixture();
        assert_eq!(read_file_under(&root, "top.txt", MAX).unwrap(), b"top");
        assert_eq!(read_file_under(&root, "empty.txt", MAX).unwrap(), b"");
        assert_eq!(
            read_file_under(&root, "services/digest.mjs", MAX).unwrap(),
            b"export const a = 1;\n"
        );
        assert_eq!(
            read_file_under(&root, "services/deep/leaf.bin", MAX).unwrap(),
            [0u8, 159, 146, 150, 255]
        );
        // The retained handle is reusable; every read is independent and repeatable.
        for _ in 0..3 {
            assert_eq!(read_file_under(&root, "top.txt", MAX).unwrap(), b"top");
        }
    }

    #[test]
    fn size_bound_is_inclusive_and_enforced_before_returning() {
        let (d, root) = fixture();
        std::fs::write(d.path().join("exact.bin"), vec![7u8; MAX as usize]).unwrap();
        std::fs::write(d.path().join("over.bin"), vec![7u8; MAX as usize + 1]).unwrap();
        assert_eq!(read_file_under(&root, "exact.bin", MAX).unwrap().len(), MAX as usize);
        assert_eq!(
            kind_of(read_file_under(&root, "over.bin", MAX)),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn missing_file_and_missing_directory_are_not_found() {
        let (_d, root) = fixture();
        for rel in ["nope.txt", "services/nope.mjs", "nodir/file.txt", "services/deep/none"] {
            assert_eq!(
                kind_of(read_file_under(&root, rel, MAX)),
                io::ErrorKind::NotFound,
                "{rel}"
            );
        }
    }

    #[test]
    fn directory_or_file_in_the_middle_of_the_path_is_refused_not_missing() {
        let (_d, root) = fixture();
        // A directory is not a regular file.
        for rel in ["services", "services/deep"] {
            let kind = kind_of(read_file_under(&root, rel, MAX));
            assert_ne!(kind, io::ErrorKind::NotFound, "{rel}");
        }
        // A regular file used as a directory component.
        assert!(read_file_under(&root, "top.txt/inner", MAX).is_err());
    }

    #[test]
    fn non_canonical_paths_are_refused_without_touching_the_filesystem() {
        let (_d, root) = fixture();
        for rel in [
            "",
            "/top.txt",
            "top.txt/",
            "a//b",
            "./top.txt",
            "../top.txt",
            "services/../top.txt",
        ] {
            assert!(read_file_under(&root, rel, MAX).is_err(), "{rel:?}");
        }
    }

    #[test]
    fn a_symlinked_final_component_is_never_followed() {
        let (d, root) = fixture();
        std::fs::write(d.path().join("secret.txt"), b"outside-listing").unwrap();
        if link_file(&d.path().join("secret.txt"), &d.path().join("services/link.mjs")) {
            assert!(read_file_under(&root, "services/link.mjs", MAX).is_err());
        }
        // A link to a file outside the root tree.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("o.txt"), b"outside-tree").unwrap();
        if link_file(&outside.path().join("o.txt"), &d.path().join("escape.txt")) {
            assert!(read_file_under(&root, "escape.txt", MAX).is_err());
        }
    }

    #[test]
    fn a_linked_intermediate_directory_is_never_followed() {
        let (d, root) = fixture();
        // Link inside the tree and a link that escapes the tree.
        link_dir(&d.path().join("services"), &d.path().join("alias"));
        assert!(read_file_under(&root, "alias/digest.mjs", MAX).is_err());
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("o.txt"), b"outside-tree").unwrap();
        link_dir(outside.path(), &d.path().join("escape"));
        assert!(read_file_under(&root, "escape/o.txt", MAX).is_err());
        // The real path still works.
        assert!(read_file_under(&root, "services/digest.mjs", MAX).is_ok());
    }

    #[test]
    fn swapping_a_checked_directory_for_a_link_after_the_fact_is_not_followed() {
        let (d, root) = fixture();
        assert!(read_file_under(&root, "services/digest.mjs", MAX).is_ok());
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path()).unwrap();
        std::fs::write(outside.path().join("digest.mjs"), b"evil").unwrap();
        std::fs::remove_dir_all(d.path().join("services")).unwrap();
        link_dir(outside.path(), &d.path().join("services"));
        assert!(read_file_under(&root, "services/digest.mjs", MAX).is_err());
    }

    #[test]
    fn root_that_is_not_a_directory_is_refused() {
        let (d, _root) = fixture();
        let file = File::open(d.path().join("top.txt")).unwrap();
        assert!(read_file_under(&file, "anything", MAX).is_err());
    }

    // ---- Windows-only name / link semantics (no equivalent on unix) ----

    #[cfg(windows)]
    #[test]
    fn windows_refuses_hard_links_streams_and_reserved_names() {
        let (d, root) = fixture();
        std::fs::hard_link(d.path().join("top.txt"), d.path().join("hard.txt")).unwrap();
        assert!(read_file_under(&root, "hard.txt", MAX).is_err(), "hard link");
        assert!(read_file_under(&root, "top.txt", MAX).is_err(), "now has two links");
        std::fs::remove_file(d.path().join("hard.txt")).unwrap();
        assert_eq!(read_file_under(&root, "top.txt", MAX).unwrap(), b"top");

        std::fs::write(d.path().join("top.txt:evil"), b"stream").unwrap();
        assert_eq!(
            kind_of(read_file_under(&root, "top.txt:evil", MAX)),
            io::ErrorKind::InvalidInput
        );
        for rel in ["CON", "services/nul.txt", "trailing.", "a\\b", "C:/x"] {
            assert_eq!(
                kind_of(read_file_under(&root, rel, MAX)),
                io::ErrorKind::InvalidInput,
                "{rel}"
            );
        }
    }
}

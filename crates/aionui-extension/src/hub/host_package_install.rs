//! Host-owned automatic install layout for an **already-local** signed package (M9 E13 slice C).
//!
//! The customer only hands over a package. The Host supplies its own extensions parent directory
//! handle ([`HostExtensionsParent`], opened once by the Host from its own trusted location — this
//! module never opens a path, and never derives a directory from the manifest, the package or
//! `LoadedExtension.directory`). Everything below is relative to that handle:
//!
//! ```text
//! {parent}/                    Host-held directory handle
//!   {name-component}/          one directory per signed extension name
//!     .wip-{nonce}/            staging for ONE attempt; renamed to the version dir on success
//!     {version-component}/     the installed root (the retained handle is this very inode)
//! ```
//!
//! # Naming (derived from the signed identity, never chosen by the customer)
//! `check_signed_manifest` yields the signed `name` and `version`. Each becomes one directory
//! component: bytes in `[A-Za-z0-9_-]` (and `.` after the first byte) are kept, every other byte —
//! including a leading `.`, `~`, `/`, `\`, NUL, spaces and all non-ASCII — becomes `~XX` (upper hex).
//! The mapping is injective, so two identities never share a component, and no component can be
//! `.`, `..`, start with `.wip-`, or contain a separator. Over 255 bytes after encoding fails closed
//! (never truncated). `demo.plugin` / `1.0.0` stays readable as `demo.plugin/1.0.0`.
//!
//! # Flow (fail closed, no live-binding change before everything is ready)
//! 1. signature check and whole-archive validation in memory (no filesystem effect on failure);
//! 2. refuse if the store already binds this extension name (`AlreadyBound`: switching an
//!    existing live binding is the later atomic-upgrade slice) or the version directory exists
//!    (`AlreadyInstalled`: no replace here);
//! 3. create/open the name directory (`mkdirat` + `openat(O_DIRECTORY|O_NOFOLLOW)`), exclusively
//!    create `.wip-{nonce}`, write the verified files into it with the 261 `O_EXCL|O_NOFOLLOW` rules;
//! 4. place it with a no-replace rename to the version component, then
//!    `SignedRootBindingStore::install_new` the staging handle (same inode, never reopened).
//!
//! # Cleanup (deletes only what this attempt created)
//! Any failure after staging removes that attempt's `.wip-{nonce}` tree (or, if the rename already
//! happened but binding failed, the version directory this call just created) with an FD-relative,
//! `O_NOFOLLOW` walk: never a path string, never following symlinks, never the parent, never a
//! pre-existing version directory. A name directory this call created is removed only if empty.
//! [`sweep_abandoned_staging`] clears `.wip-*` directories left by a crashed attempt; call it at
//! Host start before installing (it cannot tell a crashed attempt from a concurrent live one).
//!
//! # Not
//! No `current` marker, upgrade/replace, restart re-retain, `.old-*` handling (slice D), no route,
//! network, exec, permission/HTTP/plan-safe change; not execution authorization or owner proof
//! beyond the Host holding `parent`. Unix only; other targets return `Unsupported`.

use std::fs::File;

use super::curated::CuratedInstallPolicy;
use super::local_package_install::{LocalPackageError, LocalPackageInstalled, LocalPackageLimits, LocalPackageSource};
use crate::signed_root_binding::SignedRootBindingStore;

/// Maximum bytes of one encoded directory component (typical filesystem `NAME_MAX`).
const MAX_COMPONENT_BYTES: usize = 255;
/// Prefix of this module's staging directories.
const STAGING_PREFIX: &str = ".wip-";

/// The Host's own extensions parent directory handle. The caller must obtain the `File` itself
/// (from its trusted data dir at startup); there is deliberately no constructor from a path.
pub struct HostExtensionsParent {
    dir: File,
}

impl std::fmt::Debug for HostExtensionsParent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostExtensionsParent").finish_non_exhaustive()
    }
}

impl HostExtensionsParent {
    /// Wrap a Host-opened directory handle; refuses anything that is not a directory.
    pub fn new(dir: File) -> Result<Self, LocalPackageError> {
        match dir.metadata() {
            Ok(m) if m.is_dir() => Ok(Self { dir }),
            _ => Err(LocalPackageError::ParentNotDirectory),
        }
    }
}

/// Outcome of [`sweep_abandoned_staging`]: counts only, never names or paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingSweepReport {
    pub removed: usize,
    pub failed: usize,
}

/// Encode one signed identity string as a single safe directory component (see module docs).
pub(crate) fn encode_component(value: &str) -> Option<String> {
    use std::fmt::Write as _;
    if value.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    for (i, b) in value.bytes().enumerate() {
        let keep = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i != 0);
        if keep {
            out.push(char::from(b));
        } else {
            write!(out, "~{b:02X}").ok()?;
        }
        if out.len() > MAX_COMPONENT_BYTES {
            return None;
        }
    }
    Some(out)
}

/// Install `source` under the Host parent at `{name}/{version}` (derived from the signed manifest).
///
/// See the module docs for the exact flow, the refusals (`AlreadyBound`, `AlreadyInstalled`) and
/// the cleanup rules. Nothing is retained and no live binding is touched on any error.
pub fn install_local_signed_package_auto(
    store: &SignedRootBindingStore,
    policy: &CuratedInstallPolicy,
    source: &LocalPackageSource<'_>,
    parent: &HostExtensionsParent,
    limits: LocalPackageLimits,
) -> Result<LocalPackageInstalled, LocalPackageError> {
    use super::local_package_install::validate_package;
    use super::signed_manifest::check_signed_manifest;

    let checked =
        check_signed_manifest(policy, &source.entry, source.raw_manifest).map_err(LocalPackageError::Signature)?;
    let package = validate_package(&checked, source.package_zip, limits)?;
    let name_component = encode_component(checked.name()).ok_or(LocalPackageError::InvalidDirName)?;
    let version_component = encode_component(checked.version()).ok_or(LocalPackageError::InvalidDirName)?;
    if store.contains(checked.name()) {
        return Err(LocalPackageError::AlreadyBound);
    }
    imp::install(
        store,
        checked,
        package,
        &parent.dir,
        &name_component,
        &version_component,
    )
}

/// Remove `.wip-*` staging directories left under the Host parent by a crashed attempt.
///
/// Walks `parent` → each real (non-symlink, non-dot) directory → entries named `.wip-*` that are
/// real directories, deleting them FD-relative with `O_NOFOLLOW`. Version directories, files,
/// symlinks, dot-entries at the parent level and everything else are left alone.
pub fn sweep_abandoned_staging(parent: &HostExtensionsParent) -> StagingSweepReport {
    imp::sweep(&parent.dir)
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    use super::super::local_package_install::{
        LocalPackageError, LocalPackageInstalled, ValidatedPackage, sys as base, write_file_under,
    };
    use super::super::signed_manifest::SignatureCheckedManifest;
    use super::{STAGING_PREFIX, StagingSweepReport};
    use crate::signed_root_binding::{SignedRootBindingError, SignedRootBindingStore};

    /// Our own trees are shallow (zip names are bounded); anything deeper is refused, not followed.
    const MAX_REMOVE_DEPTH: u32 = 64;

    fn cstr(name: &str) -> io::Result<CString> {
        CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Dir,
        Other,
    }

    /// `fstatat(AT_SYMLINK_NOFOLLOW)`: `Ok(None)` when absent.
    fn kind_nofollow(dir: &File, name: &str) -> io::Result<Option<Kind>> {
        let c = cstr(name)?;
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
        Ok(Some(if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            Kind::Dir
        } else {
            Kind::Other
        }))
    }

    fn unlinkat(dir: &File, name: &str, flags: i32) -> io::Result<()> {
        let c = cstr(name)?;
        // SAFETY: dirfd borrowed for the call, `c` NUL-terminated.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// UTF-8 entry names of an open directory (non-UTF-8 names are skipped; `.`/`..` excluded).
    fn list_dir(dir: &File) -> io::Result<Vec<String>> {
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
    fn remove_tree_at(parent: &File, name: &str, depth: u32) -> io::Result<()> {
        if depth > MAX_REMOVE_DEPTH {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        // openat adds O_NOFOLLOW: a symlink (or non-directory) named `name` fails here.
        let dir = base::openat(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        for entry in list_dir(&dir)? {
            match kind_nofollow(&dir, &entry)? {
                Some(Kind::Dir) => remove_tree_at(&dir, &entry, depth + 1)?,
                Some(Kind::Other) => unlinkat(&dir, &entry, 0)?,
                None => {}
            }
        }
        unlinkat(parent, name, libc::AT_REMOVEDIR)
    }

    /// `renameat` that never replaces an existing destination.
    fn rename_noreplace(dir: &File, from: &str, to: &str) -> io::Result<()> {
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

    fn nonce() -> Option<String> {
        let mut bytes = [0u8; 8];
        getrandom::getrandom(&mut bytes).ok()?;
        Some(hex::encode(bytes))
    }

    fn is_exists(err: &io::Error) -> bool {
        matches!(err.raw_os_error(), Some(libc::EEXIST) | Some(libc::ENOTEMPTY))
    }

    pub(super) fn install(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        parent: &File,
        name_component: &str,
        version_component: &str,
    ) -> Result<LocalPackageInstalled, LocalPackageError> {
        // Exclusive create tells us whether this call made the name directory (for tidy cleanup).
        let created_name = match base::mkdirat(parent, name_component, false) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
            Err(_) => return Err(LocalPackageError::RootCreateFailed),
        };
        let result = (|| {
            let name_dir = base::openat(parent, name_component, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|_| LocalPackageError::RootCreateFailed)?;
            stage_place_retain(store, checked, package, &name_dir, version_component)
        })();
        if result.is_err() && created_name {
            // rmdir only succeeds on an empty directory: never removes anything that has content.
            let _ = unlinkat(parent, name_component, libc::AT_REMOVEDIR);
        }
        result
    }

    fn stage_place_retain(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        name_dir: &File,
        version_component: &str,
    ) -> Result<LocalPackageInstalled, LocalPackageError> {
        match kind_nofollow(name_dir, version_component) {
            Ok(None) => {}
            Ok(Some(_)) => return Err(LocalPackageError::AlreadyInstalled),
            Err(_) => return Err(LocalPackageError::PlaceFailed),
        }
        // Exclusive staging directory for this attempt only.
        let mut staged: Option<(String, File)> = None;
        for _ in 0..8 {
            let wip = format!(
                "{STAGING_PREFIX}{}",
                nonce().ok_or(LocalPackageError::RootCreateFailed)?
            );
            match base::mkdirat(name_dir, &wip, false) {
                Ok(()) => {
                    let root = match base::openat(name_dir, &wip, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                        Ok(f) => f,
                        Err(_) => {
                            let _ = unlinkat(name_dir, &wip, libc::AT_REMOVEDIR);
                            return Err(LocalPackageError::RootCreateFailed);
                        }
                    };
                    staged = Some((wip, root));
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(_) => return Err(LocalPackageError::RootCreateFailed),
            }
        }
        let (wip, root) = staged.ok_or(LocalPackageError::RootCreateFailed)?;

        for (rel, bytes) in &package.files {
            if let Err(e) = write_file_under(&root, rel, bytes) {
                let _ = remove_tree_at(name_dir, &wip, 0);
                return Err(e);
            }
        }
        if let Err(e) = rename_noreplace(name_dir, &wip, version_component) {
            let _ = remove_tree_at(name_dir, &wip, 0);
            return Err(if is_exists(&e) {
                LocalPackageError::AlreadyInstalled
            } else {
                LocalPackageError::PlaceFailed
            });
        }
        let _ = name_dir.sync_all();
        // `root` is the staging inode, now named `version_component`. Switch only now.
        let key = checked.name().to_owned();
        let files_written = package.files.len();
        match store.install_new(&key, checked, root) {
            Ok(()) => Ok(LocalPackageInstalled { files_written }),
            Err(e) => {
                // We just created this version directory; remove it so a retry is not blocked.
                let _ = remove_tree_at(name_dir, version_component, 0);
                Err(match e {
                    SignedRootBindingError::AlreadyBound => LocalPackageError::AlreadyBound,
                    other => LocalPackageError::Retain(other),
                })
            }
        }
    }

    pub(super) fn sweep(parent: &File) -> StagingSweepReport {
        let mut report = StagingSweepReport::default();
        let Ok(names) = list_dir(parent) else {
            return report;
        };
        for name in names {
            // Name directories are never dot-prefixed (encode_component); leave Host entries alone.
            if name.starts_with('.') || !matches!(kind_nofollow(parent, &name), Ok(Some(Kind::Dir))) {
                continue;
            }
            let Ok(name_dir) = base::openat(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) else {
                continue;
            };
            let Ok(children) = list_dir(&name_dir) else {
                continue;
            };
            let mut removed_here = 0;
            for child in children {
                if !child.starts_with(STAGING_PREFIX)
                    || !matches!(kind_nofollow(&name_dir, &child), Ok(Some(Kind::Dir)))
                {
                    continue;
                }
                match remove_tree_at(&name_dir, &child, 0) {
                    Ok(()) => {
                        report.removed += 1;
                        removed_here += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            }
            if removed_here > 0 {
                // Only succeeds if now empty.
                let _ = unlinkat(parent, &name, libc::AT_REMOVEDIR);
            }
        }
        report
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;

    use super::super::local_package_install::{LocalPackageError, LocalPackageInstalled, ValidatedPackage};
    use super::super::signed_manifest::SignatureCheckedManifest;
    use super::StagingSweepReport;
    use crate::signed_root_binding::SignedRootBindingStore;

    pub(super) fn install(
        _store: &SignedRootBindingStore,
        _checked: SignatureCheckedManifest,
        _package: ValidatedPackage,
        _parent: &File,
        _name_component: &str,
        _version_component: &str,
    ) -> Result<LocalPackageInstalled, LocalPackageError> {
        Err(LocalPackageError::Unsupported)
    }

    pub(super) fn sweep(_parent: &File) -> StagingSweepReport {
        StagingSweepReport::default()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::imp;
    use super::*;
    use crate::hub::curated::{signature_payload, sri_sha512};
    use crate::hub::local_package_install::ValidatedPackage;
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::io::{Cursor, Write};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};
    use zip::write::SimpleFileOptions;

    const PANEL: &[u8] = b"<p>panel</p>";
    const CSS: &[u8] = b"p{}";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[29; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key().verifying_key())]),
        )
    }

    struct Pkg {
        name: String,
        version: String,
        raw: Vec<u8>,
        integrity: String,
        signature: String,
        zip: Vec<u8>,
    }

    fn pkg_with(name: &str, version: &str, extra: &[(&str, &[u8])], tamper_css: bool) -> Pkg {
        let raw = format!(
            r#"{{"name":{name:?},"version":{version:?},"files":[{{"path":"panels/board.html","integrity":"{}"}},{{"path":"panels/board.css","integrity":"{}"}}]}}"#,
            sri_sha512(PANEL),
            sri_sha512(CSS)
        )
        .into_bytes();
        let integrity = sri_sha512(&raw);
        let signature = hex::encode(
            key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let css: &[u8] = if tamper_css { b"evil{}" } else { CSS };
        let mut all: Vec<(&str, &[u8])> = vec![
            ("aion-extension.json", &raw),
            ("panels/board.html", PANEL),
            ("panels/board.css", css),
        ];
        all.extend_from_slice(extra);
        for (n, b) in all {
            w.start_file(n, SimpleFileOptions::default()).unwrap();
            w.write_all(b).unwrap();
        }
        let zip = w.finish().unwrap().into_inner();
        Pkg {
            name: name.into(),
            version: version.into(),
            raw,
            integrity,
            signature,
            zip,
        }
    }

    fn pkg(version: &str) -> Pkg {
        pkg_with("demo.plugin", version, &[], false)
    }

    fn source(p: &Pkg) -> LocalPackageSource<'_> {
        LocalPackageSource {
            entry: SignedManifestEntry {
                name: &p.name,
                version: &p.version,
                pubkey_id: Some("fixture"),
                integrity: Some(&p.integrity),
                signature: Some(&p.signature),
            },
            raw_manifest: &p.raw,
            package_zip: &p.zip,
        }
    }

    struct Env {
        tmp: tempfile::TempDir,
        parent_path: PathBuf,
        parent: HostExtensionsParent,
        store: SignedRootBindingStore,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let parent_path = tmp.path().join("extensions");
        std::fs::create_dir(&parent_path).unwrap();
        let parent = HostExtensionsParent::new(File::open(&parent_path).unwrap()).unwrap();
        Env {
            tmp,
            parent_path,
            parent,
            store: SignedRootBindingStore::new(),
        }
    }

    fn install(env: &Env, p: &Pkg) -> Result<LocalPackageInstalled, LocalPackageError> {
        install_local_signed_package_auto(
            &env.store,
            &policy(),
            &source(p),
            &env.parent,
            LocalPackageLimits::default(),
        )
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn assert_parent_empty(env: &Env) {
        assert!(names(&env.parent_path).is_empty(), "{:?}", names(&env.parent_path));
        assert!(env.store.is_empty());
    }

    fn decode(component: &str) -> String {
        let b = component.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'~' {
                out.push(u8::from_str_radix(&component[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn component_encoding_is_safe_injective_and_readable() {
        assert_eq!(encode_component("demo.plugin").as_deref(), Some("demo.plugin"));
        assert_eq!(encode_component("1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(encode_component("1.0.0+build.5").as_deref(), Some("1.0.0~2Bbuild.5"));
        assert_eq!(encode_component(".").as_deref(), Some("~2E"));
        assert_eq!(encode_component("..").as_deref(), Some("~2E."));
        assert_eq!(encode_component(".wip-x").as_deref(), Some("~2Ewip-x"));
        assert_eq!(encode_component("a/b\\c").as_deref(), Some("a~2Fb~5Cc"));
        assert_eq!(encode_component("a~b").as_deref(), Some("a~7Eb"));
        assert_eq!(encode_component("é").as_deref(), Some("~C3~A9"));
        assert!(encode_component("").is_none());
        let samples = [
            "demo", ".", "..", "a/b", "a~2Fb", "~", "é", "x y", ".hidden", "a.b", "a\u{1}b",
        ];
        let mut seen = HashSet::new();
        for s in samples {
            let c = encode_component(s).unwrap();
            assert!(!c.starts_with('.') && c != "." && c != "..", "{c}");
            assert!(
                c.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-~".contains(&b)),
                "{c}"
            );
            assert_eq!(decode(&c), s);
            assert!(seen.insert(c), "collision for {s}");
        }
        // Exactly at the limit is fine; one byte over fails closed (no truncation).
        assert!(encode_component(&"a".repeat(255)).is_some());
        assert!(encode_component(&"a".repeat(256)).is_none());
        assert!(encode_component(&format!("{}+", "a".repeat(253))).is_none());
    }

    #[test]
    fn parent_must_be_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("f");
        std::fs::write(&plain, b"x").unwrap();
        assert_eq!(
            HostExtensionsParent::new(File::open(&plain).unwrap()).unwrap_err(),
            LocalPackageError::ParentNotDirectory
        );
    }

    #[test]
    fn happy_path_uses_derived_name_version_layout_and_leaves_no_staging() {
        let env = env();
        let p = pkg("1.0.0");
        let out = install(&env, &p).unwrap();
        assert_eq!(out.files_written, 3);
        assert_eq!(names(&env.parent_path), vec!["demo.plugin"]);
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["1.0.0"]);
        let root = env.parent_path.join("demo.plugin/1.0.0");
        assert_eq!(std::fs::read(root.join("panels/board.css")).unwrap(), CSS);
        assert_eq!(std::fs::read(root.join("aion-extension.json")).unwrap(), p.raw);
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        let (snap, fd) = env.store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.version(), "1.0.0");
        let (a, b) = (fd.metadata().unwrap(), std::fs::metadata(&root).unwrap());
        assert_eq!(
            (a.dev(), a.ino()),
            (b.dev(), b.ino()),
            "retained handle is the placed inode"
        );
    }

    #[test]
    fn hostile_signed_identity_cannot_escape_the_parent() {
        let env = env();
        let p = pkg_with("../escape", "1.0.0+b/../../x", &[], false);
        // Semver rejects the version, so use a legal build-metadata version for the install itself.
        assert!(install(&env, &p).is_err());
        assert_parent_empty(&env);

        let p = pkg_with("../escape", "1.0.0+b.1", &[], false);
        install(&env, &p).unwrap();
        assert_eq!(names(&env.parent_path), vec!["~2E.~2Fescape"]);
        assert_eq!(names(&env.parent_path.join("~2E.~2Fescape")), vec!["1.0.0~2Bb.1"]);
        // Nothing was created beside `extensions/` inside the temp dir.
        assert_eq!(names(env.tmp.path()), vec!["extensions"]);
        assert!(env.store.contains("../escape"));
    }

    #[test]
    fn failures_before_staging_create_nothing() {
        let env = env();
        let mut bad_sig = pkg("1.0.0");
        bad_sig.signature = "0".repeat(128);
        assert!(matches!(install(&env, &bad_sig), Err(LocalPackageError::Signature(_))));
        assert_parent_empty(&env);
        assert_eq!(
            install(&env, &pkg_with("demo.plugin", "1.0.0", &[], true)),
            Err(LocalPackageError::IntegrityMismatch)
        );
        assert_parent_empty(&env);
        assert_eq!(
            install(&env, &pkg_with("demo.plugin", "1.0.0", &[("x.css", b"x")], false)),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_parent_empty(&env);
        let long = pkg_with(&"n".repeat(256), "1.0.0", &[], false);
        assert_eq!(install(&env, &long), Err(LocalPackageError::InvalidDirName));
        assert_parent_empty(&env);
    }

    #[test]
    fn same_version_twice_is_refused_and_original_is_untouched() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let fresh_store = SignedRootBindingStore::new();
        let again = install_local_signed_package_auto(
            &fresh_store,
            &policy(),
            &source(&pkg("1.0.0")),
            &env.parent,
            LocalPackageLimits::default(),
        );
        assert_eq!(again, Err(LocalPackageError::AlreadyInstalled));
        assert!(fresh_store.is_empty());
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["1.0.0"]);
        assert_eq!(
            std::fs::read(env.parent_path.join("demo.plugin/1.0.0/panels/board.css")).unwrap(),
            CSS
        );
    }

    #[test]
    fn live_binding_is_never_switched_and_other_versions_are_not_written() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        assert_eq!(install(&env, &pkg("2.0.0")), Err(LocalPackageError::AlreadyBound));
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["1.0.0"]);
        assert_eq!(env.store.lookup_checked("demo.plugin").unwrap().version(), "1.0.0");
    }

    #[test]
    fn symlinks_named_like_name_or_version_dirs_are_not_followed() {
        let env = env();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), env.parent_path.join("demo.plugin")).unwrap();
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::RootCreateFailed));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(env.store.is_empty());
        std::fs::remove_file(env.parent_path.join("demo.plugin")).unwrap();

        std::fs::create_dir(env.parent_path.join("demo.plugin")).unwrap();
        std::os::unix::fs::symlink(outside.path(), env.parent_path.join("demo.plugin/1.0.0")).unwrap();
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::AlreadyInstalled));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        // A name directory we did not create is left in place, with no staging left in it.
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["1.0.0"]);
        assert!(env.store.is_empty());
    }

    fn validated(files: Vec<(&str, &[u8])>) -> ValidatedPackage {
        ValidatedPackage {
            files: files.into_iter().map(|(p, b)| (p.to_owned(), b.to_vec())).collect(),
        }
    }

    fn checked_for(p: &Pkg) -> crate::hub::signed_manifest::SignatureCheckedManifest {
        check_signed_manifest(&policy(), &source(p).entry, &p.raw).unwrap()
    }

    #[test]
    fn write_failure_removes_only_this_attempts_staging_and_a_created_name_dir() {
        let env = env();
        let p = pkg("1.0.0");
        // "a" is written as a file, then "a/b" needs "a" as a directory => WriteFailed mid-extract.
        let package = validated(vec![("a", b"1"), ("a/b", b"2")]);
        let dir = File::open(&env.parent_path).unwrap();
        let r = imp::install(&env.store, checked_for(&p), package, &dir, "demo.plugin", "1.0.0");
        assert_eq!(r.unwrap_err(), LocalPackageError::WriteFailed);
        assert_parent_empty(&env);

        // Pre-existing name dir with a sibling version: keep both, drop only the staging.
        std::fs::create_dir_all(env.parent_path.join("demo.plugin/0.9.0")).unwrap();
        let package = validated(vec![("a", b"1"), ("a/b", b"2")]);
        let r = imp::install(&env.store, checked_for(&p), package, &dir, "demo.plugin", "1.0.0");
        assert_eq!(r.unwrap_err(), LocalPackageError::WriteFailed);
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["0.9.0"]);
    }

    #[test]
    fn binding_failure_after_placement_removes_the_new_version_dir_only() {
        let env = env();
        let p = pkg("1.0.0");
        // Simulate a racing binder: the early public check is bypassed by calling imp directly.
        env.store
            .install_new("demo.plugin", checked_for(&p), File::open(&env.parent_path).unwrap())
            .unwrap();
        std::fs::create_dir_all(env.parent_path.join("demo.plugin/0.9.0")).unwrap();
        let dir = File::open(&env.parent_path).unwrap();
        let package = validated(vec![("aion-extension.json", &p.raw)]);
        let r = imp::install(&env.store, checked_for(&p), package, &dir, "demo.plugin", "1.0.0");
        assert_eq!(r.unwrap_err(), LocalPackageError::AlreadyBound);
        assert_eq!(names(&env.parent_path.join("demo.plugin")), vec!["0.9.0"]);
        assert_eq!(env.store.len(), 1);
    }

    #[test]
    fn sweep_removes_only_wip_directories_and_never_follows_symlinks() {
        let env = env();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious"), b"keep").unwrap();
        let root = &env.parent_path;
        // Crashed attempt with nested content and a symlink pointing outside.
        let wip = root.join("a/.wip-deadbeef");
        std::fs::create_dir_all(wip.join("x/y")).unwrap();
        std::fs::write(wip.join("x/y/f"), b"f").unwrap();
        std::os::unix::fs::symlink(outside.path(), wip.join("x/link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("precious"), wip.join("filelink")).unwrap();
        // Things that must survive.
        std::fs::create_dir_all(root.join("a/1.0.0")).unwrap();
        std::fs::write(root.join("a/1.0.0/keep"), b"k").unwrap();
        std::fs::write(root.join("a/.wip-regular-file"), b"not a dir").unwrap();
        std::fs::create_dir_all(root.join(".hidden/.wip-x")).unwrap();
        std::fs::create_dir_all(root.join("b/.old-1")).unwrap();
        std::fs::write(root.join("top-level-file"), b"t").unwrap();
        std::fs::create_dir_all(outside.path().join(".wip-z")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("linkdir")).unwrap();
        // Name dir that only held a staging dir disappears once empty.
        std::fs::create_dir_all(root.join("c/.wip-1")).unwrap();

        let report = sweep_abandoned_staging(&env.parent);
        assert_eq!(report, StagingSweepReport { removed: 2, failed: 0 });
        assert!(!wip.exists());
        assert!(!root.join("c").exists());
        assert!(root.join("a/1.0.0/keep").exists());
        assert!(root.join("a/.wip-regular-file").exists());
        assert!(root.join(".hidden/.wip-x").exists());
        assert!(root.join("b/.old-1").exists());
        assert!(root.join("top-level-file").exists());
        assert_eq!(std::fs::read(outside.path().join("precious")).unwrap(), b"keep");
        assert!(outside.path().join(".wip-z").exists(), "symlinked dir was not entered");
        assert!(
            root.join("linkdir")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn install_then_sweep_leaves_the_installed_version_alone() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        assert_eq!(sweep_abandoned_staging(&env.parent), StagingSweepReport::default());
        assert!(env.parent_path.join("demo.plugin/1.0.0/aion-extension.json").exists());
    }
}

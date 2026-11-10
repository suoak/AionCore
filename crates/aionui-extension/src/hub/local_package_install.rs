//! Host-owned install of an **already-local**, signature-checked package archive (M9 E13).
//!
//! Gives later checked-source loads an *honest* trusted root: the Host creates a brand-new
//! directory with `mkdirat` relative to a Host-held parent directory handle and keeps the
//! `openat` handle returned for that very creation (no path string is ever reopened), writes
//! only verified bytes through `openat(O_NOFOLLOW|O_EXCL)` relative to that handle, and retains
//! `(SignatureCheckedManifest, root File)` in the [`SignedRootBindingStore`].
//!
//! # What the signature covers (and therefore what is extracted)
//! `check_signed_manifest` signs the manifest bytes only. The only other bytes the signed
//! manifest can vouch for are files whose expected SRI it declares: `entryPointIntegrity`
//! (panels, workPanelViews, backgroundServices) and `handlerIntegrity` (agentTools). So the
//! package is **strict allow-list**: the manifest (byte-identical to the signed bytes) plus
//! those integrity-declared files (each SRI-verified). Any other file entry is
//! `EntryNotCoveredBySignature`; a missing covered file is `CoveredFileMissing`. Packages that
//! ship css/icons/etc. are therefore rejected until a signed file list exists.
//!
//! # Fail-closed validation (before any filesystem effect)
//! Non-canonical/absolute/`..`/backslash/NUL names, symlink or special-mode entries, duplicate
//! names (also case-folded), entry/size budgets, manifest mismatch and SRI mismatch all reject
//! *before* the root is created. Zip has no hard-link entry type; the Host only ever creates
//! regular files with `O_EXCL`, so an archive cannot produce a hard link or symlink.
//!
//! # Not
//! Not wired to any route, no network/download, no exec, no permission/plan-safe change, not
//! execution authorization, not root/owner/curation proof beyond Host possession of `parent`.
//! The caller supplies `parent`: a Host-opened directory handle. On an I/O failure after
//! creation the partially written root is left in place (no deletion in this slice) and is
//! never retained. Unix only; other targets return `Unsupported`.
//! Fixed error categories: never digests, paths or manifest text.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{Cursor, Read};

use super::curated::{CuratedInstallPolicy, sri_sha512};
use super::signed_manifest::{
    SignatureCheckedManifest, SignedManifestEntry, SignedManifestError, check_signed_manifest,
};
use crate::asset_paths::normalize_relative_asset_path;
use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::manifest::parse_manifest;
use crate::signed_root_binding::{SignedRootBindingError, SignedRootBindingStore};

/// Hard bounds applied before/while reading the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPackageLimits {
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for LocalPackageLimits {
    fn default() -> Self {
        Self {
            max_entries: 256,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Fixed categories only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPackageError {
    Signature(SignedManifestError),
    InvalidPackage,
    LimitExceeded,
    UnsafeEntryName,
    DuplicateEntry,
    SymlinkOrSpecialEntry,
    EntryNotCoveredBySignature,
    CoveredFileMissing,
    InvalidCoveredPath,
    ConflictingCoveredDigest,
    ManifestMismatch,
    IntegrityMismatch,
    InvalidDirName,
    ParentNotDirectory,
    RootCreateFailed,
    WriteFailed,
    Retain(SignedRootBindingError),
    Unsupported,
}

/// Already-local inputs: the signed entry metadata, the raw manifest bytes it signs, and the
/// package archive bytes. No path or URL: the Host must already hold these in memory.
pub struct LocalPackageSource<'a> {
    pub entry: SignedManifestEntry<'a>,
    pub raw_manifest: &'a [u8],
    pub package_zip: &'a [u8],
}

/// Result of a successful install (no paths/handles exposed; the root lives in the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPackageInstalled {
    pub files_written: usize,
}

/// Verified, fully validated package contents ready to be written.
struct ValidatedPackage {
    /// `(relative path with '/', verified bytes)` — includes the manifest.
    files: Vec<(String, Vec<u8>)>,
}

fn canonical_relative(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return None;
    }
    let normalized = normalize_relative_asset_path(name)?;
    let joined = normalized.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
    // Exact canonical form only (rejects "./a", "a//b", "a/./b").
    (joined == name).then_some(joined)
}

/// Integrity-covered files declared by the signed manifest: path -> expected SRI.
fn covered_files(checked: &SignatureCheckedManifest) -> Result<BTreeMap<String, String>, LocalPackageError> {
    let manifest = parse_manifest(checked.raw_bytes())
        .map_err(|_| LocalPackageError::Signature(SignedManifestError::InvalidManifest))?;
    let mut declared: Vec<(String, String)> = Vec::new();
    if let Some(c) = manifest.contributes.as_ref() {
        for p in &c.panels {
            if let Some(i) = p.entry_point_integrity.as_ref() {
                declared.push((p.entry_point.clone(), i.clone()));
            }
        }
        for v in &c.work_panel_views {
            if let Some(i) = v.entry_point_integrity.as_ref() {
                declared.push((v.entry_point.clone(), i.clone()));
            }
        }
        for s in &c.background_services {
            if let Some(i) = s.entry_point_integrity.as_ref() {
                declared.push((s.entry_point.clone(), i.clone()));
            }
        }
        for t in &c.agent_tools {
            if let Some(i) = t.handler_integrity.as_ref() {
                let handler = t.handler.clone().unwrap_or_default();
                declared.push((handler, i.clone()));
            }
        }
    }
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for (path, sri) in declared {
        let rel = canonical_relative(&path).ok_or(LocalPackageError::InvalidCoveredPath)?;
        if rel == EXTENSION_MANIFEST_FILE {
            return Err(LocalPackageError::InvalidCoveredPath);
        }
        match out.get(&rel) {
            Some(existing) if *existing != sri => return Err(LocalPackageError::ConflictingCoveredDigest),
            _ => {
                out.insert(rel, sri);
            }
        }
    }
    // No covered path may be a directory prefix of another covered path (file vs dir clash).
    for a in out.keys() {
        for b in out.keys() {
            if a != b && b.starts_with(&format!("{a}/")) {
                return Err(LocalPackageError::InvalidCoveredPath);
            }
        }
    }
    Ok(out)
}

fn validate_package(
    checked: &SignatureCheckedManifest,
    package_zip: &[u8],
    limits: LocalPackageLimits,
) -> Result<ValidatedPackage, LocalPackageError> {
    let covered = covered_files(checked)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(package_zip)).map_err(|_| LocalPackageError::InvalidPackage)?;
    if archive.len() > limits.max_entries {
        return Err(LocalPackageError::LimitExceeded);
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    let mut manifest_seen = false;

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|_| LocalPackageError::InvalidPackage)?;
        let raw_name = entry
            .name()
            .map_err(|_| LocalPackageError::UnsafeEntryName)?
            .into_owned();
        let is_dir = entry.is_dir();
        let bare = raw_name.strip_suffix('/').unwrap_or(&raw_name);
        let rel = canonical_relative(bare).ok_or(LocalPackageError::UnsafeEntryName)?;
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(LocalPackageError::SymlinkOrSpecialEntry);
            }
        }
        if !seen.insert(rel.to_lowercase()) {
            return Err(LocalPackageError::DuplicateEntry);
        }
        if is_dir {
            continue;
        }
        let is_manifest = rel == EXTENSION_MANIFEST_FILE;
        let expected_sri = covered.get(&rel);
        if !is_manifest && expected_sri.is_none() {
            return Err(LocalPackageError::EntryNotCoveredBySignature);
        }
        let declared = entry.size();
        if declared > limits.max_file_bytes {
            return Err(LocalPackageError::LimitExceeded);
        }
        total = total.saturating_add(declared);
        if total > limits.max_total_bytes {
            return Err(LocalPackageError::LimitExceeded);
        }
        let mut bytes = Vec::new();
        Read::take(&mut entry, limits.max_file_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| LocalPackageError::InvalidPackage)?;
        if bytes.len() as u64 != declared {
            return Err(LocalPackageError::InvalidPackage);
        }
        if is_manifest {
            if bytes != checked.raw_bytes() {
                return Err(LocalPackageError::ManifestMismatch);
            }
            manifest_seen = true;
        } else if let Some(sri) = expected_sri
            && sri_sha512(&bytes) != *sri
        {
            return Err(LocalPackageError::IntegrityMismatch);
        }
        files.push((rel, bytes));
    }
    if !manifest_seen {
        return Err(LocalPackageError::ManifestMismatch);
    }
    if covered.keys().any(|k| !files.iter().any(|(p, _)| p == k)) {
        return Err(LocalPackageError::CoveredFileMissing);
    }
    Ok(ValidatedPackage { files })
}

fn valid_root_dir_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

#[cfg(unix)]
mod sys {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};

    fn cstr(name: &str) -> io::Result<CString> {
        CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// `mkdirat(dir, name, 0700)`; `exist_ok` tolerates `EEXIST` (caller still verifies via openat).
    pub fn mkdirat(dir: &File, name: &str, exist_ok: bool) -> io::Result<()> {
        let c = cstr(name)?;
        // SAFETY: valid dirfd borrowed for the call; `c` is a NUL-terminated owned CString.
        let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o700) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if exist_ok && err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        Err(err)
    }

    /// `openat` returning an owned `File`. Always adds `O_CLOEXEC | O_NOFOLLOW`.
    pub fn openat(dir: &File, name: &str, flags: i32, mode: u32) -> io::Result<File> {
        let c = cstr(name)?;
        // SAFETY: valid dirfd borrowed for the call; `c` is NUL-terminated; mode is a plain int.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat returned a new fd that nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn create_root(parent: &File, dir_name: &str) -> Result<File, LocalPackageError> {
    // Exclusive: EEXIST (including a symlink or file of that name) is a failure.
    sys::mkdirat(parent, dir_name, false).map_err(|_| LocalPackageError::RootCreateFailed)?;
    sys::openat(parent, dir_name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|_| LocalPackageError::RootCreateFailed)
}

#[cfg(unix)]
fn write_file_under(root: &File, rel: &str, bytes: &[u8]) -> Result<(), LocalPackageError> {
    use std::io::Write;
    let parts: Vec<&str> = rel.split('/').collect();
    let (last, dirs) = parts.split_last().ok_or(LocalPackageError::WriteFailed)?;
    let mut current = root.try_clone().map_err(|_| LocalPackageError::WriteFailed)?;
    for dir in dirs {
        sys::mkdirat(&current, dir, true).map_err(|_| LocalPackageError::WriteFailed)?;
        // O_DIRECTORY|O_NOFOLLOW: a pre-existing non-directory/symlink here fails.
        current = sys::openat(&current, dir, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|_| LocalPackageError::WriteFailed)?;
    }
    let mut file = sys::openat(&current, last, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
        .map_err(|_| LocalPackageError::WriteFailed)?;
    file.write_all(bytes).map_err(|_| LocalPackageError::WriteFailed)?;
    file.sync_all().map_err(|_| LocalPackageError::WriteFailed)
}

/// Check the signature, validate the whole archive, create a fresh Host-owned root under
/// `parent`, write the verified files and retain the root in `store`.
///
/// `parent` must be a Host-opened directory handle; `root_dir_name` a single safe component
/// (`[A-Za-z0-9._-]`, not `.`/`..`) that must **not** already exist. Nothing is created unless the
/// signature and every archive entry validate. Nothing is retained on any error.
pub fn install_local_signed_package(
    store: &SignedRootBindingStore,
    policy: &CuratedInstallPolicy,
    source: &LocalPackageSource<'_>,
    parent: &File,
    root_dir_name: &str,
    limits: LocalPackageLimits,
) -> Result<LocalPackageInstalled, LocalPackageError> {
    let checked =
        check_signed_manifest(policy, &source.entry, source.raw_manifest).map_err(LocalPackageError::Signature)?;
    let package = validate_package(&checked, source.package_zip, limits)?;
    if !valid_root_dir_name(root_dir_name) {
        return Err(LocalPackageError::InvalidDirName);
    }
    if !parent
        .metadata()
        .map(|m| m.is_dir())
        .map_err(|_| LocalPackageError::ParentNotDirectory)?
    {
        return Err(LocalPackageError::ParentNotDirectory);
    }
    install_validated(store, checked, package, parent, root_dir_name)
}

#[cfg(unix)]
fn install_validated(
    store: &SignedRootBindingStore,
    checked: SignatureCheckedManifest,
    package: ValidatedPackage,
    parent: &File,
    root_dir_name: &str,
) -> Result<LocalPackageInstalled, LocalPackageError> {
    let root = create_root(parent, root_dir_name)?;
    for (rel, bytes) in &package.files {
        write_file_under(&root, rel, bytes)?;
    }
    let key = checked.name().to_owned();
    store.install(&key, checked, root).map_err(LocalPackageError::Retain)?;
    Ok(LocalPackageInstalled {
        files_written: package.files.len(),
    })
}

#[cfg(not(unix))]
fn install_validated(
    _store: &SignedRootBindingStore,
    _checked: SignatureCheckedManifest,
    _package: ValidatedPackage,
    _parent: &File,
    _root_dir_name: &str,
) -> Result<LocalPackageInstalled, LocalPackageError> {
    Err(LocalPackageError::Unsupported)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hub::curated::signature_payload;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::HashMap;
    use std::io::Write;
    use std::os::unix::fs::MetadataExt;
    use zip::write::SimpleFileOptions;

    const PANEL: &[u8] = b"<p>panel</p>";
    const HANDLER: &[u8] = b"handler();";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[23; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key().verifying_key())]),
        )
    }

    fn manifest_json() -> Vec<u8> {
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html","entryPointIntegrity":"{}"}}],"agentTools":[{{"id":"t","name":"T","planSafeActions":true,"handler":"tools/run.js","handlerIntegrity":"{}"}}]}}}}"#,
            sri_sha512(PANEL),
            sri_sha512(HANDLER)
        )
        .into_bytes()
    }

    struct Fixture {
        raw: Vec<u8>,
        integrity: String,
        signature: String,
    }

    fn fixture(raw: Vec<u8>) -> Fixture {
        let integrity = sri_sha512(&raw);
        let signature = hex::encode(
            key()
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        Fixture {
            raw,
            integrity,
            signature,
        }
    }

    fn entry(f: &Fixture) -> SignedManifestEntry<'_> {
        SignedManifestEntry {
            name: "demo.plugin",
            version: "1.0.0",
            pubkey_id: Some("fixture"),
            integrity: Some(&f.integrity),
            signature: Some(&f.signature),
        }
    }

    fn source<'a>(f: &'a Fixture, zip: &'a [u8]) -> LocalPackageSource<'a> {
        LocalPackageSource {
            entry: entry(f),
            raw_manifest: &f.raw,
            package_zip: zip,
        }
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, SimpleFileOptions::default()).unwrap();
            } else {
                w.start_file(*name, SimpleFileOptions::default()).unwrap();
                w.write_all(bytes).unwrap();
            }
        }
        w.finish().unwrap().into_inner()
    }

    fn good_entries(raw: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("panels/", vec![]),
            ("aion-extension.json", raw.to_vec()),
            ("panels/board.html", PANEL.to_vec()),
            ("tools/run.js", HANDLER.to_vec()),
        ]
    }

    fn zip_owned(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let borrowed: Vec<(&str, &[u8])> = entries.iter().map(|(n, b)| (*n, b.as_slice())).collect();
        zip_of(&borrowed)
    }

    struct Env {
        _tmp: tempfile::TempDir,
        parent_path: std::path::PathBuf,
        parent: File,
        store: SignedRootBindingStore,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let parent_path = tmp.path().to_path_buf();
        let parent = File::open(&parent_path).unwrap();
        Env {
            _tmp: tmp,
            parent_path,
            parent,
            store: SignedRootBindingStore::new(),
        }
    }

    fn run(
        env: &Env,
        f: &Fixture,
        zip: &[u8],
        dir: &str,
        limits: LocalPackageLimits,
    ) -> Result<LocalPackageInstalled, LocalPackageError> {
        install_local_signed_package(&env.store, &policy(), &source(f, zip), &env.parent, dir, limits)
    }

    fn assert_nothing_created(env: &Env) {
        assert_eq!(std::fs::read_dir(&env.parent_path).unwrap().count(), 0);
        assert!(env.store.is_empty());
    }

    #[test]
    fn happy_path_creates_host_root_writes_only_covered_files_and_retains() {
        let env = env();
        let f = fixture(manifest_json());
        let zip = zip_owned(&good_entries(&f.raw));
        let out = run(&env, &f, &zip, "demo-1.0.0", LocalPackageLimits::default()).unwrap();
        assert_eq!(out.files_written, 3);
        let root_path = env.parent_path.join("demo-1.0.0");
        assert_eq!(std::fs::read(root_path.join("panels/board.html")).unwrap(), PANEL);
        assert_eq!(std::fs::read(root_path.join("tools/run.js")).unwrap(), HANDLER);
        assert_eq!(std::fs::read(root_path.join("aion-extension.json")).unwrap(), f.raw);
        assert_eq!(std::fs::metadata(&root_path).unwrap().mode() & 0o777, 0o700);
        // Retained root is the very directory the Host created (same dev/ino), not a reopened string.
        let (snap, root) = env.store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.name(), "demo.plugin");
        let a = root.metadata().unwrap();
        let b = std::fs::metadata(&root_path).unwrap();
        assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
    }

    #[test]
    fn bad_signature_creates_nothing() {
        let env = env();
        let mut f = fixture(manifest_json());
        f.signature = "0".repeat(128);
        let zip = zip_owned(&good_entries(&f.raw));
        assert!(matches!(
            run(&env, &f, &zip, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::Signature(_))
        ));
        assert_nothing_created(&env);
    }

    #[test]
    fn manifest_in_archive_must_equal_signed_bytes() {
        let env = env();
        let f = fixture(manifest_json());
        let mut entries = good_entries(&f.raw);
        entries[1].1 = [f.raw.as_slice(), b" "].concat();
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::ManifestMismatch)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn uncovered_extra_file_is_rejected_without_effects() {
        let env = env();
        let f = fixture(manifest_json());
        let mut entries = good_entries(&f.raw);
        entries.push(("panels/extra.css", b"p{}".to_vec()));
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn integrity_mismatch_and_missing_covered_file_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        let mut tampered = good_entries(&f.raw);
        tampered[2].1 = b"<p>evil</p>".to_vec();
        assert_eq!(
            run(&env, &f, &zip_owned(&tampered), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::IntegrityMismatch)
        );
        let mut missing = good_entries(&f.raw);
        missing.remove(3);
        assert_eq!(
            run(&env, &f, &zip_owned(&missing), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::CoveredFileMissing)
        );
        let no_manifest: Vec<_> = good_entries(&f.raw)
            .into_iter()
            .filter(|(n, _)| *n != "aion-extension.json")
            .collect();
        assert_eq!(
            run(&env, &f, &zip_owned(&no_manifest), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::ManifestMismatch)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn unsafe_entry_names_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        for bad in [
            "../evil.js",
            "/abs.js",
            "a\\b.js",
            "./panels/board.html",
            "panels//board.html",
            "panels/../x",
        ] {
            let mut entries = good_entries(&f.raw);
            entries.push((bad, b"x".to_vec()));
            assert_eq!(
                run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
                Err(LocalPackageError::UnsafeEntryName),
                "name {bad}"
            );
        }
        assert_nothing_created(&env);
    }

    #[test]
    fn symlink_entry_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("aion-extension.json", SimpleFileOptions::default())
            .unwrap();
        w.write_all(&f.raw).unwrap();
        w.add_symlink("panels/board.html", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        let zip = w.finish().unwrap().into_inner();
        assert_eq!(
            run(&env, &f, &zip, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::SymlinkOrSpecialEntry)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn case_folded_duplicate_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        let mut entries = good_entries(&f.raw);
        entries.push(("Panels/Board.html", PANEL.to_vec()));
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::DuplicateEntry)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn limits_and_invalid_archives_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        let zip = zip_owned(&good_entries(&f.raw));
        let tiny = LocalPackageLimits {
            max_file_bytes: 4,
            ..LocalPackageLimits::default()
        };
        assert_eq!(run(&env, &f, &zip, "d", tiny), Err(LocalPackageError::LimitExceeded));
        let few = LocalPackageLimits {
            max_entries: 2,
            ..LocalPackageLimits::default()
        };
        assert_eq!(run(&env, &f, &zip, "d", few), Err(LocalPackageError::LimitExceeded));
        assert_eq!(
            run(&env, &f, b"not a zip", "d", LocalPackageLimits::default()),
            Err(LocalPackageError::InvalidPackage)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn invalid_dir_names_existing_dir_and_non_directory_parent_rejected() {
        let env = env();
        let f = fixture(manifest_json());
        let zip = zip_owned(&good_entries(&f.raw));
        for bad in ["", ".", "..", "a/b", "../x", "a b", "x\0y"] {
            assert_eq!(
                run(&env, &f, &zip, bad, LocalPackageLimits::default()),
                Err(LocalPackageError::InvalidDirName),
                "name {bad:?}"
            );
        }
        assert_nothing_created(&env);

        std::fs::create_dir(env.parent_path.join("taken")).unwrap();
        std::fs::write(env.parent_path.join("taken/keep"), b"k").unwrap();
        assert_eq!(
            run(&env, &f, &zip, "taken", LocalPackageLimits::default()),
            Err(LocalPackageError::RootCreateFailed)
        );
        assert_eq!(std::fs::read(env.parent_path.join("taken/keep")).unwrap(), b"k");
        assert!(env.store.is_empty());

        let file_parent_dir = tempfile::tempdir().unwrap();
        let plain = file_parent_dir.path().join("plain");
        std::fs::write(&plain, b"x").unwrap();
        let parent_file = File::open(&plain).unwrap();
        let err = install_local_signed_package(
            &env.store,
            &policy(),
            &source(&f, &zip),
            &parent_file,
            "d",
            LocalPackageLimits::default(),
        );
        assert_eq!(err, Err(LocalPackageError::ParentNotDirectory));
    }

    #[test]
    fn existing_symlink_named_like_root_is_not_followed() {
        let env = env();
        let f = fixture(manifest_json());
        let zip = zip_owned(&good_entries(&f.raw));
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), env.parent_path.join("link")).unwrap();
        assert_eq!(
            run(&env, &f, &zip, "link", LocalPackageLimits::default()),
            Err(LocalPackageError::RootCreateFailed)
        );
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(env.store.is_empty());
    }

    #[test]
    fn covered_path_conflicts_rejected() {
        let env = env();
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"a","entryPointIntegrity":"{}"}},{{"id":"q","title":"Q","entryPoint":"a/b","entryPointIntegrity":"{}"}}]}}}}"#,
            sri_sha512(b"1"),
            sri_sha512(b"2")
        )
        .into_bytes();
        let f = fixture(raw);
        let zip = zip_owned(&[("aion-extension.json", f.raw.clone()), ("a", b"1".to_vec())]);
        assert_eq!(
            run(&env, &f, &zip, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::InvalidCoveredPath)
        );
        assert_nothing_created(&env);
    }
}

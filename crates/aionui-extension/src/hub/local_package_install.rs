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
//! manifest can vouch for are files whose expected SRI it declares. The package is a
//! **strict allow-list** of exactly one of two sets, chosen by the signed manifest itself:
//!
//! * **`files` declared (non-empty):** the manifest (byte-identical to the signed bytes) plus
//!   **exactly** the paths in the top-level `files` list, each SRI-verified. Manifest validation
//!   already guarantees every `entryPointIntegrity` / `handlerIntegrity` is also in `files` with
//!   the same SRI, so `files` is the single authoring list (css/icons/assets ship this way).
//! * **`files` omitted or empty (legacy):** the manifest plus only the files whose SRI the
//!   contributions declare (`entryPointIntegrity` for panels / workPanelViews /
//!   backgroundServices, `handlerIntegrity` for agentTools), exactly as before. Existing signed
//!   packages keep installing; a package that wants other assets must declare them in `files`.
//!
//! Either way any other file entry is `EntryNotCoveredBySignature`, and a missing listed or
//! declared file is `CoveredFileMissing`. There is never a union with "looks like an asset"
//! extras and no fallback from one mode to the other.
//!
//! # Verification order
//! Each allow-listed entry is read once (bounded) and its SRI checked against those exact bytes
//! during whole-archive validation; only the verified bytes are written afterwards. A mismatch
//! rejects before the root is created.
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
use crate::asset_paths::canonical_relative_path;
use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::manifest::parse_manifest;
use crate::signed_root_binding::{SignedRootBindingError, SignedRootBindingStore};

/// Hard bounds applied before/while reading the archive.
///
/// Defaults (512 entries / 8 MiB per file / 32 MiB total) fit a typical css + icon package; they
/// are still bounded, and the Host may pass tighter or looser values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalPackageLimits {
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for LocalPackageLimits {
    fn default() -> Self {
        Self {
            max_entries: 512,
            max_file_bytes: 8 * 1024 * 1024,
            max_total_bytes: 32 * 1024 * 1024,
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
    /// Host auto-install: placing the finished staging directory failed.
    PlaceFailed,
    /// Host auto-install: the version slot holds something that is not a real directory.
    VersionSlotBlocked,
    /// Host auto-install: an older version than the installed one, without `allow_downgrade`.
    DowngradeRefused,
    /// Host auto-install: the `current` marker could not be switched; the change was undone.
    MarkerWriteFailed,
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
pub(super) struct ValidatedPackage {
    /// `(relative path with '/', verified bytes)` — includes the manifest.
    pub(super) files: Vec<(String, Vec<u8>)>,
}

fn canonical_relative(name: &str) -> Option<String> {
    canonical_relative_path(name)
}

/// Allow-listed non-manifest files declared by the signed manifest: path -> expected SRI.
///
/// Non-empty top-level `files` is the sole list; otherwise the legacy contribution-declared set.
pub(super) fn covered_files(checked: &SignatureCheckedManifest) -> Result<BTreeMap<String, String>, LocalPackageError> {
    let manifest = parse_manifest(checked.raw_bytes())
        .map_err(|_| LocalPackageError::Signature(SignedManifestError::InvalidManifest))?;
    let declared: Vec<(String, String)> = if manifest.files.is_empty() {
        legacy_contribution_declarations(&manifest)
    } else {
        manifest
            .files
            .iter()
            .map(|f| (f.path.clone(), f.integrity.clone()))
            .collect()
    };
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

/// Legacy (no `files`) coverage: `(path, SRI)` for each contribution integrity declaration.
fn legacy_contribution_declarations(manifest: &crate::types::ExtensionManifest) -> Vec<(String, String)> {
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
    declared
}

pub(super) fn validate_package(
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
pub(super) mod sys {
    use std::ffi::CString;
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};

    fn cstr(name: &str) -> io::Result<CString> {
        CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// `mkdirat(dir, name, 0700)`; `exist_ok` tolerates `EEXIST` (caller still verifies via openat).
    pub(in crate::hub) fn mkdirat(dir: &File, name: &str, exist_ok: bool) -> io::Result<()> {
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
    pub(in crate::hub) fn openat(dir: &File, name: &str, flags: i32, mode: u32) -> io::Result<File> {
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
pub(super) fn write_file_under(root: &File, rel: &str, bytes: &[u8]) -> Result<(), LocalPackageError> {
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

    // -- slice B: top-level `files` is the install allow-list --

    const CSS: &[u8] = b"p{color:red}";
    const ICON: &[u8] = b"<svg/>";

    /// Manifest listing panel + handler (contribution-declared, so they MUST be in `files`) + css + icon.
    fn files_manifest_json(extra: &[(&str, &[u8])]) -> Vec<u8> {
        let mut list = vec![
            ("panels/board.html", PANEL),
            ("tools/run.js", HANDLER),
            ("panels/board.css", CSS),
            ("icons/logo.svg", ICON),
        ];
        list.extend_from_slice(extra);
        let files: Vec<String> = list
            .iter()
            .map(|(p, b)| format!(r#"{{"path":"{p}","integrity":"{}"}}"#, sri_sha512(b)))
            .collect();
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{}],"contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html","entryPointIntegrity":"{}"}}],"agentTools":[{{"id":"t","name":"T","planSafeActions":true,"handler":"tools/run.js","handlerIntegrity":"{}"}}]}}}}"#,
            files.join(","),
            sri_sha512(PANEL),
            sri_sha512(HANDLER)
        )
        .into_bytes()
    }

    fn files_entries(raw: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("aion-extension.json", raw.to_vec()),
            ("panels/board.html", PANEL.to_vec()),
            ("tools/run.js", HANDLER.to_vec()),
            ("panels/board.css", CSS.to_vec()),
            ("icons/logo.svg", ICON.to_vec()),
        ]
    }

    #[test]
    fn files_list_installs_css_and_icons_with_verified_bytes() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        let zip = zip_owned(&files_entries(&f.raw));
        let out = run(&env, &f, &zip, "demo-files", LocalPackageLimits::default()).unwrap();
        assert_eq!(out.files_written, 5);
        let root = env.parent_path.join("demo-files");
        assert_eq!(std::fs::read(root.join("panels/board.css")).unwrap(), CSS);
        assert_eq!(std::fs::read(root.join("icons/logo.svg")).unwrap(), ICON);
        assert_eq!(std::fs::read(root.join("panels/board.html")).unwrap(), PANEL);
        assert!(env.store.lookup_clone("demo.plugin").is_some());
    }

    /// Declare-once authoring: same package, but the panel / tool carry **no** per-entry integrity.
    fn files_only_manifest_json() -> Vec<u8> {
        let files = [
            ("panels/board.html", PANEL),
            ("tools/run.js", HANDLER),
            ("panels/board.css", CSS),
        ]
        .iter()
        .map(|(p, b)| format!(r#"{{"path":"{p}","integrity":"{}"}}"#, sri_sha512(b)))
        .collect::<Vec<_>>()
        .join(",");
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{files}],"contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html"}}],"agentTools":[{{"id":"t","name":"T","planSafeActions":true,"handler":"tools/run.js"}}]}}}}"#
        )
        .into_bytes()
    }

    #[test]
    fn files_only_manifest_installs_and_inherited_entries_are_byte_verified() {
        let env1 = env();
        let f = fixture(files_only_manifest_json());
        let entries = |raw: &[u8]| {
            vec![
                ("aion-extension.json", raw.to_vec()),
                ("panels/board.html", PANEL.to_vec()),
                ("tools/run.js", HANDLER.to_vec()),
                ("panels/board.css", CSS.to_vec()),
            ]
        };
        let out = run(
            &env1,
            &f,
            &zip_owned(&entries(&f.raw)),
            "d",
            LocalPackageLimits::default(),
        )
        .unwrap();
        assert_eq!(out.files_written, 4);
        // The retained signed snapshot binds inherited integrity for panel and tool handler.
        let (snapshot, _root) = env1.store.lookup_clone("demo.plugin").unwrap();
        let panel = crate::hub::bind_entry_point_digest_from_signed(&snapshot, "p").unwrap();
        assert_eq!(panel.integrity(), sri_sha512(PANEL));
        let tool = crate::hub::bind_handler_digest_from_signed(&snapshot, "t").unwrap();
        assert_eq!(tool.integrity(), sri_sha512(HANDLER));
        // Tampered inherited entry bytes are refused exactly like declared ones.
        for i in [1, 2] {
            let env2 = env();
            let mut e = entries(&f.raw);
            e[i].1 = b"tampered".to_vec();
            assert_eq!(
                run(&env2, &f, &zip_owned(&e), "d", LocalPackageLimits::default()),
                Err(LocalPackageError::IntegrityMismatch),
                "entry {i}"
            );
            assert_nothing_created(&env2);
        }
    }

    #[test]
    fn files_list_rejects_unlisted_extra_file() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        let mut entries = files_entries(&f.raw);
        entries.push(("panels/extra.css", b"x{}".to_vec()));
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_rejects_missing_listed_file_even_if_not_contribution_declared() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        for missing in [
            "panels/board.css",
            "icons/logo.svg",
            "panels/board.html",
            "tools/run.js",
        ] {
            let entries: Vec<_> = files_entries(&f.raw)
                .into_iter()
                .filter(|(n, _)| *n != missing)
                .collect();
            assert_eq!(
                run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
                Err(LocalPackageError::CoveredFileMissing),
                "{missing}"
            );
        }
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_rejects_tampered_bytes_for_every_listed_file() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        for i in 1..5 {
            let mut entries = files_entries(&f.raw);
            entries[i].1 = b"tampered".to_vec();
            assert_eq!(
                run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
                Err(LocalPackageError::IntegrityMismatch),
                "entry {i}"
            );
        }
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_manifest_must_still_equal_signed_bytes_and_be_present() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        let mut entries = files_entries(&f.raw);
        entries[0].1 = [f.raw.as_slice(), b" "].concat();
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::ManifestMismatch)
        );
        let entries: Vec<_> = files_entries(&f.raw).into_iter().skip(1).collect();
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::ManifestMismatch)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_keeps_path_safety_symlink_and_case_fold_rules() {
        let env = env();
        let f = fixture(files_manifest_json(&[]));
        for bad in ["../evil.js", "/abs.js", "a\\b.js", "./panels/board.css", "panels//x"] {
            let mut entries = files_entries(&f.raw);
            entries.push((bad, b"x".to_vec()));
            assert_eq!(
                run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
                Err(LocalPackageError::UnsafeEntryName),
                "{bad}"
            );
        }
        let mut entries = files_entries(&f.raw);
        entries.push(("Panels/Board.CSS", CSS.to_vec()));
        assert_eq!(
            run(&env, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::DuplicateEntry)
        );
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        w.start_file("aion-extension.json", SimpleFileOptions::default())
            .unwrap();
        w.write_all(&f.raw).unwrap();
        w.add_symlink("panels/board.css", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        let zip = w.finish().unwrap().into_inner();
        assert_eq!(
            run(&env, &f, &zip, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::SymlinkOrSpecialEntry)
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_may_not_list_the_manifest_itself() {
        let env = env();
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"aion-extension.json","integrity":"{}"}}]}}"#,
            sri_sha512(b"x")
        )
        .into_bytes();
        let f = fixture(raw);
        let zip = zip_owned(&[("aion-extension.json", f.raw.clone())]);
        assert_eq!(
            run(&env, &f, &zip, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::Signature(SignedManifestError::InvalidManifest))
        );
        assert_nothing_created(&env);
    }

    #[test]
    fn files_list_only_manifest_package_has_no_other_files() {
        // `files` present but only listing one asset: contribution-less manifest, exactly that asset.
        let env = env();
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"a.css","integrity":"{}"}}]}}"#,
            sri_sha512(CSS)
        )
        .into_bytes();
        let f = fixture(raw);
        let ok = zip_owned(&[("aion-extension.json", f.raw.clone()), ("a.css", CSS.to_vec())]);
        assert_eq!(
            run(&env, &f, &ok, "d", LocalPackageLimits::default())
                .unwrap()
                .files_written,
            2
        );
    }

    #[test]
    fn legacy_manifest_without_files_keeps_contribution_only_coverage() {
        // Same as the pre-slice-B behaviour: declared files install; css is still refused.
        let env = env();
        let f = fixture(manifest_json());
        let zip = zip_owned(&good_entries(&f.raw));
        assert_eq!(
            run(&env, &f, &zip, "legacy", LocalPackageLimits::default())
                .unwrap()
                .files_written,
            3
        );
        let env2 = self::env();
        let mut entries = good_entries(&f.raw);
        entries.push(("panels/board.css", CSS.to_vec()));
        assert_eq!(
            run(&env2, &f, &zip_owned(&entries), "d", LocalPackageLimits::default()),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_nothing_created(&env2);
    }

    #[test]
    fn empty_files_array_is_legacy_not_a_license_for_extras() {
        let env = env();
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[],"contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html","entryPointIntegrity":"{}"}}]}}}}"#,
            sri_sha512(PANEL)
        )
        .into_bytes();
        let f = fixture(raw);
        let ok = zip_owned(&[
            ("aion-extension.json", f.raw.clone()),
            ("panels/board.html", PANEL.to_vec()),
        ]);
        assert_eq!(
            run(&env, &f, &ok, "d", LocalPackageLimits::default())
                .unwrap()
                .files_written,
            2
        );
        let env2 = self::env();
        let extra = zip_owned(&[
            ("aion-extension.json", f.raw.clone()),
            ("panels/board.html", PANEL.to_vec()),
            ("x.css", CSS.to_vec()),
        ]);
        assert_eq!(
            run(&env2, &f, &extra, "d", LocalPackageLimits::default()),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_nothing_created(&env2);
    }

    #[test]
    fn default_limits_are_the_modest_bump_and_still_bounded() {
        let d = LocalPackageLimits::default();
        assert_eq!(
            (d.max_entries, d.max_file_bytes, d.max_total_bytes),
            (512, 8 << 20, 32 << 20)
        );
    }

    #[test]
    fn typical_asset_package_fits_default_limits_but_over_budget_is_refused() {
        // 300 small assets (> old 256 entry cap) and a 5 MiB asset (> old 4 MiB per-file cap).
        let big = vec![7u8; 5 * 1024 * 1024];
        let small: Vec<(String, Vec<u8>)> = (0..300)
            .map(|i| (format!("assets/{i}.txt"), vec![i as u8; 8]))
            .collect();
        let mut listed: Vec<(String, Vec<u8>)> = small.clone();
        listed.push(("assets/big.bin".into(), big.clone()));
        let files: Vec<String> = listed
            .iter()
            .map(|(p, b)| format!(r#"{{"path":"{p}","integrity":"{}"}}"#, sri_sha512(b)))
            .collect();
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{}]}}"#,
            files.join(",")
        )
        .into_bytes();
        let f = fixture(raw);
        let mut entries: Vec<(&str, Vec<u8>)> = vec![("aion-extension.json", f.raw.clone())];
        entries.extend(listed.iter().map(|(p, b)| (p.as_str(), b.clone())));
        let zip = zip_owned(&entries);
        let env = env();
        assert_eq!(
            run(&env, &f, &zip, "big", LocalPackageLimits::default())
                .unwrap()
                .files_written,
            302
        );
        // The same package under the previous defaults is refused before any effect.
        let old = LocalPackageLimits {
            max_entries: 256,
            max_file_bytes: 4 * 1024 * 1024,
            max_total_bytes: 16 * 1024 * 1024,
        };
        let env2 = self::env();
        assert_eq!(run(&env2, &f, &zip, "d", old), Err(LocalPackageError::LimitExceeded));
        assert_nothing_created(&env2);
        // Over the new per-file bound is refused too.
        let huge = LocalPackageLimits {
            max_file_bytes: 1024,
            ..LocalPackageLimits::default()
        };
        let env3 = self::env();
        assert_eq!(run(&env3, &f, &zip, "d", huge), Err(LocalPackageError::LimitExceeded));
        assert_nothing_created(&env3);
    }
}

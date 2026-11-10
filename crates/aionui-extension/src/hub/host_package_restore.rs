//! Re-retain installed extensions after a Host restart (M9 E13 slice D).
//!
//! The Host re-opens **its own** parent directory handle ([`HostExtensionsParent`]) and calls
//! [`restore_installed_extensions`]. For every real directory under it, the `current` marker the
//! installer wrote names the installed version; only handles are used from there on:
//!
//! 1. `openat(O_DIRECTORY|O_NOFOLLOW)` the name directory, read `current` through it (bounded,
//!    regular file only, `O_NOFOLLOW`);
//! 2. the marker must parse strictly, its signed name must encode to the directory's own name and
//!    its version to the recorded component (no guessing a version);
//! 3. `openat(O_DIRECTORY|O_NOFOLLOW)` that version directory — this handle becomes the root;
//! 4. read `aion-extension.json` through the root and run `check_signed_manifest` again with the
//!    Host policy and the signed entry metadata from the marker (signature, integrity and key id are
//!    verified against the Host's keys; nothing in the marker is trusted by itself);
//! 5. every file the signed manifest lists (`files`, or the legacy contribution-declared set) is
//!    read through the root and its bytes checked against its SRI;
//! 6. `SignedRootBindingStore::install_new` the handle. An existing binding is never replaced.
//!
//! Any failure leaves that extension **unbound** with a fixed rejection category — never a guessed
//! version, a path-string reopen or an invented root. It does not stop the other extensions and
//! deletes nothing (use `sweep_abandoned_staging` for leftovers). Files not named by the signed
//! manifest are not enumerated here; they stay unreachable to checked loads and are refused by the
//! install allow-list at the next install.
//!
//! Unix only; other targets restore nothing.

use super::curated::CuratedInstallPolicy;
use super::host_package_install::HostExtensionsParent;
use super::local_package_install::LocalPackageLimits;
use super::signed_manifest::SignedManifestError;
use crate::signed_root_binding::SignedRootBindingStore;

/// Why one extension was not re-bound. Fixed categories only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreRejection {
    MarkerMissing,
    MarkerInvalid,
    /// Marker name/version does not encode to the directory / component it was found under.
    IdentityMismatch,
    VersionDirMissing,
    ManifestUnreadable,
    Signature(SignedManifestError),
    InvalidManifest,
    CoveredFileMissing,
    IntegrityMismatch,
    AlreadyBound,
    Unsupported,
}

/// Counts plus the (encoded, charset-safe) name-directory components that were rejected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: usize,
    pub rejected: Vec<(String, RestoreRejection)>,
}

/// Re-bind every installed extension under `parent` that verifies; see the module docs.
pub fn restore_installed_extensions(
    store: &SignedRootBindingStore,
    policy: &CuratedInstallPolicy,
    parent: &HostExtensionsParent,
    limits: LocalPackageLimits,
) -> RestoreReport {
    let _guard = parent.lock.lock().unwrap_or_else(|e| e.into_inner());
    imp::restore(store, policy, &parent.dir, limits)
}

#[cfg(unix)]
mod imp {
    use std::fs::File;

    use super::super::curated::sri_sha512;
    use super::super::host_fs::{Kind, kind_nofollow, list_dir, read_file_under};
    use super::super::host_package_install::{CurrentMarker, MARKER_FILE, MAX_MARKER_BYTES, encode_component};
    use super::super::local_package_install::{covered_files, sys as base};
    use super::super::signed_manifest::{MAX_MANIFEST_BYTES, SignedManifestEntry, check_signed_manifest};
    use super::{CuratedInstallPolicy, LocalPackageLimits, RestoreRejection, RestoreReport, SignedRootBindingStore};
    use crate::constants::EXTENSION_MANIFEST_FILE;
    use crate::signed_root_binding::SignedRootBindingError;

    pub(super) fn restore(
        store: &SignedRootBindingStore,
        policy: &CuratedInstallPolicy,
        parent: &File,
        limits: LocalPackageLimits,
    ) -> RestoreReport {
        let mut report = RestoreReport::default();
        let Ok(mut names) = list_dir(parent) else {
            return report;
        };
        names.sort();
        for name in names {
            if name.starts_with('.') || !matches!(kind_nofollow(parent, &name), Ok(Some(Kind::Dir))) {
                continue;
            }
            match restore_one(store, policy, parent, &name, limits) {
                Ok(()) => report.restored += 1,
                Err(why) => report.rejected.push((name, why)),
            }
        }
        report
    }

    fn restore_one(
        store: &SignedRootBindingStore,
        policy: &CuratedInstallPolicy,
        parent: &File,
        dir_name: &str,
        limits: LocalPackageLimits,
    ) -> Result<(), RestoreRejection> {
        let name_dir = base::openat(parent, dir_name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|_| RestoreRejection::MarkerMissing)?;
        // Absent marker is "never installed through the Host layout" — distinct from a bad one.
        match kind_nofollow(&name_dir, MARKER_FILE) {
            Ok(None) => return Err(RestoreRejection::MarkerMissing),
            Ok(Some(_)) => {}
            Err(_) => return Err(RestoreRejection::MarkerInvalid),
        }
        let bytes =
            read_file_under(&name_dir, MARKER_FILE, MAX_MARKER_BYTES).map_err(|_| RestoreRejection::MarkerInvalid)?;
        let marker = CurrentMarker::parse(&bytes).ok_or(RestoreRejection::MarkerInvalid)?;
        if encode_component(&marker.name).as_deref() != Some(dir_name)
            || encode_component(&marker.version).as_deref() != Some(marker.component.as_str())
        {
            return Err(RestoreRejection::IdentityMismatch);
        }
        let root = base::openat(&name_dir, &marker.component, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|_| RestoreRejection::VersionDirMissing)?;
        let raw = read_file_under(&root, EXTENSION_MANIFEST_FILE, MAX_MANIFEST_BYTES as u64)
            .map_err(|_| RestoreRejection::ManifestUnreadable)?;
        let entry = SignedManifestEntry {
            name: &marker.name,
            version: &marker.version,
            pubkey_id: Some(&marker.pubkey_id),
            integrity: Some(&marker.integrity),
            signature: Some(&marker.signature),
        };
        let checked = check_signed_manifest(policy, &entry, &raw).map_err(RestoreRejection::Signature)?;
        let covered = covered_files(&checked).map_err(|_| RestoreRejection::InvalidManifest)?;
        let mut total: u64 = 0;
        for (rel, sri) in &covered {
            let bytes =
                read_file_under(&root, rel, limits.max_file_bytes).map_err(|_| RestoreRejection::CoveredFileMissing)?;
            total = total.saturating_add(bytes.len() as u64);
            if total > limits.max_total_bytes {
                return Err(RestoreRejection::IntegrityMismatch);
            }
            if sri_sha512(&bytes) != *sri {
                return Err(RestoreRejection::IntegrityMismatch);
            }
        }
        let key = checked.name().to_owned();
        store.install_new(&key, checked, root).map_err(|e| match e {
            SignedRootBindingError::AlreadyBound => RestoreRejection::AlreadyBound,
            _ => RestoreRejection::VersionDirMissing,
        })
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;

    use super::{CuratedInstallPolicy, LocalPackageLimits, RestoreReport, SignedRootBindingStore};

    pub(super) fn restore(
        _store: &SignedRootBindingStore,
        _policy: &CuratedInstallPolicy,
        _parent: &File,
        _limits: LocalPackageLimits,
    ) -> RestoreReport {
        RestoreReport::default()
    }
}

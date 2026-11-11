//! Host wiring of the signed-package install / restore chain (M9 E13 slice G).
//!
//! [`HostSignedInstall`] bundles what the running app must hold to use slices C/D:
//! the Host-opened extensions parent ([`HostExtensionsParent`], opened by the **Host** from its own
//! data directory — see `aionui-app`), the signature policy, and the process-wide
//! [`SignedRootBindingStore`]. It is only constructed when a signature policy is configured
//! ([`signature_policy_configured`]): with no policy the app behaves exactly as before
//! (no directory, no restore, no signed install path).
//!
//! Nothing here opens a path, derives a directory from a manifest / package / index entry, or runs
//! anything. Package bytes are verified (`check_signed_manifest` + per-file SRIs) before a single
//! file is written, and a failure never falls back to the unsigned directory flow.

use std::sync::Arc;

use super::builtin_packs::{BuiltinOutcome, BuiltinPack, BuiltinReport, BuiltinSet, BuiltinSkip, usable_key_ids};
use super::curated::{CuratedInstallPolicy, evaluate_install_with};
use super::host_package_install::{
    AutoInstallOptions, AutoInstallReport, HostExtensionsParent, UninstallOutcome, install_local_signed_package_auto,
    sweep_abandoned_staging, uninstall_local_signed_package,
};
use super::host_package_restore::{RestoreRejection, restore_installed_extensions};
use super::index_manager::HubIndexEntry;
use super::local_package_install::{LocalPackageError, LocalPackageLimits, LocalPackageSource, read_package_manifest};
use super::signed_manifest::SignedManifestEntry;
use crate::signed_root_binding::SignedRootBindingStore;

/// A signature policy is "configured" when the Host can verify signatures at all or demands them.
/// An allowlist alone is the legacy soft gate and does **not** enable the signed-install path.
pub fn signature_policy_configured(policy: &CuratedInstallPolicy) -> bool {
    policy.require_signature || !policy.pubkeys.is_empty()
}

/// Largest package archive the Host will read into memory (zip bytes; extraction limits apply on top).
const MAX_PACKAGE_ZIP_BYTES: u64 = 64 * 1024 * 1024;

/// Counts and fixed categories only (never paths, names beyond the encoded dir component, or bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupReport {
    pub swept_removed: usize,
    pub swept_failed: usize,
    pub restored: usize,
    pub rejected: Vec<(String, RestoreRejection)>,
}

/// Host-held signed-install context. Cheap to clone.
#[derive(Clone)]
pub struct HostSignedInstall {
    parent: Arc<HostExtensionsParent>,
    policy: Arc<CuratedInstallPolicy>,
    store: Arc<SignedRootBindingStore>,
}

impl std::fmt::Debug for HostSignedInstall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostSignedInstall").finish_non_exhaustive()
    }
}

impl HostSignedInstall {
    /// `None` unless a signature policy is configured (then behaviour stays exactly as before).
    pub fn new(
        parent: HostExtensionsParent,
        policy: CuratedInstallPolicy,
        store: Arc<SignedRootBindingStore>,
    ) -> Option<Self> {
        signature_policy_configured(&policy).then(|| Self {
            parent: Arc::new(parent),
            policy: Arc::new(policy),
            store,
        })
    }

    /// Host start: clear crashed-attempt leftovers, then re-bind every installed extension that
    /// still verifies. A rejected extension is logged and left unbound; this never fails or panics
    /// the caller. Blocking file IO: call from a blocking context.
    pub fn startup_sweep_and_restore(&self) -> StartupReport {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let swept = sweep_abandoned_staging(&self.parent);
            let restored =
                restore_installed_extensions(&self.store, &self.policy, &self.parent, LocalPackageLimits::default());
            StartupReport {
                swept_removed: swept.removed,
                swept_failed: swept.failed,
                restored: restored.restored,
                rejected: restored.rejected,
            }
        }));
        let report = run.unwrap_or_else(|_| {
            tracing::error!("signed extensions: startup sweep/restore panicked; nothing was bound");
            StartupReport::default()
        });
        for (component, why) in &report.rejected {
            // `MarkerMissing` is a directory that was never installed through the Host layout.
            if *why == RestoreRejection::MarkerMissing {
                tracing::debug!(component = %component, "signed extensions: not a Host-layout install; left unbound");
            } else {
                tracing::warn!(component = %component, reason = ?why, "signed extensions: restore rejected; left unbound");
            }
        }
        tracing::info!(
            restored = report.restored,
            rejected = report.rejected.len(),
            swept_removed = report.swept_removed,
            swept_failed = report.swept_failed,
            "signed extensions: startup restore finished"
        );
        report
    }

    /// Ensure the app's built-in signed packs are installed (blocking; run after
    /// [`Self::startup_sweep_and_restore`]). See `hub::builtin_packs` for the policy: forward-only,
    /// self-healing, key collisions fail closed. Never fails or panics the caller; each pack's
    /// outcome is reported and logged.
    pub fn ensure_builtin_packs(&self, set: &BuiltinSet<'_>) -> BuiltinReport {
        let usable = usable_key_ids(&self.policy, set);
        let mut report = BuiltinReport::default();
        for pack in set.packs {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if !usable.contains(pack.pubkey_id) {
                    return BuiltinOutcome::Skipped(BuiltinSkip::KeyUnavailable);
                }
                self.ensure_one_builtin(pack)
            }));
            let outcome = run.unwrap_or_else(|_| {
                tracing::error!(pack = %pack.name, "signed extensions: built-in install panicked");
                BuiltinOutcome::Failed(LocalPackageError::PlaceFailed)
            });
            match outcome {
                BuiltinOutcome::Installed(kind) => {
                    tracing::info!(pack = %pack.name, version = %pack.version, ?kind, "signed extensions: built-in installed");
                }
                BuiltinOutcome::AlreadyCurrent | BuiltinOutcome::KeptNewer => {
                    tracing::debug!(pack = %pack.name, ?outcome, "signed extensions: built-in left as is");
                }
                BuiltinOutcome::Skipped(why) => {
                    tracing::warn!(pack = %pack.name, ?why, "signed extensions: built-in skipped");
                }
                BuiltinOutcome::Failed(why) => {
                    tracing::warn!(pack = %pack.name, reason = ?why, "signed extensions: built-in install refused");
                }
            }
            report.outcomes.push((pack.name.to_owned(), outcome));
        }
        report
    }

    fn ensure_one_builtin(&self, pack: &BuiltinPack<'_>) -> BuiltinOutcome {
        // Cheap skip from the live binding. Anything unparsable falls through to the install chain,
        // which makes the authoritative decision from the binding / marker.
        if let Some(bound) = self.store.lookup_checked(pack.name)
            && let (Ok(have), Ok(want)) = (
                semver::Version::parse(bound.version()),
                semver::Version::parse(pack.version),
            )
        {
            match have.cmp(&want) {
                std::cmp::Ordering::Equal => return BuiltinOutcome::AlreadyCurrent,
                std::cmp::Ordering::Greater => return BuiltinOutcome::KeptNewer,
                std::cmp::Ordering::Less => {}
            }
        }
        let raw = match read_package_manifest(pack.package_zip) {
            Ok(raw) => raw,
            Err(why) => return BuiltinOutcome::Failed(why),
        };
        // The built-in installs under the built-in signature alone: the soft allowlist is not applied.
        let policy = CuratedInstallPolicy::new(Default::default(), true, self.policy.pubkeys.clone());
        let source = LocalPackageSource {
            entry: SignedManifestEntry {
                name: pack.name,
                version: pack.version,
                pubkey_id: Some(pack.pubkey_id),
                integrity: Some(pack.integrity),
                signature: Some(pack.signature),
            },
            raw_manifest: &raw,
            package_zip: pack.package_zip,
        };
        match install_local_signed_package_auto(
            &self.store,
            &policy,
            &source,
            &self.parent,
            LocalPackageLimits::default(),
            AutoInstallOptions::default(),
        ) {
            Ok(report) => BuiltinOutcome::Installed(report.kind),
            Err(LocalPackageError::DowngradeRefused) => BuiltinOutcome::KeptNewer,
            Err(why) => BuiltinOutcome::Failed(why),
        }
    }

    /// Verify and install one locally supplied package for `entry` (blocking).
    ///
    /// Gates, in order: the existing curated soft gate for the index entry (allowlist / curated flag),
    /// then `check_signed_manifest` with the index entry's signed metadata over the archive's own
    /// manifest bytes, whole-archive validation, per-file SRI checks while extracting, and the
    /// upgrade / downgrade policy of slice D.
    pub(crate) fn install_package(
        &self,
        entry: &HubIndexEntry,
        package_zip: &[u8],
    ) -> Result<AutoInstallReport, SignedPackageError> {
        let raw = read_package_manifest(package_zip).map_err(SignedPackageError::Package)?;
        evaluate_install_with(&self.policy, entry, Some(&raw)).map_err(|_| SignedPackageError::Curated)?;
        let source = LocalPackageSource {
            entry: SignedManifestEntry {
                name: &entry.name,
                version: &entry.version,
                pubkey_id: entry.pubkey_id.as_deref(),
                integrity: entry.integrity.as_deref(),
                signature: entry.signature.as_deref(),
            },
            raw_manifest: &raw,
            package_zip,
        };
        install_local_signed_package_auto(
            &self.store,
            &self.policy,
            &source,
            &self.parent,
            LocalPackageLimits::default(),
            AutoInstallOptions::default(),
        )
        .map_err(SignedPackageError::Package)
    }

    /// Uninstall a Host-layout install of `name` (blocking).
    pub(crate) fn uninstall(&self, name: &str) -> Result<UninstallOutcome, LocalPackageError> {
        uninstall_local_signed_package(&self.store, &self.parent, name)
    }

    pub(crate) fn max_package_zip_bytes(&self) -> u64 {
        MAX_PACKAGE_ZIP_BYTES
    }
}

/// Fixed categories only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignedPackageError {
    /// The curated soft gate (allowlist / curated flag / signature requirement) refused the entry.
    Curated,
    Package(LocalPackageError),
}

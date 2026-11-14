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

use super::builtin_packs::{
    BuiltinOutcome, BuiltinPack, BuiltinPins, BuiltinReport, BuiltinSet, BuiltinSkip, PinVerdict, usable_key_ids,
};
use super::curated::{CuratedInstallPolicy, evaluate_install_with};
use super::host_package_install::{
    AutoInstallOptions, AutoInstallReport, HostExtensionsParent, UninstallOutcome, install_local_signed_package_auto,
    sweep_abandoned_staging, uninstall_local_signed_package,
};
use super::host_package_restore::{RestoreRejection, restore_installed_extensions_pinned};
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
    /// Windows: same-version replaces interrupted by a crash and put back before restore.
    pub swept_rolled_back: usize,
    pub restored: usize,
    pub rejected: Vec<(String, RestoreRejection)>,
}

/// Host-held signed-install context. Cheap to clone.
#[derive(Clone)]
pub struct HostSignedInstall {
    parent: Arc<HostExtensionsParent>,
    policy: Arc<CuratedInstallPolicy>,
    store: Arc<SignedRootBindingStore>,
    /// The app's built-in identities (empty unless [`Self::with_builtin_pins`] was called).
    pins: Arc<BuiltinPins>,
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
            pins: Arc::new(BuiltinPins::default()),
        })
    }

    /// Reserve and pin the app's built-in names (design §T.1 / §T.7 / §T.8). Call before
    /// [`Self::startup_sweep_and_restore`] with the same set later given to
    /// [`Self::ensure_builtin_packs`]: restore then only re-binds a built-in name that is exactly
    /// the pinned identity, hub installs of a built-in name are refused, and `ensure` installs the
    /// pinned version over anything else (including a newer one: the binary is the authority).
    #[must_use]
    pub fn with_builtin_pins(mut self, set: &BuiltinSet<'_>) -> Self {
        self.pins = Arc::new(BuiltinPins::from_set(set));
        self
    }

    /// Whether `name` is reserved by a built-in (ASCII case-insensitive): such a name can only be
    /// provisioned by the app itself, never installed or updated from the hub.
    pub fn is_builtin_name(&self, name: &str) -> bool {
        self.pins.is_reserved(name)
    }

    /// Host start: clear crashed-attempt leftovers, then re-bind every installed extension that
    /// still verifies. A rejected extension is logged and left unbound; this never fails or panics
    /// the caller. Blocking file IO: call from a blocking context.
    pub fn startup_sweep_and_restore(&self) -> StartupReport {
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let swept = sweep_abandoned_staging(&self.parent);
            let restored = restore_installed_extensions_pinned(
                &self.store,
                &self.policy,
                &self.pins,
                &self.parent,
                LocalPackageLimits::default(),
            );
            StartupReport {
                swept_removed: swept.removed,
                swept_failed: swept.failed,
                swept_rolled_back: swept.rolled_back,
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
            } else if *why == RestoreRejection::BuiltinPinMismatch {
                tracing::info!(component = %component, "signed extensions: built-in name does not match the app's pin; left unbound for the built-in provisioner");
            } else {
                tracing::warn!(component = %component, reason = ?why, "signed extensions: restore rejected; left unbound");
            }
        }
        tracing::info!(
            restored = report.restored,
            rejected = report.rejected.len(),
            swept_removed = report.swept_removed,
            swept_failed = report.swept_failed,
            swept_rolled_back = report.swept_rolled_back,
            "signed extensions: startup restore finished"
        );
        report
    }

    /// Ensure the app's built-in signed packs are installed (blocking; run after
    /// [`Self::startup_sweep_and_restore`], with the set given to [`Self::with_builtin_pins`]). See
    /// `hub::builtin_packs` for the policy: the binary's pin is authoritative (upgrade and rollback),
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
                if self.pins.verdict_pack(&self.policy, pack) != PinVerdict::Pinned {
                    return BuiltinOutcome::Skipped(BuiltinSkip::NotPinned);
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
                BuiltinOutcome::AlreadyCurrent => {
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
        // Only exactly the pinned identity counts as current; anything else bound under this name
        // (it cannot come from restore, which refuses it) is replaced by the pinned copy.
        if let Some(bound) = self.store.lookup_checked(pack.name)
            && self.pins.verdict_checked(&self.policy, &bound) == PinVerdict::Pinned
        {
            return BuiltinOutcome::AlreadyCurrent;
        }
        let raw = match read_package_manifest(pack.package_zip) {
            Ok(raw) => raw,
            Err(why) => return BuiltinOutcome::Failed(why),
        };
        // The built-in installs under the built-in signature and pin alone: the soft allowlist is
        // not applied.
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
        // The binary is the authority for a built-in name: an app rollback rolls the built-in back
        // to the pinned version. Downgrade is allowed here only (every other install refuses it).
        match install_local_signed_package_auto(
            &self.store,
            &policy,
            &source,
            &self.parent,
            LocalPackageLimits::default(),
            AutoInstallOptions { allow_downgrade: true },
        ) {
            Ok(report) => BuiltinOutcome::Installed(report.kind),
            Err(why) => BuiltinOutcome::Failed(why),
        }
    }

    /// Verify and install one locally supplied package for `entry` (blocking).
    ///
    /// Gates, in order: built-in name reservation (a built-in name is never installed from here),
    /// the existing curated soft gate for the index entry (allowlist / curated flag),
    /// then `check_signed_manifest` with the index entry's signed metadata over the archive's own
    /// manifest bytes, whole-archive validation, per-file SRI checks while extracting, and the
    /// upgrade / downgrade policy of slice D.
    pub(crate) fn install_package(
        &self,
        entry: &HubIndexEntry,
        package_zip: &[u8],
    ) -> Result<AutoInstallReport, SignedPackageError> {
        // A built-in name is provisioned by the app only (§T.8), whatever the entry's signer.
        if self.pins.is_reserved(&entry.name) {
            return Err(SignedPackageError::ReservedByBuiltin);
        }
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
    /// The name is reserved by a built-in pack the app provisions itself.
    ReservedByBuiltin,
    Package(LocalPackageError),
}

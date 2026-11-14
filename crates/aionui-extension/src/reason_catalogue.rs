//! Catalogue of the fixed extension reason codes (design §9 S16).
//!
//! Every refusal the Host reports for an extension carries a **stable, lower-case `snake_case`
//! code** inside an existing response field — the error message of a refused asset (`403`), a
//! refused background-service start, a hub install / update / uninstall result `msg`, or the
//! `reason` of an agent-tool invoke result. No response shape changes: the code is part of the
//! text the UI already receives. A client finds it as a **whole token** (bounded by anything that
//! is not `[a-z0-9_]`) and maps it to its own translated text; [`hint`] gives the English one.
//!
//! Hints are short, end-user facing and actionable. They never contain a path, URL, host, IP,
//! digest or key material (enforced by tests), so they are safe to show as-is. Codes are part of
//! the contract: never rename or reuse one; add new ones at the end of their area.
//!
//! The author-facing list lives in `docs/extension-reason-codes.md` and is checked against this
//! table by a test (regenerate it with `AIONCORE_WRITE_REASON_DOC=1`).

/// Where a code is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonArea {
    /// Opening an extension page or asset (HTTP 403 message).
    AssetServe,
    /// Starting an extension background service (start error message).
    BackgroundService,
    /// Invoking an extension agent tool / command (`reason` of the invoke result).
    AgentTool,
    /// Installing, updating or uninstalling from the hub (result `msg`).
    HubInstall,
}

impl ReasonArea {
    pub fn title(self) -> &'static str {
        match self {
            Self::AssetServe => "Opening extension pages and files",
            Self::BackgroundService => "Starting background services",
            Self::AgentTool => "Extension commands and agent tools",
            Self::HubInstall => "Installing, updating and removing extensions",
        }
    }
}

/// One catalogue entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReasonHint {
    pub code: &'static str,
    pub area: ReasonArea,
    /// Friendly, actionable English text for end users.
    pub hint: &'static str,
}

const fn r(code: &'static str, area: ReasonArea, hint: &'static str) -> ReasonHint {
    ReasonHint { code, area, hint }
}

use ReasonArea::{AgentTool, AssetServe, BackgroundService, HubInstall};

/// The full catalogue, grouped by area.
pub const CATALOGUE: &[ReasonHint] = &[
    // ---- opening pages and files -------------------------------------------------------------
    r(
        "entry_point_integrity_declared_unchecked_serve",
        AssetServe,
        "This extension page requires an integrity check that this copy cannot provide, so it was not opened. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "signed_entry_point_integrity_declared_unchecked_serve",
        AssetServe,
        "This signed extension page can only be opened from its verified installation. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "signed_snapshot_inconclusive",
        AssetServe,
        "The extension's signed description could not be checked, so it was blocked for safety. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "verified_asset_invalid_snapshot",
        AssetServe,
        "The extension's list of signed files is invalid. Reinstall the extension, or contact its publisher if this keeps happening.",
    ),
    r(
        "verified_asset_not_listed",
        AssetServe,
        "This file is not part of the extension's signed package, so it was not opened. Contact the extension's publisher.",
    ),
    r(
        "verified_asset_missing",
        AssetServe,
        "A file of this extension is missing from its installation. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "verified_asset_unreadable",
        AssetServe,
        "A file of this extension could not be read safely, for example because it was replaced by a link. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "verified_asset_integrity_mismatch",
        AssetServe,
        "A file of this extension was changed after it was installed. Reinstall the extension to restore the original files.",
    ),
    r(
        "verified_asset_unavailable",
        AssetServe,
        "Signed extension files cannot be opened on this platform.",
    ),
    r(
        "theme_css_invalid_path",
        AssetServe,
        "The extension's theme points to a file name that is not allowed. Contact the extension's publisher.",
    ),
    r(
        "theme_css_not_utf8",
        AssetServe,
        "The extension's theme file is not readable text. Contact the extension's publisher.",
    ),
    // ---- background services -----------------------------------------------------------------
    r(
        "entry_point_integrity_declared_unchecked_spawn",
        BackgroundService,
        "This background service requires an integrity check that this copy cannot provide, so it was not started. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "signed_entry_point_integrity_declared_unchecked_spawn",
        BackgroundService,
        "This signed background service can only start from its verified installation. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "bound_service_root_missing",
        BackgroundService,
        "The verified installation of this extension is not available right now. Restart CSBU WorkMate; if it persists, reinstall the extension.",
    ),
    r(
        "bound_service_invalid_snapshot",
        BackgroundService,
        "The extension's list of signed files is invalid, so the service was not started. Reinstall the extension or contact its publisher.",
    ),
    r(
        "bound_service_signed_snapshot_inconclusive",
        BackgroundService,
        "The signed description of this service is incomplete or ambiguous, so it was not started. Contact the extension's publisher.",
    ),
    r(
        "bound_service_integrity_required",
        BackgroundService,
        "This service is not covered by the extension's signature, so it was not started. Contact the extension's publisher.",
    ),
    r(
        "bound_service_entry_mismatch",
        BackgroundService,
        "The service does not match its signed description, so it was not started. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "bound_service_entry_missing",
        BackgroundService,
        "The service's program file is missing from the installation. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "bound_service_entry_unreadable",
        BackgroundService,
        "The service's program file could not be read safely. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "bound_service_integrity_mismatch",
        BackgroundService,
        "The service's program file was changed after installation, so it was not started. Reinstall the extension to restore it.",
    ),
    r(
        "bound_service_verified_read_unavailable",
        BackgroundService,
        "Signed background services cannot be started on this platform.",
    ),
    r(
        "bound_service_runtime_required",
        BackgroundService,
        "This service does not say how it should run, so it was not started. Contact the extension's publisher.",
    ),
    r(
        "bound_service_runtime_unsupported",
        BackgroundService,
        "This service needs a runtime that CSBU WorkMate does not provide for extensions. Contact the extension's publisher.",
    ),
    r(
        "bound_service_shell_script_unsupported",
        BackgroundService,
        "Shell-script services are not allowed for signed extensions. Contact the extension's publisher.",
    ),
    r(
        "bound_service_native_unsupported",
        BackgroundService,
        "Native program services are not allowed for extensions. Contact the extension's publisher.",
    ),
    r(
        "bound_service_entry_kind_unsupported",
        BackgroundService,
        "The service's program file is not a supported script type. Contact the extension's publisher.",
    ),
    r(
        "bound_service_capabilities_unavailable",
        BackgroundService,
        "This service needs extra permissions that this version of CSBU WorkMate cannot grant yet. Update the app when a newer version is available.",
    ),
    r(
        "bound_service_runtime_requires_signed_files",
        BackgroundService,
        "This service can only run from a signed installation. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "bound_service_runner_unavailable",
        BackgroundService,
        "This service is verified, but running extension services is not enabled in this version of CSBU WorkMate.",
    ),
    r(
        "unsigned_extension_code_disabled",
        BackgroundService,
        "This extension is not signed, so CSBU WorkMate does not run its background services or setup scripts. Install a signed version from the WorkMate hub, or ask your administrator.",
    ),
    // ---- agent tools / commands --------------------------------------------------------------
    r(
        "integrity_binding_unavailable",
        AgentTool,
        "This extension command cannot run because its signed installation could not be verified. Reinstall the extension from the WorkMate hub.",
    ),
    r(
        "agent_tool_not_plan_safe",
        AgentTool,
        "This tool can make changes, and extension tools that make changes cannot run in this version of CSBU WorkMate.",
    ),
    r(
        "handler_integrity_checked",
        AgentTool,
        "The command's code was verified. Running extension code is not enabled in this version, so nothing was executed.",
    ),
    // ---- hub install / update / uninstall ----------------------------------------------------
    r(
        "name_reserved_by_builtin",
        HubInstall,
        "This extension is built into CSBU WorkMate and is updated together with the app. Update the app instead.",
    ),
    r(
        "curated_install_refused",
        HubInstall,
        "This extension is not approved for installation here. Choose a curated extension or ask your administrator.",
    ),
    r(
        "curated_not_in_index",
        HubInstall,
        "This extension is not listed in the WorkMate hub. Refresh the hub and try again.",
    ),
    r(
        "curated_not_curated",
        HubInstall,
        "Only curated or built-in extensions can be installed. Choose a curated extension from the WorkMate hub.",
    ),
    r(
        "curated_not_allowlisted",
        HubInstall,
        "This extension is not on your organisation's allowed list. Ask your administrator to allow it.",
    ),
    r(
        "curated_integrity_missing",
        HubInstall,
        "The hub entry for this extension has no integrity value, so it cannot be verified. Contact the extension's publisher.",
    ),
    r(
        "curated_integrity_mismatch",
        HubInstall,
        "The extension's files do not match the hub listing. Refresh the hub and try again, or contact the publisher.",
    ),
    r(
        "curated_signature_required",
        HubInstall,
        "This installation only accepts signed extensions, and this one is not signed. Contact the extension's publisher.",
    ),
    r(
        "curated_signature_missing_key_id",
        HubInstall,
        "The extension's signature does not say which key signed it. Contact the extension's publisher.",
    ),
    r(
        "curated_signature_invalid",
        HubInstall,
        "The extension's signature is not valid. Do not install it; contact the extension's publisher.",
    ),
    r(
        "curated_unknown_key",
        HubInstall,
        "The extension was signed by a key this installation does not trust. Ask your administrator.",
    ),
    r(
        "curated_key_config_invalid",
        HubInstall,
        "The trusted signing keys are configured incorrectly. Ask your administrator to fix the configuration.",
    ),
    r(
        "signature_manifest_limit",
        HubInstall,
        "The extension's description is too large to be accepted. Contact the extension's publisher.",
    ),
    r(
        "signature_metadata_limit",
        HubInstall,
        "The extension's signing information is too large to be accepted. Contact the extension's publisher.",
    ),
    r(
        "signature_invalid_identity",
        HubInstall,
        "The extension's name or version is not valid. Contact the extension's publisher.",
    ),
    r(
        "signature_not_allowlisted",
        HubInstall,
        "This extension is not on your organisation's allowed list. Ask your administrator to allow it.",
    ),
    r(
        "signature_missing_key_id",
        HubInstall,
        "The extension's signature does not say which key signed it. Contact the extension's publisher.",
    ),
    r(
        "signature_invalid_key_id",
        HubInstall,
        "The extension's signing key name is not valid. Contact the extension's publisher.",
    ),
    r(
        "signature_unknown_key",
        HubInstall,
        "The extension was signed by a key this installation does not trust. Ask your administrator.",
    ),
    r(
        "signature_missing_integrity",
        HubInstall,
        "The extension's listing has no integrity value, so it cannot be verified. Contact the extension's publisher.",
    ),
    r(
        "signature_invalid_integrity",
        HubInstall,
        "The extension's integrity value is malformed. Contact the extension's publisher.",
    ),
    r(
        "signature_integrity_mismatch",
        HubInstall,
        "The extension's description does not match its signed listing. Refresh the hub and try again, or contact the publisher.",
    ),
    r(
        "signature_missing_signature",
        HubInstall,
        "This extension is not signed, and only signed extensions are accepted. Contact the extension's publisher.",
    ),
    r(
        "signature_invalid_signature",
        HubInstall,
        "The extension's signature is not valid. Do not install it; contact the extension's publisher.",
    ),
    r(
        "signature_invalid_manifest",
        HubInstall,
        "The extension's description is not valid. Contact the extension's publisher.",
    ),
    r(
        "signature_identity_mismatch",
        HubInstall,
        "The extension's name or version does not match its signed listing. Refresh the hub and try again, or contact the publisher.",
    ),
    r(
        "package_invalid",
        HubInstall,
        "The extension package is damaged or not a valid package. Download it again or contact the publisher.",
    ),
    r(
        "package_limit_exceeded",
        HubInstall,
        "The extension package is too large or has too many files. Contact the extension's publisher.",
    ),
    r(
        "package_unsafe_entry_name",
        HubInstall,
        "The extension package contains a file name that is not allowed on this system. Contact the extension's publisher.",
    ),
    r(
        "package_duplicate_entry",
        HubInstall,
        "The extension package contains the same file twice. Contact the extension's publisher.",
    ),
    r(
        "package_symlink_or_special_entry",
        HubInstall,
        "The extension package contains links or special files, which are not allowed. Contact the extension's publisher.",
    ),
    r(
        "package_entry_not_covered_by_signature",
        HubInstall,
        "The extension package contains files that are not covered by its signature. Contact the extension's publisher.",
    ),
    r(
        "package_covered_file_missing",
        HubInstall,
        "A signed file is missing from the extension package. Download it again or contact the publisher.",
    ),
    r(
        "package_invalid_covered_path",
        HubInstall,
        "The extension lists a file name that is not allowed. Contact the extension's publisher.",
    ),
    r(
        "package_conflicting_covered_digest",
        HubInstall,
        "The extension lists the same file with two different integrity values. Contact the extension's publisher.",
    ),
    r(
        "package_manifest_mismatch",
        HubInstall,
        "The description inside the package differs from the signed one. Download it again or contact the publisher.",
    ),
    r(
        "package_integrity_mismatch",
        HubInstall,
        "A file in the extension package does not match its signature. Download it again; do not install a modified package.",
    ),
    r(
        "package_invalid_dir_name",
        HubInstall,
        "The extension's name or version cannot be stored on this system. Contact the extension's publisher.",
    ),
    r(
        "package_case_collision",
        HubInstall,
        "Another installed extension or version has the same name apart from upper and lower case. Remove it first, then try again.",
    ),
    r(
        "package_place_failed",
        HubInstall,
        "The extension could not be put in place. Nothing was changed; try again.",
    ),
    r(
        "package_version_slot_blocked",
        HubInstall,
        "Something unexpected is in the way of this extension version. Remove the extension and install it again.",
    ),
    r(
        "hub_package_not_found",
        HubInstall,
        "The extension's files were not found in the hub folder. Refresh the hub list and try again.",
    ),
    r(
        "hub_verification_failed",
        HubInstall,
        "The extension's description file is invalid, so it was not installed. Contact the extension's publisher.",
    ),
    r(
        "hub_not_installed",
        HubInstall,
        "This extension is not installed, so it cannot be updated. Install it first.",
    ),
    r(
        "hub_remove_failed",
        HubInstall,
        "The extension could not be removed completely. Close anything that uses it and try again.",
    ),
    r(
        "package_lifecycle_hooks_refused",
        HubInstall,
        "This extension asks to run setup programs, which signed extensions cannot do. Contact the extension's publisher.",
    ),
    r(
        "package_downgrade_refused",
        HubInstall,
        "A newer version of this extension is already installed. Older versions are not installed automatically.",
    ),
    r(
        "package_marker_write_failed",
        HubInstall,
        "The installation could not be completed. Nothing was changed; check free disk space and try again.",
    ),
    r(
        "package_parent_not_directory",
        HubInstall,
        "The extensions folder of CSBU WorkMate is not usable. Restart the app; if it persists, contact support.",
    ),
    r(
        "package_remove_failed",
        HubInstall,
        "The extension could not be removed completely. Close anything using it, restart CSBU WorkMate and try again.",
    ),
    r(
        "package_root_create_failed",
        HubInstall,
        "The extension folder could not be created. Check free disk space and permissions, then try again.",
    ),
    r(
        "package_write_failed",
        HubInstall,
        "The extension files could not be written. Check free disk space, then try again.",
    ),
    r(
        "package_retain_failed",
        HubInstall,
        "The extension was installed but could not be activated. Restart CSBU WorkMate.",
    ),
    r(
        "package_unsupported_platform",
        HubInstall,
        "Signed extensions cannot be installed on this platform.",
    ),
];

/// English hint for `code`, if it is a catalogue code.
pub fn hint(code: &str) -> Option<&'static str> {
    CATALOGUE.iter().find(|e| e.code == code).map(|e| e.hint)
}

/// The first catalogue code that appears as a whole token in `text` (a message or reason the
/// Host produced), the way a client is expected to find it.
pub fn find_code(text: &str) -> Option<&'static str> {
    text.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
        .find_map(|token| CATALOGUE.iter().find(|e| e.code == token).map(|e| e.code))
}

/// The catalogue rendered as the author document (`docs/extension-reason-codes.md` body).
pub fn render_markdown_tables() -> String {
    let mut out = String::new();
    for area in [AssetServe, BackgroundService, AgentTool, HubInstall] {
        out.push_str(&format!("### {}\n\n| Code | Hint |\n|---|---|\n", area.title()));
        for e in CATALOGUE.iter().filter(|e| e.area == area) {
            out.push_str(&format!("| `{}` | {} |\n", e.code, e.hint));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::asset_integrity_gate::AssetServeGateError;
    use crate::background_service_start_gate::BackgroundServiceStartGateError;
    use crate::bound_background_start::BoundBackgroundStartError;
    use crate::hub::curated::CuratedGateError;
    use crate::hub::host_signed_install::SignedPackageError;
    use crate::hub::local_package_install::LocalPackageError;
    use crate::hub::signed_manifest::SignedManifestError;
    use crate::hub::verified_asset::VerifiedAssetError;
    use crate::signed_root_binding::SignedRootBindingError;

    const SIGNATURE: [SignedManifestError; 14] = [
        SignedManifestError::ManifestLimit,
        SignedManifestError::MetadataLimit,
        SignedManifestError::InvalidIdentity,
        SignedManifestError::NotAllowlisted,
        SignedManifestError::MissingKeyId,
        SignedManifestError::InvalidKeyId,
        SignedManifestError::UnknownKey,
        SignedManifestError::MissingIntegrity,
        SignedManifestError::InvalidIntegrity,
        SignedManifestError::IntegrityMismatch,
        SignedManifestError::MissingSignature,
        SignedManifestError::InvalidSignature,
        SignedManifestError::InvalidManifest,
        SignedManifestError::IdentityMismatch,
    ];

    fn package_errors() -> Vec<LocalPackageError> {
        use LocalPackageError as E;
        let mut all = vec![
            E::InvalidPackage,
            E::LimitExceeded,
            E::UnsafeEntryName,
            E::DuplicateEntry,
            E::SymlinkOrSpecialEntry,
            E::EntryNotCoveredBySignature,
            E::CoveredFileMissing,
            E::InvalidCoveredPath,
            E::ConflictingCoveredDigest,
            E::ManifestMismatch,
            E::IntegrityMismatch,
            E::InvalidDirName,
            E::CaseCollision,
            E::PlaceFailed,
            E::VersionSlotBlocked,
            E::DowngradeRefused,
            E::MarkerWriteFailed,
            E::ParentNotDirectory,
            E::RemoveFailed,
            E::RootCreateFailed,
            E::WriteFailed,
            E::Retain(SignedRootBindingError::IdentityMismatch),
            E::Retain(SignedRootBindingError::RootUnavailable),
            E::Retain(SignedRootBindingError::RootNotDirectory),
            E::Retain(SignedRootBindingError::AlreadyBound),
            E::Unsupported,
            E::LifecycleHooksRefused,
        ];
        all.extend(SIGNATURE.iter().map(|s| E::Signature(*s)));
        all
    }

    fn curated_errors() -> Vec<CuratedGateError> {
        let name = || "demo.plugin".to_owned();
        vec![
            CuratedGateError::NotInIndex,
            CuratedGateError::NotCurated { name: name() },
            CuratedGateError::NotAllowlisted { name: name() },
            CuratedGateError::IntegrityMissing { name: name() },
            CuratedGateError::IntegrityMismatch { name: name() },
            CuratedGateError::SignatureRequired { name: name() },
            CuratedGateError::SignatureMissingPubkey { name: name() },
            CuratedGateError::SignatureInvalid { name: name() },
            CuratedGateError::PubkeyUnknown {
                name: name(),
                pubkey_id: "k1".into(),
            },
            CuratedGateError::PubkeyConfig("bad".into()),
        ]
    }

    /// Every code the Host can emit, gathered from the code-bearing types and constants.
    fn produced() -> BTreeSet<&'static str> {
        let mut out = BTreeSet::new();
        for e in [
            AssetServeGateError::DeclaredIntegrityCannotBeChecked,
            AssetServeGateError::SignedIntegrityCannotBeChecked,
            AssetServeGateError::SignedSnapshotInconclusive,
        ] {
            out.insert(e.reason());
        }
        for e in [
            BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked,
            BackgroundServiceStartGateError::SignedIntegrityCannotBeChecked,
            BackgroundServiceStartGateError::SignedSnapshotInconclusive,
        ] {
            out.insert(e.reason());
        }
        for e in [
            VerifiedAssetError::InvalidSnapshot,
            VerifiedAssetError::NotListed,
            VerifiedAssetError::Missing,
            VerifiedAssetError::Unreadable,
            VerifiedAssetError::IntegrityMismatch,
            VerifiedAssetError::Unavailable,
        ] {
            out.insert(e.code());
        }
        for e in BoundBackgroundStartError::ALL_FOR_TESTS {
            out.insert(e.reason());
        }
        for e in package_errors() {
            out.insert(e.code());
            out.insert(SignedPackageError::Package(e).code());
        }
        out.insert(SignedPackageError::Curated.code());
        out.insert(SignedPackageError::ReservedByBuiltin.code());
        for e in curated_errors() {
            out.insert(e.code());
        }
        out.insert(crate::host_runtime::INTEGRITY_BINDING_UNAVAILABLE_REASON);
        out.insert(crate::host_runtime::PLAN_MODE_REFUSED_REASON);
        for code in [
            crate::hub::installer::HUB_PACKAGE_NOT_FOUND,
            crate::hub::installer::HUB_VERIFICATION_FAILED,
            crate::hub::installer::HUB_NOT_INSTALLED,
            crate::hub::installer::HUB_REMOVE_FAILED,
        ] {
            out.insert(code);
        }
        // aionui-app `CheckedAgentToolPrepare::ACK_REASON` (the app crate is not a dependency).
        out.insert("handler_integrity_checked");
        out.insert("theme_css_invalid_path");
        out.insert("theme_css_not_utf8");
        out.insert(crate::unsigned_code_policy::UNSIGNED_CODE_DISABLED);
        out
    }

    #[test]
    fn catalogue_covers_exactly_the_codes_the_host_emits() {
        let catalogue: BTreeSet<&str> = CATALOGUE.iter().map(|e| e.code).collect();
        assert_eq!(catalogue.len(), CATALOGUE.len(), "duplicate code in the catalogue");
        let produced = produced();
        let missing: Vec<_> = produced.difference(&catalogue).collect();
        let orphan: Vec<_> = catalogue.difference(&produced).collect();
        assert!(missing.is_empty(), "codes without a hint: {missing:?}");
        assert!(orphan.is_empty(), "hints for codes nothing emits: {orphan:?}");
    }

    #[test]
    fn codes_are_snake_case_tokens() {
        for e in CATALOGUE {
            assert!(
                !e.code.is_empty()
                    && e.code.len() <= 64
                    && e.code.starts_with(|c: char| c.is_ascii_lowercase())
                    && e.code
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                    && !e.code.contains("__")
                    && !e.code.ends_with('_'),
                "{}",
                e.code
            );
            assert_eq!(find_code(e.code), Some(e.code));
            assert_eq!(hint(e.code), Some(e.hint));
        }
        assert_eq!(hint("no_such_code"), None);
    }

    #[test]
    fn hints_are_friendly_and_never_leak_paths_addresses_or_digests() {
        for e in CATALOGUE {
            let h = e.hint;
            assert!(h.is_ascii(), "{}", e.code);
            assert!(h.starts_with(|c: char| c.is_ascii_uppercase()), "{}", e.code);
            assert!(h.ends_with('.'), "{}", e.code);
            assert!((20..=200).contains(&h.len()), "{} ({} chars)", e.code, h.len());
            for banned in ["/", "\\", "http", "@", ":", "~", "sha", "sri", "{", "}", "_"] {
                assert!(
                    !h.to_ascii_lowercase().contains(banned),
                    "{}: contains {banned:?}",
                    e.code
                );
            }
            assert!(!h.bytes().any(|b| b.is_ascii_digit()), "{}: contains a digit", e.code);
        }
    }

    #[test]
    fn codes_are_found_in_the_messages_the_host_actually_builds() {
        // Asset / theme / i18n reasons carry their code.
        for e in [
            VerifiedAssetError::InvalidSnapshot,
            VerifiedAssetError::NotListed,
            VerifiedAssetError::Missing,
            VerifiedAssetError::Unreadable,
            VerifiedAssetError::IntegrityMismatch,
            VerifiedAssetError::Unavailable,
        ] {
            let msg = format!("Extension asset refused: {}", e.reason());
            assert_eq!(find_code(&msg), Some(e.code()), "{msg}");
        }
        // Background-service start refusals (existing message format).
        for e in BoundBackgroundStartError::ALL_FOR_TESTS {
            let text = crate::error::ExtensionError::from(e).to_string();
            assert_eq!(find_code(&text), Some(e.reason()), "{text}");
        }
        let text =
            crate::error::ExtensionError::from(BackgroundServiceStartGateError::SignedSnapshotInconclusive).to_string();
        assert_eq!(find_code(&text), Some("signed_snapshot_inconclusive"));
        // Hub messages.
        for e in package_errors() {
            let msg = format!(
                "Signed package install failed: {}",
                SignedPackageError::Package(e).code()
            );
            assert_eq!(find_code(&msg), Some(e.code()));
        }
        for e in curated_errors() {
            let msg = format!("{e} ({})", e.code());
            assert_eq!(find_code(&msg), Some(e.code()), "{msg}");
        }
        let builtin =
            "Extension 'workmate.x' is built into CSBU WorkMate and is updated with the app (name_reserved_by_builtin)";
        assert_eq!(find_code(builtin), Some("name_reserved_by_builtin"));
        // A longer code is never mistaken for a shorter one it contains.
        assert_eq!(
            find_code("refused: bound_service_signed_snapshot_inconclusive"),
            Some("bound_service_signed_snapshot_inconclusive")
        );
    }

    const DOC: &str = "docs/extension-reason-codes.md";
    const BEGIN: &str = "<!-- BEGIN GENERATED: reason_catalogue -->\n";
    const END: &str = "<!-- END GENERATED: reason_catalogue -->";

    #[test]
    fn author_doc_lists_the_catalogue() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DOC);
        // Normalize line endings: a Windows checkout may convert the doc to CRLF.
        let doc = std::fs::read_to_string(&path)
            .expect("reason code doc")
            .replace("\r\n", "\n");
        let start = doc.find(BEGIN).expect("begin marker") + BEGIN.len();
        let end = doc.find(END).expect("end marker");
        let expected = format!("\n{}", render_markdown_tables());
        if doc[start..end] != expected {
            if std::env::var_os("AIONCORE_WRITE_REASON_DOC").is_some() {
                let updated = format!("{}{}{}", &doc[..start], expected, &doc[end..]);
                std::fs::write(&path, updated).unwrap();
                return;
            }
            panic!("{DOC} is out of date: rerun with AIONCORE_WRITE_REASON_DOC=1");
        }
    }
}

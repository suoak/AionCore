pub mod background_service_entry_point_digest;
pub(crate) mod bound_manifest;
pub mod curated;
pub mod curated_signed_retain;
pub mod entry_point_digest;
pub mod handler_digest;
#[cfg(unix)]
mod host_fs;
#[cfg(windows)]
mod host_fs_windows;
pub mod host_package_install;
pub mod host_package_restore;
pub mod host_signed_install;
pub mod index_manager;
pub mod installer;
pub mod local_package_install;
pub mod signed_manifest;
pub mod sri_format;
#[cfg(test)]
pub(crate) mod test_support;
pub mod verified_asset;
pub(crate) mod verified_reader;
pub mod work_panel_view_entry_point_digest;

pub use background_service_entry_point_digest::{
    BackgroundServiceEntryPointDigestError, bind_background_service_entry_point_digest_from_signed,
    expected_background_service_entry_point_integrity,
};
pub use curated::{
    ALLOWLIST_ENV, CuratedGateError, CuratedInstallPolicy, REQUIRE_SIGNATURE_ENV, SIGNING_PUBKEYS_ENV,
    signature_payload, sri_sha512,
};
pub use curated_signed_retain::{
    CuratedSignedRetainOutcome, retain_checked_signed_root, try_retain_curated_signed_root,
};
pub use entry_point_digest::{
    EntryPointDigestBinding, EntryPointDigestError, bind_entry_point_digest_from_signed, expected_entry_point_integrity,
};
pub use handler_digest::{
    HandlerDigestBinding, HandlerDigestError, bind_handler_digest_from_signed, expected_handler_integrity,
};
pub use host_package_install::{
    AutoInstallOptions, AutoInstallReport, HostExtensionsParent, InstallKind, StagingSweepReport, UninstallOutcome,
    install_local_signed_package_auto, sweep_abandoned_staging, uninstall_local_signed_package,
};
pub use host_package_restore::{RestoreRejection, RestoreReport, restore_installed_extensions};
pub use host_signed_install::{HostSignedInstall, StartupReport, signature_policy_configured};
pub use index_manager::HubIndexManager;
pub use installer::HubInstaller;
pub use local_package_install::{
    LocalPackageError, LocalPackageInstalled, LocalPackageLimits, LocalPackageSource, install_local_signed_package,
    read_package_manifest,
};
pub use sri_format::{CANONICAL_SHA512_SRI_LEN, is_canonical_sha512_sri};
pub use verified_asset::{
    FilesIndex, MAX_VERIFIED_ASSET_BYTES, VerifiedAsset, VerifiedAssetError, read_verified_listed,
};
pub use work_panel_view_entry_point_digest::{
    WorkPanelViewEntryPointDigestError, bind_work_panel_view_entry_point_digest_from_signed,
    expected_work_panel_view_entry_point_integrity,
};

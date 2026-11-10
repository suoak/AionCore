//! Host wiring of the signed-package install / restore chain (M9 E13 slice G).
//!
//! # The one trusted root, and why opening it is allowed
//! The Host opens **one** directory handle, once, at startup: `{data_dir}/extensions-signed`.
//! `data_dir` is the Host's own configured data directory (`AppConfig.data_dir`, the same directory
//! that already holds the database, the extension state file and the key material); the leaf name is
//! a fixed constant. No component of the path comes from a manifest, a package, a hub index entry,
//! `LoadedExtension.directory`, `AIONUI_EXTENSIONS_PATH` or any other extension-controlled string.
//! The handle is opened `O_DIRECTORY | O_NOFOLLOW` (a symlinked leaf is refused), after which the
//! install / restore / uninstall code in `aionui-extension` only ever works relative to it.
//! The trust assumption is the one the Host already makes for its database and key files: its own
//! data directory is not writable by untrusted parties.
//!
//! Only when a signature policy is configured ([`aionui_extension::signature_policy_configured`]) is
//! anything created or opened; otherwise the app behaves exactly as before.

use std::path::Path;
use std::sync::Arc;

use aionui_extension::hub::curated::{CuratedGateError, CuratedInstallPolicy};
use aionui_extension::{HostSignedInstall, SignedRootBindingStore, signature_policy_configured};

/// Open the Host's signed-install parent directory handle (creating the directory if needed).
#[cfg(unix)]
pub(crate) fn open_signed_extensions_parent(
    data_dir: &Path,
) -> std::io::Result<aionui_extension::hub::HostExtensionsParent> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

    let dir = data_dir.join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    // Refuse a symlink / non-directory leaf (the open below also refuses a symlink atomically).
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "signed extensions directory is not a real directory",
        ));
    }
    let handle = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&dir)?;
    aionui_extension::hub::HostExtensionsParent::new(handle)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a directory"))
}

/// Build the signed-install context, then sweep abandoned staging and restore bindings.
///
/// `None` (and no filesystem effect) when no signature policy is configured, when the policy
/// configuration is broken, or when the Host directory cannot be opened — the app then behaves
/// exactly as before. A restore rejection never fails startup: it is logged and the extension is
/// left unbound.
pub(crate) async fn build_host_signed_install(
    data_dir: &Path,
    policy: Result<CuratedInstallPolicy, CuratedGateError>,
    store: Arc<SignedRootBindingStore>,
) -> Option<HostSignedInstall> {
    let policy = match policy {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(error = %error, "signed extensions: signature policy is invalid; signed install disabled");
            return None;
        }
    };
    if !signature_policy_configured(&policy) {
        return None;
    }
    #[cfg(unix)]
    {
        let data_dir = data_dir.to_path_buf();
        let joined = tokio::task::spawn_blocking(move || {
            let parent = match open_signed_extensions_parent(&data_dir) {
                Ok(parent) => parent,
                Err(error) => {
                    tracing::warn!(error = %error, "signed extensions: cannot open the Host directory; signed install disabled");
                    return None;
                }
            };
            let ctx = HostSignedInstall::new(parent, policy, store)?;
            ctx.startup_sweep_and_restore();
            Some(ctx)
        })
        .await;
        match joined {
            Ok(ctx) => ctx,
            Err(_) => {
                tracing::error!("signed extensions: startup task failed; signed install disabled");
                None
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (data_dir, policy, store);
        tracing::warn!("signed extensions: not supported on this platform; signed install disabled");
        None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn verifying_policy() -> CuratedInstallPolicy {
        let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        )
    }

    #[tokio::test]
    async fn no_policy_means_no_directory_no_context() {
        let tmp = tempfile::tempdir().unwrap();
        let none = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new());
        let ctx = build_host_signed_install(tmp.path(), Ok(none), Arc::new(SignedRootBindingStore::new())).await;
        assert!(ctx.is_none());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn broken_policy_config_disables_signed_install_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let broken = Err(CuratedGateError::PubkeyConfig("bad".into()));
        let ctx = build_host_signed_install(tmp.path(), broken, Arc::new(SignedRootBindingStore::new())).await;
        assert!(ctx.is_none());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn configured_policy_opens_the_fixed_host_directory_and_runs_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = build_host_signed_install(
            tmp.path(),
            Ok(verifying_policy()),
            Arc::new(SignedRootBindingStore::new()),
        )
        .await;
        assert!(ctx.is_some());
        assert!(tmp.path().join("extensions-signed").is_dir());
    }

    #[tokio::test]
    async fn symlinked_host_directory_leaf_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), tmp.path().join("extensions-signed")).unwrap();
        let ctx = build_host_signed_install(
            tmp.path(),
            Ok(verifying_policy()),
            Arc::new(SignedRootBindingStore::new()),
        )
        .await;
        assert!(ctx.is_none());
        assert_eq!(std::fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn corrupt_install_content_does_not_fail_startup() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("extensions-signed");
        std::fs::create_dir_all(dir.join("broken")).unwrap();
        std::fs::write(dir.join("broken/current"), b"not json").unwrap();
        std::fs::create_dir_all(dir.join("demo/.wip-abc")).unwrap();
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(tmp.path(), Ok(verifying_policy()), store.clone()).await;
        assert!(ctx.is_some());
        assert!(store.is_empty());
        assert!(!dir.join("demo/.wip-abc").exists());
    }
}

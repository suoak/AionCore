//! Host wiring of the signed-package install / restore chain (M9 E13 slice G).
//!
//! # The one trusted root, and why opening it is allowed
//! The Host opens **one** directory handle, once, at startup: `{data_dir}/extensions-signed`.
//! `data_dir` is the Host's own configured data directory (`AppConfig.data_dir`, the same directory
//! that already holds the database, the extension state file and the key material); the leaf name is
//! a fixed constant. No component of the path comes from a manifest, a package, a hub index entry,
//! `LoadedExtension.directory`, `AIONUI_EXTENSIONS_PATH` or any other extension-controlled string.
//! The handle is opened `O_DIRECTORY | O_NOFOLLOW` on unix; on Windows it is opened with
//! `FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT` and refused when the opened object is
//! a reparse point (symlink / junction / mount point) or not a directory. A linked leaf is refused
//! either way, after which the install / restore / uninstall code in `aionui-extension` only ever
//! works relative to the handle.
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

/// Windows counterpart of the unix opener above (slice W2b): same fixed leaf under the Host data
/// directory, a linked or non-directory leaf is refused, and the handle itself is checked to be a
/// real (non-reparse) directory so a leaf swapped for a junction between the check and the open is
/// caught too.
#[cfg(windows)]
pub(crate) fn open_signed_extensions_parent(
    data_dir: &Path,
) -> std::io::Result<aionui_extension::hub::HostExtensionsParent> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};

    // FILE_FLAG_BACKUP_SEMANTICS (open a directory) | FILE_FLAG_OPEN_REPARSE_POINT (never traverse).
    const OPEN_FLAGS: u32 = 0x0200_0000 | 0x0020_0000;
    // FILE_LIST_DIRECTORY | FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | FILE_TRAVERSE |
    // FILE_READ_ATTRIBUTES | DELETE | SYNCHRONIZE: what the handle-relative install code needs from
    // its parent (the same mask `aionui-extension` gives the directories it creates itself).
    const PARENT_ACCESS: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x80 | 0x1_0000 | 0x10_0000;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let refuse = |why: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidInput, why);

    let dir = data_dir.join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME);
    std::fs::create_dir_all(&dir)?;
    // `symlink_metadata` reports a symlink or junction leaf as a link, never as a directory.
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(refuse("signed extensions directory is not a real directory"));
    }
    let handle = std::fs::OpenOptions::new()
        .access_mode(PARENT_ACCESS)
        .custom_flags(OPEN_FLAGS)
        .open(&dir)?;
    let attributes = handle.metadata()?.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(refuse("signed extensions directory is not a real directory"));
    }
    aionui_extension::hub::HostExtensionsParent::new(handle).map_err(|_| refuse("not a directory"))
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
    #[cfg(any(unix, windows))]
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
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (data_dir, policy, store);
        tracing::warn!("signed extensions: not supported on this platform; signed install disabled");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// Directory link `link -> target`: a symlink on unix, a junction (no privilege needed) on Windows.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(status.status.success(), "mklink /J failed");
        }
    }

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
    async fn linked_host_directory_leaf_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        link_dir(elsewhere.path(), &tmp.path().join("extensions-signed"));
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

    // ---- slice W3: the whole chain end to end (runs for real on windows-latest CI) ----------------

    /// Curator packs and signs an author directory; the Host installs it through the same parent
    /// opener the app uses, "restarts" (everything dropped, fresh binding store), and
    /// `build_host_signed_install` sweeps and restores it; the routes then serve it verified from
    /// the retained root, and refuse a file that was tampered with after install.
    #[tokio::test]
    async fn pack_install_restart_restore_and_verified_serve_end_to_end() {
        use aionui_extension::hub::curated::{signature_payload, sri_sha512};
        use aionui_extension::hub::signed_manifest::SignedManifestEntry;
        use aionui_extension::hub::{
            AutoInstallOptions, LocalPackageLimits, LocalPackageSource, install_local_signed_package_auto,
        };
        use aionui_extension::loader::ScanPath;
        use aionui_extension::pack::{PackOptions, pack_extension_dir};
        use aionui_extension::registry::ExtensionRegistry;
        use aionui_extension::routes::{ExtensionRouterState, extension_routes};
        use aionui_extension::state::ExtensionStateStore;
        use aionui_extension::types::ExtensionSource;
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use ed25519_dalek::Signer;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        const HTML: &[u8] = b"<html>w3 board</html>";

        // -- curator: author directory -> pack -> sign --
        let author = tempfile::tempdir().unwrap();
        let write = |rel: &str, bytes: &[u8]| {
            let path = author.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(
            "aion-extension.json",
            &serde_json::to_vec(&serde_json::json!({
                "name": "demo.plugin", "version": "1.0.0",
                "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
            }))
            .unwrap(),
        );
        write("panels/board.html", HTML);
        write("panels/board.css", b"p{color:red}");
        let packed = pack_extension_dir(author.path(), &PackOptions::default()).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let signature = hex::encode(
            key.sign(signature_payload("demo.plugin", "1.0.0", &packed.manifest_integrity).as_bytes())
                .to_bytes(),
        );

        // -- Host, first run: install through the app's own parent opener --
        let data_dir = tempfile::tempdir().unwrap();
        {
            let parent = open_signed_extensions_parent(data_dir.path()).unwrap();
            let store = SignedRootBindingStore::new();
            let report = install_local_signed_package_auto(
                &store,
                &verifying_policy(),
                &LocalPackageSource {
                    entry: SignedManifestEntry {
                        name: "demo.plugin",
                        version: "1.0.0",
                        pubkey_id: Some("fixture"),
                        integrity: Some(&packed.manifest_integrity),
                        signature: Some(&signature),
                    },
                    raw_manifest: &packed.manifest,
                    package_zip: &packed.zip,
                },
                &parent,
                LocalPackageLimits::default(),
                AutoInstallOptions::default(),
            )
            .unwrap();
            assert_eq!(report.files_written, packed.files.len() + 1);
            assert!(store.contains("demo.plugin"));
        } // parent + store dropped: the "process" ends.

        // -- Host, restart: fresh state, then build_host_signed_install --
        let state_dir = tempfile::tempdir().unwrap();
        let registry = ExtensionRegistry::new(
            ExtensionStateStore::new(state_dir.path().join("states.json")),
            Arc::new(aionui_realtime::BroadcastEventBus::new(64)),
            "1.0.0".into(),
        );
        let router_state = ExtensionRouterState::new(registry.clone());
        let store = router_state.host_runtime.signed_roots.clone();
        assert!(store.is_empty());
        let ctx = build_host_signed_install(data_dir.path(), Ok(verifying_policy()), store.clone()).await;
        assert!(ctx.is_some());
        assert!(store.contains("demo.plugin"), "startup restore re-bound the install");
        assert_eq!(store.lookup_checked("demo.plugin").unwrap().version(), "1.0.0");
        registry.set_signed_roots(store.clone());
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: data_dir.path().join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME),
                source: ExtensionSource::Local,
            }])
            .await
            .unwrap();
        let router =
            extension_routes(router_state).layer(axum::extract::Extension(aionui_auth::CurrentUser::local_default()));
        let get = |uri: &'static str| {
            let router = router.clone();
            async move {
                let response = router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                (
                    response.status(),
                    response.into_body().collect().await.unwrap().to_bytes().to_vec(),
                )
            }
        };

        // -- verified serve from the retained root --
        let (status, body) = get("/api/extensions/demo.plugin/assets/panels/board.html").await;
        assert_eq!((status, &body[..]), (StatusCode::OK, HTML));
        let (status, _) = get("/api/extensions/demo.plugin/assets/aion-extension.json").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "the manifest itself is not an asset");

        // -- tamper on disk after install: verified serve refuses the changed file --
        let root = data_dir
            .path()
            .join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME)
            .join("demo.plugin")
            .join("1.0.0");
        assert_eq!(
            sri_sha512(&std::fs::read(root.join("panels/board.css")).unwrap()),
            packed
                .files
                .iter()
                .find(|f| f.path == "panels/board.css")
                .unwrap()
                .integrity
        );
        std::fs::write(root.join("panels/board.css"), b"evil").unwrap();
        let (status, _) = get("/api/extensions/demo.plugin/assets/panels/board.css").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
}

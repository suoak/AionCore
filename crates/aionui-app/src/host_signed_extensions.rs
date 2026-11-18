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
use aionui_extension::hub::{BuiltinSet, merge_builtin_policy};
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
    // FILE_READ_ATTRIBUTES | SYNCHRONIZE: what the handle-relative install code needs from its
    // parent (the same mask `aionui-extension` gives the directories it creates itself). No
    // `DELETE`: on Windows 10/11/Server 2022 a held `DELETE` handle on a directory makes every
    // rename into it fail with STATUS_SHARING_VIOLATION.
    const PARENT_ACCESS: u32 = 0x1 | 0x2 | 0x4 | 0x20 | 0x80 | 0x10_0000;
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

/// Build the signed-install context, then sweep abandoned staging, restore bindings and ensure the
/// app's built-in signed packs (`builtins`; production: the curator key + three curated packs, see
/// `builtin_extensions`).
///
/// The production built-in key alone makes the policy configured, so a Host with no
/// `WORKMATE_HUB_SIGNING_PUBKEYS` still opens its directory and installs the built-ins.
/// `None` (and no filesystem effect) when no signature policy is configured (no env key and an
/// empty built-in set), when the policy configuration is broken, or when the Host directory cannot
/// be opened — the app then behaves
/// exactly as before. A restore rejection never fails startup: it is logged and the extension is
/// left unbound.
pub(crate) async fn build_host_signed_install(
    data_dir: &Path,
    policy: Result<CuratedInstallPolicy, CuratedGateError>,
    store: Arc<SignedRootBindingStore>,
    builtins: &'static BuiltinSet<'static>,
) -> Option<HostSignedInstall> {
    let policy = match policy {
        Ok(policy) => policy,
        Err(error) => {
            tracing::warn!(error = %error, "signed extensions: signature policy is invalid; signed install disabled");
            return None;
        }
    };
    // Built-in keys are merged in (never overriding a Host key of the same id). With no built-ins
    // this is the identity, so an unconfigured Host stays unconfigured and touches no directory.
    let (policy, merged) = merge_builtin_policy(policy, builtins);
    for id in &merged.conflicting_key_ids {
        tracing::warn!(key_id = %id, "signed extensions: built-in key id collides with a Host key; its packs are skipped");
    }
    for id in &merged.invalid_key_ids {
        tracing::warn!(key_id = %id, "signed extensions: built-in key is not a valid public key; dropped");
    }
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
            // Built-in names are pinned before restore, so a non-pinned copy of one is never re-bound.
            let ctx = HostSignedInstall::new(parent, policy, store)?.with_builtin_pins(builtins);
            ctx.startup_sweep_and_restore();
            ctx.ensure_builtin_packs(builtins);
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
        let _ = (data_dir, policy, store, builtins);
        tracing::warn!("signed extensions: not supported on this platform; signed install disabled");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// No built-ins: the "unconfigured / env-only Host" behaviour, kept separate from the
    /// production set (which configures the policy on its own).
    static NO_BUILTINS: BuiltinSet<'static> = BuiltinSet::EMPTY;

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

    /// The Host-held signed-install parent must not block a rename into it: Windows 10/11/Server
    /// 2022 open a rename's target directory sharing only read + write, which fails while any
    /// handle to it holds `DELETE` (3.0.0 packaged first run: every built-in `PlaceFailed`).
    #[cfg(windows)]
    #[test]
    fn signed_parent_handle_does_not_block_the_kernel_rename_target_open() {
        use std::os::windows::fs::OpenOptionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let _held = open_signed_extensions_parent(tmp.path()).unwrap();
        std::fs::OpenOptions::new()
            .access_mode(0x2 | 0x4 | 0x10_0000) // FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY | SYNCHRONIZE
            .share_mode(0x1 | 0x2) // FILE_SHARE_READ | FILE_SHARE_WRITE, as the kernel does
            .custom_flags(0x0200_0000) // FILE_FLAG_BACKUP_SEMANTICS (open a directory)
            .open(tmp.path().join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME))
            .expect("the Host parent handle must not hold DELETE");
    }

    #[tokio::test]
    async fn no_policy_means_no_directory_no_context() {
        let tmp = tempfile::tempdir().unwrap();
        let none = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new());
        let ctx = build_host_signed_install(
            tmp.path(),
            Ok(none),
            Arc::new(SignedRootBindingStore::new()),
            &NO_BUILTINS,
        )
        .await;
        assert!(ctx.is_none());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn broken_policy_config_disables_signed_install_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let broken = Err(CuratedGateError::PubkeyConfig("bad".into()));
        let ctx = build_host_signed_install(
            tmp.path(),
            broken,
            Arc::new(SignedRootBindingStore::new()),
            &NO_BUILTINS,
        )
        .await;
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
            &NO_BUILTINS,
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
            &NO_BUILTINS,
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
        let ctx = build_host_signed_install(tmp.path(), Ok(verifying_policy()), store.clone(), &NO_BUILTINS).await;
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
        let ctx = build_host_signed_install(data_dir.path(), Ok(verifying_policy()), store.clone(), &NO_BUILTINS).await;
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

    // ---- slice B1: built-in packs through the app's startup path ----------------------------------

    /// A leaked (`'static`) built-in set with one signed pack (test key; independent of the production set).
    fn leaked_builtins(version: &str) -> &'static aionui_extension::hub::BuiltinSet<'static> {
        use aionui_extension::hub::curated::signature_payload;
        use aionui_extension::hub::{BuiltinKey, BuiltinPack, BuiltinSet};
        use aionui_extension::pack::{PackOptions, pack_extension_dir};
        use ed25519_dalek::Signer;

        let author = tempfile::tempdir().unwrap();
        let write = |rel: &str, bytes: &[u8]| {
            let path = author.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(
            "aion-extension.json",
            &serde_json::to_vec(&serde_json::json!({
                "name": "demo.builtin", "version": version,
                "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
            }))
            .unwrap(),
        );
        write("panels/board.html", b"<p>builtin</p>");
        let packed = pack_extension_dir(author.path(), &PackOptions::default()).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[61; 32]);
        let signature = hex::encode(
            key.sign(signature_payload("demo.builtin", version, &packed.manifest_integrity).as_bytes())
                .to_bytes(),
        );
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        let keys: &'static [BuiltinKey<'static>] = Box::leak(Box::new([BuiltinKey {
            id: "app-builtin-test",
            public_key: key.verifying_key().to_bytes(),
        }]));
        let packs: &'static [BuiltinPack<'static>] = Box::leak(Box::new([BuiltinPack {
            name: "demo.builtin",
            version: leak(version.to_owned()),
            pubkey_id: "app-builtin-test",
            integrity: leak(packed.manifest_integrity),
            signature: leak(signature),
            package_zip: Box::leak(packed.zip.into_boxed_slice()),
        }]));
        Box::leak(Box::new(BuiltinSet { keys, packs }))
    }

    fn no_policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new())
    }

    #[tokio::test]
    async fn builtin_pack_installs_at_startup_without_any_env_policy_and_is_restored_next_start() {
        let data_dir = tempfile::tempdir().unwrap();
        let builtins = leaked_builtins("1.0.0");
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(data_dir.path(), Ok(no_policy()), store.clone(), builtins).await;
        assert!(ctx.is_some());
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
        assert!(
            data_dir
                .path()
                .join("extensions-signed")
                .join("demo.builtin")
                .join("1.0.0")
                .join("panels")
                .join("board.html")
                .is_file()
        );
        drop((ctx, store));

        // Restart: restored from disk, ensure is a no-op.
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(data_dir.path(), Ok(no_policy()), store.clone(), builtins).await;
        assert!(ctx.is_some());
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
    }

    #[tokio::test]
    async fn builtin_key_colliding_with_a_host_key_installs_nothing() {
        let data_dir = tempfile::tempdir().unwrap();
        let builtins = leaked_builtins("1.0.0");
        let mut host = no_policy();
        host.pubkeys.insert(
            "app-builtin-test".into(),
            ed25519_dalek::SigningKey::from_bytes(&[62; 32]).verifying_key(),
        );
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(data_dir.path(), Ok(host), store.clone(), builtins).await;
        assert!(ctx.is_some());
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn invalid_policy_config_installs_no_builtins() {
        let data_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SignedRootBindingStore::new());
        let broken = Err(CuratedGateError::PubkeyConfig("bad".into()));
        let ctx = build_host_signed_install(data_dir.path(), broken, store.clone(), leaked_builtins("1.0.0")).await;
        assert!(ctx.is_none() && store.is_empty());
        assert_eq!(std::fs::read_dir(data_dir.path()).unwrap().count(), 0);
    }

    // ---- slice B5: the committed sample packs, installed through the Host path -------------------

    /// A test-local set built from the committed `builtin/index.json` + zips (CI test/dev key only;
    /// production signatures live in `builtin_extensions`).
    fn committed_test_signed_set() -> (&'static aionui_extension::hub::BuiltinSet<'static>, std::path::PathBuf) {
        use aionui_extension::hub::{BuiltinKey, BuiltinPack, BuiltinSet};

        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repo.join("builtin/index.json")).unwrap()).unwrap();
        let leak = |s: &str| -> &'static str { Box::leak(s.to_owned().into_boxed_str()) };
        let mut public_key = [0u8; 32];
        hex::decode_to_slice(index["publicKeyHex"].as_str().unwrap(), &mut public_key).unwrap();
        let key_id = leak(index["pubkeyId"].as_str().unwrap());
        assert_eq!(key_id, "workmate-test-dev");
        let keys: &'static [BuiltinKey<'static>] = Box::leak(Box::new([BuiltinKey { id: key_id, public_key }]));
        let packs: Vec<BuiltinPack<'static>> = index["packs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| BuiltinPack {
                name: leak(e["name"].as_str().unwrap()),
                version: leak(e["version"].as_str().unwrap()),
                pubkey_id: leak(e["pubkeyId"].as_str().unwrap()),
                integrity: leak(e["integrity"].as_str().unwrap()),
                signature: leak(e["signature"].as_str().unwrap()),
                package_zip: Box::leak(
                    std::fs::read(repo.join("builtin").join(e["zip"].as_str().unwrap()))
                        .unwrap()
                        .into_boxed_slice(),
                ),
            })
            .collect();
        let packs: &'static [BuiltinPack<'static>] = Box::leak(packs.into_boxed_slice());
        (Box::leak(Box::new(BuiltinSet { keys, packs })), repo)
    }

    fn repo_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// The committed test-signed set (CI pipeline key) — exercises the same zips as production.
    #[tokio::test]
    async fn committed_test_signed_packs_install_list_and_serve_verified_end_to_end() {
        let (builtins, _) = committed_test_signed_set();
        curated_packs_install_list_and_serve_verified(builtins, no_policy).await;
    }

    /// Production: the real `builtins()` (curator public key + curator-signed packs) with NO
    /// `WORKMATE_HUB_SIGNING_PUBKEYS`: startup installs and binds all three packs, and the theme,
    /// panel, work-panel view and every verified asset are served.
    #[tokio::test]
    async fn production_builtins_install_list_and_serve_verified_without_env_policy() {
        curated_packs_install_list_and_serve_verified(crate::builtin_extensions::builtins(), no_policy).await;
    }

    /// Production built-ins alongside an operator-configured env key: merged, still installed.
    #[tokio::test]
    async fn production_builtins_install_alongside_an_env_configured_host_key() {
        curated_packs_install_list_and_serve_verified(crate::builtin_extensions::builtins(), verifying_policy).await;
    }

    /// The three B5 packs install from their committed zips, survive a restart, are listed by the
    /// B3 listing rule (signed `files` authorise the entry points) and are served verified: every
    /// listed file byte-identical to its source, the manifest never, a tampered file never.
    async fn curated_packs_install_list_and_serve_verified(
        builtins: &'static BuiltinSet<'static>,
        policy: fn() -> CuratedInstallPolicy,
    ) {
        use aionui_extension::loader::ScanPath;
        use aionui_extension::registry::ExtensionRegistry;
        use aionui_extension::routes::{ExtensionRouterState, extension_routes};
        use aionui_extension::state::ExtensionStateStore;
        use aionui_extension::types::ExtensionSource;
        use axum::body::Body;
        use axum::http::{Request, StatusCode, header};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let repo = repo_root();
        assert!(!builtins.is_empty());
        let data_dir = tempfile::tempdir().unwrap();
        // First start installs; second start (fresh in-memory state) restores from disk.
        for pass in 0..2 {
            let state_dir = tempfile::tempdir().unwrap();
            let registry = ExtensionRegistry::new(
                ExtensionStateStore::new(state_dir.path().join("states.json")),
                Arc::new(aionui_realtime::BroadcastEventBus::new(64)),
                "1.0.0".into(),
            );
            let router_state = ExtensionRouterState::new(registry.clone());
            let store = router_state.host_runtime.signed_roots.clone();
            let ctx = build_host_signed_install(data_dir.path(), Ok(policy()), store.clone(), builtins).await;
            assert!(ctx.is_some());
            for name in [
                "workmate.theme-dusk-ember",
                "workmate.sample-shortcuts",
                "workmate.sample-checklist",
            ] {
                assert_eq!(store.lookup_checked(name).unwrap().version(), "1.0.0", "{name}");
            }
            registry.set_signed_roots(store.clone());
            registry
                .initialize_with_scan_paths(vec![ScanPath {
                    path: data_dir.path().join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME),
                    source: ExtensionSource::Local,
                }])
                .await
                .unwrap();
            let router = extension_routes(router_state)
                .layer(axum::extract::Extension(aionui_auth::CurrentUser::local_default()));
            let get = |uri: String| {
                let router = router.clone();
                async move {
                    let response = router
                        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    let status = response.status();
                    let content_type = response
                        .headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    (
                        status,
                        content_type,
                        response.into_body().collect().await.unwrap().to_bytes().to_vec(),
                    )
                }
            };
            let listing = |body: Vec<u8>| -> Vec<serde_json::Value> {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                v["data"].as_array().cloned().unwrap_or_default()
            };

            // -- B3 listing: only the two samples contribute, with the declared ids --
            let (status, _, body) = get("/api/extensions/panels".into()).await;
            assert_eq!(status, StatusCode::OK);
            let panels = listing(body);
            let shortcuts: Vec<_> = panels
                .iter()
                .filter(|p| p["extensionName"] == "workmate.sample-shortcuts")
                .collect();
            assert_eq!(shortcuts.len(), 1, "{panels:?}");
            assert_eq!(shortcuts[0]["id"], "ext-workmate.sample-shortcuts-shortcuts");
            assert_eq!(shortcuts[0]["title"], "Shortcuts & tips");
            assert_eq!(
                shortcuts[0]["entryPoint"],
                "/api/extensions/workmate.sample-shortcuts/assets/panels/shortcuts.html"
            );
            assert!(
                panels.iter().all(|p| p["extensionName"] != "workmate.sample-checklist"),
                "the checklist is a work-panel view, not a panel"
            );
            let (status, _, body) = get("/api/extensions/work-panel-views".into()).await;
            assert_eq!(status, StatusCode::OK);
            let views = listing(body);
            let checklist: Vec<_> = views
                .iter()
                .filter(|v| v["extensionName"] == "workmate.sample-checklist")
                .collect();
            assert_eq!(checklist.len(), 1, "{views:?}");
            assert_eq!(checklist[0]["id"], "ext-workmate.sample-checklist-checklist");
            assert_eq!(checklist[0]["title"], "Plan checklist");
            assert_eq!(
                checklist[0]["entryPoint"],
                "/api/extensions/workmate.sample-checklist/assets/views/checklist.html"
            );
            assert_eq!(checklist[0]["placement"], "secondary");
            assert!(views.iter().all(|v| v["extensionName"] != "workmate.sample-shortcuts"));

            // -- theme: Dusk & Ember is listed with its CSS loaded from the verified root --
            let (status, _, body) = get("/api/extensions/themes".into()).await;
            assert_eq!(status, StatusCode::OK);
            let themes = listing(body);
            let dusk: Vec<_> = themes
                .iter()
                .filter(|t| {
                    t["id"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("ext-workmate.theme-dusk-ember-")
                })
                .collect();
            assert_eq!(dusk.len(), 1, "{themes:?}");
            assert!(
                dusk[0]["name"].as_str().unwrap().starts_with("Dusk & Ember"),
                "{:?}",
                dusk[0]
            );
            let css = dusk[0]["css"].as_str().unwrap();
            let committed_css =
                std::fs::read_to_string(repo.join("extensions/workmate.theme-dusk-ember/themes/dusk-ember.css"))
                    .unwrap();
            assert_eq!(css.trim(), committed_css.trim());

            // -- verified serve: every file is byte-identical to its source and typed --
            for (name, rel, content_type) in [
                ("workmate.theme-dusk-ember", "themes/dusk-ember.css", "text/css"),
                ("workmate.theme-dusk-ember", "assets/cover.svg", "image/svg+xml"),
                ("workmate.sample-shortcuts", "panels/shortcuts.html", "text/html"),
                ("workmate.sample-shortcuts", "panels/shortcuts.css", "text/css"),
                ("workmate.sample-shortcuts", "assets/icon.svg", "image/svg+xml"),
                ("workmate.sample-checklist", "views/checklist.html", "text/html"),
                ("workmate.sample-checklist", "views/checklist.css", "text/css"),
                ("workmate.sample-checklist", "views/checklist.js", "javascript"),
                ("workmate.sample-checklist", "assets/icon.svg", "image/svg+xml"),
            ] {
                let (status, ct, body) = get(format!("/api/extensions/{name}/assets/{rel}")).await;
                assert_eq!(status, StatusCode::OK, "{name}/{rel}");
                assert!(ct.contains(content_type), "{name}/{rel}: {ct}");
                assert_eq!(
                    body,
                    std::fs::read(repo.join("extensions").join(name).join(rel)).unwrap()
                );
            }
            for name in ["workmate.sample-shortcuts", "workmate.sample-checklist"] {
                let (status, _, _) = get(format!("/api/extensions/{name}/assets/aion-extension.json")).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{name}: the manifest is not an asset");
                let (status, _, _) = get(format!("/api/extensions/{name}/assets/i18n/fr.json")).await;
                assert_ne!(status, StatusCode::OK, "{name}: an unlisted file is never served");
            }

            // -- tamper after install (last pass): verified serve refuses the changed file --
            if pass == 1 {
                let root = data_dir
                    .path()
                    .join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME)
                    .join("workmate.sample-checklist")
                    .join("1.0.0");
                std::fs::write(root.join("views/checklist.js"), b"evil()").unwrap();
                let (status, _, _) =
                    get("/api/extensions/workmate.sample-checklist/assets/views/checklist.js".into()).await;
                assert_eq!(status, StatusCode::FORBIDDEN);
            }
        }
    }

    // ---- production built-ins: startup behaviour without any env configuration ---------------

    #[tokio::test]
    async fn production_builtins_configure_the_host_without_env_and_restore_on_restart() {
        let data_dir = tempfile::tempdir().unwrap();
        for _pass in 0..2 {
            let store = Arc::new(SignedRootBindingStore::new());
            let ctx = build_host_signed_install(
                data_dir.path(),
                Ok(no_policy()),
                store.clone(),
                crate::builtin_extensions::builtins(),
            )
            .await;
            assert!(ctx.is_some(), "the curator key alone configures the policy");
            for pack in crate::builtin_extensions::builtins().packs {
                assert_eq!(store.lookup_checked(pack.name).unwrap().version(), pack.version);
                assert!(
                    data_dir
                        .path()
                        .join(aionui_extension::SIGNED_EXTENSIONS_DIR_NAME)
                        .join(pack.name)
                        .join(pack.version)
                        .join("aion-extension.json")
                        .is_file()
                );
            }
        }
    }

    // ---- slice S3b-1: built-in names are reserved and pinned by the app binary -------------------

    #[tokio::test]
    async fn builtin_follows_the_app_binary_on_update_and_on_rollback() {
        let data_dir = tempfile::tempdir().unwrap();
        let (old, new) = (leaked_builtins("1.0.0"), leaked_builtins("1.1.0"));
        for (set, want) in [(old, "1.0.0"), (new, "1.1.0"), (old, "1.0.0"), (old, "1.0.0")] {
            let store = Arc::new(SignedRootBindingStore::new());
            let ctx = build_host_signed_install(data_dir.path(), Ok(no_policy()), store.clone(), set).await;
            assert!(ctx.is_some());
            assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), want);
        }
    }

    /// A validly signed package under a production built-in name from another signer the Host
    /// trusts (env key) is on disk, newer than the built-in: startup does not re-bind it and puts
    /// the curator-signed built-in back; the hub path cannot bring it back either.
    #[tokio::test]
    async fn a_squatted_production_builtin_name_is_replaced_by_the_pinned_pack_at_startup() {
        use aionui_extension::hub::curated::signature_payload;
        use aionui_extension::hub::signed_manifest::SignedManifestEntry;
        use aionui_extension::hub::{
            AutoInstallOptions, LocalPackageLimits, LocalPackageSource, install_local_signed_package_auto,
        };
        use aionui_extension::pack::{PackOptions, pack_extension_dir};
        use ed25519_dalek::Signer;

        let pinned = &crate::builtin_extensions::builtins().packs[1];
        let third = ed25519_dalek::SigningKey::from_bytes(&[64; 32]);
        let host = || {
            let mut p = no_policy();
            p.pubkeys.insert("third".into(), third.verifying_key());
            p
        };

        let author = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(author.path().join("panels")).unwrap();
        std::fs::write(
            author.path().join("aion-extension.json"),
            serde_json::to_vec(&serde_json::json!({
                "name": pinned.name, "version": "9.9.9",
                "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(author.path().join("panels/board.html"), b"<p>squatter</p>").unwrap();
        let packed = pack_extension_dir(author.path(), &PackOptions::default()).unwrap();
        let signature = hex::encode(
            third
                .sign(signature_payload(pinned.name, "9.9.9", &packed.manifest_integrity).as_bytes())
                .to_bytes(),
        );
        let data_dir = tempfile::tempdir().unwrap();
        {
            // Installed before this app version reserved the name (no pins in play here).
            let parent = open_signed_extensions_parent(data_dir.path()).unwrap();
            let raw = aionui_extension::hub::read_package_manifest(&packed.zip).unwrap();
            install_local_signed_package_auto(
                &SignedRootBindingStore::new(),
                &host(),
                &LocalPackageSource {
                    entry: SignedManifestEntry {
                        name: pinned.name,
                        version: "9.9.9",
                        pubkey_id: Some("third"),
                        integrity: Some(&packed.manifest_integrity),
                        signature: Some(&signature),
                    },
                    raw_manifest: &raw,
                    package_zip: &packed.zip,
                },
                &parent,
                LocalPackageLimits::default(),
                AutoInstallOptions::default(),
            )
            .unwrap();
        }
        for _restart in 0..2 {
            let store = Arc::new(SignedRootBindingStore::new());
            let ctx = build_host_signed_install(
                data_dir.path(),
                Ok(host()),
                store.clone(),
                crate::builtin_extensions::builtins(),
            )
            .await
            .expect("configured");
            let bound = store.lookup_checked(pinned.name).unwrap();
            assert_eq!(
                (bound.version(), bound.key_id()),
                (pinned.version, crate::builtin_extensions::CURATOR_KEY_ID)
            );
            assert_eq!(
                aionui_extension::hub::curated::sri_sha512(bound.raw_bytes()),
                pinned.integrity
            );
            for pack in crate::builtin_extensions::builtins().packs {
                assert!(ctx.is_builtin_name(pack.name));
            }
        }
    }

    #[tokio::test]
    async fn empty_builtin_set_without_env_stays_an_unconfigured_host() {
        let data_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(data_dir.path(), Ok(no_policy()), store.clone(), &NO_BUILTINS).await;
        assert!(ctx.is_none() && store.is_empty());
        assert_eq!(std::fs::read_dir(data_dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn host_key_colliding_with_the_curator_key_id_installs_no_production_pack() {
        let data_dir = tempfile::tempdir().unwrap();
        let mut host = no_policy();
        host.pubkeys.insert(
            crate::builtin_extensions::CURATOR_KEY_ID.into(),
            ed25519_dalek::SigningKey::from_bytes(&[63; 32]).verifying_key(),
        );
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(
            data_dir.path(),
            Ok(host),
            store.clone(),
            crate::builtin_extensions::builtins(),
        )
        .await;
        assert!(ctx.is_some());
        assert!(
            store.is_empty(),
            "a colliding Host key never lets the curator packs through"
        );
    }

    #[tokio::test]
    async fn broken_policy_config_installs_no_production_builtins() {
        let data_dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SignedRootBindingStore::new());
        let ctx = build_host_signed_install(
            data_dir.path(),
            Err(CuratedGateError::PubkeyConfig("bad".into())),
            store.clone(),
            crate::builtin_extensions::builtins(),
        )
        .await;
        assert!(ctx.is_none() && store.is_empty());
        assert_eq!(std::fs::read_dir(data_dir.path()).unwrap().count(), 0);
    }
}

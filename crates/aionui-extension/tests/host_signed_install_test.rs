//! M9 E13 slice G: Host wiring of signed local-package install / restore / uninstall through the
//! existing hub installer. Unix only (FD-relative Host layout).
#![cfg(unix)]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
use aionui_extension::hub::{
    BuiltinKey, BuiltinPack, BuiltinSet, HostExtensionsParent, HostSignedInstall, HubIndexManager, HubInstaller,
    RestoreRejection, signature_policy_configured,
};
use aionui_extension::loader::ScanPath;
use aionui_extension::registry::ExtensionRegistry;
use aionui_extension::routes::{ExtensionRouterState, extension_routes};
use aionui_extension::signed_root_binding::SignedRootBindingStore;
use aionui_extension::state::ExtensionStateStore;
use aionui_extension::types::ExtensionSource;
use aionui_realtime::BroadcastEventBus;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use tempfile::TempDir;

const HTML: &[u8] = b"<html>signed board</html>";
const CSS: &[u8] = b"p{color:red}";

fn key() -> SigningKey {
    SigningKey::from_bytes(&[41; 32])
}

fn policy() -> CuratedInstallPolicy {
    CuratedInstallPolicy::new(
        HashSet::new(),
        false,
        HashMap::from([("fixture".into(), key().verifying_key())]),
    )
}

fn manifest(version: &str, html: &[u8]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "name": "demo.plugin", "version": version,
        "files": [
            {"path": "panels/board.html", "integrity": sri_sha512(html)},
            {"path": "panels/board.css", "integrity": sri_sha512(CSS)},
        ],
        "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
    }))
    .unwrap()
}

fn package(raw: &[u8], html: &[u8]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    for (name, bytes) in [
        ("aion-extension.json", raw),
        ("panels/board.html", html),
        ("panels/board.css", CSS),
    ] {
        w.start_file(name, o).unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap().into_inner()
}

fn index_entry(raw: &[u8], version: &str, sign_with: &SigningKey) -> serde_json::Value {
    let integrity = sri_sha512(raw);
    let signature = hex::encode(
        sign_with
            .sign(signature_payload("demo.plugin", version, &integrity).as_bytes())
            .to_bytes(),
    );
    json!({
        "name": "demo.plugin", "version": version, "curated": true,
        "integrity": integrity, "signature": signature, "pubkey_id": "fixture",
    })
}

struct Env {
    hub: TempDir,
    signed_dir: TempDir,
    _state: TempDir,
    store: Arc<SignedRootBindingStore>,
    registry: ExtensionRegistry,
    router_state: ExtensionRouterState,
    installer: HubInstaller,
    ctx: Option<HostSignedInstall>,
}

async fn env(with_policy: bool) -> Env {
    env_pinned(with_policy, None).await
}

/// [`env`] whose signed-install context reserves the names in `builtins`.
async fn env_pinned(with_policy: bool, builtins: Option<&BuiltinSet<'_>>) -> Env {
    let hub = TempDir::new().unwrap();
    let signed_dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let registry = ExtensionRegistry::new(
        ExtensionStateStore::new(state.path().join("states.json")),
        Arc::new(BroadcastEventBus::new(64)),
        "1.0.0".into(),
    );
    registry
        .initialize_with_scan_paths(vec![
            ScanPath {
                path: signed_dir.path().to_path_buf(),
                source: ExtensionSource::Local,
            },
            ScanPath {
                path: hub.path().to_path_buf(),
                source: ExtensionSource::Local,
            },
        ])
        .await
        .unwrap();
    let router_state = ExtensionRouterState::new(registry.clone());
    let store = router_state.host_runtime.signed_roots.clone();
    registry.set_signed_roots(store.clone());
    let ctx = if with_policy {
        HostSignedInstall::new(
            HostExtensionsParent::new(File::open(signed_dir.path()).unwrap()).unwrap(),
            policy(),
            store.clone(),
        )
        .map(|ctx| match builtins {
            Some(set) => ctx.with_builtin_pins(set),
            None => ctx,
        })
    } else {
        None
    };
    let index_manager = HubIndexManager::new(hub.path().to_path_buf(), registry.clone());
    let installer = HubInstaller::new(index_manager, registry.clone()).with_signed_install(ctx.clone());
    Env {
        hub,
        signed_dir,
        _state: state,
        store,
        registry,
        router_state,
        installer,
        ctx,
    }
}

fn write_index(dir: &Path, entries: &[serde_json::Value]) {
    std::fs::write(
        dir.join("index.json"),
        serde_json::to_vec(&json!({"schema_version": 1, "extensions": entries})).unwrap(),
    )
    .unwrap();
}

async fn stage(env: &Env, version: &str, html: &[u8], sign_with: &SigningKey) {
    let raw = manifest(version, html);
    std::fs::write(env.hub.path().join("demo.plugin.zip"), package(&raw, html)).unwrap();
    write_index(env.hub.path(), &[index_entry(&raw, version, sign_with)]);
}

#[test]
fn only_a_verifying_or_requiring_policy_counts_as_configured() {
    assert!(signature_policy_configured(&policy()));
    assert!(signature_policy_configured(&CuratedInstallPolicy::new(
        HashSet::new(),
        true,
        HashMap::new()
    )));
    assert!(!signature_policy_configured(&CuratedInstallPolicy::new(
        HashSet::from(["demo.plugin".to_owned()]),
        false,
        HashMap::new()
    )));
    let tmp = TempDir::new().unwrap();
    let parent = HostExtensionsParent::new(File::open(tmp.path()).unwrap()).unwrap();
    assert!(
        HostSignedInstall::new(
            parent,
            CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new()),
            Arc::new(SignedRootBindingStore::new())
        )
        .is_none()
    );
}

#[tokio::test]
async fn signed_package_installs_binds_loads_and_survives_restart_and_upgrade() {
    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    let result = e.installer.install("demo.plugin").await;
    assert!(result.success, "{result:?}");
    assert!(e.store.contains("demo.plugin"));
    // The registry sees it through the Host layout, from the signed dir.
    let ext = e.registry.get_extension_by_name("demo.plugin").await.expect("loaded");
    assert!(ext.directory.starts_with(e.signed_dir.path().to_str().unwrap()));
    assert!(ext.directory.ends_with("demo.plugin/1.0.0"));

    // Restart: a fresh store + restore from the marker, handles only.
    let store2 = Arc::new(SignedRootBindingStore::new());
    let ctx2 = HostSignedInstall::new(
        HostExtensionsParent::new(File::open(e.signed_dir.path()).unwrap()).unwrap(),
        policy(),
        store2.clone(),
    )
    .unwrap();
    let report = ctx2.startup_sweep_and_restore();
    assert_eq!((report.restored, report.rejected.len()), (1, 0), "{report:?}");
    assert!(store2.contains("demo.plugin"));

    // Upgrade = hand over a newer package.
    stage(&e, "1.1.0", b"<html>v2</html>", &key()).await;
    let up = e.installer.update("demo.plugin").await;
    assert!(up.success, "{up:?}");
    let loaded = e.registry.get_loaded_extensions().await;
    assert_eq!(
        loaded.iter().find(|l| l.name == "demo.plugin").unwrap().version,
        "1.1.0"
    );
}

#[tokio::test]
async fn bad_signature_is_refused_and_never_falls_back_to_the_unsigned_directory_flow() {
    let e = env(true).await;
    // A perfectly loadable legacy directory exists for the same name...
    let legacy = e.hub.path().join("demo.plugin");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("aion-extension.json"),
        serde_json::to_vec(&json!({"name": "demo.plugin", "version": "1.0.0"})).unwrap(),
    )
    .unwrap();
    // ...but the supplied package is signed by an untrusted key.
    stage(&e, "1.0.0", HTML, &SigningKey::from_bytes(&[99; 32])).await;
    let result = e.installer.install("demo.plugin").await;
    assert!(!result.success);
    assert!(!e.store.contains("demo.plugin"));
    assert_eq!(std::fs::read_dir(e.signed_dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn tampered_package_bytes_and_unsigned_entry_are_refused() {
    let e = env(true).await;
    let raw = manifest("1.0.0", HTML);
    std::fs::write(
        e.hub.path().join("demo.plugin.zip"),
        package(&raw, b"<html>tampered</html>"),
    )
    .unwrap();
    write_index(e.hub.path(), &[index_entry(&raw, "1.0.0", &key())]);
    assert!(!e.installer.install("demo.plugin").await.success);
    // Entry without signature metadata while a policy is configured.
    write_index(
        e.hub.path(),
        &[json!({"name": "demo.plugin", "version": "1.0.0", "curated": true})],
    );
    std::fs::write(e.hub.path().join("demo.plugin.zip"), package(&raw, HTML)).unwrap();
    assert!(!e.installer.install("demo.plugin").await.success);
    assert!(!e.store.contains("demo.plugin"));
}

#[tokio::test]
async fn restore_rejection_is_reported_and_leaves_the_extension_unbound_without_failing_startup() {
    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    assert!(e.installer.install("demo.plugin").await.success);
    // Tamper an installed file behind the Host's back.
    std::fs::write(e.signed_dir.path().join("demo.plugin/1.0.0/panels/board.css"), b"evil").unwrap();
    // Plus an unrelated directory that was never installed through the Host layout.
    std::fs::create_dir(e.signed_dir.path().join("stray")).unwrap();
    let store2 = Arc::new(SignedRootBindingStore::new());
    let ctx2 = HostSignedInstall::new(
        HostExtensionsParent::new(File::open(e.signed_dir.path()).unwrap()).unwrap(),
        policy(),
        store2.clone(),
    )
    .unwrap();
    let report = ctx2.startup_sweep_and_restore();
    assert_eq!(report.restored, 0);
    assert!(
        report
            .rejected
            .contains(&("demo.plugin".to_owned(), RestoreRejection::IntegrityMismatch))
    );
    assert!(
        report
            .rejected
            .contains(&("stray".to_owned(), RestoreRejection::MarkerMissing))
    );
    assert!(store2.is_empty());
}

#[tokio::test]
async fn startup_sweeps_abandoned_staging() {
    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    assert!(e.installer.install("demo.plugin").await.success);
    let wip = e.signed_dir.path().join("demo.plugin/.wip-deadbeef");
    std::fs::create_dir(&wip).unwrap();
    std::fs::write(wip.join("x"), b"x").unwrap();
    let report = e.ctx.as_ref().unwrap().startup_sweep_and_restore();
    assert_eq!(report.swept_removed, 1);
    assert!(!wip.exists());
}

#[tokio::test]
async fn uninstall_removes_marker_binding_and_tree_and_the_registry_entry() {
    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    assert!(e.installer.install("demo.plugin").await.success);
    let result = e.installer.uninstall("demo.plugin").await;
    assert!(result.success, "{result:?}");
    assert!(!e.store.contains("demo.plugin"));
    assert!(!e.signed_dir.path().join("demo.plugin").exists());
    assert!(
        e.registry
            .get_loaded_extensions()
            .await
            .iter()
            .all(|l| l.name != "demo.plugin")
    );
    // A restart finds nothing to re-bind.
    let report = e.ctx.as_ref().unwrap().startup_sweep_and_restore();
    assert_eq!(report.restored, 0);
    assert!(!e.installer.uninstall("demo.plugin").await.success);
}

#[tokio::test]
async fn without_a_policy_behaviour_is_exactly_todays_directory_flow() {
    let e = env(false).await;
    assert!(!e.installer.signed_install_enabled());
    // Signed-looking package is ignored; the legacy directory flow decides.
    stage(&e, "1.0.0", HTML, &key()).await;
    let r = e.installer.install("demo.plugin").await;
    assert!(!r.success, "no directory => legacy 'directory not found' failure");
    assert!(e.store.is_empty());
    let legacy = e.hub.path().join("demo.plugin");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("aion-extension.json"), manifest("1.0.0", HTML)).unwrap();
    // Legacy flow soft gate still needs a valid signature (entry is signed) and passes here.
    let r = e.installer.install("demo.plugin").await;
    assert!(r.success, "{r:?}");
    assert!(e.store.is_empty(), "legacy flow never creates a signed binding");
}

#[tokio::test]
async fn installed_signed_extension_is_served_verified_from_its_retained_root() {
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    assert!(e.installer.install("demo.plugin").await.success);
    let router = extension_routes(e.router_state.clone()).layer(Extension(aionui_auth::CurrentUser::local_default()));
    let get = |uri: &'static str| {
        let router = router.clone();
        async move {
            let r = router
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            (r.status(), r.into_body().collect().await.unwrap().to_bytes())
        }
    };
    let (status, body) = get("/api/extensions/demo.plugin/assets/panels/board.html").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, HTML));
    let (status, _) = get("/api/extensions/demo.plugin/assets/aion-extension.json").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Tamper after install: verified serve refuses, with no path fallback.
    std::fs::write(e.signed_dir.path().join("demo.plugin/1.0.0/panels/board.css"), b"evil").unwrap();
    let (status, _) = get("/api/extensions/demo.plugin/assets/panels/board.css").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tampered_disk_manifest_is_not_advertised_after_reload() {
    let e = env(true).await;
    stage(&e, "1.0.0", HTML, &key()).await;
    assert!(e.installer.install("demo.plugin").await.success);
    assert!(e.registry.get_extension_by_name("demo.plugin").await.is_some());
    assert!(
        e.registry
            .get_agent_tools()
            .await
            .iter()
            .all(|t| t.extension_name != "demo.plugin")
    );
    // Tamper the signed tree's manifest to advertise a tool the signature never covered.
    let manifest_path = e.signed_dir.path().join("demo.plugin/1.0.0/aion-extension.json");
    let mut tampered: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    tampered["contributes"]["agentTools"] = serde_json::json!([{"id": "evil", "name": "Evil", "handler": "evil.js"}]);
    std::fs::write(&manifest_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    e.registry.hot_reload().await;
    assert!(
        e.registry.get_extension_by_name("demo.plugin").await.is_none(),
        "mismatch must refuse the extension entirely"
    );
    assert!(
        e.registry
            .get_agent_tools()
            .await
            .iter()
            .all(|t| t.id != "evil" && t.extension_name != "demo.plugin")
    );
    // Binding remains; verified serve still uses the snapshot (not the registry).
    assert!(e.store.contains("demo.plugin"));
}

#[tokio::test]
async fn hub_install_and_update_of_a_builtin_name_are_refused_before_any_flow() {
    // The app pins `demo.plugin` 1.0.0 (signed with the fixture key) as a built-in.
    let raw = manifest("1.0.0", HTML);
    let zip = package(&raw, HTML);
    let integrity = sri_sha512(&raw);
    let signature = hex::encode(
        key()
            .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
            .to_bytes(),
    );
    let keys = [BuiltinKey {
        id: "fixture",
        public_key: key().verifying_key().to_bytes(),
    }];
    let packs = [BuiltinPack {
        name: "demo.plugin",
        version: "1.0.0",
        pubkey_id: "fixture",
        integrity: &integrity,
        signature: &signature,
        package_zip: &zip,
    }];
    let set = BuiltinSet {
        keys: &keys,
        packs: &packs,
    };
    let e = env_pinned(true, Some(&set)).await;
    let refused = |r: &aionui_extension::hub::installer::HubResult| {
        !r.success && r.msg.as_deref().is_some_and(|m| m.contains("built into CSBU WorkMate"))
    };

    // A validly signed, curated, newer package of the built-in name (signed path).
    stage(&e, "1.1.0", b"<html>v2</html>", &key()).await;
    let r = e.installer.install("demo.plugin").await;
    assert!(refused(&r), "{r:?}");
    let r = e.installer.update("demo.plugin").await;
    assert!(refused(&r), "{r:?}");
    assert!(e.store.is_empty());
    assert_eq!(std::fs::read_dir(e.signed_dir.path()).unwrap().count(), 0);

    // The unsigned directory flow is refused too (no package file, loadable legacy directory).
    std::fs::remove_file(e.hub.path().join("demo.plugin.zip")).unwrap();
    let legacy = e.hub.path().join("demo.plugin");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(
        legacy.join("aion-extension.json"),
        manifest("1.1.0", b"<html>v2</html>"),
    )
    .unwrap();
    let r = e.installer.install("demo.plugin").await;
    assert!(refused(&r), "{r:?}");

    // A case variant of the built-in name, even flagged `bundled` in the (writable) index.
    write_index(
        e.hub.path(),
        &[json!({"name": "Demo.Plugin", "version": "1.0.0", "curated": true, "bundled": true})],
    );
    let r = e.installer.install("Demo.Plugin").await;
    assert!(refused(&r), "{r:?}");
    assert!(e.store.is_empty());
}

//! M9 E13 packing helper: pack -> sign (test key) -> `install_local_signed_package_auto` ->
//! restore -> verified serve, plus end-to-end checks of the `workmate-pack` binary. Runs on unix
//! (FD-relative Host layout) and on Windows (handle-relative layout, slice W3) — the Windows CI job
//! runs it for real, so the whole signed-install chain is exercised on NTFS.
#![cfg(any(unix, windows))]

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
use aionui_extension::hub::signed_manifest::SignedManifestEntry;
use aionui_extension::hub::{
    AutoInstallOptions, HostExtensionsParent, HostSignedInstall, LocalPackageError, LocalPackageLimits,
    LocalPackageSource, install_local_signed_package_auto,
};
use aionui_extension::loader::ScanPath;
use aionui_extension::pack::{PackOptions, pack_extension_dir};
use aionui_extension::registry::ExtensionRegistry;
use aionui_extension::routes::{ExtensionRouterState, extension_routes};
use aionui_extension::signed_root_binding::SignedRootBindingStore;
use aionui_extension::state::ExtensionStateStore;
use aionui_extension::types::ExtensionSource;
use aionui_realtime::BroadcastEventBus;
use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{Request, StatusCode};
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

const HTML: &[u8] = b"<html>packed board</html>";
const LOGO: &[u8] = &[0, 1, 2, 3, 254, 255];

/// Open the Host-held parent directory handle: a plain `File::open` on unix, the same access mask
/// the Host app uses (`open_signed_extensions_parent`) on Windows.
fn open_host_dir(path: &Path) -> File {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // list | add file | add subdir | traverse | read attrs | DELETE | SYNCHRONIZE, opened with
        // FILE_FLAG_BACKUP_SEMANTICS so a directory can be opened at all.
        std::fs::OpenOptions::new()
            .access_mode(0x1 | 0x2 | 0x4 | 0x20 | 0x80 | 0x1_0000 | 0x10_0000)
            .custom_flags(0x0200_0000)
            .open(path)
            .unwrap()
    }
    #[cfg(not(windows))]
    {
        File::open(path).unwrap()
    }
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&[52; 32])
}

fn policy() -> CuratedInstallPolicy {
    CuratedInstallPolicy::new(
        HashSet::new(),
        false,
        HashMap::from([("fixture".into(), key().verifying_key())]),
    )
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// An author's source directory: a manifest WITHOUT `files`, plus panel, css, i18n and a binary asset.
fn author_dir() -> TempDir {
    let tmp = TempDir::new().unwrap();
    write(
        tmp.path(),
        "aion-extension.json",
        serde_json::to_vec_pretty(&serde_json::json!({
            "name": "demo.plugin", "version": "1.0.0",
            "i18n": {"locales": ["en"], "directory": "i18n"},
            "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
        }))
        .unwrap()
        .as_slice(),
    );
    write(tmp.path(), "panels/board.html", HTML);
    write(tmp.path(), "panels/board.css", b"p{color:red}");
    write(tmp.path(), "assets/logo.bin", LOGO);
    write(tmp.path(), "i18n/en.json", br#"{"greeting":"packed hello"}"#);
    tmp
}

fn sign(name: &str, version: &str, integrity: &str) -> String {
    hex::encode(
        key()
            .sign(signature_payload(name, version, integrity).as_bytes())
            .to_bytes(),
    )
}

fn install(
    store: &SignedRootBindingStore,
    parent: &HostExtensionsParent,
    manifest: &[u8],
    zip: &[u8],
    integrity: &str,
) -> Result<(), LocalPackageError> {
    let signature = sign("demo.plugin", "1.0.0", integrity);
    install_local_signed_package_auto(
        store,
        &policy(),
        &LocalPackageSource {
            entry: SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(integrity),
                signature: Some(&signature),
            },
            raw_manifest: manifest,
            package_zip: zip,
        },
        parent,
        LocalPackageLimits::default(),
        AutoInstallOptions::default(),
    )
    .map(|_| ())
}

async fn get(router: &Router, method: &str, uri: &str, body: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let r = router
        .clone()
        .oneshot(req.body(Body::from(body.unwrap_or("").to_owned())).unwrap())
        .await
        .unwrap();
    (r.status(), r.into_body().collect().await.unwrap().to_bytes().to_vec())
}

#[tokio::test]
async fn packed_package_installs_restores_and_is_served_verified() {
    let src = author_dir();
    let packed = pack_extension_dir(src.path(), &PackOptions::default()).unwrap();
    assert_eq!(packed.files.len(), 4);

    // Sign (curator step) + install through the Host's auto installer.
    let signed_dir = TempDir::new().unwrap();
    let parent = HostExtensionsParent::new(open_host_dir(signed_dir.path())).unwrap();
    let store = SignedRootBindingStore::new();
    install(
        &store,
        &parent,
        &packed.manifest,
        &packed.zip,
        &packed.manifest_integrity,
    )
    .unwrap();
    assert!(store.contains("demo.plugin"));
    // Every packed file landed, byte for byte, under {name}/{version}.
    let root = signed_dir.path().join("demo.plugin/1.0.0");
    for f in &packed.files {
        assert_eq!(
            sri_sha512(&std::fs::read(root.join(&f.path)).unwrap()),
            f.integrity,
            "{}",
            f.path
        );
    }
    assert_eq!(
        std::fs::read(root.join("aion-extension.json")).unwrap(),
        packed.manifest
    );

    // Restart: fresh store, restore from the marker, then load + serve from the retained root.
    let state = TempDir::new().unwrap();
    let registry = ExtensionRegistry::new(
        ExtensionStateStore::new(state.path().join("states.json")),
        Arc::new(BroadcastEventBus::new(64)),
        "1.0.0".into(),
    );
    let router_state = ExtensionRouterState::new(registry.clone());
    let store2 = router_state.host_runtime.signed_roots.clone();
    registry.set_signed_roots(store2.clone());
    let ctx = HostSignedInstall::new(
        HostExtensionsParent::new(open_host_dir(signed_dir.path())).unwrap(),
        policy(),
        store2.clone(),
    )
    .unwrap();
    let report = ctx.startup_sweep_and_restore();
    assert_eq!((report.restored, report.rejected.len()), (1, 0), "{report:?}");
    registry
        .initialize_with_scan_paths(vec![ScanPath {
            path: signed_dir.path().to_path_buf(),
            source: ExtensionSource::Local,
        }])
        .await
        .unwrap();
    let router = extension_routes(router_state).layer(Extension(aionui_auth::CurrentUser::local_default()));

    let (status, body) = get(
        &router,
        "GET",
        "/api/extensions/demo.plugin/assets/panels/board.html",
        None,
    )
    .await;
    assert_eq!((status, &body[..]), (StatusCode::OK, HTML));
    let (status, body) = get(
        &router,
        "GET",
        "/api/extensions/demo.plugin/assets/assets/logo.bin",
        None,
    )
    .await;
    assert_eq!((status, &body[..]), (StatusCode::OK, LOGO));
    let (status, _) = get(
        &router,
        "GET",
        "/api/extensions/demo.plugin/assets/aion-extension.json",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = get(&router, "POST", "/api/extensions/i18n", Some(r#"{"locale":"en"}"#)).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["demo.plugin"]["greeting"], "packed hello");

    // Tamper after install: verified serve refuses.
    std::fs::write(root.join("panels/board.css"), b"evil").unwrap();
    let (status, _) = get(
        &router,
        "GET",
        "/api/extensions/demo.plugin/assets/panels/board.css",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[test]
fn packer_output_is_rejected_by_the_installer_when_swapped_or_unsigned() {
    let src = author_dir();
    let packed = pack_extension_dir(src.path(), &PackOptions::default()).unwrap();
    let signed_dir = TempDir::new().unwrap();
    let parent = HostExtensionsParent::new(open_host_dir(signed_dir.path())).unwrap();
    let store = SignedRootBindingStore::new();

    // A zip packed from edited sources does not match the signed manifest's SRIs.
    write(src.path(), "panels/board.html", b"<html>edited after signing</html>");
    let edited = pack_extension_dir(src.path(), &PackOptions::default()).unwrap();
    let err = install(
        &store,
        &parent,
        &packed.manifest,
        &edited.zip,
        &packed.manifest_integrity,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            LocalPackageError::ManifestMismatch | LocalPackageError::IntegrityMismatch
        ),
        "{err:?}"
    );
    // A signature over a different manifest is refused.
    let err = install(
        &store,
        &parent,
        &edited.manifest,
        &edited.zip,
        &packed.manifest_integrity,
    )
    .unwrap_err();
    assert!(matches!(err, LocalPackageError::Signature(_)), "{err:?}");
    assert!(!store.contains("demo.plugin"));
    assert_eq!(std::fs::read_dir(signed_dir.path()).unwrap().count(), 0);
}

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_workmate-pack"))
}

#[test]
fn cli_dry_run_writes_nothing_and_reports_the_manifest_integrity() {
    let src = author_dir();
    let before = std::fs::read(src.path().join("aion-extension.json")).unwrap();
    let out = bin().arg(src.path()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["name"], "demo.plugin");
    assert_eq!(report["fileCount"], 4);
    assert_eq!(report["filesChanged"], true);
    assert_eq!(report["written"], serde_json::json!([]));
    let packed = pack_extension_dir(src.path(), &PackOptions::default()).unwrap();
    assert_eq!(report["manifestIntegrity"], packed.manifest_integrity);
    assert_eq!(std::fs::read(src.path().join("aion-extension.json")).unwrap(), before);
}

#[test]
fn cli_writes_zip_and_manifest_and_update_then_check_is_clean() {
    let src = author_dir();
    let outdir = TempDir::new().unwrap();
    let zip = outdir.path().join("demo.zip");
    let manifest = outdir.path().join("manifest.json");

    // --check on a stale manifest: exit 3, nothing written.
    let check = bin().arg(src.path()).arg("--check").output().unwrap();
    assert_eq!(check.status.code(), Some(3));

    let out = bin()
        .arg(src.path())
        .args(["--zip-out"])
        .arg(&zip)
        .args(["--manifest-out"])
        .arg(&manifest)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let packed = pack_extension_dir(src.path(), &PackOptions::default()).unwrap();
    assert_eq!(std::fs::read(&zip).unwrap(), packed.zip);
    assert_eq!(std::fs::read(&manifest).unwrap(), packed.manifest);
    // Source manifest untouched so far.
    assert!(
        !String::from_utf8(std::fs::read(src.path().join("aion-extension.json")).unwrap())
            .unwrap()
            .contains("sha512-")
    );

    // Existing outputs need --force.
    let again = bin().arg(src.path()).arg("--zip-out").arg(&zip).output().unwrap();
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));
    let forced = bin()
        .arg(src.path())
        .arg("--zip-out")
        .arg(&zip)
        .arg("--force")
        .output()
        .unwrap();
    assert!(forced.status.success());

    // --update-manifest rewrites the source manifest; then --check is clean and the output is stable.
    let upd = bin().arg(src.path()).arg("--update-manifest").output().unwrap();
    assert!(upd.status.success(), "{}", String::from_utf8_lossy(&upd.stderr));
    assert_eq!(
        std::fs::read(src.path().join("aion-extension.json")).unwrap(),
        packed.manifest
    );
    let check = bin().arg(src.path()).arg("--check").output().unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let report: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(report["filesChanged"], false);
    // No stray temp files next to the outputs.
    let names: Vec<String> = std::fs::read_dir(outdir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
}

// Uses unix symlinks directly; Windows link refusal is covered by the pack and reader tests.
#[cfg(unix)]
#[test]
fn cli_refuses_outputs_inside_source_symlink_outputs_bad_sources_and_misuse() {
    let src = author_dir();
    // Output inside the source dir.
    let inside = bin()
        .arg(src.path())
        .arg("--zip-out")
        .arg(src.path().join("x.zip"))
        .output()
        .unwrap();
    assert_eq!(inside.status.code(), Some(1));
    assert!(!src.path().join("x.zip").exists());
    let nested = bin()
        .arg(src.path())
        .arg("--manifest-out")
        .arg(src.path().join("panels/m.json"))
        .output()
        .unwrap();
    assert_eq!(nested.status.code(), Some(1));

    // Symlink output is never followed, even with --force.
    let outdir = TempDir::new().unwrap();
    let victim = outdir.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();
    let link = outdir.path().join("out.zip");
    std::os::unix::fs::symlink(&victim, &link).unwrap();
    let r = bin()
        .arg(src.path())
        .arg("--zip-out")
        .arg(&link)
        .arg("--force")
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(1));
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep");

    // A symlink in the source is refused; nothing is written.
    std::os::unix::fs::symlink("panels/board.html", src.path().join("alias.html")).unwrap();
    let zip = outdir.path().join("never.zip");
    let r = bin().arg(src.path()).arg("--zip-out").arg(&zip).output().unwrap();
    assert_eq!(r.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&r.stderr).contains("alias.html"));
    assert!(!zip.exists());

    // Misuse.
    assert_eq!(bin().output().unwrap().status.code(), Some(2));
    assert_eq!(bin().arg("--bogus").output().unwrap().status.code(), Some(1));
    assert_eq!(
        bin()
            .arg(src.path())
            .arg("--check")
            .arg("--update-manifest")
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        bin()
            .arg(outdir.path().join("missing-dir"))
            .output()
            .unwrap()
            .status
            .code(),
        Some(1)
    );
}

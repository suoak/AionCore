//! M9 E13 slice B5: policy tests for the two built-in sample packs (`workmate.sample-shortcuts`,
//! `workmate.sample-checklist`) and their committed, test-signed artefacts.
//!
//! These read the committed source directories, zips and `builtin/index.json`. The install /
//! listing / verified-serve behaviour is covered in `aionui-app` (`host_signed_extensions`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use aionui_extension::hub::curated::{signature_payload, sri_sha512};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::Value;

const SHORTCUTS: &str = "workmate.sample-shortcuts";
const CHECKLIST: &str = "workmate.sample-checklist";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn source(name: &str) -> PathBuf {
    repo().join("extensions").join(name)
}

fn manifest(name: &str) -> Value {
    serde_json::from_slice(&std::fs::read(source(name).join("aion-extension.json")).unwrap()).unwrap()
}

fn text(name: &str, rel: &str) -> String {
    std::fs::read_to_string(source(name).join(rel)).unwrap()
}

fn all_files(dir: &Path, base: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            all_files(&path, base, out);
        } else {
            out.insert(path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/"));
        }
    }
}

fn listed(manifest: &Value) -> BTreeMap<String, String> {
    manifest["files"]
        .as_array()
        .expect("signed files list")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_owned(),
                f["integrity"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn contribution(name: &str, kind: &str) -> Value {
    let m = manifest(name);
    let contributes = m["contributes"].as_object().unwrap();
    assert_eq!(contributes.keys().collect::<Vec<_>>(), vec![kind], "{name}");
    let items = contributes[kind].as_array().unwrap();
    assert_eq!(items.len(), 1, "{name}");
    items[0].clone()
}

#[test]
fn sample_manifests_are_minimal_unprivileged_and_fully_listed() {
    for (name, kind) in [(SHORTCUTS, "panels"), (CHECKLIST, "workPanelViews")] {
        let m = manifest(name);
        assert_eq!(m["name"], name);
        assert_eq!(m["version"], "1.0.0");
        assert_eq!(m["license"], "Apache-2.0");
        assert!(m["displayName"].as_str().unwrap().contains("CSBU WorkMate"), "{name}");

        // No privilege, hook, runtime or background surface of any kind.
        let allowed: BTreeSet<&str> = [
            "author",
            "contributes",
            "description",
            "displayName",
            "files",
            "i18n",
            "icon",
            "license",
            "name",
            "permissions",
            "version",
        ]
        .into();
        for key in m.as_object().unwrap().keys() {
            assert!(allowed.contains(key.as_str()), "{name}: unexpected manifest key {key}");
        }
        assert_eq!(
            m["permissions"],
            serde_json::json!({"network": false, "shell": false, "storage": false}),
            "{name}"
        );

        // Every shipped file (but the manifest) is in the signed list, with the right SRI.
        let mut on_disk = BTreeSet::new();
        all_files(&source(name), &source(name), &mut on_disk);
        on_disk.remove("aion-extension.json");
        let files = listed(&m);
        assert_eq!(files.keys().cloned().collect::<BTreeSet<_>>(), on_disk, "{name}");
        for (path, integrity) in &files {
            assert_eq!(
                *integrity,
                sri_sha512(&std::fs::read(source(name).join(path)).unwrap()),
                "{name}/{path}"
            );
        }
        assert!(files.contains_key(m["icon"].as_str().unwrap()));
        for locale in m["i18n"]["locales"].as_array().unwrap() {
            assert!(files.contains_key(&format!("i18n/{}.json", locale.as_str().unwrap())));
        }

        // The entry point is pinned explicitly and equals its signed-list entry.
        let item = contribution(name, kind);
        let entry = item["entryPoint"].as_str().unwrap();
        assert_eq!(item["entryPointIntegrity"].as_str().unwrap(), files[entry], "{name}");
        assert!(files.contains_key(item["icon"].as_str().unwrap()));
    }
    assert_eq!(
        contribution(CHECKLIST, "workPanelViews")["placement"],
        "secondary",
        "the checklist lives next to the session"
    );
}

#[test]
fn sample_i18n_bundles_cover_the_same_keys_in_both_locales() {
    for name in [SHORTCUTS, CHECKLIST] {
        let en: Value = serde_json::from_str(&text(name, "i18n/en.json")).unwrap();
        let zh: Value = serde_json::from_str(&text(name, "i18n/zh-CN.json")).unwrap();
        assert_eq!(
            en.as_object().unwrap().keys().collect::<Vec<_>>(),
            zh.as_object().unwrap().keys().collect::<Vec<_>>(),
            "{name}"
        );
        assert_ne!(en, zh, "{name}: zh-CN is actually translated");
    }
}

#[test]
fn shortcuts_panel_is_static_markup_without_any_script() {
    let html = text(SHORTCUTS, "panels/shortcuts.html").to_lowercase();
    assert!(!html.contains("<script"), "no script element");
    assert!(!html.contains("javascript:"));
    assert!(!html.contains("<iframe") && !html.contains("<object") && !html.contains("<embed"));
    assert!(!html.contains("<form"));
    // No inline event handler attributes (` onclick=` ...).
    let bytes = html.as_bytes();
    for (i, w) in bytes.windows(3).enumerate() {
        if w == b" on" {
            let rest: String = html[i + 3..].chars().take_while(|c| c.is_ascii_alphabetic()).collect();
            assert!(
                !html[i + 3 + rest.len()..].starts_with('=') || rest.is_empty(),
                "inline handler on{rest}"
            );
        }
    }
    assert!(!text(SHORTCUTS, "panels/shortcuts.css").contains("@import"));
    assert!(html.contains("http-equiv=\"content-security-policy\""));
    assert!(html.contains("default-src 'none'"));
    assert!(
        !html.contains("script-src"),
        "scripts are not even allowed by the page CSP"
    );
}

#[test]
fn checklist_script_is_local_and_has_no_network_storage_or_dynamic_code() {
    let html = text(CHECKLIST, "views/checklist.html").to_lowercase();
    assert!(html.contains("http-equiv=\"content-security-policy\""));
    assert!(html.contains("default-src 'none'"));
    assert!(html.contains("script-src 'self'"));
    for banned in [
        "unsafe-inline",
        "unsafe-eval",
        "connect-src",
        "http://",
        "https://",
        "<form",
    ] {
        assert!(!html.contains(banned), "checklist.html mentions {banned}");
    }
    // The only script element is the same-origin file; there are no inline handlers.
    assert_eq!(html.matches("<script").count(), 1);
    assert!(html.contains("<script src=\"checklist.js\""));

    let js = text(CHECKLIST, "views/checklist.js");
    for banned in [
        "fetch(",
        "XMLHttpRequest",
        "WebSocket",
        "EventSource",
        "sendBeacon",
        "localStorage",
        "sessionStorage",
        "indexedDB",
        "document.cookie",
        "caches.",
        "eval(",
        "new Function",
        "Function(",
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "importScripts",
        "postMessage",
        "window.parent",
        "window.top",
        "import(",
        "http://",
        "https://",
    ] {
        assert!(!js.contains(banned), "checklist.js uses {banned}");
    }
    assert!(!text(CHECKLIST, "views/checklist.css").contains("@import"));
}

#[test]
fn user_visible_brand_is_csbu_workmate_never_the_upstream_name() {
    for name in [SHORTCUTS, CHECKLIST] {
        let mut files = BTreeSet::new();
        all_files(&source(name), &source(name), &mut files);
        for rel in files {
            let body = text(name, &rel).to_lowercase();
            assert!(!body.contains("aionui"), "{name}/{rel} names the upstream product");
        }
    }
}

#[test]
fn committed_index_signs_the_sample_zips_with_the_test_key_and_zips_match_sources() {
    let index: Value = serde_json::from_slice(&std::fs::read(repo().join("builtin/index.json")).unwrap()).unwrap();
    assert_eq!(index["pubkeyId"], "workmate-test-dev");
    let mut pk = [0u8; 32];
    hex::decode_to_slice(index["publicKeyHex"].as_str().unwrap(), &mut pk).unwrap();
    let verifying = VerifyingKey::from_bytes(&pk).unwrap();

    for name in [SHORTCUTS, CHECKLIST] {
        let entry = index["packs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .unwrap_or_else(|| panic!("{name} missing from builtin/index.json"));
        assert_eq!(entry["version"], "1.0.0");
        assert_eq!(entry["pubkeyId"], "workmate-test-dev");
        assert_eq!(entry["zip"], format!("{name}.zip"));

        let zip_bytes = std::fs::read(repo().join("builtin").join(entry["zip"].as_str().unwrap())).unwrap();
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes)).unwrap();
        let mut names = BTreeSet::new();
        let mut manifest_bytes = Vec::new();
        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            let path = file.name().expect("valid utf-8 entry name").into_owned();
            // Every archived file is byte-identical to the committed source tree.
            assert_eq!(bytes, std::fs::read(source(name).join(&path)).unwrap(), "{name}/{path}");
            if path == "aion-extension.json" {
                manifest_bytes = bytes;
            }
            names.insert(path);
        }
        let mut on_disk = BTreeSet::new();
        all_files(&source(name), &source(name), &mut on_disk);
        assert_eq!(names, on_disk, "{name}: zip and source tree list the same files");

        let integrity = sri_sha512(&manifest_bytes);
        assert_eq!(entry["integrity"], integrity.as_str());
        let signature = hex::decode(entry["signature"].as_str().unwrap()).unwrap();
        verifying
            .verify(
                signature_payload(name, "1.0.0", &integrity).as_bytes(),
                &Signature::from_slice(&signature).unwrap(),
            )
            .unwrap_or_else(|e| panic!("{name}: bad signature: {e}"));
    }
}

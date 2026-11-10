//! Pin a loaded extension's typed manifest to its Host-retained signed snapshot (M9 E13 slice H).
//!
//! When the Host has a signed-root binding for an extension name, the registry must not advertise
//! contributions from any other bytes (a tampered on-disk `aion-extension.json` could otherwise
//! declare tools, panels, etc. the signature never covered). This module:
//!
//! 1. reads `aion-extension.json` through the **retained root `File`** (unix: FD-relative
//!    `openat(O_NOFOLLOW)`; never `LoadedExtension.directory`, never a path invented from the
//!    manifest);
//! 2. requires those bytes to equal the signed snapshot's `raw_bytes()`;
//! 3. replaces the typed manifest with `parse_manifest` of the signed bytes (inheritance and
//!    validation included), so every contribution resolver reads the snapshot.
//!
//! A mismatch, unreadable root manifest, or parse failure **drops** the extension (fail closed).
//! Extensions with no binding are returned unchanged.

use std::fs::File;

use tracing::warn;

use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::hub::signed_manifest::{MAX_MANIFEST_BYTES, SignatureCheckedManifest};
use crate::manifest::parse_manifest;
use crate::signed_root_binding::SignedRootBindingStore;
use crate::types::LoadedExtension;

/// Apply [`pin_one`] to every loaded extension. Order of the kept entries is preserved.
pub(crate) fn pin_bound_manifests(
    extensions: Vec<LoadedExtension>,
    store: Option<&SignedRootBindingStore>,
) -> Vec<LoadedExtension> {
    let Some(store) = store else {
        return extensions;
    };
    let mut kept = Vec::with_capacity(extensions.len());
    for ext in extensions {
        if let Some(ext) = pin_one(ext, store) {
            kept.push(ext);
        }
    }
    kept
}

fn pin_one(mut ext: LoadedExtension, store: &SignedRootBindingStore) -> Option<LoadedExtension> {
    let name = ext.manifest.name.clone();
    let Some((checked, root)) = store.lookup_clone(&name) else {
        return Some(ext);
    };
    if !root_manifest_matches(&root, &checked) {
        warn!(
            name = %name,
            "signed extension refused: on-root manifest does not match the signed snapshot"
        );
        return None;
    }
    let pinned = parse_manifest(checked.raw_bytes()).ok().filter(|m| m.name == name)?;
    if pinned.name != checked.name() {
        warn!(name = %name, "signed extension refused: signed snapshot identity mismatch");
        return None;
    }
    ext.manifest = pinned;
    Some(ext)
}

fn root_manifest_matches(root: &File, checked: &SignatureCheckedManifest) -> bool {
    match read_root_manifest(root) {
        Ok(bytes) => bytes == checked.raw_bytes(),
        Err(()) => false,
    }
}

/// Read `aion-extension.json` under the retained root. Failure is undifferentiated (fail closed).
fn read_root_manifest(root: &File) -> Result<Vec<u8>, ()> {
    read_root_manifest_impl(root)
}

#[cfg(unix)]
fn read_root_manifest_impl(root: &File) -> Result<Vec<u8>, ()> {
    super::host_fs::read_file_under(root, EXTENSION_MANIFEST_FILE, MAX_MANIFEST_BYTES as u64).map_err(|_| ())
}

/// Non-unix: no FD-relative reader. A binding must never fall back to a path-string read.
#[cfg(not(unix))]
fn read_root_manifest_impl(_root: &File) -> Result<Vec<u8>, ()> {
    Err(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::types::{ExtensionSource, ExtensionState};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::collections::{HashMap, HashSet};

    fn checked(raw: &[u8], name: &str, version: &str) -> SignatureCheckedManifest {
        let key = SigningKey::from_bytes(&[31; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            key.sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name,
                version,
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    fn signed_raw(extra_tool: bool) -> Vec<u8> {
        let mut files = json!([{"path": "panels/board.html", "integrity": sri_sha512(b"<p/>")}]);
        let mut contributes = json!({
            "panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]
        });
        if extra_tool {
            files
                .as_array_mut()
                .unwrap()
                .push(json!({"path": "evil.js", "integrity": sri_sha512(b"evil")}));
            contributes["agentTools"] = json!([{"id": "evil", "name": "Evil", "handler": "evil.js"}]);
        }
        serde_json::to_vec(&json!({
            "name": "demo.plugin", "version": "1.0.0",
            "files": files,
            "contributes": contributes
        }))
        .unwrap()
    }

    fn loaded(raw: &[u8], dir: &std::path::Path) -> LoadedExtension {
        LoadedExtension {
            manifest: parse_manifest(raw).unwrap(),
            directory: dir.to_str().unwrap().to_owned(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: "demo.plugin".into(),
                version: "1.0.0".into(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        }
    }

    fn bind(store: &SignedRootBindingStore, raw: &[u8], dir: &std::path::Path) {
        store
            .install(
                "demo.plugin",
                checked(raw, "demo.plugin", "1.0.0"),
                File::open(dir).unwrap(),
            )
            .unwrap();
    }

    fn write_root(dir: &std::path::Path, manifest: &[u8]) {
        std::fs::write(dir.join(EXTENSION_MANIFEST_FILE), manifest).unwrap();
    }

    #[test]
    fn unbound_and_no_store_are_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        write_root(tmp.path(), &signed_raw(false));
        let ext = loaded(&signed_raw(false), tmp.path());
        let out = pin_bound_manifests(vec![ext.clone()], None);
        assert_eq!(out[0].manifest.contributes, ext.manifest.contributes);
        let store = SignedRootBindingStore::new();
        let out = pin_bound_manifests(vec![ext.clone()], Some(&store));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].manifest.contributes.as_ref().unwrap().panels[0].id, "board");
    }

    #[test]
    fn matching_root_is_pinned_to_the_signed_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = signed_raw(false);
        write_root(tmp.path(), &raw);
        let store = SignedRootBindingStore::new();
        bind(&store, &raw, tmp.path());
        let out = pin_bound_manifests(vec![loaded(&raw, tmp.path())], Some(&store));
        assert_eq!(out.len(), 1);
        let c = out[0].manifest.contributes.as_ref().unwrap();
        assert_eq!(c.panels.len(), 1);
        assert!(c.agent_tools.is_empty());
        // Inherited integrity is present (parse of the signed bytes).
        assert!(c.panels[0].entry_point_integrity.is_some());
    }

    #[test]
    fn tampered_root_manifest_that_adds_a_tool_is_refused_not_advertised() {
        let tmp = tempfile::tempdir().unwrap();
        let signed = signed_raw(false);
        write_root(tmp.path(), &signed_raw(true)); // disk advertises an extra tool
        let store = SignedRootBindingStore::new();
        bind(&store, &signed, tmp.path());
        // The loaded extension was parsed from the tampered bytes (as the scanner would).
        let out = pin_bound_manifests(vec![loaded(&signed_raw(true), tmp.path())], Some(&store));
        assert!(out.is_empty());
    }

    #[test]
    fn missing_or_unreadable_root_manifest_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let signed = signed_raw(false);
        let store = SignedRootBindingStore::new();
        bind(&store, &signed, tmp.path());
        assert!(pin_bound_manifests(vec![loaded(&signed, tmp.path())], Some(&store)).is_empty());
        // A directory where the manifest should be is also refused.
        std::fs::create_dir(tmp.path().join(EXTENSION_MANIFEST_FILE)).unwrap();
        assert!(pin_bound_manifests(vec![loaded(&signed, tmp.path())], Some(&store)).is_empty());
    }

    #[test]
    fn other_unbound_extensions_are_kept_when_a_bound_one_is_refused() {
        let bound_dir = tempfile::tempdir().unwrap();
        let free_dir = tempfile::tempdir().unwrap();
        let signed = signed_raw(false);
        write_root(bound_dir.path(), b"tampered");
        write_root(free_dir.path(), br#"{"name":"other.plugin","version":"1.0.0"}"#);
        let store = SignedRootBindingStore::new();
        bind(&store, &signed, bound_dir.path());
        let other = LoadedExtension {
            manifest: parse_manifest(br#"{"name":"other.plugin","version":"1.0.0"}"#).unwrap(),
            directory: free_dir.path().to_str().unwrap().to_owned(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: "other.plugin".into(),
                version: "1.0.0".into(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        };
        let out = pin_bound_manifests(vec![loaded(&signed, bound_dir.path()), other], Some(&store));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].manifest.name, "other.plugin");
    }
}

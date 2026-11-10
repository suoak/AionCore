//! Host-facing composition: signed snapshot panel binding → single checked source read.
//!
//! Combines extension `bind_entry_point_digest_from_signed` with `read_checked_source`.
//! Missing/None `entryPointIntegrity` is fail-closed (`Digest(NotPresent)`); this path
//! never falls back to an unchecked read. Success returns the same owned verified
//! UTF-8 snapshot as `read_checked_source` (single-read integrity).
//!
//! Scoped to standalone `contributes.panels` (same scope as the digest binder).
//! Trust model unchanged: the Host must already trust the signed snapshot, supply
//! a trustworthy root/`/proc` view, and separately authorize any later panel load.
//! This helper does not authenticate root/owner/curation and is not a panel-load
//! or iframe/execution permit. Not M9/E13 completion.
use crate::extension_js_checked_source::{CheckedSourceError, IntegrityCheckedSource, read_checked_source};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use aionui_extension::hub::{EntryPointDigestError, bind_entry_point_digest_from_signed};
use std::{fs::File, path::Path};

/// Fixed categories only; never include panel ids, paths, digests or source text.
#[derive(Debug, PartialEq, Eq)]
pub enum PanelSourceBindError {
    Digest(EntryPointDigestError),
    Checked(CheckedSourceError),
}

/// Resolve one panel's declared `(entryPoint, expected SRI)` from a signature-checked
/// manifest snapshot, then read that entry point once under a Host-trusted root and
/// compare against the extracted SRI.
///
/// Fail-closed: absence of `entryPointIntegrity` → `Digest(NotPresent)`; ambiguous
/// panel id, missing panel, invalid re-parse, or empty entry point → the corresponding
/// `EntryPointDigestError`; SRI/path/reader failures → `Checked(...)`. No silent
/// unchecked fallback. Does not load panels, unlock iframes, or expand routes/gates.
pub fn load_checked_panel_source_from_signed(
    checked: &SignatureCheckedManifest,
    panel_id: &str,
    root: &File,
) -> Result<IntegrityCheckedSource, PanelSourceBindError> {
    let binding = bind_entry_point_digest_from_signed(checked, panel_id).map_err(PanelSourceBindError::Digest)?;
    // `entry_point()` is non-empty when Ok (empty → EntryPointMissing at bind time).
    read_checked_source(root, Path::new(binding.entry_point()), binding.integrity())
        .map_err(PanelSourceBindError::Checked)
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::hub::EntryPointDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        collections::{HashMap, HashSet},
        fs,
    };
    use tempfile::TempDir;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn sign(raw: &[u8], name: &str, version: &str) -> (String, String) {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        (integrity, signature)
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let (integrity, signature) = sign(raw, "demo.plugin", "1.0.0");
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    fn manifest_json(panels: &str) -> String {
        format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":{panels}}}}}"#)
    }

    fn fixture() -> (TempDir, File) {
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn happy_path_returns_exact_owned_snapshot() {
        let (temp, root) = fixture();
        let body = "<html><body>看板</body></html>\n";
        fs::create_dir_all(temp.path().join("panels")).unwrap();
        fs::write(temp.path().join("panels/board.html"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"panels/board.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let source = load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());
        assert_eq!(source.into_source().into_bytes(), body.as_bytes());
    }

    #[test]
    fn missing_integrity_is_not_present_not_unchecked_read() {
        let (temp, root) = fixture();
        // File exists; composition must still refuse without declared integrity.
        fs::write(temp.path().join("panels_board.html"), b"should-not-be-read").unwrap();
        let raw = manifest_json(r#"[{"id":"board","title":"Board","entryPoint":"panels_board.html"}]"#);
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).err(),
            Some(PanelSourceBindError::Digest(EntryPointDigestError::NotPresent))
        );
    }

    #[test]
    fn ambiguous_panel_id_fail_closed() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"<html/>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"A","entryPoint":"a.html","entryPointIntegrity":"{sri}"}},{{"id":"board","title":"B","entryPoint":"b.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).err(),
            Some(PanelSourceBindError::Digest(EntryPointDigestError::AmbiguousPanelId))
        );
    }

    #[test]
    fn unknown_panel_id_is_not_found() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"<html/>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "other", &root).err(),
            Some(PanelSourceBindError::Digest(EntryPointDigestError::PanelNotFound))
        );
    }

    #[test]
    fn hash_mismatch_when_file_differs_from_declared_sri() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("p.html"), b"<html>actual</html>").unwrap();
        let sri = sri_sha512(b"<html>declared-other</html>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).err(),
            Some(PanelSourceBindError::Checked(CheckedSourceError::IntegrityMismatch))
        );
    }

    #[test]
    fn bad_sri_rejected_before_composition_via_signed_check() {
        // Invalid present SRI fails parse_manifest inside check_signed_manifest;
        // composition never sees an unchecked path. Documented fail-closed gate.
        let raw =
            manifest_json(r#"[{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"sha256-x"}]"#);
        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let err = check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw.as_bytes(),
        )
        .err();
        assert_eq!(
            err,
            Some(aionui_extension::hub::signed_manifest::SignedManifestError::InvalidManifest)
        );
    }

    #[test]
    fn empty_entry_point_with_integrity_rejected_before_composition() {
        // validate_manifest requires non-empty entryPoint when integrity is present.
        let sri = sri_sha512(b"<html/>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
        ));
        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let err = check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw.as_bytes(),
        )
        .err();
        assert_eq!(
            err,
            Some(aionui_extension::hub::signed_manifest::SignedManifestError::InvalidManifest)
        );
    }

    #[test]
    fn missing_entry_point_file_surfaces_reader_error() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"<html/>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"missing.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        match load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).err() {
            Some(PanelSourceBindError::Checked(CheckedSourceError::Reader(_))) => {}
            other => panic!("expected Checked(Reader(_)), got {other:?}"),
        }
    }

    /// Chain proof: Host-created root from the local signed package install feeds the
    /// existing checked-source compose (no path-string reopen anywhere).
    #[test]
    fn local_package_install_root_feeds_checked_panel_source() {
        use aionui_extension::hub::{LocalPackageLimits, LocalPackageSource, install_local_signed_package};
        use aionui_extension::signed_root_binding::SignedRootBindingStore;
        use std::io::Write;

        let body = "<html><body>pkg</body></html>\n";
        let sri = sri_sha512(body.as_bytes());
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"panels/board.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("aion-extension.json", opts).unwrap();
        writer.write_all(raw.as_bytes()).unwrap();
        writer.start_file("panels/board.html", opts).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
        let archive = writer.finish().unwrap().into_inner();

        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let parent_dir = tempfile::tempdir().unwrap();
        let parent = File::open(parent_dir.path()).unwrap();
        let store = SignedRootBindingStore::new();
        install_local_signed_package(
            &store,
            &policy(),
            &LocalPackageSource {
                entry: SignedManifestEntry {
                    name: "demo.plugin",
                    version: "1.0.0",
                    pubkey_id: Some("fixture"),
                    integrity: Some(&integrity),
                    signature: Some(&signature),
                },
                raw_manifest: raw.as_bytes(),
                package_zip: &archive,
            },
            &parent,
            "demo-1.0.0",
            LocalPackageLimits::default(),
        )
        .unwrap();
        let (snapshot, root) = store.lookup_clone("demo.plugin").unwrap();
        let source = load_checked_panel_source_from_signed(&snapshot, "board", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());
    }

    /// Declare-once chain proof: the panel carries **no** `entryPointIntegrity`; its SRI comes from
    /// the top-level `files` list. Install -> retained root -> checked load must behave exactly like
    /// the per-entry-declared package, and a tampered root file must still be refused.
    #[test]
    fn files_only_package_inherits_integrity_through_install_and_checked_load() {
        use aionui_extension::hub::{LocalPackageLimits, LocalPackageSource, install_local_signed_package};
        use aionui_extension::signed_root_binding::SignedRootBindingStore;
        use std::io::Write;

        let body = "<html><body>files-only</body></html>\n";
        let sri = sri_sha512(body.as_bytes());
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"panels/board.html","integrity":"{sri}"}}],"contributes":{{"panels":[{{"id":"board","title":"Board","entryPoint":"panels/board.html"}}]}}}}"#
        );
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        writer.start_file("aion-extension.json", opts).unwrap();
        writer.write_all(raw.as_bytes()).unwrap();
        writer.start_file("panels/board.html", opts).unwrap();
        writer.write_all(body.as_bytes()).unwrap();
        let archive = writer.finish().unwrap().into_inner();

        let (integrity, signature) = sign(raw.as_bytes(), "demo.plugin", "1.0.0");
        let parent_dir = tempfile::tempdir().unwrap();
        let parent = File::open(parent_dir.path()).unwrap();
        let store = SignedRootBindingStore::new();
        install_local_signed_package(
            &store,
            &policy(),
            &LocalPackageSource {
                entry: SignedManifestEntry {
                    name: "demo.plugin",
                    version: "1.0.0",
                    pubkey_id: Some("fixture"),
                    integrity: Some(&integrity),
                    signature: Some(&signature),
                },
                raw_manifest: raw.as_bytes(),
                package_zip: &archive,
            },
            &parent,
            "demo-files-only",
            LocalPackageLimits::default(),
        )
        .unwrap();
        let (snapshot, root) = store.lookup_clone("demo.plugin").unwrap();
        let source = load_checked_panel_source_from_signed(&snapshot, "board", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());

        // Post-install tamper of the retained root is caught by the checked reader (inherited SRI is enforced).
        std::fs::write(
            parent_dir.path().join("demo-files-only/panels/board.html"),
            "<html>evil</html>",
        )
        .unwrap();
        assert!(load_checked_panel_source_from_signed(&snapshot, "board", &root).is_err());
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    use aionui_extension::hub::EntryPointDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[11; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    #[test]
    fn missing_integrity_still_digest_not_present_when_disabled() {
        // Digest stage runs before platform gate; never unchecked fallback.
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"panels":[{"id":"board","title":"Board","entryPoint":"p.html"}]}}"#;
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw), "board", &root).err(),
            Some(PanelSourceBindError::Digest(EntryPointDigestError::NotPresent))
        );
    }

    #[test]
    fn present_binding_is_unavailable_when_disabled_not_unchecked() {
        let body = b"<html>board</html>";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_panel_source_from_signed(&checked(raw.as_bytes()), "board", &root).err(),
            Some(PanelSourceBindError::Checked(CheckedSourceError::Unavailable))
        );
    }
}

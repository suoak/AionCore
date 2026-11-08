//! Host-facing composition: signed snapshot workPanelView binding → single checked source read.
//!
//! Combines extension `bind_work_panel_view_entry_point_digest_from_signed` with
//! `read_checked_source`. Missing/None `entryPointIntegrity` is fail-closed
//! (`Digest(NotPresent)`); this path never falls back to an unchecked read.
//! Success returns the same owned verified UTF-8 snapshot as `read_checked_source`
//! (single-read integrity).
//!
//! Scoped to `contributes.workPanelViews` only (sibling of panels compose; ids
//! do not cross-resolve). Trust model unchanged: the Host must already trust the
//! signed snapshot, supply a trustworthy root/`/proc` view, and separately
//! authorize any later view load. This helper does not authenticate
//! root/owner/curation and is not a view-load or iframe/execution permit.
//! Not M9/E13 completion.
use crate::extension_js_checked_source::{CheckedSourceError, IntegrityCheckedSource, read_checked_source};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use aionui_extension::hub::{WorkPanelViewEntryPointDigestError, bind_work_panel_view_entry_point_digest_from_signed};
use std::{fs::File, path::Path};

/// Fixed categories only; never include view ids, paths, digests or source text.
#[derive(Debug, PartialEq, Eq)]
pub enum WorkPanelViewSourceBindError {
    Digest(WorkPanelViewEntryPointDigestError),
    Checked(CheckedSourceError),
}

/// Resolve one workPanelView's declared `(entryPoint, expected SRI)` from a
/// signature-checked manifest snapshot, then read that entry point once under a
/// Host-trusted root and compare against the extracted SRI.
///
/// Fail-closed: absence of `entryPointIntegrity` → `Digest(NotPresent)`; ambiguous
/// view id, missing view, invalid re-parse, or empty entry point → the corresponding
/// `WorkPanelViewEntryPointDigestError`; SRI/path/reader failures → `Checked(...)`.
/// No silent unchecked fallback. Does not load views, unlock iframes, or expand
/// routes/gates. Does not consult `contributes.panels`.
pub fn load_checked_work_panel_view_source_from_signed(
    checked: &SignatureCheckedManifest,
    view_id: &str,
    root: &File,
) -> Result<IntegrityCheckedSource, WorkPanelViewSourceBindError> {
    let binding = bind_work_panel_view_entry_point_digest_from_signed(checked, view_id)
        .map_err(WorkPanelViewSourceBindError::Digest)?;
    // `entry_point()` is non-empty when Ok (empty → EntryPointMissing at bind time).
    read_checked_source(root, Path::new(binding.entry_point()), binding.integrity())
        .map_err(WorkPanelViewSourceBindError::Checked)
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::hub::WorkPanelViewEntryPointDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        collections::{HashMap, HashSet},
        fs,
    };
    use tempfile::TempDir;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[14; 32])
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

    fn views_json(views: &str) -> String {
        format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":{views}}}}}"#)
    }

    fn fixture() -> (TempDir, File) {
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn happy_path_returns_exact_owned_snapshot() {
        let (temp, root) = fixture();
        let body = "<html><body>工作视图</body></html>\n";
        fs::create_dir_all(temp.path().join("views")).unwrap();
        fs::write(temp.path().join("views/side.html"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"views/side.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let source = load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());
        assert_eq!(source.into_source().into_bytes(), body.as_bytes());
    }

    #[test]
    fn missing_integrity_is_not_present_not_unchecked_read() {
        let (temp, root) = fixture();
        // File exists; composition must still refuse without declared integrity.
        fs::write(temp.path().join("views_side.html"), b"should-not-be-read").unwrap();
        let raw = views_json(r#"[{"id":"side","title":"Side","entryPoint":"views_side.html"}]"#);
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).err(),
            Some(WorkPanelViewSourceBindError::Digest(
                WorkPanelViewEntryPointDigestError::NotPresent
            ))
        );
    }

    #[test]
    fn ambiguous_view_id_fail_closed() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"<html/>");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"A","entryPoint":"a.html","entryPointIntegrity":"{sri}"}},{{"id":"side","title":"B","entryPoint":"b.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).err(),
            Some(WorkPanelViewSourceBindError::Digest(
                WorkPanelViewEntryPointDigestError::AmbiguousViewId
            ))
        );
    }

    #[test]
    fn unknown_view_id_is_not_found() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"<html/>");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "other", &root).err(),
            Some(WorkPanelViewSourceBindError::Digest(
                WorkPanelViewEntryPointDigestError::ViewNotFound
            ))
        );
    }

    #[test]
    fn hash_mismatch_when_file_differs_from_declared_sri() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("v.html"), b"<html>actual</html>").unwrap();
        let sri = sri_sha512(b"<html>declared-other</html>");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).err(),
            Some(WorkPanelViewSourceBindError::Checked(
                CheckedSourceError::IntegrityMismatch
            ))
        );
    }

    #[test]
    fn bad_sri_rejected_before_composition_via_signed_check() {
        // Invalid present SRI fails parse_manifest inside check_signed_manifest;
        // composition never sees an unchecked path. Documented fail-closed gate.
        let raw =
            views_json(r#"[{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"sha256-x"}]"#);
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
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
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
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"missing.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        match load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).err() {
            Some(WorkPanelViewSourceBindError::Checked(CheckedSourceError::Reader(_))) => {}
            other => panic!("expected Checked(Reader(_)), got {other:?}"),
        }
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    use aionui_extension::hub::WorkPanelViewEntryPointDigestError;
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[14; 32])
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
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"workPanelViews":[{"id":"side","title":"Side","entryPoint":"v.html"}]}}"#;
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw), "side", &root).err(),
            Some(WorkPanelViewSourceBindError::Digest(
                WorkPanelViewEntryPointDigestError::NotPresent
            ))
        );
    }

    #[test]
    fn present_binding_is_unavailable_when_disabled_not_unchecked() {
        let body = b"<html>side</html>";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_work_panel_view_source_from_signed(&checked(raw.as_bytes()), "side", &root).err(),
            Some(WorkPanelViewSourceBindError::Checked(CheckedSourceError::Unavailable))
        );
    }
}

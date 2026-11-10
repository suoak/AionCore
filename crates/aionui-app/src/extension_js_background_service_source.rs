//! Host-facing composition: signed snapshot backgroundService binding → single checked source read.
//!
//! Combines extension `bind_background_service_entry_point_digest_from_signed` with
//! `read_checked_source`. Missing/None `entryPointIntegrity` is fail-closed
//! (`Digest(NotPresent)`); this path never falls back to an unchecked read.
//! Success returns the same owned verified UTF-8 snapshot as `read_checked_source`
//! (single-read integrity).
//!
//! Scoped to `contributes.backgroundServices` only (sibling of panels / workPanelViews
//! compose; ids do not cross-resolve). Trust model unchanged: the Host must already
//! trust the signed snapshot, supply a trustworthy root/`/proc` view, and separately
//! authorize any later service start. This helper does not authenticate
//! root/owner/curation, is not a service-start/spawn/execution permit, and is NOT wired
//! into `HostRuntime` start (which still resolves path strings under `ext.directory`).
//! Not M9/E13 completion.
use crate::extension_js_checked_source::{CheckedSourceError, IntegrityCheckedSource, read_checked_source};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use aionui_extension::hub::{
    BackgroundServiceEntryPointDigestError, bind_background_service_entry_point_digest_from_signed,
};
use std::{fs::File, path::Path};

/// Fixed categories only; never include service ids, paths, digests or source text.
#[derive(Debug, PartialEq, Eq)]
pub enum BackgroundServiceSourceBindError {
    Digest(BackgroundServiceEntryPointDigestError),
    Checked(CheckedSourceError),
}

/// Resolve one backgroundService's declared `(entryPoint, expected SRI)` from a
/// signature-checked manifest snapshot, then read that entry point once under a
/// Host-trusted root and compare against the extracted SRI.
///
/// Fail-closed: absence of `entryPointIntegrity` → `Digest(NotPresent)`; ambiguous
/// service id, missing service, invalid re-parse, or empty entry point → the corresponding
/// `BackgroundServiceEntryPointDigestError`; SRI/path/reader failures → `Checked(...)`.
/// No silent unchecked fallback. Does not start/spawn services or expand routes/gates.
/// Does not consult `contributes.panels` or `contributes.workPanelViews`.
pub fn load_checked_background_service_source_from_signed(
    checked: &SignatureCheckedManifest,
    service_id: &str,
    root: &File,
) -> Result<IntegrityCheckedSource, BackgroundServiceSourceBindError> {
    let binding = bind_background_service_entry_point_digest_from_signed(checked, service_id)
        .map_err(BackgroundServiceSourceBindError::Digest)?;
    // `entry_point()` is non-empty when Ok (empty → EntryPointMissing at bind time).
    read_checked_source(root, Path::new(binding.entry_point()), binding.integrity())
        .map_err(BackgroundServiceSourceBindError::Checked)
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::hub::BackgroundServiceEntryPointDigestError;
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

    fn services_json(services: &str) -> String {
        format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"backgroundServices":{services}}}}}"#)
    }

    fn fixture() -> (TempDir, File) {
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        (temp, root)
    }

    #[test]
    fn happy_path_returns_exact_owned_snapshot() {
        let (temp, root) = fixture();
        let body = "console.log('后台服务');\n";
        fs::create_dir_all(temp.path().join("services")).unwrap();
        fs::write(temp.path().join("services/worker.js"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"services/worker.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        let source =
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).unwrap();
        assert_eq!(source.as_str().as_bytes(), body.as_bytes());
        assert_eq!(source.into_source().into_bytes(), body.as_bytes());
    }

    #[test]
    fn missing_integrity_is_not_present_not_unchecked_read() {
        let (temp, root) = fixture();
        // File exists; composition must still refuse without declared integrity.
        fs::write(temp.path().join("services_worker.js"), b"should-not-be-read").unwrap();
        let raw = services_json(r#"[{"id":"worker","entryPoint":"services_worker.js"}]"#);
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Digest(
                BackgroundServiceEntryPointDigestError::NotPresent
            ))
        );
    }

    #[test]
    fn ambiguous_service_id_fail_closed() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"void 0;");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"a.js","entryPointIntegrity":"{sri}"}},{{"id":"worker","entryPoint":"b.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Digest(
                BackgroundServiceEntryPointDigestError::AmbiguousServiceId
            ))
        );
    }

    #[test]
    fn unknown_service_id_is_not_found() {
        let (_temp, root) = fixture();
        let sri = sri_sha512(b"void 0;");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"v.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "other", &root).err(),
            Some(BackgroundServiceSourceBindError::Digest(
                BackgroundServiceEntryPointDigestError::ServiceNotFound
            ))
        );
    }

    #[test]
    fn panel_or_view_with_same_id_does_not_satisfy_service_lookup() {
        // Cross-array isolation: only backgroundServices are consulted.
        let (temp, root) = fixture();
        fs::write(temp.path().join("p.html"), b"<html/>").unwrap();
        let sri = sri_sha512(b"<html/>");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"worker","title":"P","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}],"workPanelViews":[{{"id":"worker","title":"V","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Digest(
                BackgroundServiceEntryPointDigestError::ServiceNotFound
            ))
        );
    }

    #[test]
    fn hash_mismatch_when_file_differs_from_declared_sri() {
        let (temp, root) = fixture();
        fs::write(temp.path().join("v.js"), b"actual();").unwrap();
        let sri = sri_sha512(b"declared_other();");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"v.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Checked(
                CheckedSourceError::IntegrityMismatch
            ))
        );
    }

    #[test]
    fn bad_sri_rejected_before_composition_via_signed_check() {
        // Invalid present SRI fails parse_manifest inside check_signed_manifest;
        // composition never sees an unchecked path. Documented fail-closed gate.
        let raw = services_json(r#"[{"id":"worker","entryPoint":"v.js","entryPointIntegrity":"sha256-x"}]"#);
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
        let sri = sri_sha512(b"void 0;");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
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
        let sri = sri_sha512(b"void 0;");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"missing.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        match load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err() {
            Some(BackgroundServiceSourceBindError::Checked(CheckedSourceError::Reader(_))) => {}
            other => panic!("expected Checked(Reader(_)), got {other:?}"),
        }
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    use aionui_extension::hub::BackgroundServiceEntryPointDigestError;
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
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"backgroundServices":[{"id":"worker","entryPoint":"v.js"}]}}"#;
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Digest(
                BackgroundServiceEntryPointDigestError::NotPresent
            ))
        );
    }

    #[test]
    fn present_binding_is_unavailable_when_disabled_not_unchecked() {
        let body = b"worker();";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"v.js","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let root = tempfile::tempfile().unwrap();
        assert_eq!(
            load_checked_background_service_source_from_signed(&checked(raw.as_bytes()), "worker", &root).err(),
            Some(BackgroundServiceSourceBindError::Checked(
                CheckedSourceError::Unavailable
            ))
        );
    }
}

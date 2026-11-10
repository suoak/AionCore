//! Optional per-backgroundService entry-point digest extraction from typed/signed manifests.
//!
//! Sibling of `entry_point_digest` (standalone `contributes.panels`) and
//! `work_panel_view_entry_point_digest` (`contributes.workPanelViews`). A present,
//! valid `entryPointIntegrity` value is only a **declared expected SRI** inside
//! already-trusted manifest bytes. Missing/None never means verified.
//! Combining this with `SignatureCheckedManifest` + Host `read_checked_source`
//! can form a host-mediated binding; this module alone does not authorize service
//! start/execution, prove curation/owner/root or read entry-point files.
//!
//! Scoped to `contributes.backgroundServices` only (not panels / workPanelViews).
//! HostRuntime start only *refuses* declared integrity (see `background_service_start_gate`); it does not spawn from checked source.
use super::entry_point_digest::EntryPointDigestBinding;
use super::signed_manifest::SignatureCheckedManifest;
use super::sri_format::is_canonical_sha512_sri;
use crate::manifest::parse_manifest;
use crate::types::ExtensionManifest;

/// Fixed categories; never include digests, paths, service ids or raw JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundServiceEntryPointDigestError {
    ServiceNotFound,
    AmbiguousServiceId,
    NotPresent,
    EntryPointMissing,
    InvalidIntegrity,
    InvalidManifest,
}

/// Look up one backgroundService by **exact `id`** and return its declared digest binding.
///
/// Precedence is exact service id only (not entry-point path). Duplicate ids among
/// `contributes.backgroundServices` fail closed with `AmbiguousServiceId` because the pure
/// JSON array parser keeps every element. Missing field => `NotPresent` (not Ok).
/// Present invalid SRI => `InvalidIntegrity`. Present with empty `entry_point`
/// => `EntryPointMissing`. No digest is invented. Does not consult panels / workPanelViews.
pub fn expected_background_service_entry_point_integrity(
    manifest: &ExtensionManifest,
    service_id: &str,
) -> Result<EntryPointDigestBinding, BackgroundServiceEntryPointDigestError> {
    let services = manifest
        .contributes
        .as_ref()
        .map(|c| c.background_services.as_slice())
        .unwrap_or(&[]);
    let matches: Vec<_> = services.iter().filter(|svc| svc.id == service_id).collect();
    match matches.as_slice() {
        [] => Err(BackgroundServiceEntryPointDigestError::ServiceNotFound),
        [_, _, ..] => Err(BackgroundServiceEntryPointDigestError::AmbiguousServiceId),
        [svc] => {
            let Some(integrity) = svc.entry_point_integrity.as_deref() else {
                return Err(BackgroundServiceEntryPointDigestError::NotPresent);
            };
            if !is_canonical_sha512_sri(integrity) {
                return Err(BackgroundServiceEntryPointDigestError::InvalidIntegrity);
            }
            if svc.entry_point.is_empty() {
                return Err(BackgroundServiceEntryPointDigestError::EntryPointMissing);
            }
            Ok(EntryPointDigestBinding::new(
                svc.entry_point.clone(),
                integrity.to_owned(),
            ))
        }
    }
}

/// Re-parse a signature-checked manifest **byte snapshot** and extract one backgroundService binding.
///
/// Uses the owned raw bytes only — no filesystem reread. Success still requires
/// a present valid field; absence is `NotPresent`, never a synthetic Ok digest.
/// Host trust in the signed snapshot and later `read_checked_source` comparison
/// remain separate. Duplicate JSON object keys follow serde_json Value last-wins
/// during parse; duplicate array entries with the same service id fail closed here.
pub fn bind_background_service_entry_point_digest_from_signed(
    checked: &SignatureCheckedManifest,
    service_id: &str,
) -> Result<EntryPointDigestBinding, BackgroundServiceEntryPointDigestError> {
    let manifest =
        parse_manifest(checked.raw_bytes()).map_err(|_| BackgroundServiceEntryPointDigestError::InvalidManifest)?;
    expected_background_service_entry_point_integrity(&manifest, service_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::entry_point_digest::{
        EntryPointDigestError, bind_entry_point_digest_from_signed, expected_entry_point_integrity,
    };
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::hub::work_panel_view_entry_point_digest::{
        WorkPanelViewEntryPointDigestError, bind_work_panel_view_entry_point_digest_from_signed,
        expected_work_panel_view_entry_point_integrity,
    };
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[17; 32])
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

    #[test]
    fn missing_field_is_not_present_not_ok() {
        let raw = services_json(r#"[{"id":"worker","entryPoint":"services/worker.js"}]"#);
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(
            expected_background_service_entry_point_integrity(&manifest, "worker"),
            Err(BackgroundServiceEntryPointDigestError::NotPresent)
        );
        assert_eq!(
            bind_background_service_entry_point_digest_from_signed(&checked(raw.as_bytes()), "worker"),
            Err(BackgroundServiceEntryPointDigestError::NotPresent)
        );
    }

    #[test]
    fn present_valid_returns_exact_entry_point_and_sri() {
        let sri = sri_sha512(b"console.log('worker')");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"services/worker.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        let binding =
            bind_background_service_entry_point_digest_from_signed(&checked(raw.as_bytes()), "worker").unwrap();
        assert_eq!(binding.entry_point(), "services/worker.js");
        assert_eq!(binding.integrity(), sri);
        assert_eq!(binding.integrity().as_bytes(), sri.as_bytes());
    }

    #[test]
    fn camelcase_alias_parses() {
        let sri = sri_sha512(b"svc");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let svc = &manifest.contributes.as_ref().unwrap().background_services[0];
        assert_eq!(svc.entry_point_integrity.as_deref(), Some(sri.as_str()));
        assert_eq!(
            expected_background_service_entry_point_integrity(&manifest, "worker")
                .unwrap()
                .entry_point(),
            "s.js"
        );
    }

    #[test]
    fn invalid_present_sri_rejected_at_parse() {
        let raw = services_json(r#"[{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"sha256-x"}]"#);
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn malformed_sri_forms_rejected() {
        let good = sri_sha512(b"svc");
        let mut bad_pad = good.as_bytes().to_vec();
        bad_pad[92] = b'B';
        for sri in [
            String::new(),
            good.replacen("sha512-", "SHA512-", 1),
            format!("{good} "),
            String::from_utf8(bad_pad).unwrap(),
            "sha512-".to_owned() + &"A".repeat(88),
        ] {
            let raw = services_json(&format!(
                r#"[{{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"{sri}"}}]"#
            ));
            assert!(parse_manifest(raw.as_bytes()).is_err(), "{sri:?}");
        }
    }

    #[test]
    fn integrity_with_empty_entry_point_rejected() {
        let sri = sri_sha512(b"svc");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn wrong_service_and_unknown_service() {
        let sri = sri_sha512(b"svc");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        let checked = checked(raw.as_bytes());
        assert_eq!(
            bind_background_service_entry_point_digest_from_signed(&checked, "other"),
            Err(BackgroundServiceEntryPointDigestError::ServiceNotFound)
        );
    }

    #[test]
    fn duplicate_service_ids_fail_closed_for_binder() {
        let sri = sri_sha512(b"svc");
        let raw = services_json(&format!(
            r#"[{{"id":"worker","entryPoint":"a.js","entryPointIntegrity":"{sri}"}},{{"id":"worker","entryPoint":"b.js","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(manifest.contributes.as_ref().unwrap().background_services.len(), 2);
        assert_eq!(
            expected_background_service_entry_point_integrity(&manifest, "worker"),
            Err(BackgroundServiceEntryPointDigestError::AmbiguousServiceId)
        );
        assert_eq!(
            bind_background_service_entry_point_digest_from_signed(&checked(raw.as_bytes()), "worker"),
            Err(BackgroundServiceEntryPointDigestError::AmbiguousServiceId)
        );
    }

    #[test]
    fn duplicate_object_key_last_wins_is_documented_by_parse() {
        let sri_first = sri_sha512(b"first");
        let sri_last = sri_sha512(b"last");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"{sri_first}","entryPointIntegrity":"{sri_last}"}}]}}}}"#
        );
        let binding =
            bind_background_service_entry_point_digest_from_signed(&checked(raw.as_bytes()), "worker").unwrap();
        assert_eq!(binding.integrity(), sri_last);
        assert_ne!(binding.integrity(), sri_first);
    }

    #[test]
    fn backcompat_manifest_without_field_still_parses() {
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"backgroundServices":[{"id":"worker","entryPoint":"services/worker.js"}]}}"#;
        let manifest = parse_manifest(raw).unwrap();
        assert!(
            manifest.contributes.as_ref().unwrap().background_services[0]
                .entry_point_integrity
                .is_none()
        );
    }

    #[test]
    fn cross_array_isolation_panels_views_and_services() {
        // Cross-scope isolation: each binder resolves only its contribute array,
        // even when string ids collide across panels / workPanelViews / backgroundServices.
        let sri_panel = sri_sha512(b"panel");
        let sri_view = sri_sha512(b"view");
        let sri_svc = sri_sha512(b"svc");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"same","title":"Panel","entryPoint":"p.html","entryPointIntegrity":"{sri_panel}"}}],"workPanelViews":[{{"id":"same","title":"View","entryPoint":"v.html","entryPointIntegrity":"{sri_view}"}}],"backgroundServices":[{{"id":"same","entryPoint":"s.js","entryPointIntegrity":"{sri_svc}"}}]}}}}"#
        );
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let checked = checked(raw.as_bytes());

        let panel_binding = expected_entry_point_integrity(&manifest, "same").unwrap();
        assert_eq!(panel_binding.entry_point(), "p.html");
        assert_eq!(panel_binding.integrity(), sri_panel);

        let view_binding = expected_work_panel_view_entry_point_integrity(&manifest, "same").unwrap();
        assert_eq!(view_binding.entry_point(), "v.html");
        assert_eq!(view_binding.integrity(), sri_view);

        let svc_binding = expected_background_service_entry_point_integrity(&manifest, "same").unwrap();
        assert_eq!(svc_binding.entry_point(), "s.js");
        assert_eq!(svc_binding.integrity(), sri_svc);

        assert_eq!(
            bind_entry_point_digest_from_signed(&checked, "same")
                .unwrap()
                .integrity(),
            sri_panel
        );
        assert_eq!(
            bind_work_panel_view_entry_point_digest_from_signed(&checked, "same")
                .unwrap()
                .integrity(),
            sri_view
        );
        assert_eq!(
            bind_background_service_entry_point_digest_from_signed(&checked, "same")
                .unwrap()
                .integrity(),
            sri_svc
        );

        // Only-services id is PanelNotFound / ViewNotFound for sibling binders.
        let services_only = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"s.js","entryPointIntegrity":"{sri_svc}"}}]}}}}"#
        );
        let services_manifest = parse_manifest(services_only.as_bytes()).unwrap();
        assert_eq!(
            expected_entry_point_integrity(&services_manifest, "worker"),
            Err(EntryPointDigestError::PanelNotFound)
        );
        assert_eq!(
            expected_work_panel_view_entry_point_integrity(&services_manifest, "worker"),
            Err(WorkPanelViewEntryPointDigestError::ViewNotFound)
        );

        // Only-panels id is ServiceNotFound for the backgroundServices binder.
        let panels_only = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri_panel}"}}]}}}}"#
        );
        assert_eq!(
            expected_background_service_entry_point_integrity(
                &parse_manifest(panels_only.as_bytes()).unwrap(),
                "board"
            ),
            Err(BackgroundServiceEntryPointDigestError::ServiceNotFound)
        );

        // Only-views id is ServiceNotFound for the backgroundServices binder.
        let views_only = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri_view}"}}]}}}}"#
        );
        assert_eq!(
            expected_background_service_entry_point_integrity(&parse_manifest(views_only.as_bytes()).unwrap(), "side"),
            Err(BackgroundServiceEntryPointDigestError::ServiceNotFound)
        );
    }

    #[test]
    fn integrity_inherited_from_files_binds_exactly_like_declared() {
        let sri = sri_sha512(b"inherit body");
        let head = r#"{"name":"demo.plugin","version":"1.0.0""#;
        let files_only = format!(
            r#"{head},"files":[{{"path":"services/worker.sh","integrity":"{sri}"}}],"contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"services/worker.sh"}}]}}}}"#
        );
        let declared = format!(
            r#"{head},"contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"services/worker.sh","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let a = parse_manifest(files_only.as_bytes()).unwrap();
        let b = parse_manifest(declared.as_bytes()).unwrap();
        let ba = expected_background_service_entry_point_integrity(&a, "worker").unwrap();
        let bb = expected_background_service_entry_point_integrity(&b, "worker").unwrap();
        assert_eq!(ba.entry_point(), bb.entry_point());
        assert_eq!(ba.integrity(), bb.integrity());
        assert_eq!(ba.integrity(), sri);
        // The signed-snapshot binder re-parses the same bytes and yields the same binding.
        let sa =
            bind_background_service_entry_point_digest_from_signed(&checked(files_only.as_bytes()), "worker").unwrap();
        assert_eq!(sa.integrity(), sri);
        assert_eq!(sa.entry_point(), "services/worker.sh");
        // An entry that is not listed in `files` is refused at parse: no binder can ever see it.
        let unlisted = files_only.replace(r#""path":"services/worker.sh""#, r#""path":"other.bin""#);
        assert!(parse_manifest(unlisted.as_bytes()).is_err());
        // ...and the signing check itself refuses such a snapshot, so no signed binding can exist.
        let (integrity, signature) = sign(unlisted.as_bytes(), "demo.plugin", "1.0.0");
        assert!(
            check_signed_manifest(
                &policy(),
                &SignedManifestEntry {
                    name: "demo.plugin",
                    version: "1.0.0",
                    pubkey_id: Some("fixture"),
                    integrity: Some(&integrity),
                    signature: Some(&signature),
                },
                unlisted.as_bytes(),
            )
            .is_err()
        );
    }
}

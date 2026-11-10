//! Optional per-workPanelView entry-point digest extraction from typed/signed manifests.
//!
//! Sibling of `entry_point_digest` (standalone `contributes.panels`). A present,
//! valid `entryPointIntegrity` value is only a **declared expected SRI** inside
//! already-trusted manifest bytes. Missing/None never means verified.
//! Combining this with `SignatureCheckedManifest` + Host `read_checked_source`
//! can form a host-mediated binding; this module alone does not authorize view
//! load/execution, prove curation/owner/root or read entry-point files.
//!
//! Scoped to `contributes.workPanelViews` only (panels / backgroundServices have siblings).
use super::entry_point_digest::EntryPointDigestBinding;
use super::signed_manifest::SignatureCheckedManifest;
use super::sri_format::is_canonical_sha512_sri;
use crate::manifest::parse_manifest;
use crate::types::ExtensionManifest;

/// Fixed categories; never include digests, paths, view ids or raw JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkPanelViewEntryPointDigestError {
    ViewNotFound,
    AmbiguousViewId,
    NotPresent,
    EntryPointMissing,
    InvalidIntegrity,
    InvalidManifest,
}

/// Look up one workPanelView by **exact `id`** and return its declared digest binding.
///
/// Precedence is exact view id only (not entry-point path). Duplicate ids among
/// `contributes.workPanelViews` fail closed with `AmbiguousViewId` because the pure
/// JSON array parser keeps every element. Missing field => `NotPresent` (not Ok).
/// Present invalid SRI => `InvalidIntegrity`. Present with empty `entry_point`
/// => `EntryPointMissing`. No digest is invented. Does not consult `contributes.panels`.
pub fn expected_work_panel_view_entry_point_integrity(
    manifest: &ExtensionManifest,
    view_id: &str,
) -> Result<EntryPointDigestBinding, WorkPanelViewEntryPointDigestError> {
    let views = manifest
        .contributes
        .as_ref()
        .map(|c| c.work_panel_views.as_slice())
        .unwrap_or(&[]);
    let matches: Vec<_> = views.iter().filter(|view| view.id == view_id).collect();
    match matches.as_slice() {
        [] => Err(WorkPanelViewEntryPointDigestError::ViewNotFound),
        [_, _, ..] => Err(WorkPanelViewEntryPointDigestError::AmbiguousViewId),
        [view] => {
            let Some(integrity) = view.entry_point_integrity.as_deref() else {
                return Err(WorkPanelViewEntryPointDigestError::NotPresent);
            };
            if !is_canonical_sha512_sri(integrity) {
                return Err(WorkPanelViewEntryPointDigestError::InvalidIntegrity);
            }
            if view.entry_point.is_empty() {
                return Err(WorkPanelViewEntryPointDigestError::EntryPointMissing);
            }
            Ok(EntryPointDigestBinding::new(
                view.entry_point.clone(),
                integrity.to_owned(),
            ))
        }
    }
}

/// Re-parse a signature-checked manifest **byte snapshot** and extract one workPanelView binding.
///
/// Uses the owned raw bytes only — no filesystem reread. Success still requires
/// a present valid field; absence is `NotPresent`, never a synthetic Ok digest.
/// Host trust in the signed snapshot and later `read_checked_source` comparison
/// remain separate. Duplicate JSON object keys follow serde_json Value last-wins
/// during parse; duplicate array entries with the same view id fail closed here.
pub fn bind_work_panel_view_entry_point_digest_from_signed(
    checked: &SignatureCheckedManifest,
    view_id: &str,
) -> Result<EntryPointDigestBinding, WorkPanelViewEntryPointDigestError> {
    let manifest =
        parse_manifest(checked.raw_bytes()).map_err(|_| WorkPanelViewEntryPointDigestError::InvalidManifest)?;
    expected_work_panel_view_entry_point_integrity(&manifest, view_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::entry_point_digest::{
        EntryPointDigestError, bind_entry_point_digest_from_signed, expected_entry_point_integrity,
    };
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[13; 32])
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

    #[test]
    fn missing_field_is_not_present_not_ok() {
        let raw = views_json(r#"[{"id":"side","title":"Side","entryPoint":"views/side.html"}]"#);
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(
            expected_work_panel_view_entry_point_integrity(&manifest, "side"),
            Err(WorkPanelViewEntryPointDigestError::NotPresent)
        );
        assert_eq!(
            bind_work_panel_view_entry_point_digest_from_signed(&checked(raw.as_bytes()), "side"),
            Err(WorkPanelViewEntryPointDigestError::NotPresent)
        );
    }

    #[test]
    fn present_valid_returns_exact_entry_point_and_sri() {
        let sri = sri_sha512(b"<html>side</html>");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"views/side.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let binding = bind_work_panel_view_entry_point_digest_from_signed(&checked(raw.as_bytes()), "side").unwrap();
        assert_eq!(binding.entry_point(), "views/side.html");
        assert_eq!(binding.integrity(), sri);
        assert_eq!(binding.integrity().as_bytes(), sri.as_bytes());
    }

    #[test]
    fn camelcase_alias_parses() {
        let sri = sri_sha512(b"view");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let view = &manifest.contributes.as_ref().unwrap().work_panel_views[0];
        assert_eq!(view.entry_point_integrity.as_deref(), Some(sri.as_str()));
        assert_eq!(
            expected_work_panel_view_entry_point_integrity(&manifest, "side")
                .unwrap()
                .entry_point(),
            "v.html"
        );
    }

    #[test]
    fn invalid_present_sri_rejected_at_parse() {
        let raw =
            views_json(r#"[{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"sha256-x"}]"#);
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn malformed_sri_forms_rejected() {
        let good = sri_sha512(b"view");
        let mut bad_pad = good.as_bytes().to_vec();
        bad_pad[92] = b'B';
        for sri in [
            String::new(),
            good.replacen("sha512-", "SHA512-", 1),
            format!("{good} "),
            String::from_utf8(bad_pad).unwrap(),
            "sha512-".to_owned() + &"A".repeat(88),
        ] {
            let raw = views_json(&format!(
                r#"[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]"#
            ));
            assert!(parse_manifest(raw.as_bytes()).is_err(), "{sri:?}");
        }
    }

    #[test]
    fn integrity_with_empty_entry_point_rejected() {
        let sri = sri_sha512(b"view");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn wrong_view_and_unknown_view() {
        let sri = sri_sha512(b"view");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let checked = checked(raw.as_bytes());
        assert_eq!(
            bind_work_panel_view_entry_point_digest_from_signed(&checked, "other"),
            Err(WorkPanelViewEntryPointDigestError::ViewNotFound)
        );
    }

    #[test]
    fn duplicate_view_ids_fail_closed_for_binder() {
        let sri = sri_sha512(b"view");
        let raw = views_json(&format!(
            r#"[{{"id":"side","title":"A","entryPoint":"a.html","entryPointIntegrity":"{sri}"}},{{"id":"side","title":"B","entryPoint":"b.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(manifest.contributes.as_ref().unwrap().work_panel_views.len(), 2);
        assert_eq!(
            expected_work_panel_view_entry_point_integrity(&manifest, "side"),
            Err(WorkPanelViewEntryPointDigestError::AmbiguousViewId)
        );
        assert_eq!(
            bind_work_panel_view_entry_point_digest_from_signed(&checked(raw.as_bytes()), "side"),
            Err(WorkPanelViewEntryPointDigestError::AmbiguousViewId)
        );
    }

    #[test]
    fn duplicate_object_key_last_wins_is_documented_by_parse() {
        let sri_first = sri_sha512(b"first");
        let sri_last = sri_sha512(b"last");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri_first}","entryPointIntegrity":"{sri_last}"}}]}}}}"#
        );
        let binding = bind_work_panel_view_entry_point_digest_from_signed(&checked(raw.as_bytes()), "side").unwrap();
        assert_eq!(binding.integrity(), sri_last);
        assert_ne!(binding.integrity(), sri_first);
    }

    #[test]
    fn backcompat_manifest_without_field_still_parses() {
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"workPanelViews":[{"id":"side","title":"Side","entryPoint":"views/side.html"}]}}"#;
        let manifest = parse_manifest(raw).unwrap();
        assert!(
            manifest.contributes.as_ref().unwrap().work_panel_views[0]
                .entry_point_integrity
                .is_none()
        );
    }

    #[test]
    fn panels_binder_ignores_work_panel_views_and_vice_versa() {
        // Cross-scope isolation: panels binder never resolves workPanelViews ids;
        // workPanelViews binder never resolves panels ids — even when ids collide.
        let sri_panel = sri_sha512(b"panel");
        let sri_view = sri_sha512(b"view");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"same","title":"Panel","entryPoint":"p.html","entryPointIntegrity":"{sri_panel}"}}],"workPanelViews":[{{"id":"same","title":"View","entryPoint":"v.html","entryPointIntegrity":"{sri_view}"}}]}}}}"#
        );
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let checked = checked(raw.as_bytes());

        let panel_binding = expected_entry_point_integrity(&manifest, "same").unwrap();
        assert_eq!(panel_binding.entry_point(), "p.html");
        assert_eq!(panel_binding.integrity(), sri_panel);

        let view_binding = expected_work_panel_view_entry_point_integrity(&manifest, "same").unwrap();
        assert_eq!(view_binding.entry_point(), "v.html");
        assert_eq!(view_binding.integrity(), sri_view);

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

        // Only-panels id is ViewNotFound for the workPanelViews binder.
        let panels_only = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri_panel}"}}]}}}}"#
        );
        assert_eq!(
            expected_work_panel_view_entry_point_integrity(&parse_manifest(panels_only.as_bytes()).unwrap(), "board"),
            Err(WorkPanelViewEntryPointDigestError::ViewNotFound)
        );
        // Only-views id is PanelNotFound for the panels binder.
        let views_only = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri_view}"}}]}}}}"#
        );
        assert_eq!(
            expected_entry_point_integrity(&parse_manifest(views_only.as_bytes()).unwrap(), "side"),
            Err(EntryPointDigestError::PanelNotFound)
        );
    }

    #[test]
    fn integrity_inherited_from_files_binds_exactly_like_declared() {
        let sri = sri_sha512(b"inherit body");
        let head = r#"{"name":"demo.plugin","version":"1.0.0""#;
        let files_only = format!(
            r#"{head},"files":[{{"path":"views/side.html","integrity":"{sri}"}}],"contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"views/side.html"}}]}}}}"#
        );
        let declared = format!(
            r#"{head},"contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"views/side.html","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let a = parse_manifest(files_only.as_bytes()).unwrap();
        let b = parse_manifest(declared.as_bytes()).unwrap();
        let ba = expected_work_panel_view_entry_point_integrity(&a, "side").unwrap();
        let bb = expected_work_panel_view_entry_point_integrity(&b, "side").unwrap();
        assert_eq!(ba.entry_point(), bb.entry_point());
        assert_eq!(ba.integrity(), bb.integrity());
        assert_eq!(ba.integrity(), sri);
        // The signed-snapshot binder re-parses the same bytes and yields the same binding.
        let sa = bind_work_panel_view_entry_point_digest_from_signed(&checked(files_only.as_bytes()), "side").unwrap();
        assert_eq!(sa.integrity(), sri);
        assert_eq!(sa.entry_point(), "views/side.html");
        // An entry that is not listed in `files` is refused at parse: no binder can ever see it.
        let unlisted = files_only.replace(r#""path":"views/side.html""#, r#""path":"other.bin""#);
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

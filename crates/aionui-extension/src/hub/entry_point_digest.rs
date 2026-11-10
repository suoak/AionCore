//! Optional per-panel entry-point digest extraction from typed/signed manifests.
//!
//! A present, valid `entryPointIntegrity` value is only a **declared expected SRI**
//! inside already-trusted manifest bytes. Missing/None never means verified.
//! Combining this with `SignatureCheckedManifest` + Host `read_checked_source`
//! can form a host-mediated binding; this module alone does not authorize panel
//! load/execution, prove curation/owner/root or read entry-point files.
//!
//! Scoped to standalone `contributes.panels` (workPanelViews / backgroundServices have siblings).
use super::signed_manifest::SignatureCheckedManifest;
use super::sri_format::is_canonical_sha512_sri;
use crate::manifest::parse_manifest;
use crate::types::ExtensionManifest;

/// Fixed categories; never include digests, paths, panel ids or raw JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPointDigestError {
    PanelNotFound,
    AmbiguousPanelId,
    NotPresent,
    EntryPointMissing,
    InvalidIntegrity,
    InvalidManifest,
}

/// Exact entry-point relative path and expected canonical SRI copied from the manifest.
///
/// Private fields with borrowing getters. This is not a panel-load or execution permit.
#[derive(Clone, PartialEq, Eq)]
pub struct EntryPointDigestBinding {
    entry_point: String,
    integrity: String,
}

impl std::fmt::Debug for EntryPointDigestBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Summary only: never dump entry-point path or digest bytes into logs/asserts.
        f.write_str("EntryPointDigestBinding")
    }
}

impl EntryPointDigestBinding {
    /// Construct a binding from already-validated non-empty entry path + canonical SRI.
    ///
    /// `pub(crate)` so sibling contribute binders (workPanelViews / backgroundServices) can reuse the
    /// same opaque type without exposing a public invent-digest API.
    pub(crate) fn new(entry_point: String, integrity: String) -> Self {
        Self { entry_point, integrity }
    }

    pub fn entry_point(&self) -> &str {
        &self.entry_point
    }

    pub fn integrity(&self) -> &str {
        &self.integrity
    }
}

/// Look up one panel by **exact `id`** and return its declared digest binding.
///
/// Precedence is exact panel id only (not entry-point path). Duplicate ids among
/// `contributes.panels` fail closed with `AmbiguousPanelId` because the pure
/// JSON array parser keeps every element. Missing field => `NotPresent` (not Ok).
/// Present invalid SRI => `InvalidIntegrity`. Present with empty `entry_point`
/// => `EntryPointMissing`. No digest is invented.
pub fn expected_entry_point_integrity(
    manifest: &ExtensionManifest,
    panel_id: &str,
) -> Result<EntryPointDigestBinding, EntryPointDigestError> {
    let panels = manifest
        .contributes
        .as_ref()
        .map(|c| c.panels.as_slice())
        .unwrap_or(&[]);
    let matches: Vec<_> = panels.iter().filter(|panel| panel.id == panel_id).collect();
    match matches.as_slice() {
        [] => Err(EntryPointDigestError::PanelNotFound),
        [_, _, ..] => Err(EntryPointDigestError::AmbiguousPanelId),
        [panel] => {
            let Some(integrity) = panel.entry_point_integrity.as_deref() else {
                return Err(EntryPointDigestError::NotPresent);
            };
            if !is_canonical_sha512_sri(integrity) {
                return Err(EntryPointDigestError::InvalidIntegrity);
            }
            if panel.entry_point.is_empty() {
                return Err(EntryPointDigestError::EntryPointMissing);
            }
            Ok(EntryPointDigestBinding::new(
                panel.entry_point.clone(),
                integrity.to_owned(),
            ))
        }
    }
}

/// Re-parse a signature-checked manifest **byte snapshot** and extract one panel binding.
///
/// Uses the owned raw bytes only — no filesystem reread. Success still requires
/// a present valid field; absence is `NotPresent`, never a synthetic Ok digest.
/// Host trust in the signed snapshot and later `read_checked_source` comparison
/// remain separate. Duplicate JSON object keys follow serde_json Value last-wins
/// during parse; duplicate array entries with the same panel id fail closed here.
pub fn bind_entry_point_digest_from_signed(
    checked: &SignatureCheckedManifest,
    panel_id: &str,
) -> Result<EntryPointDigestBinding, EntryPointDigestError> {
    let manifest = parse_manifest(checked.raw_bytes()).map_err(|_| EntryPointDigestError::InvalidManifest)?;
    expected_entry_point_integrity(&manifest, panel_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::manifest::parse_manifest;
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

    #[test]
    fn missing_field_is_not_present_not_ok() {
        let raw = manifest_json(r#"[{"id":"board","title":"Board","entryPoint":"panels/board.html"}]"#);
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(
            expected_entry_point_integrity(&manifest, "board"),
            Err(EntryPointDigestError::NotPresent)
        );
        assert_eq!(
            bind_entry_point_digest_from_signed(&checked(raw.as_bytes()), "board"),
            Err(EntryPointDigestError::NotPresent)
        );
    }

    #[test]
    fn present_valid_returns_exact_entry_point_and_sri() {
        let sri = sri_sha512(b"<html>board</html>");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"panels/board.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let binding = bind_entry_point_digest_from_signed(&checked(raw.as_bytes()), "board").unwrap();
        assert_eq!(binding.entry_point(), "panels/board.html");
        assert_eq!(binding.integrity(), sri);
        assert_eq!(binding.integrity().as_bytes(), sri.as_bytes());
    }

    #[test]
    fn camelcase_alias_parses() {
        let sri = sri_sha512(b"panel");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let panel = &manifest.contributes.as_ref().unwrap().panels[0];
        assert_eq!(panel.entry_point_integrity.as_deref(), Some(sri.as_str()));
        assert_eq!(
            expected_entry_point_integrity(&manifest, "board")
                .unwrap()
                .entry_point(),
            "p.html"
        );
    }

    #[test]
    fn invalid_present_sri_rejected_at_parse() {
        let raw =
            manifest_json(r#"[{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"sha256-x"}]"#);
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn malformed_sri_forms_rejected() {
        let good = sri_sha512(b"panel");
        let mut bad_pad = good.as_bytes().to_vec();
        bad_pad[92] = b'B';
        for sri in [
            String::new(),
            good.replacen("sha512-", "SHA512-", 1),
            format!("{good} "),
            String::from_utf8(bad_pad).unwrap(),
            "sha512-".to_owned() + &"A".repeat(88),
        ] {
            let raw = manifest_json(&format!(
                r#"[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]"#
            ));
            assert!(parse_manifest(raw.as_bytes()).is_err(), "{sri:?}");
        }
    }

    #[test]
    fn integrity_with_empty_entry_point_rejected() {
        let sri = sri_sha512(b"panel");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"","entryPointIntegrity":"{sri}"}}]"#
        ));
        assert!(parse_manifest(raw.as_bytes()).is_err());
    }

    #[test]
    fn wrong_panel_and_unknown_panel() {
        let sri = sri_sha512(b"panel");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let checked = checked(raw.as_bytes());
        assert_eq!(
            bind_entry_point_digest_from_signed(&checked, "other"),
            Err(EntryPointDigestError::PanelNotFound)
        );
    }

    #[test]
    fn duplicate_panel_ids_fail_closed_for_binder() {
        let sri = sri_sha512(b"panel");
        let raw = manifest_json(&format!(
            r#"[{{"id":"board","title":"A","entryPoint":"a.html","entryPointIntegrity":"{sri}"}},{{"id":"board","title":"B","entryPoint":"b.html","entryPointIntegrity":"{sri}"}}]"#
        ));
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(manifest.contributes.as_ref().unwrap().panels.len(), 2);
        assert_eq!(
            expected_entry_point_integrity(&manifest, "board"),
            Err(EntryPointDigestError::AmbiguousPanelId)
        );
        assert_eq!(
            bind_entry_point_digest_from_signed(&checked(raw.as_bytes()), "board"),
            Err(EntryPointDigestError::AmbiguousPanelId)
        );
    }

    #[test]
    fn duplicate_object_key_last_wins_is_documented_by_parse() {
        let sri_first = sri_sha512(b"first");
        let sri_last = sri_sha512(b"last");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"board","title":"Board","entryPoint":"p.html","entryPointIntegrity":"{sri_first}","entryPointIntegrity":"{sri_last}"}}]}}}}"#
        );
        let binding = bind_entry_point_digest_from_signed(&checked(raw.as_bytes()), "board").unwrap();
        assert_eq!(binding.integrity(), sri_last);
        assert_ne!(binding.integrity(), sri_first);
    }

    #[test]
    fn backcompat_manifest_without_field_still_parses() {
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","contributes":{"panels":[{"id":"board","title":"Board","entryPoint":"panels/board.html"}]}}"#;
        let manifest = parse_manifest(raw).unwrap();
        assert!(
            manifest.contributes.as_ref().unwrap().panels[0]
                .entry_point_integrity
                .is_none()
        );
    }

    #[test]
    fn panels_binder_still_ignores_work_panel_views() {
        // Panels binder remains scoped to contributes.panels. A workPanelViews-only
        // id is PanelNotFound here even when entryPointIntegrity is present on the view
        // (sibling binder covers workPanelViews separately).
        let sri = sri_sha512(b"view");
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"workPanelViews":[{{"id":"side","title":"Side","entryPoint":"v.html","entryPointIntegrity":"{sri}"}}]}}}}"#
        );
        let manifest = parse_manifest(raw.as_bytes()).unwrap();
        let views = &manifest.contributes.as_ref().unwrap().work_panel_views;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].entry_point, "v.html");
        assert_eq!(views[0].entry_point_integrity.as_deref(), Some(sri.as_str()));
        assert_eq!(
            expected_entry_point_integrity(&manifest, "side"),
            Err(EntryPointDigestError::PanelNotFound)
        );
    }
}

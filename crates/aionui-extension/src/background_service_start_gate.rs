//! Fail-closed gate for Host background-service **start** (M9 E13).
//!
//! `BackgroundServiceManager::start` spawns a *file path* (`CmdBuilder::clean_cli(&script)`).
//! A checked-source read (`read_checked_source`) yields verified **bytes**, not a path the
//! kernel can exec without re-reading; spawning the path after a separate verification would
//! be a TOCTOU, and there is no honest Host directory `File` at this call site
//! (`LoadedExtension.directory` is a `String`, which must never be reopened as a trust root).
//!
//! Bound extensions whose signed snapshot declares `files` are handled by
//! [`crate::bound_background_start`] (verify through the retained root, then refuse FD-exec in
//! this slice) and never reach the path spawn below.
//!
//! So the honest slice for the **legacy / no-files** path is a **refusal gate**: if a service declares `entryPointIntegrity`
//! (in the loaded manifest, or in a Host-retained signature-checked snapshot), the
//! integrity claim cannot be honoured by the path spawn → refuse; never fall back to an
//! unchecked spawn. A service that declares nothing keeps the legacy path-spawn behaviour
//! (missing integrity is *not* "verified" — it is simply undeclared).
//!
//! Not execution authorization, not root/owner/curation proof, not E13 complete.
//! Reasons are fixed strings; never include digests, paths or manifest text.

use crate::error::ExtensionError;
use crate::hub::background_service_entry_point_digest::{
    BackgroundServiceEntryPointDigestError, bind_background_service_entry_point_digest_from_signed,
};
use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::types::{ExtensionManifest, ResolvedBackgroundService};

/// Fixed refusal categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundServiceStartGateError {
    /// Loaded manifest declares `entryPointIntegrity` for this service id.
    DeclaredIntegrityCannotBeChecked,
    /// Host-retained signed snapshot declares `entryPointIntegrity` for this service id.
    SignedIntegrityCannotBeChecked,
    /// A Host-retained signed snapshot exists but does not give a clean "undeclared" answer
    /// (service absent, ambiguous, invalid manifest/SRI, ...). Fail closed.
    SignedSnapshotInconclusive,
}

impl BackgroundServiceStartGateError {
    /// Stable machine-readable reason.
    pub fn reason(self) -> &'static str {
        match self {
            Self::DeclaredIntegrityCannotBeChecked => "entry_point_integrity_declared_unchecked_spawn",
            Self::SignedIntegrityCannotBeChecked => "signed_entry_point_integrity_declared_unchecked_spawn",
            Self::SignedSnapshotInconclusive => "signed_snapshot_inconclusive",
        }
    }
}

impl From<BackgroundServiceStartGateError> for ExtensionError {
    fn from(err: BackgroundServiceStartGateError) -> Self {
        ExtensionError::ManifestValidation(format!(
            "background service start refused: {} (declared entryPointIntegrity cannot be enforced by a path spawn)",
            err.reason()
        ))
    }
}

/// Local service id as declared in the manifest (`worker`), not the resolved `ext-{name}-{id}`.
pub fn local_background_service_id(service: &ResolvedBackgroundService) -> &str {
    let prefix = format!("ext-{}-", service.extension_name);
    service.id.strip_prefix(&prefix).unwrap_or(service.id.as_str())
}

/// Decide whether a path spawn of `service` may proceed.
///
/// - Any loaded-manifest entry with this local id that declares `entryPointIntegrity`
///   (valid or not) → refuse.
/// - If the Host retained a signature-checked snapshot, consult it too: a declared valid SRI
///   → refuse; anything other than a clean `NotPresent` → refuse (inconclusive).
/// - Otherwise `Ok(())` (undeclared; legacy spawn, **not** "verified").
///
/// Never opens a path, never re-reads disk, never invents a root.
pub fn gate_background_service_start(
    manifest: &ExtensionManifest,
    service: &ResolvedBackgroundService,
    signed: Option<&SignatureCheckedManifest>,
) -> Result<(), BackgroundServiceStartGateError> {
    let local_id = local_background_service_id(service);

    let declared_in_loaded = manifest
        .contributes
        .as_ref()
        .map(|c| c.background_services.as_slice())
        .unwrap_or(&[])
        .iter()
        .any(|svc| svc.id == local_id && svc.entry_point_integrity.is_some());
    if declared_in_loaded {
        return Err(BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked);
    }

    if let Some(checked) = signed {
        match bind_background_service_entry_point_digest_from_signed(checked, local_id) {
            Ok(_) => return Err(BackgroundServiceStartGateError::SignedIntegrityCannotBeChecked),
            Err(BackgroundServiceEntryPointDigestError::NotPresent) => {}
            Err(_) => return Err(BackgroundServiceStartGateError::SignedSnapshotInconclusive),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn sri() -> String {
        sri_sha512(b"service body")
    }

    fn manifest_json(services: &str) -> String {
        format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"backgroundServices":{services}}}}}"#)
    }

    fn manifest(services: &str) -> ExtensionManifest {
        parse_manifest(manifest_json(services).as_bytes()).unwrap()
    }

    fn resolved(local: &str) -> ResolvedBackgroundService {
        ResolvedBackgroundService {
            extension_name: "demo.plugin".into(),
            id: format!("ext-demo.plugin-{local}"),
            entry_point: "services/worker.sh".into(),
            description: None,
        }
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let key = SigningKey::from_bytes(&[17; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            key.sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy,
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
    fn local_id_strips_resolved_prefix_only() {
        assert_eq!(local_background_service_id(&resolved("worker")), "worker");
        let mut other = resolved("worker");
        other.id = "worker".into();
        assert_eq!(local_background_service_id(&other), "worker");
    }

    #[test]
    fn undeclared_service_is_not_blocked_and_not_claimed_verified() {
        let m = manifest(r#"[{"id":"worker","entryPoint":"services/worker.sh"}]"#);
        assert_eq!(gate_background_service_start(&m, &resolved("worker"), None), Ok(()));
    }

    #[test]
    fn loaded_manifest_declared_integrity_refuses() {
        let json = format!(
            r#"[{{"id":"worker","entryPoint":"services/worker.sh","entryPointIntegrity":"{}"}}]"#,
            sri()
        );
        let m = manifest(&json);
        assert_eq!(
            gate_background_service_start(&m, &resolved("worker"), None),
            Err(BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked)
        );
    }

    fn files_only_json() -> String {
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"services/worker.sh","integrity":"{}"}}],"contributes":{{"backgroundServices":[{{"id":"w","entryPoint":"services/worker.sh"}}]}}}}"#,
            sri()
        )
    }

    #[test]
    fn integrity_inherited_from_files_refuses_exactly_like_declared() {
        let inherited = parse_manifest(files_only_json().as_bytes()).unwrap();
        let declared = manifest(&format!(
            r#"[{{"id":"w","entryPoint":"services/worker.sh","entryPointIntegrity":"{}"}}]"#,
            sri()
        ));
        for m in [&inherited, &declared] {
            assert_eq!(
                gate_background_service_start(m, &resolved("w"), None),
                Err(BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked)
            );
        }
    }

    #[test]
    fn files_only_signed_snapshot_refuses_even_if_loaded_manifest_stripped() {
        let snapshot = checked(files_only_json().as_bytes());
        let stripped = manifest(r#"[{"id":"w","entryPoint":"services/worker.sh"}]"#);
        assert_eq!(
            gate_background_service_start(&stripped, &resolved("w"), Some(&snapshot)),
            Err(BackgroundServiceStartGateError::SignedIntegrityCannotBeChecked)
        );
    }

    #[test]
    fn declared_integrity_on_other_service_does_not_block_this_one() {
        let json = format!(
            r#"[{{"id":"worker","entryPoint":"services/worker.sh"}},{{"id":"other","entryPoint":"services/o.sh","entryPointIntegrity":"{}"}}]"#,
            sri()
        );
        let m = manifest(&json);
        assert_eq!(gate_background_service_start(&m, &resolved("worker"), None), Ok(()));
        assert!(gate_background_service_start(&m, &resolved("other"), None).is_err());
    }

    #[test]
    fn duplicate_ids_with_any_declaration_refuse() {
        let json = format!(
            r#"[{{"id":"worker","entryPoint":"a.sh"}},{{"id":"worker","entryPoint":"b.sh","entryPointIntegrity":"{}"}}]"#,
            sri()
        );
        let m = manifest(&json);
        assert_eq!(
            gate_background_service_start(&m, &resolved("worker"), None),
            Err(BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked)
        );
    }

    #[test]
    fn same_id_panel_declaration_does_not_trigger_service_gate() {
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"panels":[{{"id":"worker","title":"w","entryPoint":"p.html","entryPointIntegrity":"{}"}}],"backgroundServices":[{{"id":"worker","entryPoint":"services/worker.sh"}}]}}}}"#,
            sri()
        );
        let m = parse_manifest(raw.as_bytes()).unwrap();
        assert_eq!(gate_background_service_start(&m, &resolved("worker"), None), Ok(()));
    }

    #[test]
    fn signed_snapshot_declaring_integrity_refuses_even_if_loaded_manifest_stripped() {
        let signed_raw = manifest_json(&format!(
            r#"[{{"id":"worker","entryPoint":"services/worker.sh","entryPointIntegrity":"{}"}}]"#,
            sri()
        ));
        let snapshot = checked(signed_raw.as_bytes());
        let stripped = manifest(r#"[{"id":"worker","entryPoint":"services/worker.sh"}]"#);
        assert_eq!(
            gate_background_service_start(&stripped, &resolved("worker"), Some(&snapshot)),
            Err(BackgroundServiceStartGateError::SignedIntegrityCannotBeChecked)
        );
    }

    #[test]
    fn signed_snapshot_without_declaration_allows_undeclared() {
        let signed_raw = manifest_json(r#"[{"id":"worker","entryPoint":"services/worker.sh"}]"#);
        let snapshot = checked(signed_raw.as_bytes());
        let m = manifest(r#"[{"id":"worker","entryPoint":"services/worker.sh"}]"#);
        assert_eq!(
            gate_background_service_start(&m, &resolved("worker"), Some(&snapshot)),
            Ok(())
        );
    }

    #[test]
    fn signed_snapshot_missing_service_is_inconclusive_refuse() {
        let signed_raw = manifest_json(r#"[{"id":"different","entryPoint":"services/d.sh"}]"#);
        let snapshot = checked(signed_raw.as_bytes());
        let m = manifest(r#"[{"id":"worker","entryPoint":"services/worker.sh"}]"#);
        assert_eq!(
            gate_background_service_start(&m, &resolved("worker"), Some(&snapshot)),
            Err(BackgroundServiceStartGateError::SignedSnapshotInconclusive)
        );
    }

    #[test]
    fn error_maps_to_manifest_validation_with_fixed_reason_only() {
        let err: ExtensionError = BackgroundServiceStartGateError::DeclaredIntegrityCannotBeChecked.into();
        let text = err.to_string();
        assert!(text.contains("entry_point_integrity_declared_unchecked_spawn"));
        assert!(!text.contains("sha512-"));
        assert!(matches!(err, ExtensionError::ManifestValidation(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn manager_start_refuses_before_spawn_when_integrity_declared() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        let script = dir.path().join("worker.sh");
        std::fs::write(&script, format!("#!/bin/sh\necho hi > {}\n", marker.display())).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","permissions":{{"shell":true}},"contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"worker.sh","entryPointIntegrity":"{}"}}]}}}}"#,
            sri()
        );
        let ext = LoadedExtension {
            manifest: parse_manifest(raw.as_bytes()).unwrap(),
            directory: dir.path().display().to_string(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: "demo.plugin".into(),
                version: "1.0.0".into(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        };
        let mut svc = resolved("worker");
        svc.entry_point = "worker.sh".into();
        let manager = BackgroundServiceManager::new();
        let err = manager.start("alice", &ext, &svc).await.unwrap_err();
        assert!(matches!(err, ExtensionError::ManifestValidation(_)));
        assert!(
            err.to_string()
                .contains("entry_point_integrity_declared_unchecked_spawn")
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists(), "script must not run when integrity declared");
        assert!(!manager.is_running("alice", "demo.plugin", &svc.id).await);
    }
}

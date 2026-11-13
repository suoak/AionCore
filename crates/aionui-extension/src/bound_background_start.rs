//! Bound+`files` background-service start policy (M9 E13 slices I → S1).
//!
//! When the Host has a retained signed root whose snapshot declares `files`, a service may
//! **not** be started by a path string (`LoadedExtension.directory`, a canonicalized script path,
//! or anything else). The only runtime a bound service can ever use is `workmate-js` — the Host's
//! embedded, restricted QuickJS runner, fed **verified in-memory bytes** (never a path, never
//! argv). `node`, `shell` and `native` are reserved runtime names that are refused permanently,
//! each with its own fixed reason; there is no guessing from the file extension and no fallback.
//!
//! [`verify_bound_files_service`] is the policy pipeline. Everything it decides comes from the
//! **signed snapshot** (never from the loaded manifest or the disk copy), in this order:
//!
//! 1. no `files` → [`BoundFilesStart::NotApplicable`] (legacy gate + path spawn), **unless** the
//!    signed service declares a `runtime` — a declared runtime must never take the legacy path
//!    spawn (`bound_service_runtime_requires_signed_files`);
//! 2. the retained root must have been supplied (no root → refuse, never a path fallback);
//! 3. the service must be uniquely present in the snapshot;
//! 4. `runtime` must be declared (`bound_service_runtime_required`) and be `workmate-js`
//!    (`node` → `…_runtime_unsupported`, `shell` → `…_shell_script_unsupported`,
//!    `native` → `…_native_unsupported`);
//! 5. the entry must be a `.js` / `.mjs` file;
//! 6. `uses` must be empty for now (capability *authorization* arrives in S3;
//!    a valid S2 `capabilities` schema still yields `bound_service_capabilities_unavailable`);
//! 7. the entry must be the signed, listed, SRI-declared entry; its bytes are read once through
//!    the retained root (`openat(O_NOFOLLOW)`, ≤ 8 MiB regular file, SRI of the bytes read — the
//!    same reader as verified asset serve) and returned as [`BoundServiceReady`].
//!
//! `permissions.shell` plays **no role** for a `workmate-js` service: it neither allows nor blocks
//! it (the legacy path keeps its own `shell` gate).
//!
//! # Not in this slice
//! There is no runner yet. A [`BoundFilesStart::Ready`] outcome is refused by the caller with
//! `bound_service_runner_unavailable`; nothing is spawned and no path exec happens. Capabilities,
//! scheduling, sandboxing and consent are later slices.
//!
//! Reasons are fixed strings; never include digests, paths or bytes.

use std::fs::File;

use crate::asset_paths::normalize_relative_asset_path;
use crate::error::ExtensionError;
use crate::hub::background_service_entry_point_digest::{
    BackgroundServiceEntryPointDigestError, bind_background_service_entry_point_digest_from_signed,
};
use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::hub::{FilesIndex, VerifiedAssetError, read_verified_listed};
use crate::manifest::parse_manifest;
use crate::types::{
    ExtBackgroundService, ExtensionManifest, ResolvedBackgroundService, ServiceCapabilityKind, ServiceRuntime,
};

use super::background_service_start_gate::local_background_service_id;

/// A fully verified `workmate-js` service: the entry bytes (already SRI-checked) plus the
/// signed scheduling/capability declaration. Holds source bytes; `Debug` prints sizes only.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundServiceReady {
    bytes: Vec<u8>,
    interval_seconds: u64,
    uses: Vec<ServiceCapabilityKind>,
}

impl BoundServiceReady {
    /// Verified entry source bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Tick interval in seconds, already clamped to `[10, 86400]`.
    pub fn interval_seconds(&self) -> u64 {
        self.interval_seconds
    }

    pub fn uses(&self) -> &[ServiceCapabilityKind] {
        &self.uses
    }
}

impl std::fmt::Debug for BoundServiceReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundServiceReady")
            .field("byte_len", &self.bytes.len())
            .field("interval_seconds", &self.interval_seconds)
            .field("uses", &self.uses)
            .finish()
    }
}

/// Outcome of the bound+`files` start check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundFilesStart {
    /// No `files` in the signed snapshot (and no declared runtime): the legacy gate + path spawn
    /// applies.
    NotApplicable,
    /// The service passed the whole policy pipeline and its entry bytes were verified. There is
    /// no runner in this slice, so the caller must refuse (and must not path-spawn).
    Ready(BoundServiceReady),
}

/// Fixed refusal categories for the bound+`files` path. Never a path spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundBackgroundStartError {
    /// Snapshot declares `files` but the Host did not supply the retained root `File`.
    RootMissing,
    /// Snapshot `files` could not be parsed (should not happen for a checked snapshot).
    InvalidSnapshot,
    /// The signed snapshot does not give a unique service with integrity (absent, ambiguous,
    /// invalid SRI, missing entry point, …).
    SignedSnapshotInconclusive,
    /// `files` is declared but this service has no inherited or declared integrity.
    IntegrityRequired,
    /// The requested entry path does not equal the signed service `entryPoint`.
    EntryMismatch,
    /// Listed file missing under the retained root.
    Missing,
    /// Not a regular file, symlink, oversize, or other unsafe read.
    Unreadable,
    /// Bytes read do not match the listed SRI.
    IntegrityMismatch,
    /// Platform cannot read FD-relative (non-unix).
    Unavailable,
    /// A bound service must declare `runtime` explicitly.
    RuntimeRequired,
    /// `runtime: node` (deferred; permanently refused for bound services).
    RuntimeUnsupported,
    /// `runtime: shell` (permanently refused).
    ShellScriptUnsupported,
    /// `runtime: native` (permanently refused).
    NativeUnsupported,
    /// The `workmate-js` entry is not a `.js` / `.mjs` file.
    EntryKindUnsupported,
    /// The service lists capability kinds in `uses`; capability authorization is not available yet (S3).
    CapabilitiesUnavailable,
    /// A declared `runtime` without a signed `files` snapshot: never the legacy path spawn.
    RuntimeRequiresSignedFiles,
    /// The service is verified and startable in principle, but the Host has no runner yet.
    RunnerUnavailable,
}

impl BoundBackgroundStartError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::RootMissing => "bound_service_root_missing",
            Self::InvalidSnapshot => "bound_service_invalid_snapshot",
            Self::SignedSnapshotInconclusive => "bound_service_signed_snapshot_inconclusive",
            Self::IntegrityRequired => "bound_service_integrity_required",
            Self::EntryMismatch => "bound_service_entry_mismatch",
            Self::Missing => "bound_service_entry_missing",
            Self::Unreadable => "bound_service_entry_unreadable",
            Self::IntegrityMismatch => "bound_service_integrity_mismatch",
            Self::Unavailable => "bound_service_verified_read_unavailable",
            Self::RuntimeRequired => "bound_service_runtime_required",
            Self::RuntimeUnsupported => "bound_service_runtime_unsupported",
            Self::ShellScriptUnsupported => "bound_service_shell_script_unsupported",
            Self::NativeUnsupported => "bound_service_native_unsupported",
            Self::EntryKindUnsupported => "bound_service_entry_kind_unsupported",
            Self::CapabilitiesUnavailable => "bound_service_capabilities_unavailable",
            Self::RuntimeRequiresSignedFiles => "bound_service_runtime_requires_signed_files",
            Self::RunnerUnavailable => "bound_service_runner_unavailable",
        }
    }
}

impl From<BoundBackgroundStartError> for ExtensionError {
    fn from(err: BoundBackgroundStartError) -> Self {
        ExtensionError::ManifestValidation(format!(
            "background service start refused: {} (bound service must not be started from a path string)",
            err.reason()
        ))
    }
}

impl From<VerifiedAssetError> for BoundBackgroundStartError {
    fn from(err: VerifiedAssetError) -> Self {
        match err {
            VerifiedAssetError::InvalidSnapshot => Self::InvalidSnapshot,
            VerifiedAssetError::NotListed => Self::EntryMismatch,
            VerifiedAssetError::Missing => Self::Missing,
            VerifiedAssetError::Unreadable => Self::Unreadable,
            VerifiedAssetError::IntegrityMismatch => Self::IntegrityMismatch,
            VerifiedAssetError::Unavailable => Self::Unavailable,
        }
    }
}

/// Relative entry path from a resolved service `entry_point` (asset URL or relative), with no
/// filesystem access and no extension-directory string.
pub(crate) fn relative_service_entry_point(entry_point: &str) -> Option<std::path::PathBuf> {
    let relative = if let Some(rest) = entry_point.strip_prefix("/api/extensions/") {
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() == 3 && parts[1] == "assets" {
            parts[2]
        } else {
            return None;
        }
    } else if entry_point.contains("://") || entry_point.starts_with('/') {
        return None;
    } else {
        entry_point
    };
    normalize_relative_asset_path(relative)
}

/// The service with local id `local_id` in `manifest`, only when exactly one matches.
fn unique_service<'a>(manifest: &'a ExtensionManifest, local_id: &str) -> Option<&'a ExtBackgroundService> {
    let mut matches = manifest
        .contributes
        .iter()
        .flat_map(|c| c.background_services.iter())
        .filter(|svc| svc.id == local_id);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

/// True when any service with local id `local_id` in `manifest` declares a runtime.
pub(crate) fn declares_runtime(manifest: &ExtensionManifest, local_id: &str) -> bool {
    manifest
        .contributes
        .iter()
        .flat_map(|c| c.background_services.iter())
        .any(|svc| svc.id == local_id && svc.runtime.is_some())
}

fn is_js_entry(entry_point: &str) -> bool {
    let lower = entry_point.to_ascii_lowercase();
    lower.ends_with(".js") || lower.ends_with(".mjs")
}

/// Run the bound+`files` policy pipeline (module docs) against the retained root, or report that
/// the legacy path applies.
///
/// Never opens `LoadedExtension.directory`. `root` must be the Host-retained directory `File`.
pub fn verify_bound_files_service(
    checked: &SignatureCheckedManifest,
    root: Option<&File>,
    service: &ResolvedBackgroundService,
) -> Result<BoundFilesStart, BoundBackgroundStartError> {
    let local_id = local_background_service_id(service);
    let Some(index) = FilesIndex::from_signed(checked).map_err(|_| BoundBackgroundStartError::InvalidSnapshot)? else {
        // No `files`: legacy — but a declared runtime may never take the legacy path spawn.
        let signed_manifest =
            parse_manifest(checked.raw_bytes()).map_err(|_| BoundBackgroundStartError::InvalidSnapshot)?;
        if declares_runtime(&signed_manifest, local_id) {
            return Err(BoundBackgroundStartError::RuntimeRequiresSignedFiles);
        }
        return Ok(BoundFilesStart::NotApplicable);
    };
    let Some(root) = root else {
        return Err(BoundBackgroundStartError::RootMissing);
    };

    let signed_manifest =
        parse_manifest(checked.raw_bytes()).map_err(|_| BoundBackgroundStartError::InvalidSnapshot)?;
    let signed_service =
        unique_service(&signed_manifest, local_id).ok_or(BoundBackgroundStartError::SignedSnapshotInconclusive)?;

    match signed_service.runtime {
        None => return Err(BoundBackgroundStartError::RuntimeRequired),
        Some(ServiceRuntime::Node) => return Err(BoundBackgroundStartError::RuntimeUnsupported),
        Some(ServiceRuntime::Shell) => return Err(BoundBackgroundStartError::ShellScriptUnsupported),
        Some(ServiceRuntime::Native) => return Err(BoundBackgroundStartError::NativeUnsupported),
        Some(ServiceRuntime::WorkmateJs) => {}
    }
    if !is_js_entry(&signed_service.entry_point) {
        return Err(BoundBackgroundStartError::EntryKindUnsupported);
    }
    if !signed_service.uses.is_empty() {
        return Err(BoundBackgroundStartError::CapabilitiesUnavailable);
    }

    let binding = match bind_background_service_entry_point_digest_from_signed(checked, local_id) {
        Ok(binding) => binding,
        Err(BackgroundServiceEntryPointDigestError::NotPresent) => {
            return Err(BoundBackgroundStartError::IntegrityRequired);
        }
        Err(_) => return Err(BoundBackgroundStartError::SignedSnapshotInconclusive),
    };

    let requested =
        relative_service_entry_point(&service.entry_point).ok_or(BoundBackgroundStartError::EntryMismatch)?;
    let (listed, listed_sri) = index
        .resolve(&requested)
        .map_err(|_| BoundBackgroundStartError::EntryMismatch)?;
    if listed != binding.entry_point() || listed_sri != binding.integrity() {
        return Err(BoundBackgroundStartError::EntryMismatch);
    }

    let asset = read_verified_listed(root, listed, listed_sri)?;
    Ok(BoundFilesStart::Ready(BoundServiceReady {
        bytes: asset.into_bytes(),
        interval_seconds: signed_service.effective_interval_seconds(),
        uses: signed_service.uses.clone(),
    }))
}

/// Map a successful [`BoundFilesStart`] onto the refusal this slice applies: there is no runner,
/// so a `Ready` service is refused (`bound_service_runner_unavailable`), never spawned.
/// `NotApplicable` is not converted; the caller continues with the legacy gate.
pub fn refuse_ready_without_runner(outcome: BoundFilesStart) -> Result<(), BoundBackgroundStartError> {
    match outcome {
        BoundFilesStart::NotApplicable => Ok(()),
        BoundFilesStart::Ready(_) => Err(BoundBackgroundStartError::RunnerUnavailable),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{Value, json};
    use std::collections::{HashMap, HashSet};

    const SCRIPT: &[u8] = b"export async function run(input) { return input; }\n";
    const ENTRY: &str = "services/digest.mjs";

    fn policy_key() -> SigningKey {
        SigningKey::from_bytes(&[43; 32])
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let key = policy_key();
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

    /// Signed manifest with `files` listing `entry` (SCRIPT bytes) and one service.
    fn manifest_with(entry: &str, service_extra: Value, shell: Option<bool>) -> Vec<u8> {
        let mut service = json!({"id": "worker", "entryPoint": entry});
        service
            .as_object_mut()
            .unwrap()
            .extend(service_extra.as_object().unwrap().clone());
        let mut m = json!({
            "name": "demo.plugin", "version": "1.0.0",
            "files": [{"path": entry, "integrity": sri_sha512(SCRIPT)}],
            "contributes": {"backgroundServices": [service]}
        });
        if let Some(shell) = shell {
            m["permissions"] = json!({"shell": shell});
        }
        serde_json::to_vec(&m).unwrap()
    }

    fn js_manifest() -> Vec<u8> {
        manifest_with(ENTRY, json!({"runtime": "workmate-js"}), None)
    }

    /// A signed manifest whose service lists `uses`, plus a valid S2 capabilities block.
    /// Start is still refused with `bound_service_capabilities_unavailable` (consent is S3).
    fn manifest_with_uses(entry: &str, kinds: &[&str], shell: Option<bool>) -> Vec<u8> {
        use crate::capabilities::{capability_fingerprint, minimal_capabilities_for, permissions_covering};
        let rust_kinds: Vec<ServiceCapabilityKind> = kinds
            .iter()
            .map(|kind| match *kind {
                "network" => ServiceCapabilityKind::Network,
                "files" => ServiceCapabilityKind::Files,
                "events" => ServiceCapabilityKind::Events,
                "storage" => ServiceCapabilityKind::Storage,
                "secrets" => ServiceCapabilityKind::Secrets,
                other => panic!("unknown kind {other}"),
            })
            .collect();
        let caps = minimal_capabilities_for(&rust_kinds);
        let fp = capability_fingerprint(&caps).unwrap();
        let perms = permissions_covering(&caps).unwrap();
        let mut manifest: Value = serde_json::from_slice(&manifest_with(
            entry,
            json!({"runtime": "workmate-js", "uses": kinds}),
            shell,
        ))
        .unwrap();
        manifest["capabilities"] = serde_json::to_value(&caps).unwrap();
        manifest["capabilityFingerprint"] = json!(fp);
        let mut perms_json = serde_json::to_value(&perms).unwrap();
        if let Some(existing) = manifest.get("permissions").and_then(Value::as_object)
            && let Some(obj) = perms_json.as_object_mut()
        {
            for (key, value) in existing {
                obj.entry(key.clone()).or_insert(value.clone());
            }
        }
        manifest["permissions"] = perms_json;
        serde_json::to_vec(&manifest).unwrap()
    }

    fn no_files_manifest(extra: Value) -> Vec<u8> {
        let mut service = json!({"id": "worker", "entryPoint": "services/worker.sh"});
        service
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::to_vec(&json!({
            "name": "demo.plugin", "version": "1.0.0", "permissions": {"shell": true},
            "contributes": {"backgroundServices": [service]}
        }))
        .unwrap()
    }

    fn resolved(path: &str) -> ResolvedBackgroundService {
        ResolvedBackgroundService {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-worker".into(),
            entry_point: path.into(),
            description: None,
        }
    }

    fn root_with(entry: &str, body: &[u8]) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(entry);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        let root = File::open(dir.path()).unwrap();
        (dir, root)
    }

    fn verify(raw: &[u8], root: Option<&File>, entry: &str) -> Result<BoundFilesStart, BoundBackgroundStartError> {
        verify_bound_files_service(&checked(raw), root, &resolved(entry))
    }

    #[test]
    fn snapshot_without_files_and_without_runtime_is_not_applicable() {
        let (_d, root) = root_with("services/worker.sh", b"#!/bin/sh\n");
        assert_eq!(
            verify(&no_files_manifest(json!({})), Some(&root), "services/worker.sh"),
            Ok(BoundFilesStart::NotApplicable)
        );
    }

    #[test]
    fn a_declared_runtime_never_takes_the_legacy_path_without_signed_files() {
        let (_d, root) = root_with("services/worker.sh", b"#!/bin/sh\n");
        for runtime in ["workmate-js", "node", "shell", "native"] {
            assert_eq!(
                verify(
                    &no_files_manifest(json!({"runtime": runtime})),
                    Some(&root),
                    "services/worker.sh"
                ),
                Err(BoundBackgroundStartError::RuntimeRequiresSignedFiles),
                "{runtime}"
            );
        }
    }

    #[test]
    fn workmate_js_is_verified_and_returns_the_exact_bytes_without_any_shell_permission() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        for shell in [None, Some(false), Some(true)] {
            let raw = manifest_with(ENTRY, json!({"runtime": "workmate-js"}), shell);
            match verify(&raw, Some(&root), ENTRY).unwrap() {
                BoundFilesStart::Ready(ready) => {
                    assert_eq!(ready.bytes(), SCRIPT, "{shell:?}");
                    assert_eq!(ready.byte_len(), SCRIPT.len());
                    assert_eq!(ready.interval_seconds(), 60);
                    assert!(ready.uses().is_empty());
                }
                other => panic!("expected Ready, got {other:?}"),
            }
        }
        // Asset-URL form of the same entry.
        assert!(matches!(
            verify(
                &js_manifest(),
                Some(&root),
                "/api/extensions/demo.plugin/assets/services/digest.mjs"
            ),
            Ok(BoundFilesStart::Ready(_))
        ));
        // `.js` and upper-case extensions are accepted too.
        for entry in ["services/a.js", "services/B.MJS"] {
            let (_d, root) = root_with(entry, SCRIPT);
            let raw = manifest_with(entry, json!({"runtime": "workmate-js"}), None);
            assert!(
                matches!(verify(&raw, Some(&root), entry), Ok(BoundFilesStart::Ready(_))),
                "{entry}"
            );
        }
    }

    #[test]
    fn interval_is_clamped_and_defaults_to_sixty() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        for (declared, expected) in [
            (json!({}), 60),
            (json!({"intervalSeconds": 0}), 10),
            (json!({"intervalSeconds": 1}), 10),
            (json!({"intervalSeconds": 10}), 10),
            (json!({"intervalSeconds": 120}), 120),
            (json!({"intervalSeconds": 86400}), 86400),
            (json!({"intervalSeconds": 86401}), 86400),
            (json!({"intervalSeconds": u64::MAX}), 86400),
        ] {
            let mut extra = json!({"runtime": "workmate-js"});
            extra
                .as_object_mut()
                .unwrap()
                .extend(declared.as_object().unwrap().clone());
            let raw = manifest_with(ENTRY, extra, None);
            match verify(&raw, Some(&root), ENTRY).unwrap() {
                BoundFilesStart::Ready(r) => assert_eq!(r.interval_seconds(), expected, "{declared}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn runtime_matrix_refuses_everything_but_workmate_js_without_reading_the_entry() {
        // Entry bytes are valid and SRI-listed in every case; the refusal is by runtime alone.
        let cases: [(Option<&str>, &str, BoundBackgroundStartError); 4] = [
            (None, ENTRY, BoundBackgroundStartError::RuntimeRequired),
            (Some("node"), ENTRY, BoundBackgroundStartError::RuntimeUnsupported),
            (
                Some("shell"),
                "services/worker.sh",
                BoundBackgroundStartError::ShellScriptUnsupported,
            ),
            (
                Some("native"),
                "services/worker",
                BoundBackgroundStartError::NativeUnsupported,
            ),
        ];
        for (runtime, entry, expected) in cases {
            let (_d, root) = root_with(entry, SCRIPT);
            let extra = runtime.map_or(json!({}), |r| json!({"runtime": r}));
            // permissions.shell = true must not rescue a refused runtime.
            let raw = manifest_with(entry, extra, Some(true));
            assert_eq!(verify(&raw, Some(&root), entry), Err(expected), "{runtime:?}");
            // Same verdict with the entry file absent: nothing was read.
            let empty = tempfile::tempdir().unwrap();
            let empty_root = File::open(empty.path()).unwrap();
            assert_eq!(
                verify(&raw, Some(&empty_root), entry),
                Err(expected),
                "{runtime:?} (no file)"
            );
        }
    }

    #[test]
    fn workmate_js_entry_must_be_a_js_module() {
        for entry in [
            "services/worker.sh",
            "services/run.py",
            "services/a.txt",
            "services/js",
            "services/mjs.txt",
            "services/x.cjs",
        ] {
            let (_d, root) = root_with(entry, SCRIPT);
            let raw = manifest_with(entry, json!({"runtime": "workmate-js"}), None);
            assert_eq!(
                verify(&raw, Some(&root), entry),
                Err(BoundBackgroundStartError::EntryKindUnsupported),
                "{entry}"
            );
        }
    }

    #[test]
    fn listing_capability_kinds_is_refused_until_capabilities_exist() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        for kind in ["network", "files", "events", "storage", "secrets"] {
            let raw = manifest_with_uses(ENTRY, &[kind], None);
            assert_eq!(
                verify(&raw, Some(&root), ENTRY),
                Err(BoundBackgroundStartError::CapabilitiesUnavailable),
                "{kind}"
            );
        }
    }

    #[test]
    fn files_declared_without_a_root_is_refused_not_a_path_fallback() {
        assert_eq!(
            verify(&js_manifest(), None, ENTRY),
            Err(BoundBackgroundStartError::RootMissing)
        );
    }

    #[test]
    fn tampered_or_missing_or_symlinked_entry_is_refused() {
        let (d, root) = root_with(ENTRY, SCRIPT);
        let raw = js_manifest();
        std::fs::write(d.path().join(ENTRY), b"export const evil = 1;\n").unwrap();
        assert_eq!(
            verify(&raw, Some(&root), ENTRY),
            Err(BoundBackgroundStartError::IntegrityMismatch)
        );
        std::fs::remove_file(d.path().join(ENTRY)).unwrap();
        assert_eq!(
            verify(&raw, Some(&root), ENTRY),
            Err(BoundBackgroundStartError::Missing)
        );
        // A symlink at the entry (even to the right bytes) is never followed.
        std::fs::write(d.path().join("real.mjs"), SCRIPT).unwrap();
        std::os::unix::fs::symlink("../real.mjs", d.path().join(ENTRY)).unwrap();
        assert!(matches!(
            verify(&raw, Some(&root), ENTRY),
            Err(BoundBackgroundStartError::Unreadable | BoundBackgroundStartError::Missing)
        ));
    }

    #[test]
    fn unlisted_or_mismatched_request_path_is_refused() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        let raw = js_manifest();
        for requested in ["other.mjs", "../services/digest.mjs", "services/../services/x.mjs"] {
            assert_eq!(
                verify(&raw, Some(&root), requested),
                Err(BoundBackgroundStartError::EntryMismatch),
                "{requested}"
            );
        }
    }

    #[test]
    fn unknown_or_ambiguous_service_in_the_snapshot_is_refused() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        // Different local id in the snapshot.
        let raw = serde_json::to_vec(&json!({
            "name": "demo.plugin", "version": "1.0.0",
            "files": [{"path": ENTRY, "integrity": sri_sha512(SCRIPT)}],
            "contributes": {"backgroundServices": [{"id": "other", "entryPoint": ENTRY, "runtime": "workmate-js"}]}
        }))
        .unwrap();
        assert_eq!(
            verify(&raw, Some(&root), ENTRY),
            Err(BoundBackgroundStartError::SignedSnapshotInconclusive)
        );
        // Two services with the same id.
        let raw = serde_json::to_vec(&json!({
            "name": "demo.plugin", "version": "1.0.0",
            "files": [{"path": ENTRY, "integrity": sri_sha512(SCRIPT)}],
            "contributes": {"backgroundServices": [
                {"id": "worker", "entryPoint": ENTRY, "runtime": "workmate-js"},
                {"id": "worker", "entryPoint": ENTRY, "runtime": "workmate-js"}
            ]}
        }))
        .unwrap();
        assert_eq!(
            verify(&raw, Some(&root), ENTRY),
            Err(BoundBackgroundStartError::SignedSnapshotInconclusive)
        );
    }

    #[test]
    fn relative_entry_accepts_asset_url_and_rejects_escape() {
        assert_eq!(
            relative_service_entry_point("/api/extensions/demo.plugin/assets/services/worker.sh")
                .unwrap()
                .as_os_str(),
            "services/worker.sh"
        );
        assert!(relative_service_entry_point("../x").is_none());
        assert!(relative_service_entry_point("/etc/passwd").is_none());
        assert!(relative_service_entry_point("https://x.test/a.sh").is_none());
    }

    #[test]
    fn reasons_are_fixed_unique_and_never_include_digests_or_paths() {
        use BoundBackgroundStartError as E;
        let all = [
            E::RootMissing,
            E::InvalidSnapshot,
            E::SignedSnapshotInconclusive,
            E::IntegrityRequired,
            E::EntryMismatch,
            E::Missing,
            E::Unreadable,
            E::IntegrityMismatch,
            E::Unavailable,
            E::RuntimeRequired,
            E::RuntimeUnsupported,
            E::ShellScriptUnsupported,
            E::NativeUnsupported,
            E::EntryKindUnsupported,
            E::CapabilitiesUnavailable,
            E::RuntimeRequiresSignedFiles,
            E::RunnerUnavailable,
        ];
        let reasons: HashSet<&str> = all.iter().map(|e| e.reason()).collect();
        assert_eq!(reasons.len(), all.len());
        for e in all {
            assert!(e.reason().starts_with("bound_service_"));
            let text = ExtensionError::from(e).to_string();
            assert!(text.contains(e.reason()));
            assert!(!text.contains("sha512-") && !text.contains(".mjs") && !text.contains("services/"));
        }
    }

    #[test]
    fn ready_debug_never_prints_the_source_bytes() {
        let (_d, root) = root_with(ENTRY, SCRIPT);
        let out = verify(&js_manifest(), Some(&root), ENTRY).unwrap();
        let text = format!("{out:?}");
        assert!(text.contains("byte_len") && !text.contains("export async"));
    }

    fn loaded(raw: &[u8], dir: &std::path::Path) -> crate::types::LoadedExtension {
        use crate::manifest::parse_manifest;
        use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};
        LoadedExtension {
            manifest: parse_manifest(raw).unwrap(),
            directory: dir.display().to_string(),
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

    fn write_executable(path: &std::path::Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A decoy executable under a `LoadedExtension.directory` that would run under the legacy
    /// path spawn. Returns `(dir, marker)`.
    fn decoy(entry: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("spawned");
        write_executable(
            &dir.path().join(entry),
            &format!("#!/bin/sh\necho hi > {}\n", marker.display()),
        );
        (dir, marker)
    }

    #[tokio::test]
    async fn manager_refuses_a_verified_workmate_js_service_without_a_runner_and_never_spawns() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        let (_root_dir, root) = root_with(ENTRY, SCRIPT);
        let (decoy_dir, marker) = decoy(ENTRY);
        let raw = js_manifest();
        let store = SignedRootBindingStore::new();
        store.install("demo.plugin", checked(&raw), root).unwrap();
        let ext = loaded(&raw, decoy_dir.path());
        let manager = BackgroundServiceManager::new();
        let err = manager
            .start_with_binding("alice", &ext, &resolved(ENTRY), &store)
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("bound_service_runner_unavailable"), "{text}");
        assert!(!text.contains(".mjs") && !text.contains("sha512-"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists(), "decoy directory entry must not run");
        assert!(
            !manager
                .is_running("alice", "demo.plugin", "ext-demo.plugin-worker")
                .await
        );
    }

    #[tokio::test]
    async fn manager_refuses_every_other_bound_shape_without_a_spawn() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        // (entry, extra service fields, expected reason)
        let cases: [(&str, Value, &str); 6] = [
            ("services/worker.sh", json!({}), "bound_service_runtime_required"),
            (
                "services/worker.sh",
                json!({"runtime": "shell"}),
                "bound_service_shell_script_unsupported",
            ),
            (
                "services/worker",
                json!({"runtime": "native"}),
                "bound_service_native_unsupported",
            ),
            (ENTRY, json!({"runtime": "node"}), "bound_service_runtime_unsupported"),
            (
                "services/worker.sh",
                json!({"runtime": "workmate-js"}),
                "bound_service_entry_kind_unsupported",
            ),
            (
                ENTRY,
                json!({"runtime": "workmate-js", "uses": ["network"]}),
                "bound_service_capabilities_unavailable",
            ),
        ];
        for (entry, extra, reason) in cases {
            let (_root_dir, root) = root_with(entry, SCRIPT);
            let (decoy_dir, marker) = decoy(entry);
            // Old-style loaded permission: shell = true must not matter either way.
            let raw = if extra.get("uses").is_some() {
                manifest_with_uses(entry, &["network"], Some(true))
            } else {
                manifest_with(entry, extra, Some(true))
            };
            let store = SignedRootBindingStore::new();
            store.install("demo.plugin", checked(&raw), root).unwrap();
            let ext = loaded(&raw, decoy_dir.path());
            let err = BackgroundServiceManager::new()
                .start_with_binding("alice", &ext, &resolved(entry), &store)
                .await
                .unwrap_err();
            assert!(err.to_string().contains(reason), "{entry}: {err}");
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(!marker.exists(), "{entry}: nothing may spawn");
        }
    }

    #[tokio::test]
    async fn tampered_retained_root_is_refused_before_anything_else_runs() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        let (root_dir, root) = root_with(ENTRY, SCRIPT);
        let (decoy_dir, marker) = decoy(ENTRY);
        let raw = js_manifest();
        let store = SignedRootBindingStore::new();
        store.install("demo.plugin", checked(&raw), root).unwrap();
        let ext = loaded(&raw, decoy_dir.path());
        std::fs::write(root_dir.path().join(ENTRY), b"export const evil = 1;\n").unwrap();
        let err = BackgroundServiceManager::new()
            .start_with_binding("alice", &ext, &resolved(ENTRY), &store)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_integrity_mismatch"));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn unbound_extension_declaring_a_runtime_never_path_spawns_but_undeclared_still_does() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        // Declared runtime, no binding at all: refused, the executable decoy does not run.
        let (dir, marker) = decoy("services/worker.sh");
        let raw = no_files_manifest(json!({"runtime": "workmate-js"}));
        let ext = loaded(&raw, dir.path());
        let err = BackgroundServiceManager::new()
            .start_with_binding(
                "alice",
                &ext,
                &resolved("services/worker.sh"),
                &SignedRootBindingStore::new(),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_runtime_requires_signed_files"));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!marker.exists());

        // Bound snapshot without `files` but with a runtime: same refusal.
        let (_root_dir, root) = root_with("services/worker.sh", b"x");
        let store = SignedRootBindingStore::new();
        store.install("demo.plugin", checked(&raw), root).unwrap();
        let err = BackgroundServiceManager::new()
            .start_with_binding("alice", &ext, &resolved("services/worker.sh"), &store)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_runtime_requires_signed_files"));
        assert!(!marker.exists());

        // Legacy: no runtime, no binding -> unchanged path spawn (not "verified").
        let raw = no_files_manifest(json!({}));
        let ext = loaded(&raw, dir.path());
        let manager = BackgroundServiceManager::new();
        let status = manager
            .start_with_binding(
                "alice",
                &ext,
                &resolved("services/worker.sh"),
                &SignedRootBindingStore::new(),
            )
            .await
            .unwrap();
        assert!(status.running);
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
        while !marker.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(marker.exists(), "legacy undeclared service must still spawn");
        let _ = manager.stop("alice", "demo.plugin", "ext-demo.plugin-worker").await;
    }

    #[tokio::test]
    async fn files_snapshot_without_root_does_not_path_spawn() {
        use crate::host_runtime::BackgroundServiceManager;

        let (dir, marker) = decoy(ENTRY);
        let raw = js_manifest();
        let ext = loaded(&raw, dir.path());
        let err = BackgroundServiceManager::new()
            .start_with_signed("alice", &ext, &resolved(ENTRY), Some(&checked(&raw)), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_root_missing"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists());
    }
}

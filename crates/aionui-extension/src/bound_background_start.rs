//! Bound+`files` background-service start (M9 E13 slice I).
//!
//! When the Host has a retained signed root whose snapshot declares `files`, a service may
//! **not** be started by a path string (`LoadedExtension.directory`, a canonicalized script
//! path, or anything else). The honest steps are:
//!
//! 1. require a signed `entryPoint` / `entryPointIntegrity` for this service (inheritance from
//!    `files` counts; absence is a refusal, not a legacy spawn);
//! 2. read that exact listed file through the retained root (`openat(O_NOFOLLOW)`, SRI of the
//!    bytes read — the same reader as verified asset serve);
//! 3. start the process from those verified bytes via a same-FD `fexecve` / `execveat`.
//!
//! Step 3 is **not implemented in this slice**. An honest FD-based exec cannot be done through
//! the existing `CmdBuilder` / `tokio::process::Command` path-spawn stack; macOS has no `fexecve`;
//! and `fexecve` of a shebang script (the current `worker.sh` shape) makes the kernel pass a
//! procfs path to an interpreter (Node/shell), which is neither portable nor a clearly safe
//! exec policy. This module therefore **verifies and refuses** (`FdExecUnavailable`). It never
//! falls back to a path spawn. Leftover decisions (later slice): Linux `memfd`+`execveat` of a verified ELF, shebang/Node
//! policy, a macOS primitive, and child cwd/env that does not use `LoadedExtension.directory`.
//!
//! Snapshots without `files`, and extensions with no binding, are [`BoundFilesStart::NotApplicable`]:
//! the existing start gate + path spawn stay as they are today.
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
use crate::types::{ExtPermissions, ResolvedBackgroundService};

use super::background_service_start_gate::local_background_service_id;

/// Outcome of the bound+`files` start check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundFilesStart {
    /// No `files` in the signed snapshot: the legacy gate + path spawn applies.
    NotApplicable,
    /// The listed entry was read from the retained root and matched its SRI. FD-exec is not
    /// available in this slice; the caller must refuse (and must not path-spawn).
    Verified { byte_len: usize },
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
    /// The signed snapshot does not grant `permissions.shell`.
    ShellPermissionDenied,
    /// Entry was verified; no honest FD-based exec is available. Not a spawn.
    FdExecUnavailable,
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
            Self::ShellPermissionDenied => "bound_service_shell_permission_denied",
            Self::FdExecUnavailable => "bound_service_fd_exec_unavailable",
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

/// Verify a bound+`files` service against the retained root, or report that the legacy path applies.
///
/// Never opens `LoadedExtension.directory`. `root` must be the Host-retained directory `File`.
pub fn verify_bound_files_service(
    checked: &SignatureCheckedManifest,
    root: Option<&File>,
    service: &ResolvedBackgroundService,
) -> Result<BoundFilesStart, BoundBackgroundStartError> {
    let Some(index) = FilesIndex::from_signed(checked).map_err(|_| BoundBackgroundStartError::InvalidSnapshot)? else {
        return Ok(BoundFilesStart::NotApplicable);
    };
    let Some(root) = root else {
        return Err(BoundBackgroundStartError::RootMissing);
    };

    let signed_manifest =
        parse_manifest(checked.raw_bytes()).map_err(|_| BoundBackgroundStartError::InvalidSnapshot)?;
    let perms = signed_manifest
        .permissions
        .clone()
        .unwrap_or_else(ExtPermissions::default);
    if perms.shell != Some(true) {
        return Err(BoundBackgroundStartError::ShellPermissionDenied);
    }

    let local_id = local_background_service_id(service);
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
    Ok(BoundFilesStart::Verified {
        byte_len: asset.bytes().len(),
    })
}

/// Refuse after a successful [`BoundFilesStart::Verified`] — no path spawn, no exec.
pub fn refuse_fd_exec_unavailable() -> BoundBackgroundStartError {
    BoundBackgroundStartError::FdExecUnavailable
}

/// Map a verified-or-refused bound start onto an [`ExtensionError`]. `NotApplicable` is not
/// converted here; the caller continues with the legacy gate.
pub fn refuse_verified_without_fd_exec(verified: BoundFilesStart) -> Result<(), BoundBackgroundStartError> {
    match verified {
        BoundFilesStart::NotApplicable => Ok(()),
        BoundFilesStart::Verified { .. } => Err(BoundBackgroundStartError::FdExecUnavailable),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    const SCRIPT: &[u8] = b"#!/bin/sh\necho hi\n";

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

    fn files_manifest(integrity: Option<&str>, shell: bool, extra_files: &str) -> Vec<u8> {
        let sri = sri_sha512(SCRIPT);
        let integ = match integrity {
            Some("") => String::new(),
            Some(i) => format!(r#""entryPointIntegrity":"{i}","#),
            None => String::new(),
        };
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","permissions":{{"shell":{shell}}},"files":[{{"path":"services/worker.sh","integrity":"{sri}"}}{extra_files}],"contributes":{{"backgroundServices":[{{"id":"worker","entryPoint":"services/worker.sh",{integ}"description":"w"}}]}}}}"#
        )
        .into_bytes()
    }

    fn no_files_manifest() -> Vec<u8> {
        br#"{"name":"demo.plugin","version":"1.0.0","permissions":{"shell":true},"contributes":{"backgroundServices":[{"id":"worker","entryPoint":"services/worker.sh"}]}}"#.to_vec()
    }

    fn resolved(path: &str) -> ResolvedBackgroundService {
        ResolvedBackgroundService {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-worker".into(),
            entry_point: path.into(),
            description: None,
        }
    }

    fn root_with_script(body: &[u8]) -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("services")).unwrap();
        std::fs::write(dir.path().join("services/worker.sh"), body).unwrap();
        let root = File::open(dir.path()).unwrap();
        (dir, root)
    }

    #[test]
    fn snapshot_without_files_is_not_applicable() {
        let (_d, root) = root_with_script(SCRIPT);
        assert_eq!(
            verify_bound_files_service(
                &checked(&no_files_manifest()),
                Some(&root),
                &resolved("services/worker.sh")
            ),
            Ok(BoundFilesStart::NotApplicable)
        );
    }

    #[test]
    fn inherited_integrity_is_verified_against_the_retained_root() {
        let (_d, root) = root_with_script(SCRIPT);
        // No per-entry integrity: inherited from `files`.
        let c = checked(&files_manifest(None, true, ""));
        match verify_bound_files_service(&c, Some(&root), &resolved("services/worker.sh")).unwrap() {
            BoundFilesStart::Verified { byte_len } => assert_eq!(byte_len, SCRIPT.len()),
            other => panic!("expected Verified, got {other:?}"),
        }
        // Asset-URL form of the same entry is accepted and verified.
        let url = "/api/extensions/demo.plugin/assets/services/worker.sh";
        assert!(matches!(
            verify_bound_files_service(&c, Some(&root), &resolved(url)),
            Ok(BoundFilesStart::Verified { .. })
        ));
        assert_eq!(
            refuse_verified_without_fd_exec(
                verify_bound_files_service(&c, Some(&root), &resolved("services/worker.sh")).unwrap()
            ),
            Err(BoundBackgroundStartError::FdExecUnavailable)
        );
    }

    #[test]
    fn files_declared_without_a_root_is_refused_not_a_path_fallback() {
        let c = checked(&files_manifest(None, true, ""));
        assert_eq!(
            verify_bound_files_service(&c, None, &resolved("services/worker.sh")),
            Err(BoundBackgroundStartError::RootMissing)
        );
    }

    #[test]
    fn tampered_or_missing_entry_is_refused() {
        let (d, root) = root_with_script(SCRIPT);
        let c = checked(&files_manifest(None, true, ""));
        std::fs::write(d.path().join("services/worker.sh"), b"#!/bin/sh\nevil\n").unwrap();
        assert_eq!(
            verify_bound_files_service(&c, Some(&root), &resolved("services/worker.sh")),
            Err(BoundBackgroundStartError::IntegrityMismatch)
        );
        std::fs::remove_file(d.path().join("services/worker.sh")).unwrap();
        assert_eq!(
            verify_bound_files_service(&c, Some(&root), &resolved("services/worker.sh")),
            Err(BoundBackgroundStartError::Missing)
        );
    }

    #[test]
    fn unlisted_or_mismatched_request_path_is_refused() {
        let (_d, root) = root_with_script(SCRIPT);
        let c = checked(&files_manifest(None, true, ""));
        assert_eq!(
            verify_bound_files_service(&c, Some(&root), &resolved("other.sh")),
            Err(BoundBackgroundStartError::EntryMismatch)
        );
        assert_eq!(
            verify_bound_files_service(&c, Some(&root), &resolved("../services/worker.sh")),
            Err(BoundBackgroundStartError::EntryMismatch)
        );
    }

    #[test]
    fn signed_snapshot_without_shell_permission_is_refused() {
        let (_d, root) = root_with_script(SCRIPT);
        let c = checked(&files_manifest(None, false, ""));
        assert_eq!(
            verify_bound_files_service(&c, Some(&root), &resolved("services/worker.sh")),
            Err(BoundBackgroundStartError::ShellPermissionDenied)
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
    fn reasons_are_fixed_and_never_include_digests() {
        let err: ExtensionError = BoundBackgroundStartError::FdExecUnavailable.into();
        let text = err.to_string();
        assert!(text.contains("bound_service_fd_exec_unavailable"));
        assert!(!text.contains("sha512-"));
        assert!(!text.contains("worker.sh"));
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
        std::fs::write(path, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[tokio::test]
    async fn manager_verifies_bound_service_and_refuses_exec_without_path_spawn() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        let (root_dir, root) = root_with_script(SCRIPT);
        // A tempting executable in a *different* LoadedExtension.directory would spawn under the
        // legacy path. The bound path must not touch it.
        let decoy = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(decoy.path().join("services")).unwrap();
        let marker = decoy.path().join("spawned");
        write_executable(
            &decoy.path().join("services/worker.sh"),
            &format!(
                "#!/bin/sh
echo hi > {}
",
                marker.display()
            ),
        );
        // Also place a runnable copy under the retained root (the honest file).
        write_executable(
            &root_dir.path().join("services/worker.sh"),
            std::str::from_utf8(SCRIPT).unwrap(),
        );

        let raw = files_manifest(None, true, "");
        let snapshot = checked(&raw);
        let store = SignedRootBindingStore::new();
        store.install("demo.plugin", snapshot, root).unwrap();
        let ext = loaded(&raw, decoy.path());
        let manager = BackgroundServiceManager::new();
        let err = manager
            .start_with_binding("alice", &ext, &resolved("services/worker.sh"), &store)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_fd_exec_unavailable"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists(), "decoy directory script must not run");
        assert!(
            !manager
                .is_running("alice", "demo.plugin", "ext-demo.plugin-worker")
                .await
        );

        // Tampered root entry: integrity mismatch, still no spawn.
        std::fs::write(
            root_dir.path().join("services/worker.sh"),
            b"#!/bin/sh
evil
",
        )
        .unwrap();
        let err = manager
            .start_with_binding("alice", &ext, &resolved("services/worker.sh"), &store)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_integrity_mismatch"));
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn unbound_undeclared_service_still_path_spawns() {
        use crate::host_runtime::BackgroundServiceManager;
        use crate::signed_root_binding::SignedRootBindingStore;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("services")).unwrap();
        let marker = dir.path().join("spawned");
        write_executable(
            &dir.path().join("services/worker.sh"),
            &format!(
                "#!/bin/sh
echo hi > {}
",
                marker.display()
            ),
        );
        let raw = no_files_manifest();
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

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("services")).unwrap();
        let marker = dir.path().join("spawned");
        write_executable(
            &dir.path().join("services/worker.sh"),
            &format!("#!/bin/sh\necho hi > {}\n", marker.display()),
        );
        let raw = files_manifest(None, true, "");
        let ext = loaded(&raw, dir.path());
        let err = BackgroundServiceManager::new()
            .start_with_signed(
                "alice",
                &ext,
                &resolved("services/worker.sh"),
                Some(&checked(&raw)),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bound_service_root_missing"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists());
    }
}

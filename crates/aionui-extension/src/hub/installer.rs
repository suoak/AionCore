use std::path::Path;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::error::ExtensionError;
use crate::manifest::{parse_manifest, validate_manifest};
use crate::registry::ExtensionRegistry;
use crate::resolvers::resolve_extension_contributions;
use crate::types::{ExtensionSource, ExtensionState, LoadedExtension};

use super::curated::evaluate_install;
use super::host_package_install::{UninstallOutcome, encode_component};
use super::host_signed_install::{HostSignedInstall, SignedPackageError};
use super::index_manager::{HubIndexEntry, HubIndexManager};

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Outcome of a Hub install/update/uninstall operation.
#[derive(Debug, Clone)]
pub struct HubResult {
    pub success: bool,
    pub msg: Option<String>,
}

impl HubResult {
    fn ok() -> Self {
        Self {
            success: true,
            msg: None,
        }
    }

    fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            msg: Some(msg.into()),
        }
    }
}

/// Info about an available update.
#[derive(Debug, Clone)]
pub struct HubUpdateInfo {
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
}

// ---------------------------------------------------------------------------
// HubInstaller
// ---------------------------------------------------------------------------

/// Handles extension installation, update, uninstall, and verification.
///
/// For this phase, remote downloading is a stub — extensions must already
/// be present in the Hub directory or have a bundled flag. The installer
/// verifies the manifest and contributions, then triggers a hot reload.
#[derive(Clone)]
pub struct HubInstaller {
    index_manager: HubIndexManager,
    registry: ExtensionRegistry,
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Host signed-package install context; `None` (no signature policy) keeps today's flow exactly.
    signed: Option<HostSignedInstall>,
}

impl HubInstaller {
    pub fn new(index_manager: HubIndexManager, registry: ExtensionRegistry) -> Self {
        let broadcaster = registry.event_broadcaster();
        Self {
            index_manager,
            registry,
            broadcaster,
            signed: None,
        }
    }

    /// Enable the signed local-package path (see [`HostSignedInstall`]). Without this call the
    /// installer behaves exactly as before.
    pub fn with_signed_install(mut self, signed: Option<HostSignedInstall>) -> Self {
        self.signed = signed;
        self
    }

    /// True when a signed local package for `entry` is installed through the Host layout.
    pub fn signed_install_enabled(&self) -> bool {
        self.signed.is_some()
    }

    /// Install an extension from the Hub by name.
    ///
    /// Flow: look up in index → verify the extension directory exists →
    /// validate manifest → verify contributions → trigger hot reload.
    pub async fn install(&self, name: &str) -> HubResult {
        info!(name, "hub: installing extension");
        self.broadcast_state_changed(name, "installing", None);

        let entry = match self.index_manager.get_extension(name) {
            Some(e) => e,
            None => {
                let error = format!("Extension '{name}' not found in hub index");
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                return HubResult::err(error);
            }
        };

        if let Some(result) = self.refuse_builtin_name(&entry, name) {
            return result;
        }

        // Signed local package (only with a configured signature policy): verified install or a
        // refusal; a failure never falls back to the unsigned directory flow below.
        if let Some(result) = self.try_signed_package(&entry, name).await {
            return result;
        }

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(&entry.name);

        // For now, the extension directory must already exist (no remote download).
        // Future: download from entry.download_url and extract.
        if !ext_dir.exists() {
            warn!(name, dir = %ext_dir.display(), "hub: extension directory not found");
            let error = hub_message("Extension package not found", HUB_PACKAGE_NOT_FOUND);
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        // M9 E13 curated marketplace gate (D13-1): curated/allowlist + optional sign.
        // Soft gate only — does NOT retain SignatureCheckedManifest. HubInstaller holds
        // path strings only and must NOT File::open(ext_dir) to invent a trusted root.
        // Host layers that already have check_signed_manifest + a Host-opened directory
        // File should call hub::try_retain_curated_signed_root / HostRuntime::try_retain_*.
        let manifest_path = ext_dir.join(EXTENSION_MANIFEST_FILE);
        let manifest_bytes = std::fs::read(&manifest_path).ok();
        if let Err(gate) = evaluate_install(&entry, manifest_bytes.as_deref()) {
            let error = format!("{gate} ({})", gate.code());
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if let Err(e) = self.verify_installation(&ext_dir) {
            warn!(name, error = %e, "hub: installation verification failed");
            let error = hub_message("Installation verification failed", HUB_VERIFICATION_FAILED);
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        // Trigger hot reload to pick up the new extension.
        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "installed", None);

        info!(name, "hub: extension installed successfully");
        HubResult::ok()
    }

    /// Retry a previously failed installation.
    pub async fn retry_install(&self, name: &str) -> HubResult {
        debug!(name, "hub: retrying installation");
        self.install(name).await
    }

    /// Update an installed extension to the latest version from the index.
    ///
    /// For this phase, update is equivalent to re-verifying the existing
    /// directory (which may have been updated externally) and hot-reloading.
    pub async fn update(&self, name: &str) -> HubResult {
        info!(name, "hub: updating extension");
        self.broadcast_state_changed(name, "updating", None);

        let entry = match self.index_manager.get_extension(name) {
            Some(e) => e,
            None => {
                let error = format!("Extension '{name}' not found in hub index");
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                return HubResult::err(error);
            }
        };

        if let Some(result) = self.refuse_builtin_name(&entry, name) {
            return result;
        }

        if let Some(result) = self.try_signed_package(&entry, name).await {
            return result;
        }

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(&entry.name);

        if !ext_dir.exists() {
            warn!(name, dir = %ext_dir.display(), "hub: update target not installed");
            let error = hub_message("Extension not installed", HUB_NOT_INSTALLED);
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        // Soft gate only; signed-root retain requires Host File — see try_retain_curated_signed_root.
        let manifest_path = ext_dir.join(EXTENSION_MANIFEST_FILE);
        let manifest_bytes = std::fs::read(&manifest_path).ok();
        if let Err(gate) = evaluate_install(&entry, manifest_bytes.as_deref()) {
            let error = format!("{gate} ({})", gate.code());
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if let Err(e) = self.verify_installation(&ext_dir) {
            warn!(name, error = %e, "hub: update verification failed");
            let error = hub_message("Update verification failed", HUB_VERIFICATION_FAILED);
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "installed", None);

        info!(name, "hub: extension updated successfully");
        HubResult::ok()
    }

    /// Uninstall an extension by removing its directory and hot-reloading.
    pub async fn uninstall(&self, name: &str) -> HubResult {
        if let Err(msg) = validate_hub_name(name) {
            self.broadcast_state_changed(name, "failed", Some(msg.clone()));
            return HubResult::err(msg);
        }

        info!(name, "hub: uninstalling extension");

        // Host signed-layout install first (fail closed: marker, binding, then tree).
        let mut removed_signed = false;
        if let Some(signed) = self.signed.clone() {
            let owned = name.to_owned();
            match tokio::task::spawn_blocking(move || signed.uninstall(&owned)).await {
                Ok(Ok(UninstallOutcome::Removed)) => removed_signed = true,
                Ok(Ok(UninstallOutcome::NotInstalled)) => {}
                Ok(Err(err)) => {
                    let error = format!("Failed to remove signed extension: {}", err.code());
                    self.broadcast_state_changed(name, "failed", Some(error.clone()));
                    return HubResult::err(error);
                }
                Err(_) => {
                    let error = "Failed to remove signed extension".to_owned();
                    self.broadcast_state_changed(name, "failed", Some(error.clone()));
                    return HubResult::err(error);
                }
            }
        }

        let target_dir = self.index_manager.install_target_dir();
        let ext_dir = target_dir.join(name);

        if !ext_dir.exists() && !removed_signed {
            let error = format!("Extension '{name}' is not installed");
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        if ext_dir.exists()
            && let Err(e) = std::fs::remove_dir_all(&ext_dir)
        {
            warn!(
                name,
                error = %e,
                "hub: failed to remove extension directory"
            );
            let error = hub_message("Failed to remove extension directory", HUB_REMOVE_FAILED);
            self.broadcast_state_changed(name, "failed", Some(error.clone()));
            return HubResult::err(error);
        }

        self.registry.hot_reload().await;
        self.broadcast_state_changed(name, "uninstalled", None);

        info!(name, "hub: extension uninstalled successfully");
        HubResult::ok()
    }

    /// A built-in name (design §T.8) is provisioned and updated by the app itself: hub install and
    /// update refuse it before either the signed or the unsigned directory flow runs, whatever the
    /// index entry says (`bundled`, `curated`, signer). `None` = not a built-in name.
    fn refuse_builtin_name(&self, entry: &HubIndexEntry, name: &str) -> Option<HubResult> {
        let signed = self.signed.as_ref()?;
        if !signed.is_builtin_name(name) && !signed.is_builtin_name(&entry.name) {
            return None;
        }
        let error = format!(
            "Extension '{name}' is built into CSBU WorkMate and is updated with the app (name_reserved_by_builtin)"
        );
        warn!(name, "hub: refused install/update of a built-in name");
        self.broadcast_state_changed(name, "failed", Some(error.clone()));
        Some(HubResult::err(error))
    }

    /// Signed local-package path. `None` = not applicable (no signed context, unsafe name, or no
    /// package file for this entry): the caller continues with today's flow. `Some` = decided here.
    async fn try_signed_package(&self, entry: &HubIndexEntry, name: &str) -> Option<HubResult> {
        let signed = self.signed.clone()?;
        // The package file name is `{name}.zip` next to the index; the name must be its own safe
        // directory component (identity under the Host's encoding), otherwise no package applies.
        if encode_component(&entry.name).as_deref() != Some(entry.name.as_str()) {
            return None;
        }
        let package_path = self
            .index_manager
            .install_target_dir()
            .join(format!("{}.zip", entry.name));
        let max = signed.max_package_zip_bytes();
        let entry_owned = entry.clone();
        let installed = tokio::task::spawn_blocking(move || {
            let bytes = match read_local_package(&package_path, max) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return None,
                Err(()) => {
                    return Some(Err(SignedPackageError::Package(
                        super::local_package_install::LocalPackageError::InvalidPackage,
                    )));
                }
            };
            Some(signed.install_package(&entry_owned, &bytes))
        })
        .await;
        let outcome = match installed {
            Ok(Some(outcome)) => outcome,
            Ok(None) => return None,
            Err(_) => {
                let error = "Signed package install failed".to_owned();
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                return Some(HubResult::err(error));
            }
        };
        match outcome {
            Ok(report) => {
                info!(name, kind = ?report.kind, "hub: signed package installed");
                self.registry.hot_reload().await;
                self.broadcast_state_changed(name, "installed", None);
                Some(HubResult::ok())
            }
            Err(err) => {
                let error = format!("Signed package install failed: {}", err.code());
                warn!(name, "hub: signed package install refused");
                self.broadcast_state_changed(name, "failed", Some(error.clone()));
                Some(HubResult::err(error))
            }
        }
    }

    /// Check for available updates across all installed extensions.
    ///
    /// Compares installed versions against the Hub index.
    pub async fn check_updates(&self) -> Vec<HubUpdateInfo> {
        let index_list = self.index_manager.load_index().await;
        let loaded = self.registry.get_loaded_extensions().await;

        let mut updates = Vec::new();

        for hub_ext in &index_list {
            if hub_ext.bundled {
                continue;
            }

            if let Some(installed) = loaded.iter().find(|l| l.name == hub_ext.name)
                && is_newer(&hub_ext.version, &installed.version)
            {
                updates.push(HubUpdateInfo {
                    name: hub_ext.name.clone(),
                    current_version: installed.version.clone(),
                    latest_version: hub_ext.version.clone(),
                });
            }
        }

        updates
    }

    /// Verify that an extension directory contains a valid manifest
    /// and that its contributions can be resolved without errors.
    pub fn verify_installation(&self, ext_dir: &Path) -> Result<(), ExtensionError> {
        let manifest_path = ext_dir.join(EXTENSION_MANIFEST_FILE);

        if !manifest_path.exists() {
            return Err(ExtensionError::ManifestValidation(format!(
                "Manifest not found: {}",
                manifest_path.display()
            )));
        }

        let bytes = std::fs::read(&manifest_path)?;
        let manifest = parse_manifest(&bytes)?;
        validate_manifest(&manifest)?;

        // Build a temporary LoadedExtension to test contribution resolution.
        let loaded = LoadedExtension {
            manifest,
            directory: ext_dir.to_str().unwrap_or_default().to_owned(),
            source: ExtensionSource::Local,
            state: ExtensionState {
                name: "verification-check".into(),
                version: "0.0.0".into(),
                enabled: true,
                installed_at: None,
                last_activated_at: None,
            },
        };

        // Resolve contributions — this validates CSS files exist for themes,
        // route namespaces for webui, etc.
        let _contributions = resolve_extension_contributions(&loaded);

        debug!(
            dir = %ext_dir.display(),
            "hub: installation verification passed"
        );
        Ok(())
    }

    fn broadcast_state_changed(&self, name: &str, status: &str, error: Option<String>) {
        self.broadcaster.broadcast(WebSocketMessage::new(
            "hub.state-changed",
            json!({
                "name": name,
                "status": status,
                "error": error,
            }),
        ));
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate an extension name to prevent path traversal attacks.
/// Fixed reason codes ([`crate::reason_catalogue`]) of the legacy hub-directory flow. The public
/// `HubResult.msg` carries `<fixed text> (<code>)` only, never a local path or an OS error text
/// (internal security review F9); the detail goes to the Host log.
pub(crate) const HUB_PACKAGE_NOT_FOUND: &str = "hub_package_not_found";
pub(crate) const HUB_VERIFICATION_FAILED: &str = "hub_verification_failed";
pub(crate) const HUB_NOT_INSTALLED: &str = "hub_not_installed";
pub(crate) const HUB_REMOVE_FAILED: &str = "hub_remove_failed";

fn hub_message(text: &str, code: &str) -> String {
    format!("{text} ({code})")
}

fn validate_hub_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(format!("Invalid extension name: '{name}'"));
    }
    Ok(())
}

/// Check if `index_version` is newer than `installed_version`.
fn is_newer(index_version: &str, installed_version: &str) -> bool {
    let Ok(idx) = semver::Version::parse(index_version) else {
        return false;
    };
    let Ok(inst) = semver::Version::parse(installed_version) else {
        return false;
    };
    idx > inst
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

/// Read a locally supplied package archive: a regular file (never a symlink), bounded. `Ok(None)`
/// when absent. The bytes are untrusted until `check_signed_manifest` and the SRI checks pass.
fn read_local_package(path: &Path, max: u64) -> Result<Option<Vec<u8>>, ()> {
    use std::io::Read as _;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !meta.is_file() || meta.len() > max {
        return Err(());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| ())?
        .take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > max {
        return Err(());
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_realtime::BroadcastEventBus;

    #[test]
    fn hub_result_ok() {
        let r = HubResult::ok();
        assert!(r.success);
        assert!(r.msg.is_none());
    }

    #[test]
    fn hub_result_err() {
        let r = HubResult::err("something failed");
        assert!(!r.success);
        assert_eq!(r.msg.as_deref(), Some("something failed"));
    }

    #[test]
    fn is_newer_true() {
        assert!(is_newer("2.0.0", "1.0.0"));
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(is_newer("1.0.1", "1.0.0"));
    }

    #[test]
    fn is_newer_false() {
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "2.0.0"));
    }

    #[test]
    fn is_newer_invalid_versions() {
        assert!(!is_newer("not-semver", "1.0.0"));
        assert!(!is_newer("1.0.0", "not-semver"));
    }

    #[test]
    fn verify_installation_no_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn verify_installation_invalid_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join(EXTENSION_MANIFEST_FILE), b"not valid json").unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn verify_installation_valid_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = serde_json::json!({
            "name": "test-ext",
            "version": "1.0.0"
        });
        std::fs::write(
            tmp.path().join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_ok());
    }

    #[test]
    fn verify_installation_reserved_name_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let manifest = serde_json::json!({
            "name": "aion-internal-ext",
            "version": "1.0.0"
        });
        std::fs::write(
            tmp.path().join(EXTENSION_MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let registry = make_test_registry();
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);

        let result = installer.verify_installation(tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn validate_hub_name_rejects_traversal() {
        assert!(validate_hub_name("../etc").is_err());
        assert!(validate_hub_name("foo/../../bar").is_err());
        assert!(validate_hub_name("foo\\bar").is_err());
        assert!(validate_hub_name("").is_err());
        assert!(validate_hub_name("..").is_err());
    }

    #[test]
    fn validate_hub_name_accepts_valid() {
        assert!(validate_hub_name("my-extension").is_ok());
        assert!(validate_hub_name("ext_v2").is_ok());
        assert!(validate_hub_name("a").is_ok());
    }

    fn make_test_registry() -> ExtensionRegistry {
        use crate::state::ExtensionStateStore;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        // Leak the TempDir so it lives long enough for the test.
        std::mem::forget(tmp);
        ExtensionRegistry::new(store, bus, "1.0.0".into())
    }

    #[tokio::test]
    async fn install_broadcasts_installing_then_failed_for_missing_index_entry() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = crate::state::ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus.clone(), "1.0.0".into());
        let index_mgr = HubIndexManager::new(tmp.path().to_path_buf(), registry.clone());
        let installer = HubInstaller::new(index_mgr, registry);
        let mut rx = bus.subscribe();

        let result = installer.install("missing-ext").await;

        assert!(!result.success);
        let first = rx.recv().await.unwrap();
        assert_eq!(first.name, "hub.state-changed");
        assert_eq!(first.data["name"], "missing-ext");
        assert_eq!(first.data["status"], "installing");

        let second = rx.recv().await.unwrap();
        assert_eq!(second.name, "hub.state-changed");
        assert_eq!(second.data["status"], "failed");
    }
}

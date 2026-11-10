//! Plugin Contract v1 Host runtime (M9 E13 deepen).
//!
//! Wires curated contributions beyond list/resolve:
//! - command invoke (ack + optional panel open hint)
//! - agent-tool invoke with D13-2 `planSafeActions` gate
//! - background service start/stop (relative entry under extension dir)
//!
//! Brand: CSBU WorkMate. Curated-only (D13-1); no arbitrary third-party sandbox.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aionui_runtime::Builder as CmdBuilder;
use serde::Serialize;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::background_service_start_gate::gate_background_service_start;
use crate::bound_background_start::{refuse_verified_without_fd_exec, verify_bound_files_service};
use crate::error::ExtensionError;
use crate::hub::curated::CuratedInstallPolicy;
use crate::hub::curated_signed_retain::{
    CuratedSignedRetainOutcome, retain_checked_signed_root as retain_checked_into_store,
    try_retain_curated_signed_root as try_retain_into_store,
};
use crate::hub::signed_manifest::{SignatureCheckedManifest, SignedManifestEntry};
use crate::signed_root_binding::SignedRootBindingStore;
use crate::types::{
    ExtPermissions, LoadedExtension, ResolvedAgentTool, ResolvedBackgroundService, ResolvedCommand, ResolvedPanel,
};

/// In-process tracker for started background services (per Host process).
#[derive(Default)]
pub struct BackgroundServiceManager {
    inner: Mutex<HashMap<(String, String, String), RunningService>>,
}

struct RunningService {
    extension_name: String,
    service_id: String,
    pid: Option<u32>,
    child: Child,
    started_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundServiceStatus {
    pub id: String,
    pub extension_name: String,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
}

impl BackgroundServiceManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(user_id: &str, extension_name: &str, service_id: &str) -> (String, String, String) {
        (user_id.to_owned(), extension_name.to_owned(), service_id.to_owned())
    }

    // Only a confirmed exit allows a replacement process. Keep the handle on
    // probe errors so an uncertain state cannot create duplicate services.
    fn reap_exited(services: &mut HashMap<(String, String, String), RunningService>, key: &(String, String, String)) {
        let exited = match services.get_mut(key) {
            Some(service) => match service.child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(error) => {
                    warn!(%error, extension = %service.extension_name, service = %service.service_id,
                        "could not inspect background service; retaining handle");
                    false
                }
            },
            None => false,
        };
        if exited {
            services.remove(key);
        }
    }

    pub async fn status_for(
        &self,
        user_id: &str,
        services: &[ResolvedBackgroundService],
    ) -> Vec<BackgroundServiceStatus> {
        let mut guard = self.inner.lock().await;
        services
            .iter()
            .map(|svc| {
                let key = Self::key(user_id, &svc.extension_name, &svc.id);
                Self::reap_exited(&mut guard, &key);
                let running = guard.get(&key);
                BackgroundServiceStatus {
                    id: svc.id.clone(),
                    extension_name: svc.extension_name.clone(),
                    running: running.is_some(),
                    pid: running.and_then(|r| r.pid),
                    started_at_ms: running.map(|r| r.started_at_ms),
                }
            })
            .collect()
    }

    pub async fn is_running(&self, user_id: &str, extension_name: &str, service_id: &str) -> bool {
        let key = Self::key(user_id, extension_name, service_id);
        let mut guard = self.inner.lock().await;
        Self::reap_exited(&mut guard, &key);
        guard.contains_key(&key)
    }

    /// Start a curated background service. `entry_point` must resolve under `ext.directory`
    /// (relative path or already-normalized asset URL path segment). Requires `shell` permission.
    ///
    /// Equivalent to [`Self::start_with_signed`] with no Host-retained signed snapshot.
    pub async fn start(
        &self,
        user_id: &str,
        ext: &LoadedExtension,
        service: &ResolvedBackgroundService,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        self.start_with_signed(user_id, ext, service, None, None).await
    }

    /// Look up a Host-retained binding (snapshot + root FD) and start via [`Self::start_with_signed`].
    pub async fn start_with_binding(
        &self,
        user_id: &str,
        ext: &LoadedExtension,
        service: &ResolvedBackgroundService,
        store: &SignedRootBindingStore,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        let bound = store.lookup_clone(&service.extension_name);
        match &bound {
            Some((checked, root)) => {
                self.start_with_signed(user_id, ext, service, Some(checked), Some(root))
                    .await
            }
            None => self.start_with_signed(user_id, ext, service, None, None).await,
        }
    }

    /// Like [`Self::start`], with an optional Host-retained signed snapshot and root FD.
    ///
    /// Bound+`files` (a snapshot that declares `files`): the entry is verified through the
    /// retained root (never `LoadedExtension.directory`). This slice then refuses to spawn
    /// (`bound_service_fd_exec_unavailable`); it never falls back to a path exec. A snapshot
    /// with `files` but no root is also refused.
    ///
    /// Otherwise the existing start gate applies: a declared `entryPointIntegrity` (loaded
    /// manifest or snapshot) cannot be enforced by a path spawn → refuse. Undeclared services
    /// keep the legacy path spawn (not "verified").
    pub async fn start_with_signed(
        &self,
        user_id: &str,
        ext: &LoadedExtension,
        service: &ResolvedBackgroundService,
        signed: Option<&SignatureCheckedManifest>,
        root: Option<&File>,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        if service.extension_name != ext.manifest.name {
            return Err(ExtensionError::NotFound(service.id.clone()));
        }
        let perms = ext.manifest.permissions.clone().unwrap_or_default();
        ensure_shell_permission(&perms)?;

        if let Some(checked) = signed {
            let checked = checked.clone();
            let root = match root {
                Some(file) => Some(file.try_clone().map_err(|_| {
                    ExtensionError::from(crate::bound_background_start::BoundBackgroundStartError::RootMissing)
                })?),
                None => None,
            };
            let service = service.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                verify_bound_files_service(&checked, root.as_ref(), &service).and_then(refuse_verified_without_fd_exec)
            })
            .await
            .map_err(|err| ExtensionError::HookFailed {
                extension_name: ext.manifest.name.clone(),
                hook: "backgroundService".into(),
                reason: err.to_string(),
            })?;
            match outcome {
                // NotApplicable: snapshot has no `files` — existing gate + possible path spawn.
                Ok(()) => {}
                Err(err) => return Err(err.into()),
            }
        }

        gate_background_service_start(&ext.manifest, service, signed).map_err(ExtensionError::from)?;

        let script = resolve_service_script(&ext.directory, &service.entry_point)?;
        if !script.exists() {
            return Err(ExtensionError::HookNotFound(script.display().to_string()));
        }

        let key = Self::key(user_id, &service.extension_name, &service.id);
        let mut guard = self.inner.lock().await;
        Self::reap_exited(&mut guard, &key);
        if guard.contains_key(&key) {
            let running = guard.get(&key).expect("just checked");
            return Ok(BackgroundServiceStatus {
                id: service.id.clone(),
                extension_name: service.extension_name.clone(),
                running: true,
                pid: running.pid,
                started_at_ms: Some(running.started_at_ms),
            });
        }

        let mut builder = CmdBuilder::clean_cli(&script);
        builder.current_dir(Path::new(&ext.directory));
        let child = builder.spawn().map_err(|e| ExtensionError::HookFailed {
            extension_name: ext.manifest.name.clone(),
            hook: "backgroundService".into(),
            reason: e.to_string(),
        })?;
        let pid = child.id();
        let started_at_ms = now_ms();
        info!(
            extension = %service.extension_name,
            service = %service.id,
            ?pid,
            path = %script.display(),
            "started curated background service"
        );
        guard.insert(
            key,
            RunningService {
                extension_name: service.extension_name.clone(),
                service_id: service.id.clone(),
                pid,
                child,
                started_at_ms,
            },
        );
        Ok(BackgroundServiceStatus {
            id: service.id.clone(),
            extension_name: service.extension_name.clone(),
            running: true,
            pid,
            started_at_ms: Some(started_at_ms),
        })
    }

    pub async fn stop(
        &self,
        user_id: &str,
        extension_name: &str,
        service_id: &str,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        let key = Self::key(user_id, extension_name, service_id);
        let mut guard = self.inner.lock().await;
        let Some(mut running) = guard.remove(&key) else {
            return Ok(BackgroundServiceStatus {
                id: service_id.to_owned(),
                extension_name: extension_name.to_owned(),
                running: false,
                pid: None,
                started_at_ms: None,
            });
        };
        if let Err(e) = running.child.kill().await {
            warn!(
                extension = %running.extension_name,
                service = %running.service_id,
                error = %e,
                "failed to kill background service child"
            );
        }
        let _ = running.child.wait().await;
        info!(
            extension = %running.extension_name,
            service = %running.service_id,
            "stopped curated background service"
        );
        Ok(BackgroundServiceStatus {
            id: service_id.to_owned(),
            extension_name: extension_name.to_owned(),
            running: false,
            pid: None,
            started_at_ms: None,
        })
    }
}

/// App/Host callback that turns an explicit signed-root binding into an invoke decision.
///
/// Implemented in `aionui-app` (calls `prepare_checked_agent_tool_invoke`) so the
/// extension crate does not depend on app. Default on [`HostRuntime`] is `None` →
/// refuse `integrity_binding_unavailable`. Success path must stay non-executing
/// (`dispatched=false`) unless a later knife separately authorizes supervisor use.
pub trait CheckedAgentToolPrepare: Send + Sync {
    fn prepare(
        &self,
        tool: &ResolvedAgentTool,
        local_tool_id: &str,
        checked: &SignatureCheckedManifest,
        root: &File,
    ) -> AgentToolInvokeResult;
}

/// Shared Host runtime handles hanging off [`crate::routes::ExtensionRouterState`].
#[derive(Clone, Default)]
pub struct HostRuntime {
    pub background: Arc<BackgroundServiceManager>,
    /// Host-installed `(SignatureCheckedManifest, trusted root File)` by extension name.
    pub signed_roots: Arc<SignedRootBindingStore>,
    /// Optional app-side prepare hook. Absent → binding cannot be consumed.
    pub checked_prepare: Option<Arc<dyn CheckedAgentToolPrepare>>,
}

impl HostRuntime {
    pub fn new() -> Self {
        Self {
            background: Arc::new(BackgroundServiceManager::new()),
            signed_roots: Arc::new(SignedRootBindingStore::new()),
            checked_prepare: None,
        }
    }

    /// Attach the app prepare adapter. Library/Host wiring only — not an HTTP surface.
    pub fn with_checked_prepare(mut self, prepare: Arc<dyn CheckedAgentToolPrepare>) -> Self {
        self.checked_prepare = Some(prepare);
        self
    }

    /// Host-facing curated retain: strict `check_signed_manifest` then optional bind.
    ///
    /// Pass a Host-opened directory `File`, or `None` to leave unbound — never invent
    /// a root from `LoadedExtension.directory` / install path strings.
    /// Signature failure → no binding. See [`crate::hub::curated_signed_retain::try_retain_curated_signed_root`].
    pub fn try_retain_curated_signed_root(
        &self,
        policy: &CuratedInstallPolicy,
        entry: &SignedManifestEntry<'_>,
        raw_manifest: &[u8],
        trusted_root: Option<File>,
    ) -> CuratedSignedRetainOutcome {
        try_retain_into_store(&self.signed_roots, policy, entry, raw_manifest, trusted_root)
    }

    /// Retain an already-checked snapshot when a trusted root FD is available.
    pub fn retain_checked_signed_root(
        &self,
        checked: SignatureCheckedManifest,
        trusted_root: Option<File>,
    ) -> CuratedSignedRetainOutcome {
        retain_checked_into_store(&self.signed_roots, checked, trusted_root)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommandInvokeResult {
    pub status: &'static str,
    pub command: ResolvedCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_panel: Option<ResolvedPanel>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolInvokeResult {
    pub status: &'static str,
    pub tool: ResolvedAgentTool,
    pub dispatched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

/// Stable reason when Host cannot supply `SignatureCheckedManifest` + trusted root.
/// Prefer app `prepare_checked_agent_tool_invoke` once those inputs exist.
pub const INTEGRITY_BINDING_UNAVAILABLE_REASON: &str = "integrity_binding_unavailable";

/// Refuse agent-tool invoke when integrity binding cannot be established.
///
/// Missing Host-installed signed-root binding and/or prepare hook → this refuse.
/// Do not invent trust from `LoadedExtension.directory` or unsigned re-parse.
/// Never returns `handler_stub` success or `dispatched: true`. Not execution auth.
pub fn refuse_agent_tool_without_integrity_binding(tool: ResolvedAgentTool) -> AgentToolInvokeResult {
    AgentToolInvokeResult {
        status: "refused",
        tool,
        dispatched: false,
        reason: Some(INTEGRITY_BINDING_UNAVAILABLE_REASON),
    }
}

/// Local tool id as declared in the signed manifest (`search`), not the resolved
/// `ext-{name}-{id}` form used on HTTP.
pub fn local_agent_tool_id(tool: &ResolvedAgentTool) -> &str {
    let prefix = format!("ext-{}-", tool.extension_name);
    tool.id.strip_prefix(&prefix).unwrap_or(tool.id.as_str())
}

/// Fail-closed invoke decision using Host-owned store + optional prepare hook.
///
/// - No prepare hook → `integrity_binding_unavailable`
/// - No installed binding for `tool.extension_name` → same refuse
/// - Else call the Host/app prepare hook with the retained snapshot + root FD
///
/// Never opens `LoadedExtension.directory`. Not execution authorization.
pub fn resolve_agent_tool_invoke(host: &HostRuntime, tool: ResolvedAgentTool) -> AgentToolInvokeResult {
    let Some(prepare) = host.checked_prepare.as_ref() else {
        return refuse_agent_tool_without_integrity_binding(tool);
    };
    let Some((checked, root)) = host.signed_roots.lookup_clone(&tool.extension_name) else {
        return refuse_agent_tool_without_integrity_binding(tool);
    };
    let local_id = local_agent_tool_id(&tool).to_owned();
    prepare.prepare(&tool, &local_id, &checked, &root)
}

/// Resolve a command by full resolved id (`ext-name-local`) or local id + extension.
pub fn find_command<'a>(
    commands: &'a [ResolvedCommand],
    id: &str,
    extension_name: Option<&str>,
) -> Option<&'a ResolvedCommand> {
    if let Some(ext) = extension_name {
        let prefixed = format!("ext-{ext}-{id}");
        return commands
            .iter()
            .find(|c| c.extension_name == ext && (c.id == id || c.id == prefixed));
    }
    commands.iter().find(|c| c.id == id)
}

pub fn find_agent_tool<'a>(
    tools: &'a [ResolvedAgentTool],
    id: &str,
    extension_name: Option<&str>,
) -> Option<&'a ResolvedAgentTool> {
    if let Some(ext) = extension_name {
        let prefixed = format!("ext-{ext}-{id}");
        return tools
            .iter()
            .find(|t| t.extension_name == ext && (t.id == id || t.id == prefixed));
    }
    tools.iter().find(|t| t.id == id)
}

pub fn find_background_service<'a>(
    services: &'a [ResolvedBackgroundService],
    id: &str,
    extension_name: Option<&str>,
) -> Option<&'a ResolvedBackgroundService> {
    if let Some(ext) = extension_name {
        let prefixed = format!("ext-{ext}-{id}");
        return services
            .iter()
            .find(|s| s.extension_name == ext && (s.id == id || s.id == prefixed));
    }
    services.iter().find(|s| s.id == id)
}

/// Prefer a panel whose id shares the command's local suffix.
pub fn panel_hint_for_command<'a>(panels: &'a [ResolvedPanel], command: &ResolvedCommand) -> Option<&'a ResolvedPanel> {
    let local = command
        .id
        .strip_prefix(&format!("ext-{}-", command.extension_name))
        .unwrap_or(command.id.as_str());
    panels.iter().find(|p| {
        p.extension_name == command.extension_name
            && (p.id == command.id
                || p.id.ends_with(&format!("-{local}"))
                || p.id == format!("ext-{}-{local}", command.extension_name))
    })
}

/// D13-2: plan mode may only auto-run tools that declared `planSafeActions: true`.
pub fn gate_plan_safe(tool: &ResolvedAgentTool, plan_mode: bool) -> Result<(), ExtensionError> {
    if plan_mode && !tool.plan_safe_actions {
        return Err(ExtensionError::ManifestValidation(format!(
            "agent tool '{}' refuses plan-mode invoke (planSafeActions=false)",
            tool.id
        )));
    }
    Ok(())
}

fn ensure_shell_permission(perms: &ExtPermissions) -> Result<(), ExtensionError> {
    if perms.shell != Some(true) {
        return Err(ExtensionError::ManifestValidation(
            "background service start requires permissions.shell=true".into(),
        ));
    }
    Ok(())
}

/// Map resolved entry_point (asset URL or relative/opaque) to a filesystem script under ext_dir.
fn resolve_service_script(ext_dir: &str, entry_point: &str) -> Result<PathBuf, ExtensionError> {
    let relative = if let Some(rest) = entry_point.strip_prefix("/api/extensions/") {
        // /api/extensions/{name}/assets/{rel}
        let parts: Vec<&str> = rest.splitn(3, '/').collect();
        if parts.len() == 3 && parts[1] == "assets" {
            parts[2].to_owned()
        } else {
            return Err(ExtensionError::PathTraversal(entry_point.to_owned()));
        }
    } else if entry_point.contains("://") || entry_point.starts_with('/') {
        return Err(ExtensionError::PathTraversal(entry_point.to_owned()));
    } else {
        entry_point.to_owned()
    };

    let path = Path::new(&relative);
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return Err(ExtensionError::PathTraversal(entry_point.to_owned()));
    }
    // Resolve both sides before comparing components: lexical checks do not
    // prevent a script or an intermediate directory from being a symlink.
    let root = std::fs::canonicalize(ext_dir).map_err(|_| ExtensionError::PathTraversal(entry_point.to_owned()))?;
    let script =
        std::fs::canonicalize(root.join(path)).map_err(|_| ExtensionError::PathTraversal(entry_point.to_owned()))?;
    if !script.starts_with(&root) || !script.is_file() {
        return Err(ExtensionError::PathTraversal(entry_point.to_owned()));
    }
    // Launch the resolved path, not the original symlink path. Curated plugin
    // files must not be modified concurrently with service startup.
    Ok(script)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExtPermissions, ResolvedAgentTool, ResolvedCommand, ResolvedPanel};

    #[test]
    fn find_command_by_prefixed_or_local() {
        let cmds = vec![ResolvedCommand {
            extension_name: "demo.ext".into(),
            id: "ext-demo.ext-open".into(),
            title: "Open".into(),
            keybinding: None,
            when: None,
        }];
        assert!(find_command(&cmds, "ext-demo.ext-open", None).is_some());
        assert!(find_command(&cmds, "open", Some("demo.ext")).is_some());
        assert!(find_command(&cmds, "missing", None).is_none());
    }

    #[test]
    fn refuse_without_binding_is_not_dispatched_or_stub() {
        let tool = ResolvedAgentTool {
            extension_name: "demo.ext".into(),
            id: "ext-demo.ext-read".into(),
            name: "Read".into(),
            description: None,
            plan_safe_actions: true,
            handler: None,
        };
        let result = refuse_agent_tool_without_integrity_binding(tool.clone());
        assert_eq!(result.status, "refused");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some(INTEGRITY_BINDING_UNAVAILABLE_REASON));
        assert_ne!(result.reason, Some("handler_stub"));
        assert_eq!(result.tool, tool);
    }

    #[test]
    fn local_agent_tool_id_strips_ext_prefix() {
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: None,
        };
        assert_eq!(local_agent_tool_id(&tool), "search");
    }

    #[test]
    fn resolve_without_prepare_hook_refuses_unavailable() {
        let host = HostRuntime::new();
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: None,
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "refused");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some(INTEGRITY_BINDING_UNAVAILABLE_REASON));
    }

    struct StubPrepare;
    impl CheckedAgentToolPrepare for StubPrepare {
        fn prepare(
            &self,
            tool: &ResolvedAgentTool,
            local_tool_id: &str,
            checked: &SignatureCheckedManifest,
            _root: &File,
        ) -> AgentToolInvokeResult {
            assert_eq!(local_tool_id, "search");
            assert_eq!(checked.name(), "demo.plugin");
            AgentToolInvokeResult {
                status: "accepted",
                tool: tool.clone(),
                dispatched: false,
                reason: Some("handler_integrity_checked"),
            }
        }
    }

    #[test]
    fn resolve_with_binding_and_prepare_returns_hook_result() {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::{HashMap, HashSet};

        let signing_key = SigningKey::from_bytes(&[19; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key.verifying_key())]),
        );
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        let checked = check_signed_manifest(
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
        .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();

        let host = HostRuntime::new().with_checked_prepare(Arc::new(StubPrepare));
        host.signed_roots.install("demo.plugin", checked, root).unwrap();
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: None,
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "accepted");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some("handler_integrity_checked"));
        assert_ne!(result.reason, Some("handler_stub"));
    }

    #[test]
    fn resolve_with_prepare_but_no_binding_still_unavailable() {
        let host = HostRuntime::new().with_checked_prepare(Arc::new(StubPrepare));
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: None,
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "refused");
        assert_eq!(result.reason, Some(INTEGRITY_BINDING_UNAVAILABLE_REASON));
    }

    #[test]
    fn host_try_retain_success_then_missing_root_fail_closed() {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::curated_signed_retain::CuratedSignedRetainOutcome;
        use crate::hub::signed_manifest::SignedManifestEntry;
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::{HashMap, HashSet};

        let signing_key = SigningKey::from_bytes(&[19; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key.verifying_key())]),
        );
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        let entry = SignedManifestEntry {
            name: "demo.plugin",
            version: "1.0.0",
            pubkey_id: Some("fixture"),
            integrity: Some(&integrity),
            signature: Some(&signature),
        };
        let host = HostRuntime::new();
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        assert_eq!(
            host.try_retain_curated_signed_root(&policy, &entry, raw, Some(root)),
            CuratedSignedRetainOutcome::Retained
        );
        assert!(host.signed_roots.contains("demo.plugin"));

        let host2 = HostRuntime::new();
        assert_eq!(
            host2.try_retain_curated_signed_root(&policy, &entry, raw, None),
            CuratedSignedRetainOutcome::RootHandleMissing
        );
        assert!(host2.signed_roots.is_empty());
    }

    #[test]
    fn plan_safe_gate_blocks_unsafe_in_plan_mode() {
        let tool = ResolvedAgentTool {
            extension_name: "demo.ext".into(),
            id: "ext-demo.ext-write".into(),
            name: "Write".into(),
            description: None,
            plan_safe_actions: false,
            handler: None,
        };
        assert!(gate_plan_safe(&tool, true).is_err());
        assert!(gate_plan_safe(&tool, false).is_ok());
        let safe = ResolvedAgentTool {
            plan_safe_actions: true,
            ..tool.clone()
        };
        assert!(gate_plan_safe(&safe, true).is_ok());
    }

    #[test]
    fn panel_hint_matches_local_suffix() {
        let cmd = ResolvedCommand {
            extension_name: "demo.ext".into(),
            id: "ext-demo.ext-board".into(),
            title: "Board".into(),
            keybinding: None,
            when: None,
        };
        let panels = vec![ResolvedPanel {
            extension_name: "demo.ext".into(),
            id: "ext-demo.ext-board".into(),
            title: "Board".into(),
            entry_point: "/api/extensions/demo.ext/assets/panels/board.html".into(),
            icon: None,
        }];
        assert!(panel_hint_for_command(&panels, &cmd).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exited_services_are_reaped_only_for_the_requested_user() {
        let manager = BackgroundServiceManager::new();
        for user in ["alice", "bob"] {
            let mut child = tokio::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .unwrap();
            let pid = child.id();
            // Synchronize on exit, not a timing-dependent sleep.
            child.wait().await.unwrap();
            manager.inner.lock().await.insert(
                BackgroundServiceManager::key(user, "demo", "worker"),
                RunningService {
                    extension_name: "demo".into(),
                    service_id: "worker".into(),
                    pid,
                    child,
                    started_at_ms: 1,
                },
            );
        }
        assert!(!manager.is_running("alice", "demo", "worker").await);
        assert!(
            manager
                .inner
                .lock()
                .await
                .contains_key(&BackgroundServiceManager::key("bob", "demo", "worker"))
        );
        assert!(!manager.is_running("bob", "demo", "worker").await);
        assert!(manager.inner.lock().await.is_empty());
    }

    #[test]
    fn service_keys_are_scoped_to_authenticated_user() {
        assert_ne!(
            BackgroundServiceManager::key("alice", "demo", "worker"),
            BackgroundServiceManager::key("bob", "demo", "worker")
        );
        assert_ne!(
            BackgroundServiceManager::key("a\0b", "c", "worker"),
            BackgroundServiceManager::key("a", "b\0c", "worker")
        );
    }

    #[test]
    fn resolve_service_script_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("services")).unwrap();
        std::fs::write(dir.path().join("services/worker.sh"), "exit 0\n").unwrap();
        let root = dir.path().to_str().unwrap();
        let expected = std::fs::canonicalize(dir.path().join("services/worker.sh")).unwrap();
        assert!(resolve_service_script(root, "../evil.sh").is_err());
        assert!(resolve_service_script(root, "/abs.sh").is_err());
        assert!(resolve_service_script(root, "missing.sh").is_err());
        assert!(resolve_service_script(root, "services").is_err());
        assert_eq!(resolve_service_script(root, "services/worker.sh").unwrap(), expected);
        assert_eq!(
            resolve_service_script(root, "/api/extensions/demo.ext/assets/services/worker.sh").unwrap(),
            expected
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_service_script_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("worker.sh"), "exit 0\n").unwrap();
        symlink(outside.path().join("worker.sh"), dir.path().join("script-link.sh")).unwrap();
        symlink(outside.path(), dir.path().join("directory-link")).unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(resolve_service_script(root, "script-link.sh").is_err());
        assert!(resolve_service_script(root, "directory-link/worker.sh").is_err());
        std::fs::write(dir.path().join("inside.sh"), "exit 0\n").unwrap();
        symlink(dir.path().join("inside.sh"), dir.path().join("inside-link.sh")).unwrap();
        assert_eq!(
            resolve_service_script(root, "inside-link.sh").unwrap(),
            std::fs::canonicalize(dir.path().join("inside.sh")).unwrap()
        );
    }

    #[test]
    fn shell_permission_required() {
        let mut perms = ExtPermissions::default();
        assert!(ensure_shell_permission(&perms).is_err());
        perms.shell = Some(true);
        assert!(ensure_shell_permission(&perms).is_ok());
    }
}

//! Plugin Contract v1 Host runtime (M9 E13 deepen).
//!
//! Wires curated contributions beyond list/resolve:
//! - command invoke (ack + optional panel open hint)
//! - agent-tool invoke with D13-2 `planSafeActions` gate
//! - background service start/stop (relative entry under extension dir)
//!
//! Brand: CSBU WorkMate. Curated-only (D13-1); no arbitrary third-party sandbox.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use aionui_runtime::Builder as CmdBuilder;
use serde::Serialize;
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::error::ExtensionError;
use crate::types::{
    ExtPermissions, LoadedExtension, ResolvedAgentTool, ResolvedBackgroundService, ResolvedCommand, ResolvedPanel,
};

/// In-process tracker for started background services (per Host process).
#[derive(Default)]
pub struct BackgroundServiceManager {
    inner: Mutex<HashMap<String, RunningService>>,
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

    fn key(extension_name: &str, service_id: &str) -> String {
        format!("{extension_name}\0{service_id}")
    }

    pub async fn status_for(&self, services: &[ResolvedBackgroundService]) -> Vec<BackgroundServiceStatus> {
        let guard = self.inner.lock().await;
        services
            .iter()
            .map(|svc| {
                let running = guard.get(&Self::key(&svc.extension_name, &svc.id));
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

    pub async fn is_running(&self, extension_name: &str, service_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .contains_key(&Self::key(extension_name, service_id))
    }

    /// Start a curated background service. `entry_point` must resolve under `ext.directory`
    /// (relative path or already-normalized asset URL path segment). Requires `shell` permission.
    pub async fn start(
        &self,
        ext: &LoadedExtension,
        service: &ResolvedBackgroundService,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        if service.extension_name != ext.manifest.name {
            return Err(ExtensionError::NotFound(service.id.clone()));
        }
        let perms = ext.manifest.permissions.clone().unwrap_or_default();
        ensure_shell_permission(&perms)?;

        let script = resolve_service_script(&ext.directory, &service.entry_point)?;
        if !script.exists() {
            return Err(ExtensionError::HookNotFound(script.display().to_string()));
        }

        let key = Self::key(&service.extension_name, &service.id);
        let mut guard = self.inner.lock().await;
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
        extension_name: &str,
        service_id: &str,
    ) -> Result<BackgroundServiceStatus, ExtensionError> {
        let key = Self::key(extension_name, service_id);
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

/// Shared Host runtime handles hanging off [`crate::routes::ExtensionRouterState`].
#[derive(Clone, Default)]
pub struct HostRuntime {
    pub background: Arc<BackgroundServiceManager>,
}

impl HostRuntime {
    pub fn new() -> Self {
        Self {
            background: Arc::new(BackgroundServiceManager::new()),
        }
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
    Ok(Path::new(ext_dir).join(path))
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

    #[test]
    fn resolve_service_script_rejects_traversal() {
        assert!(resolve_service_script("/ext", "../evil.sh").is_err());
        assert!(resolve_service_script("/ext", "/abs.sh").is_err());
        let ok = resolve_service_script("/ext", "services/worker.sh").unwrap();
        assert_eq!(ok, PathBuf::from("/ext/services/worker.sh"));
        let from_url = resolve_service_script("/ext", "/api/extensions/demo.ext/assets/services/worker.sh").unwrap();
        assert_eq!(from_url, PathBuf::from("/ext/services/worker.sh"));
    }

    #[test]
    fn shell_permission_required() {
        let mut perms = ExtPermissions::default();
        assert!(ensure_shell_permission(&perms).is_err());
        perms.shell = Some(true);
        assert!(ensure_shell_permission(&perms).is_ok());
    }
}

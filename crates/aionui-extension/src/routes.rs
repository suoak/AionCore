#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::path::Path as FsPath;

use axum::Router;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, DisableExtensionRequest, EnableExtensionRequest, ExtensionSummaryResponse, GetI18nRequest,
    GetPermissionsRequest, GetRiskLevelRequest, PermissionDetailResponse, PermissionSummaryResponse,
};
use aionui_auth::CurrentUser;
use aionui_common::{ApiError, now_ms};
use serde_json::json;

use crate::asset_integrity_gate::{AssetIntegrityGate, AssetServeGateError};
use crate::asset_paths::normalize_relative_asset_path;
use crate::error::ExtensionError;
use crate::host_runtime::{self, CommandInvokeResult, HostRuntime};
use crate::hub::{FilesIndex, VerifiedAssetError, read_verified_listed};
use crate::permission::{build_permission_summary, calculate_risk_level};
use crate::registry::ExtensionRegistry;

impl From<ExtensionError> for ApiError {
    fn from(err: ExtensionError) -> Self {
        match err {
            ExtensionError::ManifestValidation(msg) => ApiError::BadRequest(msg),
            ExtensionError::ReservedNamePrefix { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::InvalidVersion { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::UndefinedEnvVariable(var) => {
                ApiError::BadRequest(format!("Undefined environment variable: {var}"))
            }
            ExtensionError::FileReferenceNotFound(path) => {
                ApiError::NotFound(format!("File reference not found: {path}"))
            }
            ExtensionError::PathTraversal(path) => ApiError::BadRequest(format!("Path traversal detected: {path}")),
            ExtensionError::EngineIncompatible { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ApiVersionIncompatible { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::InvalidWebuiRouteNamespace { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ReservedWebuiRoute { .. } => ApiError::BadRequest(err.to_string()),
            ExtensionError::ThemeCssNotFound(path) => ApiError::NotFound(format!("Theme CSS not found: {path}")),
            ExtensionError::ThemeCssUnverified(reason) => {
                ApiError::Forbidden(format!("Theme CSS could not be verified: {reason}"))
            }
            ExtensionError::HookTimeout { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::HookFailed { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::HookNotFound(path) => ApiError::NotFound(format!("Hook script not found: {path}")),
            ExtensionError::ResolutionFailed { .. } => ApiError::Internal(err.to_string()),
            ExtensionError::NotFound(name) => ApiError::NotFound(format!("Extension not found: {name}")),
            ExtensionError::StatePersistence(msg) => ApiError::Internal(msg),
            ExtensionError::BuiltinSkillDeletion(name) => {
                ApiError::BadRequest(format!("Cannot delete built-in skill: {name}"))
            }
            // Transient: a peer process owns the materialize lock. The lock
            // path stays out of the response body — it is already logged and
            // reported on the startup boundary line.
            ExtensionError::BuiltinSkillsLockTimeout { .. } => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "BUILTIN_SKILLS_LOCK_TIMEOUT",
                "Builtin skills are being updated by another process. Try again shortly.",
                None,
            ),
            ExtensionError::SkillNotFound(name) => ApiError::NotFound(format!("Skill not found: {name}")),
            ExtensionError::InvalidSkillPath(path) => ApiError::BadRequest(format!("Invalid skill path: {path}")),
            ExtensionError::SkillInvalidFrontmatter(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_INVALID_FRONTMATTER",
                "Skill frontmatter is invalid.",
                None,
            ),
            ExtensionError::SkillImportNoSkillFound(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_NO_SKILL_FOUND",
                "No valid skill was found in the selected path.",
                None,
            ),
            ExtensionError::SkillImportInvalidSource(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_INVALID_SOURCE",
                "Selected path is not a skill directory, SKILL.md, parent directory, or zip archive.",
                None,
            ),
            ExtensionError::SkillImportSymlinkEntry(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_SYMLINK_ENTRY",
                "Skill import cannot contain symlink entries.",
                None,
            ),
            ExtensionError::SkillImportFileTooLarge {
                file_path,
                file_bytes,
                limit_bytes,
            } => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_FILE_TOO_LARGE",
                "Skill import contains a file that is too large.",
                Some(json!({
                    "error_path": file_path,
                    "file_bytes": file_bytes,
                    "limit_bytes": limit_bytes,
                })),
            ),
            ExtensionError::SkillImportTotalTooLarge {
                total_bytes,
                limit_bytes,
            } => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_TOTAL_TOO_LARGE",
                "Skill import is too large.",
                Some(json!({
                    "total_bytes": total_bytes,
                    "limit_bytes": limit_bytes,
                })),
            ),
            ExtensionError::SkillImportInvalidZip(_) => ApiError::coded(
                StatusCode::BAD_REQUEST,
                "SKILL_IMPORT_INVALID_ZIP",
                "Selected zip archive is not a valid skill package.",
                None,
            ),
            ExtensionError::Db(aionui_db::DbError::NotFound(msg)) => ApiError::NotFound(msg),
            ExtensionError::Db(aionui_db::DbError::Conflict(msg)) => ApiError::Conflict(msg),
            ExtensionError::Db(err) => ApiError::Internal(err.to_string()),
            ExtensionError::InvalidRequest(msg) => ApiError::BadRequest(msg),
            ExtensionError::Internal(msg) => ApiError::Internal(msg),
            ExtensionError::Io(e) => ApiError::Internal(e.to_string()),
            ExtensionError::JsonParse(e) => ApiError::BadRequest(e.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Router state
// ---------------------------------------------------------------------------

/// Shared state for extension route handlers.
#[derive(Clone)]
pub struct ExtensionRouterState {
    pub registry: ExtensionRegistry,
    /// Short-lived settings-tab asset tokens (E1 tail ④).
    pub asset_tokens: std::sync::Arc<crate::asset_tokens::AssetTokenStore>,
    /// Plugin Contract v1 Host invoke / background service handles (M9 E13).
    pub host_runtime: HostRuntime,
}

impl ExtensionRouterState {
    pub fn new(registry: ExtensionRegistry) -> Self {
        Self {
            registry,
            asset_tokens: std::sync::Arc::new(crate::asset_tokens::AssetTokenStore::new()),
            host_runtime: HostRuntime::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the extension router with all `/api/extensions/*` routes.
///
/// Includes query routes and management routes.
/// All routes require authentication (applied by the caller).
pub fn extension_routes(state: ExtensionRouterState) -> Router {
    Router::new()
        // Query routes
        .route("/api/extensions", get(get_loaded_extensions))
        .route("/api/extensions/themes", get(get_themes))
        .route("/api/extensions/assistants", get(get_assistants))
        .route("/api/extensions/acp-adapters", get(get_acp_adapters))
        .route("/api/extensions/agents", get(get_agents))
        .route("/api/extensions/mcp-servers", get(get_mcp_servers))
        .route("/api/extensions/skills", get(get_skills))
        .route("/api/extensions/channel-plugins", get(get_channel_plugins))
        .route("/api/extensions/settings-tabs", get(get_settings_tabs))
        .route("/api/extensions/commands", get(get_commands))
        .route("/api/extensions/panels", get(get_panels))
        .route("/api/extensions/work-panel-views", get(get_work_panel_views))
        .route("/api/extensions/agent-tools", get(get_agent_tools))
        .route("/api/extensions/background-services", get(get_background_services))
        .route(
            "/api/extensions/background-services/status",
            get(get_background_services_status),
        )
        .route("/api/extensions/commands/invoke", post(invoke_command))
        .route("/api/extensions/agent-tools/invoke", post(invoke_agent_tool))
        .route(
            "/api/extensions/background-services/start",
            post(start_background_service),
        )
        .route(
            "/api/extensions/background-services/stop",
            post(stop_background_service),
        )
        .route(
            "/api/extensions/{extension_name}/assets/{*asset_path}",
            get(get_extension_asset),
        )
        .route("/api/extensions/{extension_name}/asset-token", post(issue_asset_token))
        .route("/api/extensions/webui", get(get_webui))
        .route("/api/extensions/agent-activity", get(get_agent_activity))
        // Query routes with body
        .route("/api/extensions/i18n", post(get_i18n))
        .route("/api/extensions/permissions", post(get_permissions))
        .route("/api/extensions/risk-level", post(get_risk_level))
        // Management routes
        .route("/api/extensions/enable", post(enable_extension))
        .route("/api/extensions/disable", post(disable_extension))
        .with_state(state)
}

/// Routes authenticated by a short-lived asset token in the path instead of
/// the auth middleware. Mount WITHOUT `auth_middleware`: the token is the
/// credential, scoped to one user, one extension, read-only assets.
pub fn extension_asset_session_routes(state: ExtensionRouterState) -> Router {
    Router::new()
        .route(
            "/api/extensions/{extension_name}/asset-session/{token}/{*asset_path}",
            get(get_extension_asset_with_token),
        )
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Query handlers
// ---------------------------------------------------------------------------

/// `GET /api/extensions` — list all loaded extensions.
async fn get_loaded_extensions(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<ExtensionSummaryResponse>>>, ApiError> {
    let summaries = state.registry.get_loaded_extensions_for_user(&user.id).await;
    let resp: Vec<ExtensionSummaryResponse> = summaries
        .into_iter()
        .map(|s| {
            let source_str = serde_json::to_value(s.source)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_else(|| "local".to_string());
            ExtensionSummaryResponse {
                name: s.name,
                version: s.version,
                display_name: s.display_name,
                description: s.description,
                enabled: s.enabled,
                source: source_str,
            }
        })
        .collect();
    Ok(Json(ApiResponse::ok(resp)))
}

/// `GET /api/extensions/themes` — get all resolved themes.
async fn get_themes(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let themes = state.registry.get_themes_for_user(&user.id).await;
    let timestamp = now_ms();
    let value = serde_json::Value::Array(
        themes
            .into_iter()
            .map(|theme| {
                serde_json::json!({
                    "id": format!("ext-{}-{}", theme.extension_name, theme.id),
                    "name": format!("{} ({})", theme.name, theme.extension_name),
                    "cover": theme.cover_image,
                    "css": theme.css_content,
                    "is_preset": true,
                    "created_at": timestamp,
                    "updated_at": timestamp,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/assistants` — get all resolved assistants.
async fn get_assistants(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let assistants = state.registry.get_assistants_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        assistants
            .into_iter()
            .map(|assistant| {
                serde_json::json!({
                    "id": format!("ext-{}", assistant.id),
                    "name": assistant.name,
                    "description": assistant.description,
                    "avatar": assistant.icon,
                    "agentId": assistant.agent_id,
                    "context": assistant.context.unwrap_or_default(),
                    "models": assistant.models,
                    "enabledSkills": assistant.enabled_skills,
                    "prompts": assistant.prompts,
                    "isPreset": true,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": assistant.extension_name,
                    "_kind": "assistant",
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/acp-adapters` — get all resolved ACP adapters.
async fn get_acp_adapters(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let adapters = state.registry.get_acp_adapters_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        adapters
            .into_iter()
            .map(|adapter| {
                let cli_command = adapter.cli_command.clone();
                let default_cli_path = adapter.default_cli_path.clone().or_else(|| cli_command.clone());
                serde_json::json!({
                    "id": adapter.id,
                    "name": adapter.name,
                    "description": adapter.description,
                    "cliCommand": cli_command,
                    "defaultCliPath": default_cli_path,
                    "acpArgs": adapter.acp_args,
                    "env": adapter.env,
                    "avatar": adapter.avatar,
                    "authRequired": adapter.auth_required,
                    "supportsStreaming": adapter.supports_streaming.unwrap_or(false),
                    "connectionType": adapter.connection_type.unwrap_or_else(|| "cli".to_string()),
                    "endpoint": adapter.endpoint,
                    "models": adapter.models,
                    "yoloMode": adapter.yolo_mode,
                    "healthCheck": adapter.health_check,
                    "apiKeyFields": adapter.api_key_fields,
                    "isPreset": false,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": adapter.extension_name,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/agents` — get all resolved agents.
async fn get_agents(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let agents = state.registry.get_agents_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        agents
            .into_iter()
            .map(|agent| {
                serde_json::json!({
                    "id": format!("ext-{}", agent.id),
                    "name": agent.name,
                    "description": agent.description,
                    "avatar": agent.icon,
                    "agentType": agent.agent_type,
                    "context": agent.context.unwrap_or_default(),
                    "models": agent.models,
                    "enabledSkills": agent.enabled_skills,
                    "prompts": agent.prompts,
                    "isPreset": true,
                    "isBuiltin": false,
                    "enabled": true,
                    "_source": "extension",
                    "_extensionName": agent.extension_name,
                    "_kind": "agent",
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/mcp-servers` — get all resolved MCP servers.
async fn get_mcp_servers(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let servers = state.registry.get_mcp_servers_for_user(&user.id).await;
    let timestamp = now_ms();
    let value = serde_json::Value::Array(
        servers
            .into_iter()
            .map(|server| {
                let enabled = server
                    .config
                    .get("enabled")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true);
                let transport = server
                    .config
                    .get("transport")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let original_transport = transport.clone();
                let original_json = serde_json::json!({
                    "name": server.name,
                    "description": server.description,
                    "enabled": enabled,
                    "transport": original_transport,
                });
                serde_json::json!({
                    "id": format!("ext-{}-{}", server.extension_name, server.name),
                    "name": server.name,
                    "description": server.description,
                    "enabled": enabled,
                    "transport": transport,
                    "created_at": timestamp,
                    "updated_at": timestamp,
                    "original_json": serde_json::to_string_pretty(&original_json).unwrap_or_default(),
                    "_source": "extension",
                    "_extensionName": server.extension_name,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/skills` — get all resolved skills.
async fn get_skills(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let skills = state.registry.get_skills_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        skills
            .into_iter()
            .map(|skill| {
                serde_json::json!({
                    "name": skill.name,
                    "description": skill.description.unwrap_or_else(|| format!("Skill from extension: {}", skill.extension_name)),
                    "location": skill.path,
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/channel-plugins` — get all resolved channel plugins.
async fn get_channel_plugins(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let plugins = state.registry.get_channel_plugins_for_user(&user.id).await;
    let value = serde_json::Value::Array(
        plugins
            .into_iter()
            .map(|plugin| {
                serde_json::json!({
                    "id": plugin.id,
                    "type": plugin.id,
                    "name": plugin.name,
                    "platform": plugin.platform,
                    "entryPoint": plugin.entry_point,
                    "enabled": true,
                    "connected": false,
                    "active_users": 0,
                    "has_token": false,
                    "is_extension": true,
                    "extension_meta": {
                        "credentialFields": plugin.credential_fields,
                        "configFields": plugin.config_fields,
                        "description": plugin.description,
                        "extensionName": plugin.extension_name,
                        "icon": plugin.icon,
                    },
                })
            })
            .collect(),
    );
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/settings-tabs` — get all resolved settings tabs.
async fn get_settings_tabs(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let tabs = state.registry.get_settings_tabs_for_user(&user.id).await;
    let value = serde_json::to_value(&tabs).unwrap_or_default();
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/commands` — Plugin Contract v1 command contributions.
async fn get_commands(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let items = state.registry.get_commands_for_user(&user.id).await;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&items).unwrap_or_default())))
}

/// `GET /api/extensions/panels` — Plugin Contract v1 panel contributions.
async fn get_panels(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let items = authorized_panels(&state, &user.id).await;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&items).unwrap_or_default())))
}

/// Panels the Host-retained signed snapshot vouches for (bound, entry listed in the signed
/// `files`); see `hub::listing_auth`. Same item type and JSON shape as before, fewer items.
async fn authorized_panels(state: &ExtensionRouterState, user_id: &str) -> Vec<crate::types::ResolvedPanel> {
    let store = &state.host_runtime.signed_roots;
    state
        .registry
        .get_panels_for_user(user_id)
        .await
        .into_iter()
        .filter(|p| {
            crate::hub::listing_authorized(
                store,
                crate::hub::ContributionKind::Panel,
                &p.extension_name,
                &p.id,
                &p.entry_point,
            )
        })
        .collect()
}

/// `GET /api/extensions/work-panel-views` — Plugin Contract v1 work-panel views.
async fn get_work_panel_views(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let store = &state.host_runtime.signed_roots;
    let items: Vec<_> = state
        .registry
        .get_work_panel_views_for_user(&user.id)
        .await
        .into_iter()
        .filter(|v| {
            crate::hub::listing_authorized(
                store,
                crate::hub::ContributionKind::WorkPanelView,
                &v.extension_name,
                &v.id,
                &v.entry_point,
            )
        })
        .collect();
    Ok(Json(ApiResponse::ok(serde_json::to_value(&items).unwrap_or_default())))
}

/// `GET /api/extensions/agent-tools` — Plugin Contract v1 agent tools.
async fn get_agent_tools(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let items = state.registry.get_agent_tools_for_user(&user.id).await;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&items).unwrap_or_default())))
}

/// `GET /api/extensions/background-services` — Plugin Contract v1 background services (declaration only).
async fn get_background_services(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let items = state.registry.get_background_services_for_user(&user.id).await;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&items).unwrap_or_default())))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvokeCommandRequest {
    id: String,
    #[serde(default)]
    extension_name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct InvokeAgentToolRequest {
    id: String,
    #[serde(default)]
    extension_name: Option<String>,
    /// Untrusted client hint; never authorizes non-plan-safe tool execution.
    #[serde(default)]
    plan_mode: bool,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BackgroundServiceActionRequest {
    id: String,
    #[serde(default)]
    extension_name: Option<String>,
}

/// `POST /api/extensions/commands/invoke` — Host ack for a curated command; may hint a panel.
async fn invoke_command(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<InvokeCommandRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let commands = state.registry.get_commands_for_user(&user.id).await;
    let command = host_runtime::find_command(&commands, &req.id, req.extension_name.as_deref())
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("Command not found: {}", req.id)))?;
    // Only a panel the signed snapshot vouches for may be hinted to the UI.
    let panels = authorized_panels(&state, &user.id).await;
    let open_panel = host_runtime::panel_hint_for_command(&panels, &command).cloned();
    tracing::info!(
        user_id = %user.id,
        command_id = %command.id,
        extension = %command.extension_name,
        open_panel = open_panel.as_ref().map(|p| p.id.as_str()),
        "extension command invoked"
    );
    let result = CommandInvokeResult {
        status: "accepted",
        command,
        open_panel,
    };
    Ok(Json(ApiResponse::ok(serde_json::to_value(result).unwrap_or_default())))
}

/// `POST /api/extensions/agent-tools/invoke` — gate planSafeActions; Host signed-root binding or refuse.
async fn invoke_agent_tool(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<InvokeAgentToolRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let tools = state.registry.get_agent_tools_for_user(&user.id).await;
    let tool = host_runtime::find_agent_tool(&tools, &req.id, req.extension_name.as_deref())
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("Agent tool not found: {}", req.id)))?;
    // No authoritative session/run context is bound to this endpoint yet.
    // Fail closed regardless of the client hint, including false or omission.
    if let Err(err) = host_runtime::gate_plan_safe(&tool, true) {
        return Err(ApiError::Forbidden(err.to_string()));
    }
    // Host may retain an explicit signed-root binding on host_runtime.signed_roots
    // and an app prepare hook. Missing either → integrity_binding_unavailable.
    // Never invent trust from LoadedExtension.directory / unsigned re-parse.
    let _ = req.arguments;
    let result = host_runtime::resolve_agent_tool_invoke(&state.host_runtime, tool);
    tracing::info!(
        user_id = %user.id,
        tool_id = %result.tool.id,
        extension = %result.tool.extension_name,
        requested_plan_mode = req.plan_mode,
        status = result.status,
        reason = ?result.reason,
        dispatched = result.dispatched,
        "extension agent tool invoke resolved via host integrity binding"
    );
    Ok(Json(ApiResponse::ok(serde_json::to_value(result).unwrap_or_default())))
}

/// `GET /api/extensions/background-services/status` — declarations + running flags.
async fn get_background_services_status(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let items = state.registry.get_background_services_for_user(&user.id).await;
    let status = state.host_runtime.background.status_for(&user.id, &items).await;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&status).unwrap_or_default())))
}

/// `POST /api/extensions/background-services/start` — spawn curated relative entryPoint.
async fn start_background_service(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<BackgroundServiceActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let services = state.registry.get_background_services_for_user(&user.id).await;
    let service = host_runtime::find_background_service(&services, &req.id, req.extension_name.as_deref())
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("Background service not found: {}", req.id)))?;
    let ext = state
        .registry
        .get_extension_by_name_for_user(&user.id, &service.extension_name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {}", service.extension_name)))?;
    if !ext.state.enabled {
        return Err(ApiError::BadRequest(format!(
            "Extension disabled: {}",
            service.extension_name
        )));
    }
    // E13 slice I: a Host-retained binding (snapshot + root) is consulted so a bound+files
    // service is verified through the retained root and never started from a path string.
    let status = state
        .host_runtime
        .background
        .start_with_binding(&user.id, &ext, &service, &state.host_runtime.signed_roots)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&status).unwrap_or_default())))
}

/// `POST /api/extensions/background-services/stop` — kill a previously started service.
async fn stop_background_service(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<BackgroundServiceActionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let services = state.registry.get_background_services_for_user(&user.id).await;
    let service = host_runtime::find_background_service(&services, &req.id, req.extension_name.as_deref())
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("Background service not found: {}", req.id)))?;
    let status = state
        .host_runtime
        .background
        .stop(&user.id, &service.extension_name, &service.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(serde_json::to_value(&status).unwrap_or_default())))
}

/// `GET /api/extensions/{extension_name}/assets/{*asset_path}` — serve an
/// extension asset under the trusted extension root.
async fn get_extension_asset(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((extension_name, asset_path)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    serve_extension_asset(&state, &user.id, &extension_name, &asset_path, "public, max-age=3600").await
}

/// `POST /api/extensions/{extension_name}/asset-token` — issue a short-lived
/// token that reads only this extension's assets (settings-tab iframe).
async fn issue_asset_token(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(extension_name): Path<String>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    state
        .registry
        .get_extension_by_name_for_user(&user.id, &extension_name)
        .await
        .filter(|ext| ext.state.enabled)
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {extension_name}")))?;
    let (token, expires_at) = state
        .asset_tokens
        .issue(&user.id, &extension_name)
        .map_err(|err| ApiError::Internal(format!("asset token: {err}")))?;
    let base_path = format!("/api/extensions/{extension_name}/asset-session/{token}/");
    Ok(Json(ApiResponse::ok(json!({
        "token": token,
        "expires_at": expires_at,
        "base_path": base_path,
    }))))
}

/// `GET /api/extensions/{extension_name}/asset-session/{token}/{*asset_path}`
async fn get_extension_asset_with_token(
    State(state): State<ExtensionRouterState>,
    Path((extension_name, token, asset_path)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let user_id = state
        .asset_tokens
        .resolve(&token, &extension_name)
        .ok_or_else(|| ApiError::Unauthorized("Invalid or expired extension asset token".into()))?;
    serve_extension_asset(&state, &user_id, &extension_name, &asset_path, "private, max-age=600").await
}

fn asset_gate_refusal(err: AssetServeGateError) -> ApiError {
    ApiError::Forbidden(format!(
        "Extension asset refused: {} (declared entryPointIntegrity cannot be enforced by a path serve)",
        err.reason()
    ))
}

fn verified_asset_refusal(err: VerifiedAssetError) -> ApiError {
    match err {
        VerifiedAssetError::Missing => ApiError::NotFound("Extension asset not found".into()),
        other => ApiError::Forbidden(format!("Extension asset refused: {}", other.reason())),
    }
}

/// `Ok(None)` only when there is no retained binding or its signed snapshot declares no `files`
/// (verified serve not applicable). Otherwise the asset is served verified or refused.
async fn try_verified_serve(
    state: &ExtensionRouterState,
    extension_name: &str,
    asset_path: &str,
    cache_control: &'static str,
) -> Result<Option<Response>, ApiError> {
    let Some((checked, root)) = state.host_runtime.signed_roots.lookup_clone(extension_name) else {
        return Ok(None);
    };
    let Some(index) = FilesIndex::from_signed(&checked).map_err(verified_asset_refusal)? else {
        return Ok(None);
    };
    let relative_path = normalize_relative_asset_path(asset_path).ok_or_else(|| {
        ApiError::Forbidden(format!(
            "Asset path escapes extension root: {extension_name}/{asset_path}"
        ))
    })?;
    let (listed_path, integrity) = index.resolve(&relative_path).map_err(verified_asset_refusal)?;
    let (listed_path, integrity) = (listed_path.to_owned(), integrity.to_owned());
    let asset = tokio::task::spawn_blocking(move || read_verified_listed(&root, &listed_path, &integrity))
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?
        .map_err(verified_asset_refusal)?;
    let content_type = content_type_for_path(FsPath::new(asset.path()));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(asset.into_bytes()))
        .map(Some)
        .map_err(|err| ApiError::Internal(err.to_string()))
}

async fn serve_extension_asset(
    state: &ExtensionRouterState,
    user_id: &str,
    extension_name: &str,
    asset_path: &str,
    cache_control: &'static str,
) -> Result<Response, ApiError> {
    let ext = state
        .registry
        .get_extension_by_name_for_user(user_id, extension_name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {extension_name}")))?;
    if !ext.state.enabled {
        return Err(ApiError::NotFound(format!("Extension not found: {extension_name}")));
    }

    // E13 slice F: a Host-retained signed root whose snapshot declares `files` is served ONLY
    // by verified serve (listed path, FD-relative O_NOFOLLOW read through the retained root,
    // bytes compared to the signed SRI). Every failure there is a refusal; there is no fallback
    // to the path-string serve below.
    if let Some(response) = try_verified_serve(state, extension_name, asset_path, cache_control).await? {
        return Ok(response);
    }

    // Legacy / no retained binding / snapshot without `files`: unchanged behaviour. Declared
    // panel/workPanelView entryPointIntegrity cannot be enforced by a path serve → refuse that
    // entry point; never an unchecked serve.
    let signed = state.host_runtime.signed_roots.lookup_checked(extension_name);
    let integrity_gate = AssetIntegrityGate::new(&ext.manifest, signed.as_ref()).map_err(asset_gate_refusal)?;

    let canonical_root = tokio::fs::canonicalize(&ext.directory)
        .await
        .map_err(map_asset_lookup_error)?;

    let relative_path = normalize_relative_asset_path(asset_path).ok_or_else(|| {
        ApiError::Forbidden(format!(
            "Asset path escapes extension root: {extension_name}/{asset_path}"
        ))
    })?;
    integrity_gate
        .check_requested(&relative_path)
        .map_err(asset_gate_refusal)?;

    let requested_path = canonical_root.join(&relative_path);
    let canonical_asset = tokio::fs::canonicalize(&requested_path)
        .await
        .map_err(map_asset_lookup_error)?;

    if !canonical_asset.starts_with(&canonical_root) {
        return Err(ApiError::Forbidden(format!(
            "Asset path escapes extension root: {}",
            canonical_asset.display()
        )));
    }

    integrity_gate
        .check_canonical(&canonical_root, &canonical_asset)
        .await
        .map_err(asset_gate_refusal)?;

    let bytes = tokio::fs::read(&canonical_asset)
        .await
        .map_err(map_asset_lookup_error)?;

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type_for_path(&canonical_asset))
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(bytes))
        .map_err(|err| ApiError::Internal(err.to_string()))
}

/// `GET /api/extensions/webui` — get all WebUI contributions.
async fn get_webui(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let webui = state.registry.get_webui_contributions_for_user(&user.id).await;
    let value = serde_json::to_value(&webui).unwrap_or_default();
    Ok(Json(ApiResponse::ok(value)))
}

/// `GET /api/extensions/agent-activity` — get agent activity snapshot.
///
/// Returns an empty object as a placeholder; real implementation will
/// integrate with the agent subsystem's activity tracking.
async fn get_agent_activity(
    State(_state): State<ExtensionRouterState>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    // Agent activity snapshot is a cross-module concern;
    // return an empty object for now.
    Ok(Json(ApiResponse::ok(serde_json::json!({}))))
}

/// `POST /api/extensions/i18n` — get i18n data for a locale.
async fn get_i18n(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<GetI18nRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<HashMap<String, HashMap<String, String>>>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let data = state.registry.get_i18n_for_locale_for_user(&user.id, &req.locale).await;
    Ok(Json(ApiResponse::ok(data)))
}

/// `POST /api/extensions/permissions` — get permission summary for an extension.
async fn get_permissions(
    State(state): State<ExtensionRouterState>,
    body: Result<Json<GetPermissionsRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PermissionSummaryResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let ext = state
        .registry
        .get_extension_by_name(&req.name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {}", req.name)))?;

    let permissions = ext.manifest.permissions.clone().unwrap_or_default();
    let summary = build_permission_summary(&permissions);
    let risk_level = calculate_risk_level(&permissions);

    let details: Vec<PermissionDetailResponse> = summary
        .details
        .into_iter()
        .map(|d| PermissionDetailResponse {
            permission: d.permission,
            level: enum_to_string(&d.level),
            description: d.description,
        })
        .collect();

    let resp = PermissionSummaryResponse {
        permissions: serde_json::to_value(&permissions).unwrap_or_default(),
        risk_level: enum_to_string(&risk_level),
        details,
    };

    Ok(Json(ApiResponse::ok(resp)))
}

/// `POST /api/extensions/risk-level` — get risk level for an extension.
async fn get_risk_level(
    State(state): State<ExtensionRouterState>,
    body: Result<Json<GetRiskLevelRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<serde_json::Value>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;

    let ext = state
        .registry
        .get_extension_by_name(&req.name)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("Extension not found: {}", req.name)))?;

    let permissions = ext.manifest.permissions.clone().unwrap_or_default();
    let risk_level = calculate_risk_level(&permissions);

    Ok(Json(ApiResponse::ok(
        serde_json::json!({ "riskLevel": enum_to_string(&risk_level) }),
    )))
}

// ---------------------------------------------------------------------------
// Management handlers
// ---------------------------------------------------------------------------

/// `POST /api/extensions/enable` — enable an extension.
async fn enable_extension(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<EnableExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state.registry.enable_extension_for_user(&user.id, &req.name).await?;
    Ok(Json(ApiResponse::success()))
}

/// `POST /api/extensions/disable` — disable an extension.
async fn disable_extension(
    State(state): State<ExtensionRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<DisableExtensionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .registry
        .disable_extension_for_user(&user.id, &req.name, req.reason.as_deref())
        .await?;
    Ok(Json(ApiResponse::success()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Serialize a serde enum to its JSON string representation.
fn enum_to_string<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn content_type_for_path(path: &FsPath) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    HeaderValue::from_str(mime.as_ref()).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

fn map_asset_lookup_error(error: std::io::Error) -> ApiError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ApiError::NotFound("Extension asset not found".into()),
        _ => ApiError::Internal(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::state::ExtensionStateStore;
    use crate::{ExtensionSource, ScanPath};
    use aionui_realtime::BroadcastEventBus;

    fn make_state() -> ExtensionRouterState {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        std::mem::forget(tmp);
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        ExtensionRouterState::new(registry)
    }

    #[test]
    fn extension_routes_builds_router() {
        let state = make_state();
        let _router = extension_routes(state);
    }

    #[test]
    fn extension_path_traversal_maps_to_bad_request() {
        let api_err = ApiError::from(ExtensionError::PathTraversal("../secret".into()));
        assert!(matches!(api_err, ApiError::BadRequest(_)));
    }

    #[test]
    fn extension_manifest_validation_maps_to_bad_request() {
        let api_err = ApiError::from(ExtensionError::ManifestValidation("test".into()));
        assert!(matches!(api_err, ApiError::BadRequest(_)));
    }

    #[test]
    fn extension_file_reference_not_found_maps_to_not_found() {
        let api_err = ApiError::from(ExtensionError::FileReferenceNotFound("missing.md".into()));
        assert!(matches!(api_err, ApiError::NotFound(_)));
    }

    #[test]
    fn extension_io_error_maps_to_internal() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let api_err = ApiError::from(ExtensionError::Io(io_err));
        assert!(matches!(api_err, ApiError::Internal(_)));
    }

    async fn make_router_with_extension() -> (Router, tempfile::TempDir, PathBuf) {
        let (state, tmp, ext_dir) = make_state_with_extension().await;
        (
            extension_routes(state).layer(Extension(CurrentUser::local_default())),
            tmp,
            ext_dir,
        )
    }

    async fn make_state_with_extension() -> (ExtensionRouterState, tempfile::TempDir, PathBuf) {
        make_state_with_manifest(serde_json::json!({"name": "hello", "version": "1.0.0"})).await
    }

    async fn make_state_with_manifest(
        manifest: serde_json::Value,
    ) -> (ExtensionRouterState, tempfile::TempDir, PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("hello");
        std::fs::create_dir_all(ext_dir.join("settings")).unwrap();
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(ext_dir.join("settings").join("index.html"), "<h1>Hello</h1>").unwrap();

        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Env,
            }])
            .await
            .unwrap();

        (ExtensionRouterState::new(registry), tmp, ext_dir)
    }

    #[tokio::test]
    async fn agent_tool_route_does_not_trust_client_plan_mode() {
        let (state, _tmp, _ext_dir) = make_state_with_manifest(serde_json::json!({
            "name": "hello",
            "version": "1.0.0",
            "contributes": {
                "agentTools": [
                    {"id": "demo.read", "name": "Read", "planSafeActions": true},
                    {"id": "demo.write", "name": "Write", "planSafeActions": false}
                ]
            }
        }))
        .await;
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        for plan_mode in [None, Some(false), Some(true)] {
            for (action, expected) in [("read", StatusCode::OK), ("write", StatusCode::FORBIDDEN)] {
                let mut payload = serde_json::json!({"id": format!("ext-hello-demo.{action}")});
                if let Some(value) = plan_mode {
                    payload["planMode"] = serde_json::json!(value);
                }
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/extensions/agent-tools/invoke")
                            .header("content-type", "application/json")
                            .body(Body::from(payload.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                // The safe positive control also proves a missing fixture did
                // not make every request fail with a misleading 404.
                assert_eq!(response.status(), expected, "action={action}, plan_mode={plan_mode:?}");
                if expected == StatusCode::OK {
                    let body: serde_json::Value =
                        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
                    assert_eq!(body["data"]["status"], "refused");
                    assert_eq!(body["data"]["dispatched"], false);
                    assert_eq!(body["data"]["reason"], "integrity_binding_unavailable");
                    assert_ne!(body["data"]["reason"], "handler_stub");
                }
            }
        }
    }

    async fn asset_status(router: &Router, uri: &str) -> StatusCode {
        router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    fn write_panel_assets(ext_dir: &std::path::Path) {
        std::fs::create_dir_all(ext_dir.join("panels")).unwrap();
        std::fs::write(ext_dir.join("panels/board.html"), "<p>board</p>").unwrap();
        std::fs::write(ext_dir.join("panels/board.css"), "p{}").unwrap();
    }

    fn integrity_panel_manifest(with_integrity: bool) -> serde_json::Value {
        let mut panel = serde_json::json!({"id": "board", "title": "Board", "entryPoint": "panels/board.html"});
        if with_integrity {
            panel["entryPointIntegrity"] = serde_json::json!(crate::hub::curated::sri_sha512(b"<p>board</p>"));
        }
        serde_json::json!({"name": "hello", "version": "1.0.0", "contributes": {"panels": [panel]}})
    }

    #[tokio::test]
    async fn asset_route_refuses_declared_panel_entry_but_serves_other_assets() {
        let (state, _tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(true)).await;
        write_panel_assets(&ext_dir);
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/./panels/board.html").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.css").await,
            StatusCode::OK
        );
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/settings/index.html").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn asset_route_serves_undeclared_panel_entry_legacy() {
        let (state, _tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(false)).await;
        write_panel_assets(&ext_dir);
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::OK
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn asset_route_refuses_symlink_alias_of_declared_entry() {
        let (state, _tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(true)).await;
        write_panel_assets(&ext_dir);
        std::os::unix::fs::symlink(ext_dir.join("panels/board.html"), ext_dir.join("alias.html")).unwrap();
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/alias.html").await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn asset_session_route_refuses_declared_work_panel_view_entry() {
        let mut manifest = integrity_panel_manifest(false);
        manifest["contributes"] = serde_json::json!({"workPanelViews": [{
            "id": "v", "title": "V", "entryPoint": "panels/board.html",
            "entryPointIntegrity": crate::hub::curated::sri_sha512(b"<p>board</p>")
        }]});
        let (state, _tmp, ext_dir) = make_state_with_manifest(manifest).await;
        write_panel_assets(&ext_dir);
        let (token, _) = state
            .asset_tokens
            .issue(&CurrentUser::local_default().id, "hello")
            .unwrap();
        let router = extension_routes(state.clone())
            .layer(Extension(CurrentUser::local_default()))
            .merge(extension_asset_session_routes(state));
        let base = format!("/api/extensions/hello/asset-session/{token}");
        assert_eq!(
            asset_status(&router, &format!("{base}/panels/board.html")).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            asset_status(&router, &format!("{base}/panels/board.css")).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn asset_route_refuses_entry_declared_only_in_host_retained_signed_snapshot() {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::{HashMap, HashSet};

        // Loaded manifest has NO integrity (stripped); signed snapshot declares it.
        let (state, tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(false)).await;
        write_panel_assets(&ext_dir);
        let signed_raw = serde_json::to_vec(&integrity_panel_manifest(true)).unwrap();
        let key = SigningKey::from_bytes(&[17; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(&signed_raw);
        let signature = hex::encode(
            key.sign(signature_payload("hello", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        let checked = check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name: "hello",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            &signed_raw,
        )
        .unwrap();
        // Honest Host-created root for the test: the tempdir the test itself created.
        let root = crate::hub::test_support::open_dir(tmp.path());
        state.host_runtime.signed_roots.install("hello", checked, root).unwrap();
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.css").await,
            StatusCode::OK
        );
    }

    // -- slice B3: panel / work-panel-view listing needs a bound, signed-`files`-listed entry --

    const LISTED: &[u8] = b"<p>listed</p>";

    fn listing_manifest(with_files: bool) -> serde_json::Value {
        use crate::hub::curated::sri_sha512;
        let mut m = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "contributes": {
                "commands": [{"id": "board", "title": "Board"}],
                "panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}],
                "workPanelViews": [{"id": "view", "title": "View", "entryPoint": "views/view.html"}],
            }
        });
        if with_files {
            m["files"] = serde_json::json!([
                {"path": "panels/board.html", "integrity": sri_sha512(LISTED)},
                {"path": "views/view.html", "integrity": sri_sha512(LISTED)},
            ]);
        }
        m
    }

    /// State whose registry lists `listing_manifest`; optionally bound to its signed snapshot.
    async fn listing_state(with_files: bool, bind: bool) -> (Router, tempfile::TempDir) {
        let manifest = listing_manifest(with_files);
        let (state, tmp, _ext_dir) = make_state_with_manifest(manifest.clone()).await;
        if bind {
            let checked = check_for_hello(&serde_json::to_vec_pretty(&manifest).unwrap());
            let root = crate::hub::test_support::open_dir(tmp.path());
            state.host_runtime.signed_roots.install("hello", checked, root).unwrap();
        }
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        (router, tmp)
    }

    async fn get_json_data(router: &Router, uri: &str) -> serde_json::Value {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"].clone()
    }

    async fn invoke_board(router: &Router) -> serde_json::Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/extensions/commands/invoke")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"id":"board","extensionName":"hello"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["data"].clone()
    }

    #[tokio::test]
    async fn bound_signed_entries_listed_in_files_are_listed_with_unchanged_shape() {
        let (router, _tmp) = listing_state(true, true).await;
        let panels = get_json_data(&router, "/api/extensions/panels").await;
        assert_eq!(
            panels,
            serde_json::json!([{
                "extensionName": "hello",
                "id": "ext-hello-board",
                "title": "Board",
                "entryPoint": "/api/extensions/hello/assets/panels/board.html",
            }])
        );
        let views = get_json_data(&router, "/api/extensions/work-panel-views").await;
        assert_eq!(
            views,
            serde_json::json!([{
                "extensionName": "hello",
                "id": "ext-hello-view",
                "title": "View",
                "entryPoint": "/api/extensions/hello/assets/views/view.html",
            }])
        );
        let invoked = invoke_board(&router).await;
        assert_eq!(invoked["openPanel"]["id"], "ext-hello-board");
    }

    #[tokio::test]
    async fn unbound_extensions_list_no_panels_or_views_and_give_no_panel_hint() {
        for with_files in [true, false] {
            let (router, _tmp) = listing_state(with_files, false).await;
            assert_eq!(
                get_json_data(&router, "/api/extensions/panels").await,
                serde_json::json!([])
            );
            assert_eq!(
                get_json_data(&router, "/api/extensions/work-panel-views").await,
                serde_json::json!([])
            );
            let invoked = invoke_board(&router).await;
            assert_eq!(invoked["status"], "accepted");
            assert!(invoked.get("openPanel").is_none_or(|v| v.is_null()), "{invoked}");
        }
    }

    #[tokio::test]
    async fn bound_snapshot_without_files_lists_nothing() {
        let (router, _tmp) = listing_state(false, true).await;
        assert_eq!(
            get_json_data(&router, "/api/extensions/panels").await,
            serde_json::json!([])
        );
        assert_eq!(
            get_json_data(&router, "/api/extensions/work-panel-views").await,
            serde_json::json!([])
        );
        let invoked = invoke_board(&router).await;
        assert!(invoked.get("openPanel").is_none_or(|v| v.is_null()), "{invoked}");
    }

    // -- slice F: verified serve from the retained signed root --

    #[cfg(unix)]
    const V_HTML: &[u8] = b"<p>verified board</p>";
    #[cfg(unix)]
    const V_CSS: &[u8] = b"p{color:green}";

    #[cfg(unix)]
    fn signed_files_manifest() -> Vec<u8> {
        use crate::hub::curated::sri_sha512;
        serde_json::to_vec(&serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [
                {"path": "panels/board.html", "integrity": sri_sha512(V_HTML)},
                {"path": "panels/board.css", "integrity": sri_sha512(V_CSS)},
            ],
            "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
        }))
        .unwrap()
    }

    fn check_for_hello(raw: &[u8]) -> crate::hub::signed_manifest::SignatureCheckedManifest {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::{HashMap, HashSet};
        let key = SigningKey::from_bytes(&[17; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            key.sign(signature_payload("hello", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name: "hello",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    /// Router whose on-disk extension dir holds DIFFERENT (evil) bytes than the retained root.
    #[cfg(unix)]
    async fn verified_fixture() -> (Router, ExtensionRouterState, tempfile::TempDir, tempfile::TempDir) {
        let (state, tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(false)).await;
        std::fs::create_dir_all(ext_dir.join("panels")).unwrap();
        std::fs::write(ext_dir.join("panels/board.html"), "EVIL-HTML").unwrap();
        std::fs::write(ext_dir.join("panels/board.css"), "EVIL-CSS").unwrap();
        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("panels")).unwrap();
        std::fs::write(host_root.path().join("panels/board.html"), V_HTML).unwrap();
        std::fs::write(host_root.path().join("panels/board.css"), V_CSS).unwrap();
        std::fs::write(host_root.path().join("unlisted.txt"), "x").unwrap();
        let checked = check_for_hello(&signed_files_manifest());
        let root = std::fs::File::open(host_root.path()).unwrap();
        state.host_runtime.signed_roots.install("hello", checked, root).unwrap();
        let router = extension_routes(state.clone()).layer(Extension(CurrentUser::local_default()));
        (router, state, tmp, host_root)
    }

    #[cfg(unix)]
    async fn asset_body(router: &Router, uri: &str) -> (StatusCode, String, Option<String>) {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let ctype = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned(), ctype)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_returns_retained_root_bytes_for_entry_and_other_listed_files() {
        let (router, _state, _tmp, _root) = verified_fixture().await;
        let (status, body, ctype) = asset_body(&router, "/api/extensions/hello/assets/panels/board.html").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "<p>verified board</p>"));
        assert_eq!(ctype.as_deref(), Some("text/html"));
        let (status, body, ctype) = asset_body(&router, "/api/extensions/hello/assets/./panels/board.css").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "p{color:green}"));
        assert_eq!(ctype.as_deref(), Some("text/css"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_also_covers_the_asset_session_route() {
        let (_router, state, _tmp, _root) = verified_fixture().await;
        let (token, _) = state
            .asset_tokens
            .issue(&CurrentUser::local_default().id, "hello")
            .unwrap();
        let router = extension_asset_session_routes(state);
        let base = format!("/api/extensions/hello/asset-session/{token}");
        let (status, body, _) = asset_body(&router, &format!("{base}/panels/board.html")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "<p>verified board</p>"));
        let (status, _, _) = asset_body(&router, &format!("{base}/settings/index.html")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_refuses_unlisted_paths_even_if_present_on_disk_or_in_root() {
        let (router, _state, _tmp, _root) = verified_fixture().await;
        for uri in [
            "/api/extensions/hello/assets/settings/index.html", // on disk, not listed
            "/api/extensions/hello/assets/unlisted.txt",        // in root, not listed
            "/api/extensions/hello/assets/aion-extension.json", // manifest is never listed
            "/api/extensions/hello/assets/panels",
            "/api/extensions/hello/assets/panels/BOARD.html",
            "/api/extensions/hello/assets/%2E%2E%2Fsecret.txt",
        ] {
            assert_eq!(asset_status(&router, uri).await, StatusCode::FORBIDDEN, "{uri}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_refuses_tampered_symlinked_and_missing_listed_files_without_path_fallback() {
        let (router, _state, _tmp, root) = verified_fixture().await;
        std::fs::write(root.path().join("panels/board.css"), "p{color:blue}").unwrap();
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.css").await,
            StatusCode::FORBIDDEN
        );
        std::fs::remove_file(root.path().join("panels/board.html")).unwrap();
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::NOT_FOUND
        );
        // A symlink with matching bytes is still refused; the on-disk copy is never consulted.
        std::fs::write(root.path().join("real.html"), V_HTML).unwrap();
        std::os::unix::fs::symlink(root.path().join("real.html"), root.path().join("panels/board.html")).unwrap();
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::FORBIDDEN
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_uses_signed_files_not_the_loaded_manifest() {
        // Loaded manifest declares per-entry integrity for a different body; the retained signed
        // snapshot (files) is what decides, and it is served verified (no legacy 403 gate).
        let (state, _tmp, ext_dir) = make_state_with_manifest(integrity_panel_manifest(true)).await;
        write_panel_assets(&ext_dir);
        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("panels")).unwrap();
        std::fs::write(host_root.path().join("panels/board.html"), V_HTML).unwrap();
        std::fs::write(host_root.path().join("panels/board.css"), V_CSS).unwrap();
        state
            .host_runtime
            .signed_roots
            .install(
                "hello",
                check_for_hello(&signed_files_manifest()),
                std::fs::File::open(host_root.path()).unwrap(),
            )
            .unwrap();
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/panels/board.html").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "<p>verified board</p>"));
    }

    #[tokio::test]
    async fn without_retained_binding_files_manifest_keeps_legacy_gate_for_entries() {
        // No binding: unchanged behaviour. The loaded manifest's files-inherited integrity gates
        // the entry (403); a path serve of other assets is exactly what it was before.
        let sri = crate::hub::curated::sri_sha512(b"<p>board</p>");
        let manifest = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [{"path": "panels/board.html", "integrity": sri}],
            "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
        });
        let (state, _tmp, ext_dir) = make_state_with_manifest(manifest).await;
        write_panel_assets(&ext_dir);
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.html").await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/panels/board.css").await,
            StatusCode::OK
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn removing_the_binding_returns_to_legacy_behaviour() {
        let (router, state, _tmp, _root) = verified_fixture().await;
        assert!(state.host_runtime.signed_roots.remove("hello"));
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/panels/board.css").await;
        // Legacy path serve of the on-disk copy (loaded manifest has no integrity).
        assert_eq!((status, body.as_str()), (StatusCode::OK, "EVIL-CSS"));
    }

    #[cfg(unix)]
    const V_TAB: &[u8] = b"<h1>settings</h1>";
    #[cfg(unix)]
    const V_ICON: &[u8] = b"<svg></svg>";
    #[cfg(unix)]
    const V_COVER: &[u8] = b"PNGCOVER";
    #[cfg(unix)]
    const V_THEME_CSS: &[u8] = b":root{--bg:#000}";

    /// Bound+files serve of settings-tab, icon and theme-cover paths (the kinds slice J now
    /// requires to be listed). Same retained-root / O_NOFOLLOW / SRI path as slice F; unlisted 403.
    #[cfg(unix)]
    #[tokio::test]
    async fn verified_serve_covers_listed_settings_icon_and_theme_cover() {
        use crate::hub::curated::sri_sha512;
        let loaded = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "contributes": {
                "settingsTabs": [{"id": "t", "label": "T", "url": "settings/index.html", "icon": "icons/gear.svg"}],
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css", "cover_image": "themes/cover.png"}]
            }
        });
        let (state, tmp, ext_dir) = make_state_with_manifest(loaded).await;
        std::fs::create_dir_all(ext_dir.join("settings")).unwrap();
        std::fs::create_dir_all(ext_dir.join("icons")).unwrap();
        std::fs::create_dir_all(ext_dir.join("themes")).unwrap();
        std::fs::write(ext_dir.join("settings/index.html"), "EVIL-TAB").unwrap();
        std::fs::write(ext_dir.join("icons/gear.svg"), "EVIL-ICON").unwrap();
        std::fs::write(ext_dir.join("themes/cover.png"), "EVIL-COVER").unwrap();
        std::fs::write(ext_dir.join("themes/dark.css"), "EVIL-CSS").unwrap();
        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("settings")).unwrap();
        std::fs::create_dir_all(host_root.path().join("icons")).unwrap();
        std::fs::create_dir_all(host_root.path().join("themes")).unwrap();
        std::fs::write(host_root.path().join("settings/index.html"), V_TAB).unwrap();
        std::fs::write(host_root.path().join("icons/gear.svg"), V_ICON).unwrap();
        std::fs::write(host_root.path().join("themes/cover.png"), V_COVER).unwrap();
        std::fs::write(host_root.path().join("themes/dark.css"), V_THEME_CSS).unwrap();
        std::fs::write(host_root.path().join("unlisted.css"), "x").unwrap();
        let signed = serde_json::to_vec(&serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [
                {"path": "settings/index.html", "integrity": sri_sha512(V_TAB)},
                {"path": "icons/gear.svg", "integrity": sri_sha512(V_ICON)},
                {"path": "themes/cover.png", "integrity": sri_sha512(V_COVER)},
                {"path": "themes/dark.css", "integrity": sri_sha512(V_THEME_CSS)},
            ],
            "contributes": {
                "settingsTabs": [{"id": "t", "label": "T", "url": "settings/index.html", "icon": "icons/gear.svg"}],
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css", "cover_image": "themes/cover.png"}]
            }
        }))
        .unwrap();
        state
            .host_runtime
            .signed_roots
            .install(
                "hello",
                check_for_hello(&signed),
                std::fs::File::open(host_root.path()).unwrap(),
            )
            .unwrap();
        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        let _ = tmp;
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/settings/index.html").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "<h1>settings</h1>"));
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/icons/gear.svg").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "<svg></svg>"));
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/themes/cover.png").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "PNGCOVER"));
        // css_file is now listed (required for bound resolve) so a request for it is verified-served.
        let (status, body, _) = asset_body(&router, "/api/extensions/hello/assets/themes/dark.css").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, ":root{--bg:#000}"));
        assert_eq!(
            asset_status(&router, "/api/extensions/hello/assets/unlisted.css").await,
            StatusCode::FORBIDDEN
        );
    }

    /// Themes API inlines CSS: for bound+files the bytes come from the retained root, not ext_dir.
    /// Bound extension fixture for the i18n route: the loaded dir holds EVIL json, the retained
    /// root holds the signed one (`good` unless `root_body` overrides it).
    #[cfg(unix)]
    async fn i18n_bound_router(root_body: &[u8], signed_body: &[u8]) -> (Router, tempfile::TempDir, tempfile::TempDir) {
        use crate::hub::curated::sri_sha512;
        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("hello");
        std::fs::create_dir_all(ext_dir.join("i18n")).unwrap();
        let loaded = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "i18n": {"locales": ["en"], "directory": "i18n"}
        });
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&loaded).unwrap(),
        )
        .unwrap();
        std::fs::write(ext_dir.join("i18n/en.json"), r#"{"k":"EVIL"}"#).unwrap();
        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("i18n")).unwrap();
        std::fs::write(host_root.path().join("i18n/en.json"), root_body).unwrap();
        let signed = serde_json::to_vec(&serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [{"path": "i18n/en.json", "integrity": sri_sha512(signed_body)}],
            "i18n": {"locales": ["en"], "directory": "i18n"}
        }))
        .unwrap();
        std::fs::write(host_root.path().join("aion-extension.json"), &signed).unwrap();
        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        let state = ExtensionRouterState::new(registry.clone());
        registry.set_signed_roots(state.host_runtime.signed_roots.clone());
        state
            .host_runtime
            .signed_roots
            .install(
                "hello",
                check_for_hello(&signed),
                std::fs::File::open(host_root.path()).unwrap(),
            )
            .unwrap();
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Env,
            }])
            .await
            .unwrap();
        (
            extension_routes(state).layer(Extension(CurrentUser::local_default())),
            tmp,
            host_root,
        )
    }

    #[cfg(unix)]
    async fn post_i18n(router: &Router, locale: &str) -> serde_json::Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/extensions/i18n")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::json!({"locale": locale}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        body["data"].clone()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_files_i18n_route_returns_verified_root_messages() {
        let good: &[u8] = br#"{"k":"good"}"#;
        let (router, _tmp, _root) = i18n_bound_router(good, good).await;
        let data = post_i18n(&router, "en").await;
        assert_eq!(data["hello"]["k"], "good");
        assert!(post_i18n(&router, "fr").await.as_object().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_files_i18n_route_omits_extension_when_root_file_is_tampered() {
        let good: &[u8] = br#"{"k":"good"}"#;
        // Retained root now holds different bytes than the signed SRI; loaded dir is never consulted.
        let (router, _tmp, _root) = i18n_bound_router(br#"{"k":"tampered"}"#, good).await;
        let data = post_i18n(&router, "en").await;
        assert!(data.as_object().unwrap().is_empty(), "{data}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_files_get_themes_inlines_verified_css() {
        use crate::hub::curated::sri_sha512;

        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("hello");
        std::fs::create_dir_all(ext_dir.join("themes")).unwrap();
        let loaded = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "contributes": {
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css"}]
            }
        });
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&loaded).unwrap(),
        )
        .unwrap();
        std::fs::write(ext_dir.join("themes/dark.css"), "EVIL-CSS").unwrap();

        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("themes")).unwrap();
        std::fs::write(host_root.path().join("themes/dark.css"), V_THEME_CSS).unwrap();
        let signed = serde_json::to_vec(&serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [{"path": "themes/dark.css", "integrity": sri_sha512(V_THEME_CSS)}],
            "contributes": {
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css"}]
            }
        }))
        .unwrap();
        std::fs::write(host_root.path().join("aion-extension.json"), &signed).unwrap();

        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        let state = ExtensionRouterState::new(registry.clone());
        // Same store the Host wires before initialize, so resolve sees the binding.
        registry.set_signed_roots(state.host_runtime.signed_roots.clone());
        state
            .host_runtime
            .signed_roots
            .install(
                "hello",
                check_for_hello(&signed),
                std::fs::File::open(host_root.path()).unwrap(),
            )
            .unwrap();
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Env,
            }])
            .await
            .unwrap();

        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/themes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let themes = body["data"].as_array().unwrap();
        assert_eq!(themes.len(), 1);
        assert_eq!(themes[0]["css"], ":root{--bg:#000}");
        assert_ne!(themes[0]["css"], "EVIL-CSS");
        let _ = tmp;
        let _ = host_root;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bound_files_get_themes_omits_theme_when_css_is_unverified() {
        use crate::hub::curated::sri_sha512;

        let tmp = tempfile::TempDir::new().unwrap();
        let ext_root = tmp.path().join("extensions");
        let ext_dir = ext_root.join("hello");
        std::fs::create_dir_all(ext_dir.join("themes")).unwrap();
        let loaded = serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "contributes": {
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css"}]
            }
        });
        std::fs::write(
            ext_dir.join("aion-extension.json"),
            serde_json::to_vec_pretty(&loaded).unwrap(),
        )
        .unwrap();
        // Loaded dir has valid-looking CSS; the retained root's listed file is tampered.
        std::fs::write(ext_dir.join("themes/dark.css"), V_THEME_CSS).unwrap();
        let host_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(host_root.path().join("themes")).unwrap();
        std::fs::write(host_root.path().join("themes/dark.css"), "tampered").unwrap();
        let signed = serde_json::to_vec(&serde_json::json!({
            "name": "hello", "version": "1.0.0",
            "files": [{"path": "themes/dark.css", "integrity": sri_sha512(V_THEME_CSS)}],
            "contributes": {
                "themes": [{"id": "th", "name": "Th", "css_file": "themes/dark.css"}]
            }
        }))
        .unwrap();
        std::fs::write(host_root.path().join("aion-extension.json"), &signed).unwrap();

        let store = ExtensionStateStore::new(tmp.path().join("states.json"));
        let bus = Arc::new(BroadcastEventBus::new(64));
        let registry = ExtensionRegistry::new(store, bus, "1.0.0".into());
        let state = ExtensionRouterState::new(registry.clone());
        registry.set_signed_roots(state.host_runtime.signed_roots.clone());
        state
            .host_runtime
            .signed_roots
            .install(
                "hello",
                check_for_hello(&signed),
                std::fs::File::open(host_root.path()).unwrap(),
            )
            .unwrap();
        registry
            .initialize_with_scan_paths(vec![ScanPath {
                path: ext_root,
                source: ExtensionSource::Env,
            }])
            .await
            .unwrap();

        let router = extension_routes(state).layer(Extension(CurrentUser::local_default()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/themes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert!(body["data"].as_array().unwrap().is_empty(), "{body}");
        let _ = tmp;
        let _ = host_root;
    }

    #[tokio::test]
    async fn asset_token_serves_only_its_extension_without_auth() {
        let (state, _tmp, _ext_dir) = make_state_with_extension().await;
        let router = extension_routes(state.clone())
            .layer(Extension(CurrentUser::local_default()))
            .merge(extension_asset_session_routes(state));

        let issued = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/extensions/hello/asset-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&issued.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let base = body["data"]["base_path"].as_str().unwrap().to_owned();
        let token = body["data"]["token"].as_str().unwrap().to_owned();
        assert!(base.ends_with(&format!("/asset-session/{token}/")));

        let get = |uri: String| {
            let router = router.clone();
            async move {
                router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        };
        let ok = get(format!("{base}settings/index.html")).await;
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()[header::CACHE_CONTROL], "private, max-age=600");
        assert_eq!(ok.into_body().collect().await.unwrap().to_bytes(), "<h1>Hello</h1>");

        let bad = get("/api/extensions/hello/asset-session/deadbeef/settings/index.html".into()).await;
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
        let other_ext = get(format!(
            "/api/extensions/other/asset-session/{token}/settings/index.html"
        ))
        .await;
        assert_eq!(other_ext.status(), StatusCode::UNAUTHORIZED);
        let traversal = get(format!("{base}%2E%2E%2Fsecret.txt")).await;
        assert_eq!(traversal.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn asset_token_refused_for_unknown_extension() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/extensions/unknown/asset-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_extension_asset_serves_local_file() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/settings/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "public, max-age=3600");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes, "<h1>Hello</h1>");
    }

    #[tokio::test]
    async fn get_extension_asset_rejects_traversal() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/%2E%2E%2Fsecret.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn get_extension_asset_returns_not_found_for_missing_file() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/hello/assets/settings/missing.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_extension_asset_returns_not_found_for_unknown_extension() {
        let (router, _tmp, _ext_dir) = make_router_with_extension().await;
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/api/extensions/unknown/assets/settings/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

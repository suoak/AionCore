#![allow(clippy::disallowed_types)]
//! `/api/governance/*` (M8 E12-3): roles, agent run permissions and launch
//! policies.
//!
//! Signed-in user only: agent and device tokens get 403 (and are absent from
//! the agent / device route whitelists). Roles only apply in multi-user
//! (WebUI) mode; on the desktop the single user is always `owner`.
//!
//! - `GET  /api/governance/me` — effective role and capabilities.
//! - `GET  /api/governance/policies` — any user (so the UI can explain a
//!   refusal); `PUT /api/governance/policies/{key}` — owner / admin.
//! - `GET  /api/governance/roles` — owner / admin; `PUT
//!   /api/governance/roles/{user_id}` — owner, multi-user mode only.
//! - `GET / PUT /api/governance/agent-permissions` — owner / admin.
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use aionui_db::{
    AgentRunPermissionRow, GovernanceError, GovernanceService, IUserRepository, PolicyKey, PolicySet, UserRole,
};

use crate::device::DevicePrincipal;
use crate::middleware::{AgentPrincipal, CurrentUser};

#[derive(Clone)]
pub struct GovernanceRouterState {
    pub governance: Arc<GovernanceService>,
    pub users: Arc<dyn IUserRepository>,
}

fn ensure_user_session(
    device: &Option<Extension<DevicePrincipal>>,
    agent: &Option<Extension<AgentPrincipal>>,
) -> Result<(), ApiError> {
    if device.is_some() || agent.is_some() {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "GOVERNANCE_REQUIRES_USER",
            "Governance settings are only available to the signed-in user.",
            None,
        ));
    }
    Ok(())
}

fn db_unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "governance store unavailable");
    ApiError::Internal("Governance store unavailable".into())
}

fn map_governance_error(error: GovernanceError) -> ApiError {
    match error {
        GovernanceError::Invalid(reason) => ApiError::BadRequest(reason),
        GovernanceError::Conflict(reason) => ApiError::Conflict(reason),
        GovernanceError::Db(error) => db_unavailable(error),
    }
}

fn forbidden(message: &str) -> ApiError {
    ApiError::coded(StatusCode::FORBIDDEN, "GOVERNANCE_ROLE_REQUIRED", message, None)
}

async fn role_of(state: &GovernanceRouterState, user: &CurrentUser) -> Result<UserRole, ApiError> {
    state.governance.role_of(&user.id).await.map_err(db_unavailable)
}

async fn require_policy_manager(state: &GovernanceRouterState, user: &CurrentUser) -> Result<UserRole, ApiError> {
    let role = role_of(state, user).await?;
    if !role.can_manage_policies() {
        return Err(forbidden("Only an owner or admin can change governance settings."));
    }
    Ok(role)
}

#[derive(Debug, Serialize)]
pub struct GovernanceMeView {
    pub user_id: String,
    pub role: &'static str,
    /// False on the desktop (single user, always owner).
    pub roles_active: bool,
    pub can_manage_policies: bool,
    pub can_manage_roles: bool,
    pub can_view_all_audit: bool,
}

async fn me_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<GovernanceMeView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let role = role_of(&state, &user).await?;
    Ok(Json(ApiResponse::ok(GovernanceMeView {
        user_id: user.id.clone(),
        role: role.as_str(),
        roles_active: state.governance.roles_active(),
        can_manage_policies: role.can_manage_policies(),
        can_manage_roles: role.can_manage_roles() && state.governance.roles_active(),
        can_view_all_audit: role.can_view_all_audit(),
    })))
}

#[derive(Debug, Serialize)]
pub struct PolicyEntryView {
    pub key: &'static str,
    pub value: Value,
    pub updated_by: Option<String>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct PoliciesView {
    pub items: Vec<PolicyEntryView>,
}

async fn policies_view(state: &GovernanceRouterState) -> Result<PoliciesView, ApiError> {
    let rows = state.governance.policy_rows().await.map_err(db_unavailable)?;
    let set = PolicySet::from_rows(&rows);
    let items = PolicyKey::ALL
        .iter()
        .map(|key| {
            let row = rows.iter().find(|row| row.key == key.as_str());
            PolicyEntryView {
                key: key.as_str(),
                value: set.value_of(*key),
                updated_by: row.and_then(|row| row.updated_by.clone()),
                updated_at: row.map(|row| row.updated_at),
            }
        })
        .collect();
    Ok(PoliciesView { items })
}

async fn list_policies_handler(
    State(state): State<GovernanceRouterState>,
    Extension(_user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<PoliciesView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    Ok(Json(ApiResponse::ok(policies_view(&state).await?)))
}

#[derive(Debug, Deserialize)]
pub struct SetPolicyBody {
    pub value: Value,
}

async fn set_policy_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(key): Path<String>,
    Json(body): Json<SetPolicyBody>,
) -> Result<Json<ApiResponse<PoliciesView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    require_policy_manager(&state, &user).await?;
    let key = PolicyKey::parse(&key).ok_or_else(|| ApiError::NotFound(format!("unknown policy `{key}`")))?;
    state
        .governance
        .set_policy(&user.id, key, &body.value, None)
        .await
        .map_err(map_governance_error)?;
    Ok(Json(ApiResponse::ok(policies_view(&state).await?)))
}

#[derive(Debug, Serialize)]
pub struct UserRoleView {
    pub user_id: String,
    pub username: Option<String>,
    pub role: &'static str,
    /// False when the role is the default (no stored grant).
    pub explicit: bool,
    pub granted_by: Option<String>,
    pub granted_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct RolesView {
    pub roles_active: bool,
    pub items: Vec<UserRoleView>,
}

async fn roles_view(state: &GovernanceRouterState) -> Result<RolesView, ApiError> {
    let users = state.users.list_users().await.map_err(db_unavailable)?;
    let rows = state
        .governance
        .repository()
        .list_roles()
        .await
        .map_err(db_unavailable)?;
    let mut items = Vec::with_capacity(users.len());
    for user in users {
        let role = state.governance.role_of(&user.id).await.map_err(db_unavailable)?;
        let row = rows.iter().find(|row| row.user_id == user.id);
        items.push(UserRoleView {
            user_id: user.id.clone(),
            username: user.username.clone(),
            role: role.as_str(),
            explicit: row.is_some(),
            granted_by: row.and_then(|row| row.granted_by.clone()),
            granted_at: row.map(|row| row.granted_at),
        });
    }
    Ok(RolesView {
        roles_active: state.governance.roles_active(),
        items,
    })
}

async fn list_roles_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<RolesView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    require_policy_manager(&state, &user).await?;
    Ok(Json(ApiResponse::ok(roles_view(&state).await?)))
}

#[derive(Debug, Deserialize)]
pub struct SetRoleBody {
    pub role: String,
}

async fn set_role_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(target): Path<String>,
    Json(body): Json<SetRoleBody>,
) -> Result<Json<ApiResponse<RolesView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    if !state.governance.roles_active() {
        return Err(ApiError::coded(
            StatusCode::CONFLICT,
            "GOVERNANCE_ROLES_INACTIVE",
            "Roles only apply in multi-user (WebUI) mode; the desktop user is always the owner.",
            None,
        ));
    }
    let role = role_of(&state, &user).await?;
    if !role.can_manage_roles() {
        return Err(forbidden("Only an owner can change roles."));
    }
    let new_role = UserRole::parse(&body.role)
        .ok_or_else(|| ApiError::BadRequest("role must be one of owner, admin, member".into()))?;
    if state.users.find_by_id(&target).await.map_err(db_unavailable)?.is_none() {
        return Err(ApiError::NotFound(format!("user `{target}` not found")));
    }
    state
        .governance
        .set_role(&user.id, &target, new_role, None)
        .await
        .map_err(map_governance_error)?;
    Ok(Json(ApiResponse::ok(roles_view(&state).await?)))
}

#[derive(Debug, Serialize)]
pub struct AgentPermissionView {
    pub subject_kind: String,
    pub subject_id: String,
    /// `None` = unrestricted.
    pub allowed_roles: Option<Vec<String>>,
    pub updated_by: Option<String>,
    pub updated_at: i64,
}

impl From<AgentRunPermissionRow> for AgentPermissionView {
    fn from(row: AgentRunPermissionRow) -> Self {
        Self {
            subject_kind: row.subject_kind,
            subject_id: row.subject_id,
            allowed_roles: row
                .allowed_roles
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok()),
            updated_by: row.updated_by,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AgentPermissionsView {
    /// Run permissions are only enforced in multi-user mode.
    pub enforced: bool,
    pub items: Vec<AgentPermissionView>,
}

async fn agent_permissions_view(state: &GovernanceRouterState) -> Result<AgentPermissionsView, ApiError> {
    let rows = state
        .governance
        .repository()
        .list_agent_run_permissions()
        .await
        .map_err(db_unavailable)?;
    Ok(AgentPermissionsView {
        enforced: state.governance.roles_active(),
        items: rows.into_iter().map(AgentPermissionView::from).collect(),
    })
}

async fn list_agent_permissions_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<AgentPermissionsView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    require_policy_manager(&state, &user).await?;
    Ok(Json(ApiResponse::ok(agent_permissions_view(&state).await?)))
}

#[derive(Debug, Deserialize)]
pub struct SetAgentPermissionBody {
    pub subject_kind: String,
    pub subject_id: String,
    /// `null` lifts the restriction.
    pub allowed_roles: Option<Vec<String>>,
}

fn parse_roles(raw: &[String]) -> Result<Vec<UserRole>, ApiError> {
    raw.iter()
        .map(|name| {
            UserRole::parse(name)
                .ok_or_else(|| ApiError::BadRequest(format!("unknown role `{name}`; use owner, admin or member")))
        })
        .collect()
}

async fn set_agent_permission_handler(
    State(state): State<GovernanceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Json(body): Json<SetAgentPermissionBody>,
) -> Result<Json<ApiResponse<AgentPermissionsView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    require_policy_manager(&state, &user).await?;
    let roles = body.allowed_roles.as_deref().map(parse_roles).transpose()?;
    state
        .governance
        .set_agent_run_permission(
            &user.id,
            body.subject_kind.trim(),
            &body.subject_id,
            roles.as_deref(),
            None,
        )
        .await
        .map_err(map_governance_error)?;
    Ok(Json(ApiResponse::ok(agent_permissions_view(&state).await?)))
}

/// `/api/governance/*`. Mount behind `auth_middleware`.
pub fn governance_routes(state: GovernanceRouterState) -> Router {
    Router::new()
        .route("/api/governance/me", get(me_handler))
        .route("/api/governance/policies", get(list_policies_handler))
        .route("/api/governance/policies/{key}", put(set_policy_handler))
        .route("/api/governance/roles", get(list_roles_handler))
        .route("/api/governance/roles/{user_id}", put(set_role_handler))
        .route(
            "/api/governance/agent-permissions",
            get(list_agent_permissions_handler).put(set_agent_permission_handler),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_routes_require_a_user_session() {
        let device = Some(Extension(DevicePrincipal {
            device_id: "d1".into(),
            scopes: vec!["mobile_default".into()],
        }));
        let agent = Some(Extension(AgentPrincipal {
            conversation_id: "c1".into(),
        }));
        assert!(ensure_user_session(&None, &None).is_ok());
        assert!(ensure_user_session(&device, &None).is_err());
        assert!(ensure_user_session(&None, &agent).is_err());
    }

    #[test]
    fn role_lists_are_validated() {
        assert_eq!(
            parse_roles(&["owner".into(), "member".into()]).unwrap(),
            vec![UserRole::Owner, UserRole::Member]
        );
        assert!(parse_roles(&["root".into()]).is_err());
    }
}

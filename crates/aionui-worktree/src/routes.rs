#![allow(clippy::disallowed_types)]

//! `/api/worktrees*`. User identity only: these prefixes are deliberately
//! absent from the agent-token allowlists in `aionui_auth::agent_scope`, so
//! agent tokens get 403 `AGENT_SCOPE_DENIED` before reaching a handler.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::error::WorktreeError;
use crate::service::WorktreeService;
use crate::types::{
    CreateWorktreeRequest, DiffQuery, DiffResponse, DiffStatResponse, GitCapabilityResponse, ListWorktreesQuery,
    PruneResponse, RemoveWorktreeQuery, WorktreeResponse,
};

#[derive(Clone)]
pub struct WorktreeRouterState {
    pub service: WorktreeService,
}

impl From<WorktreeError> for ApiError {
    fn from(err: WorktreeError) -> Self {
        let status = match &err {
            WorktreeError::NotFound(_) => StatusCode::NOT_FOUND,
            WorktreeError::BadRequest(_)
            | WorktreeError::InvalidRef(_)
            | WorktreeError::NotARepository(_)
            | WorktreeError::RefNotFound(_)
            | WorktreeError::PathTooLong(_)
            | WorktreeError::PathEscape(_) => StatusCode::BAD_REQUEST,
            WorktreeError::BranchExists(_)
            | WorktreeError::Dirty(_)
            | WorktreeError::InvalidState { .. }
            | WorktreeError::PathConflict(_) => StatusCode::CONFLICT,
            WorktreeError::GitNotAvailable(_) | WorktreeError::GitTooOld { .. } => StatusCode::FAILED_DEPENDENCY,
            WorktreeError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            WorktreeError::Cancelled => StatusCode::CONFLICT,
            WorktreeError::Git(_) | WorktreeError::Io(_) | WorktreeError::Database(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        ApiError::coded(status, err.code(), err.to_string(), None)
    }
}

pub fn worktree_routes(state: WorktreeRouterState) -> Router {
    Router::new()
        .route("/api/worktrees", get(list_worktrees).post(create_worktree))
        .route("/api/worktrees/capabilities", get(capabilities))
        .route("/api/worktrees/prune", post(prune_worktrees))
        .route("/api/worktrees/{id}", get(get_worktree).delete(remove_worktree))
        .route("/api/worktrees/{id}/cancel", post(cancel_worktree))
        .route("/api/worktrees/{id}/retry", post(retry_worktree))
        .route("/api/worktrees/{id}/diff-stat", get(diff_stat))
        .route("/api/worktrees/{id}/diff", get(diff_file))
        .with_state(state)
}

async fn capabilities(State(state): State<WorktreeRouterState>) -> Json<ApiResponse<GitCapabilityResponse>> {
    Json(ApiResponse::ok(state.service.capabilities().await))
}

async fn create_worktree(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateWorktreeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<WorktreeResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let wt = state.service.create(&user.id, req).await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(wt))))
}

async fn list_worktrees(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListWorktreesQuery>,
) -> Result<Json<ApiResponse<Vec<WorktreeResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.list(&user.id, query).await?)))
}

async fn get_worktree(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<WorktreeResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.get(&user.id, &id).await?)))
}

async fn remove_worktree(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<RemoveWorktreeQuery>,
) -> Result<(StatusCode, Json<ApiResponse<WorktreeResponse>>), ApiError> {
    let wt = state.service.remove(&user.id, &id, query).await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(wt))))
}

async fn cancel_worktree(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<WorktreeResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.cancel(&user.id, &id).await?)))
}

async fn retry_worktree(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ApiResponse<WorktreeResponse>>), ApiError> {
    let wt = state.service.retry(&user.id, &id).await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(wt))))
}

async fn prune_worktrees(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<PruneResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.prune(&user.id).await?)))
}

async fn diff_stat(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DiffStatResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.diff_stat(&user.id, &id).await?)))
}

async fn diff_file(
    State(state): State<WorktreeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<ApiResponse<DiffResponse>>, ApiError> {
    let path = query.path.unwrap_or_default();
    Ok(Json(ApiResponse::ok(
        state.service.diff_file(&user.id, &id, &path).await?,
    )))
}

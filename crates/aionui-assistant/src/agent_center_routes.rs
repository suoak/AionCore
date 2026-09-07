#![allow(clippy::disallowed_types)]

//! HTTP routes for `/api/agent-center/*` (CSBU WorkMate 智能体中心).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::{
    AdvanceAgentWorkflowRunRequest, AgentCenterDetailResponse, AgentCenterListItem, AgentCenterListQuery,
    AgentCenterRevisionResponse, AgentCenterRunPlanResponse, AgentWorkflowRunResponse, ApiResponse,
    CreateAgentCenterRequest, DecideAgentWorkflowApprovalRequest, PublishAgentCenterRequest,
    StartAgentWorkflowRunRequest, UpdateAgentCenterRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::agent_center_service::AgentCenterService;

#[derive(Clone)]
pub struct AgentCenterRouterState {
    pub service: Arc<AgentCenterService>,
}

pub fn agent_center_routes(state: AgentCenterRouterState) -> Router {
    Router::new()
        .route("/api/agent-center/agents", get(list_agents).post(create_agent))
        .route("/api/agent-center/agents/{id}", get(get_agent).put(update_agent))
        .route("/api/agent-center/agents/{id}/publish", post(publish_agent))
        .route("/api/agent-center/agents/{id}/unpublish", post(unpublish_agent))
        .route("/api/agent-center/agents/{id}/versions", get(list_versions))
        .route("/api/agent-center/agents/{id}/run", post(run_agent))
        .route(
            "/api/agent-center/agents/{id}/workflow-runs",
            get(list_workflow_runs).post(start_workflow_run),
        )
        .route("/api/agent-center/workflow-runs/{id}", get(get_workflow_run))
        .route(
            "/api/agent-center/workflow-runs/{id}/advance",
            post(advance_workflow_run),
        )
        .route(
            "/api/agent-center/workflow-runs/{id}/approval",
            post(decide_workflow_approval),
        )
        .route("/api/agent-center/workflow-runs/{id}/cancel", post(cancel_workflow_run))
        .route("/api/agent-center/workflow-runs/{id}/retry", post(retry_workflow_run))
        .with_state(state)
}

async fn list_agents(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Query(query): Query<AgentCenterListQuery>,
) -> Result<Json<ApiResponse<Vec<AgentCenterListItem>>>, ApiError> {
    let items = state
        .service
        .list_for_user(&current_user.id, &query.scope, query.team_id.as_deref())
        .await?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn create_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    body: Result<Json<CreateAgentCenterRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentCenterDetailResponse>>), ApiError> {
    let Json(req) = body?;
    let created = state.service.create_for_user(&current_user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(created))))
}

async fn get_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let detail = state.service.get_detail_for_user(&current_user.id, &id, None).await?;
    Ok(Json(ApiResponse::ok(detail)))
}

async fn update_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let Json(req) = body?;
    let updated = state.service.update_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(updated)))
}

async fn publish_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<PublishAgentCenterRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let req = match body {
        Ok(Json(req)) => req,
        Err(_) => PublishAgentCenterRequest::default(),
    };
    let published = state.service.publish_for_user(&current_user.id, &id, req).await?;
    Ok(Json(ApiResponse::ok(published)))
}

async fn unpublish_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterDetailResponse>>, ApiError> {
    let draft = state.service.unpublish_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(draft)))
}

async fn list_versions(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AgentCenterRevisionResponse>>>, ApiError> {
    let versions = state.service.list_versions_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(versions)))
}

async fn run_agent(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentCenterRunPlanResponse>>, ApiError> {
    let plan = state.service.run_plan_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(plan)))
}

async fn start_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<StartAgentWorkflowRunRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<AgentWorkflowRunResponse>>), ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .start_workflow_run_for_user(&current_user.id, &id, req)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(run))))
}

async fn list_workflow_runs(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AgentWorkflowRunResponse>>>, ApiError> {
    let runs = state.service.list_workflow_runs_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(runs)))
}

async fn get_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let run = state.service.get_workflow_run_for_user(&current_user.id, &id).await?;
    Ok(Json(ApiResponse::ok(run)))
}

async fn advance_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AdvanceAgentWorkflowRunRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .advance_workflow_run_for_user(&current_user.id, &id, req)
        .await?;
    Ok(Json(ApiResponse::ok(run)))
}

async fn decide_workflow_approval(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<DecideAgentWorkflowApprovalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let Json(req) = body?;
    let run = state
        .service
        .decide_workflow_approval_for_user(&current_user.id, &id, req)
        .await?;
    dispatch_pending_tool_execution(&state.service, &current_user.id, &run);
    Ok(Json(ApiResponse::ok(run)))
}

async fn cancel_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let run = state
        .service
        .cancel_workflow_run_for_user(&current_user.id, &id)
        .await?;
    Ok(Json(ApiResponse::ok(run)))
}

async fn retry_workflow_run(
    State(state): State<AgentCenterRouterState>,
    Extension(current_user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentWorkflowRunResponse>>, ApiError> {
    let run = state.service.retry_workflow_run_for_user(&current_user.id, &id).await?;
    dispatch_pending_tool_execution(&state.service, &current_user.id, &run);
    Ok(Json(ApiResponse::ok(run)))
}

fn dispatch_pending_tool_execution(service: &Arc<AgentCenterService>, user_id: &str, run: &AgentWorkflowRunResponse) {
    if !matches!(
        run.next_action.as_ref(),
        Some(aionui_api_types::AgentWorkflowNextAction::InvokeTool { .. })
    ) {
        return;
    }
    let service = service.clone();
    let user_id = user_id.to_owned();
    let run_id = run.id.clone();
    tokio::spawn(async move {
        if let Err(error) = service.execute_pending_tools_for_user(&user_id, &run_id).await {
            tracing::warn!(user_id, run_id, error = %error, "agent-workflow: background MCP tool execution failed");
        }
    });
}

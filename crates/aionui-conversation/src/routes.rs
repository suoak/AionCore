#![allow(clippy::disallowed_types)]

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};

use aionui_api_types::{
    AcceptanceCriterionResponse, ActiveCountResponse, ApiResponse, ApprovalCheckQuery, ApprovalCheckResponse,
    CancelConversationRequest, CancelConversationResponse, CloneConversationRequest, ConfirmRequest,
    ConfirmationListResponse, ConversationArtifactListResponse, ConversationArtifactResponse, ConversationCapabilities,
    ConversationInputListResponse, ConversationInputReceipt, ConversationListResponse, ConversationResponse,
    CreateConversationRequest, CreateTaskSessionRequest, DecideTaskApprovalRequest, EnsureConversationRuntimeResponse,
    ExecuteApprovedPlanRequest, ForkConversationRequest, ListConversationInputsQuery, ListConversationsQuery,
    ListMessagesQuery, ListTaskSessionsQuery, MessageListResponse, MessageResponse, MessageSearchResponse,
    PlanningIsolationResponse, SearchMessagesQuery, SendMessageRequest, SendMessageResponse,
    StartAutomaticPlanningRequest, SubmitConversationInputRequest, SubmitTaskArtifactRequest,
    SubmitTaskArtifactResponse, TaskApprovalResponse, TaskArtifactResponse, TaskEvidenceResponse, TaskReviewResponse,
    TaskRunReplayQuery, TaskRunReplayResponse, TaskRunResponse, TaskSessionResponse, TaskTraceEventResponse,
    UpdateConversationArtifactRequest, UpdateConversationRequest, UpdateTaskRetryPolicyRequest,
    UpdateTaskSessionRequest, VerifyAcceptanceCriterionRequest,
};
use aionui_auth::{AgentPrincipal, CurrentUser, DevicePrincipal};
use aionui_common::ApiError;

use crate::ConversationError;
use crate::state::ConversationRouterState;

impl From<ConversationError> for ApiError {
    fn from(error: ConversationError) -> Self {
        match error {
            ConversationError::NotFound { id } => ApiError::NotFound(format!("Conversation {id} not found")),
            ConversationError::MessageNotFound { id } => ApiError::NotFound(format!("Message {id} not found")),
            ConversationError::ArtifactNotFound { id } => ApiError::NotFound(format!("Artifact {id} not found")),
            ConversationError::ActiveAgentNotFound { .. } => {
                ApiError::NotFound("No active agent for this conversation".into())
            }
            ConversationError::Archived { reason, .. } => ApiError::ConversationArchived(reason),
            ConversationError::RuntimeRetired { backend } => ApiError::coded(
                StatusCode::GONE,
                "runtime_retired",
                "This runtime has been retired. Open the conversation as history or start a new conversation.",
                Some(serde_json::json!({ "backend": backend })),
            ),
            ConversationError::BadRequest { reason } => ApiError::BadRequest(reason),
            ConversationError::Busy { reason } if reason.starts_with("CROSS_ACCOUNT_REFERENCE:") => {
                ApiError::coded(StatusCode::CONFLICT, "CROSS_ACCOUNT_REFERENCE", reason, None)
            }
            ConversationError::Busy { reason } if reason.starts_with("CONVERSATION_ACTIVE:") => {
                ApiError::coded(StatusCode::CONFLICT, "CONVERSATION_ACTIVE", reason, None)
            }
            ConversationError::Busy { reason } => ApiError::Conflict(reason),
            ConversationError::Forbidden { reason } => ApiError::Forbidden(reason),
            ConversationError::NotFoundReason { reason } => ApiError::NotFound(reason),
            ConversationError::Unauthorized { reason } => ApiError::Unauthorized(reason),
            ConversationError::RateLimited => ApiError::RateLimited,
            ConversationError::BadGateway { reason } => ApiError::BadGateway(reason),
            ConversationError::Timeout { reason } => ApiError::Timeout(reason),
            ConversationError::ConfigConfirmationTimeout {
                conversation_id,
                option_id,
                requested,
                last_observed,
            } => ApiError::coded(
                StatusCode::GATEWAY_TIMEOUT,
                "confirmation_timeout",
                "ACP runtime did not confirm the requested config option before timeout",
                Some(serde_json::json!({
                    "conversation_id": conversation_id,
                    "option_id": option_id,
                    "requested": requested,
                    "last_observed": last_observed,
                })),
            ),
            ConversationError::ConfigUpdateInProgress {
                conversation_id,
                option_id,
                requested,
            } => ApiError::coded(
                StatusCode::CONFLICT,
                "config_update_in_progress",
                "ACP config update is already in progress",
                Some(serde_json::json!({
                    "conversation_id": conversation_id,
                    "option_id": option_id,
                    "requested": requested,
                })),
            ),
            ConversationError::RuntimeRestarting { conversation_id } => ApiError::coded(
                StatusCode::CONFLICT,
                "runtime_restarting",
                "Conversation runtime is restarting",
                Some(serde_json::json!({ "conversation_id": conversation_id })),
            ),
            ConversationError::TeamRuntimeRequired {
                conversation_id,
                team_id,
            } => ApiError::coded(
                StatusCode::CONFLICT,
                "TEAM_RUNTIME_REQUIRED",
                "This conversation belongs to a team; use the team runtime session",
                Some(serde_json::json!({
                    "conversation_id": conversation_id,
                    "team_id": team_id,
                })),
            ),
            ConversationError::Unprocessable { reason } => ApiError::UnprocessableEntity(reason),
            ConversationError::BoardTransitionRejected { from, to, code, reason } => ApiError::coded(
                StatusCode::CONFLICT,
                "board_transition_rejected",
                reason,
                Some(serde_json::json!({ "from": from, "to": to, "reason": code })),
            ),
            ConversationError::BoardItemStale { id, expected, current } => ApiError::coded(
                StatusCode::CONFLICT,
                "board_item_stale",
                format!("Board item {id} was changed by someone else; reload and retry"),
                Some(serde_json::json!({ "expected_updated_at": expected, "current_updated_at": current })),
            ),
            ConversationError::CapabilityUnsupported { capability } => ApiError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "capability_unsupported",
                format!("Conversation capability '{capability}' is not supported"),
                Some(serde_json::json!({ "capability": capability })),
            ),
            ConversationError::Internal { reason } => ApiError::Internal(reason),
            ConversationError::WorkspacePathUnavailable { path } => ApiError::WorkspacePathUnavailable(path),
            ConversationError::WorkspacePathRuntimeUnavailable { path } => {
                ApiError::WorkspacePathRuntimeUnavailable(path)
            }
            ConversationError::OpenClawGatewayUnreachable { detail } => ApiError::coded(
                StatusCode::BAD_GATEWAY,
                "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE",
                "OpenClaw Gateway is not reachable",
                Some(serde_json::json!({
                    "detail": detail,
                    "error_kind": "openclaw_gateway_unreachable",
                    "backend": "openclaw",
                    "port": 18789
                })),
            ),
            ConversationError::Acp(_) => ApiError::BadGateway("Agent protocol error".into()),
        }
    }
}

/// Build the conversation router (CRUD + message flow + confirmation + extended operations).
///
/// All routes require authentication (applied by the caller).
pub fn conversation_routes(state: ConversationRouterState) -> Router {
    Router::new()
        .route("/api/task-sessions", post(create_task_session).get(list_task_sessions))
        .route("/api/task-sessions/board", get(get_task_board))
        .route("/api/task-sessions/import/github", post(import_github_issues))
        .route("/api/task-sessions/{id}/board", patch(update_task_board))
        .route(
            "/api/task-sessions/{id}/comments",
            get(list_task_comments).post(create_task_comment),
        )
        .route(
            "/api/task-sessions/{id}/comments/{comment_id}",
            patch(edit_task_comment),
        )
        .route("/api/task-sessions/{id}/review", post(review_task_board))
        .route(
            "/api/task-sessions/{id}",
            get(get_task_session).patch(update_task_session),
        )
        .route(
            "/api/task-sessions/{id}/retry-policy",
            axum::routing::put(update_task_retry_policy),
        )
        .route(
            "/api/task-sessions/{id}/planning-isolation",
            get(get_task_planning_isolation),
        )
        .route("/api/task-sessions/{id}/automatic-plan", post(start_automatic_planning))
        .route(
            "/api/task-sessions/{id}/artifacts",
            post(submit_task_artifact).get(list_task_artifacts),
        )
        .route("/api/task-sessions/{id}/approvals", get(list_task_approvals))
        .route(
            "/api/task-sessions/{id}/approvals/{approval_id}/decision",
            post(decide_task_approval),
        )
        .route("/api/task-sessions/{id}/execute", post(execute_approved_plan))
        .route("/api/task-sessions/{id}/start", post(start_task))
        .route("/api/task-sessions/{id}/issue-comment/draft", get(issue_comment_draft))
        .route("/api/task-sessions/{id}/issue-comment", post(post_issue_comment))
        .route("/api/task-sessions/{id}/follow-up", post(follow_up_task))
        .route("/api/task-sessions/{id}/deliver", post(deliver_task))
        .route("/api/task-sessions/{id}/runs", get(list_task_runs))
        .route("/api/task-sessions/{id}/runs/{run_id}/review", get(get_task_run_review))
        .route("/api/task-sessions/{id}/runs/{run_id}/trace", get(get_task_run_trace))
        .route("/api/task-sessions/{id}/runs/{run_id}/replay", get(replay_task_run))
        .route(
            "/api/task-sessions/{id}/runs/{run_id}/changes",
            get(get_task_run_changes),
        )
        .route(
            "/api/task-sessions/{id}/runs/{run_id}/approvals",
            get(get_task_run_approvals),
        )
        .route(
            "/api/task-sessions/{id}/runs/{run_id}/verification",
            get(get_task_run_verification),
        )
        .route(
            "/api/task-sessions/{id}/acceptance-criteria",
            get(list_acceptance_criteria),
        )
        .route(
            "/api/task-sessions/{id}/acceptance-criteria/{criterion_id}",
            patch(verify_acceptance_criterion),
        )
        .route("/api/conversations", post(create).get(list))
        .route("/api/conversations/{id}", get(get_one).patch(update).delete(delete_one))
        .route("/api/conversations/{id}/reset", post(reset))
        .route("/api/conversations/{id}/fork", post(fork))
        .route("/api/conversations/{id}/delegation-grants", post(grant_delegations))
        // 3.0 C9: remote approve-to-execute.
        .route(
            "/api/task-sessions/{id}/approval-requests",
            post(issue_approval_request).get(list_task_approval_requests),
        )
        .route("/api/approval-requests", get(list_pending_approval_requests))
        .route("/api/approval-requests/{id}", get(get_approval_request))
        .route("/api/approval-requests/{id}/decision", post(decide_approval_request))
        .route(
            "/api/remote-approval/settings",
            get(get_remote_approval_settings).put(put_remote_approval_settings),
        )
        .route(
            "/api/remote-approval/approvers",
            get(list_remote_approvers).post(add_remote_approver),
        )
        .route(
            "/api/remote-approval/approvers/{platform}/{identity}",
            axum::routing::delete(remove_remote_approver),
        )
        .route("/api/conversations/{id}/delegations", get(list_delegations))
        .route("/api/conversations/{id}/associated", get(associated))
        .route("/api/conversations/{id}/messages", get(list_msg).post(send_msg))
        // MUST precede the `{messageId}` wildcard below: registered after it,
        // "latest" would be captured as a message id and 404.
        .route("/api/conversations/{id}/messages/latest", get(latest_msg))
        .route("/api/conversations/{id}/messages/{messageId}", get(get_msg))
        .route("/api/conversations/{id}/inputs", get(list_inputs).post(submit_input))
        .route(
            "/api/conversations/{id}/inputs/{inputId}",
            axum::routing::delete(cancel_input),
        )
        .route("/api/conversations/{id}/capabilities", get(capabilities))
        .route("/api/conversations/{id}/artifacts", get(list_artifacts))
        .route("/api/conversations/{id}/artifacts/{artifactId}", patch(update_artifact))
        .route("/api/conversations/{id}/cancel", post(cancel))
        .route(
            "/api/conversations/{id}/terminals/{terminalId}/kill",
            post(kill_terminal),
        )
        .route("/api/conversations/{id}/runtime/ensure", post(ensure_runtime))
        .route("/api/conversations/{id}/runtime/restart", post(restart_runtime))
        .route("/api/conversations/{id}/active-lease", post(active_lease))
        // Confirmation system
        .route("/api/conversations/{id}/confirmations", get(list_confirmations))
        .route("/api/conversations/{id}/confirmations/{callId}/confirm", post(confirm))
        .route("/api/conversations/{id}/asks/{requestId}/answer", post(answer_ask))
        .route("/api/conversations/{id}/approvals/check", get(check_approval))
        .route("/api/conversations/active-count", get(active_count))
        .route("/api/conversations/clone", post(clone))
        .route("/api/messages/search", get(search_messages))
        .with_state(state)
}

// ── Handlers ───────────────────────────────────────────────────────

/// M8 Q9: body fields an agent may set on a work item it proposes. Anything
/// else (board column, assignee, priority, labels, rank, conversation or
/// agent-session binding, unknown keys) is refused, so an agent can only ever
/// propose an unassigned backlog item.
const AGENT_WORK_ITEM_FIELDS: &[&str] = &[
    "title",
    "objective",
    "acceptance_criteria",
    "mode",
    "project_id",
    "agent_type",
    "status",
];

fn agent_backlog_only(message: impl Into<String>, field: Option<&str>) -> ApiError {
    ApiError::coded(
        StatusCode::FORBIDDEN,
        "agent_work_item_backlog_only",
        message,
        field.map(|field| serde_json::json!({ "field": field })),
    )
}

/// Validate an agent-proposed work item body before it is deserialized.
fn check_agent_work_item_body(body: &serde_json::Value) -> Result<(), ApiError> {
    let Some(object) = body.as_object() else {
        return Err(ApiError::BadRequest("Invalid JSON request body.".to_owned()));
    };
    if let Some(field) = object
        .keys()
        .find(|key| !AGENT_WORK_ITEM_FIELDS.contains(&key.as_str()))
    {
        return Err(agent_backlog_only(
            format!("Agents can only create unassigned backlog work items; `{field}` is not allowed"),
            Some(field),
        ));
    }
    if let Some(status) = object.get("status")
        && status.as_str() != Some("draft")
    {
        return Err(agent_backlog_only(
            "Agents can only create draft work items in backlog",
            Some("status"),
        ));
    }
    Ok(())
}

async fn create_task_session(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    agent: Option<Extension<AgentPrincipal>>,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<TaskSessionResponse>>), ApiError> {
    let Json(body) = body.map_err(ApiError::from)?;
    if let Some(Extension(agent)) = &agent {
        check_agent_work_item_body(&body)?;
        let request: CreateTaskSessionRequest =
            serde_json::from_value(body).map_err(|_| ApiError::BadRequest("Invalid JSON request body.".to_owned()))?;
        let session = state
            .service
            .create_agent_backlog_work_item(&user.id, &agent.conversation_id, request)
            .await
            .map_err(ApiError::from)?;
        return Ok((StatusCode::CREATED, Json(ApiResponse::ok(session))));
    }
    let request: CreateTaskSessionRequest =
        serde_json::from_value(body).map_err(|_| ApiError::BadRequest("Invalid JSON request body.".to_owned()))?;
    let session = state
        .service
        .create_task_session(&user.id, request)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(session))))
}

async fn list_task_sessions(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListTaskSessionsQuery>,
) -> Result<Json<ApiResponse<Vec<TaskSessionResponse>>>, ApiError> {
    let sessions = state
        .service
        .list_task_sessions(&user.id, query.conversation_id.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(sessions)))
}

async fn get_task_session(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<TaskSessionResponse>>, ApiError> {
    let session = state
        .service
        .get_task_session(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(session)))
}

async fn get_task_planning_isolation(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<PlanningIsolationResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_task_planning_isolation(&user.id, &id).await?,
    )))
}

async fn update_task_session(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateTaskSessionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TaskSessionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let session = state
        .service
        .update_task_session(&user.id, &id, request)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(session)))
}

async fn get_task_board(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<aionui_api_types::TaskBoardQuery>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskBoardResponse>>, ApiError> {
    let board = state.service.task_board(&user.id, query).await?;
    Ok(Json(ApiResponse::ok(board)))
}

/// M8 E6-7: manual, read-only GitHub Issue import. User-only: the agent
/// scope already denies it; the explicit check keeps that true if the scope
/// list ever widens.
async fn import_github_issues(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    agent: Option<Extension<AgentPrincipal>>,
    body: Result<Json<aionui_api_types::ImportGithubIssuesRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::ImportGithubIssuesResponse>>, ApiError> {
    if agent.is_some() {
        return Err(ApiError::Forbidden("Agents cannot import GitHub issues.".to_owned()));
    }
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.import_github_issues(&user.id, request).await?;
    Ok(Json(ApiResponse::ok(response)))
}

/// Board routes are user-only: agent runtime tokens are denied on
/// `/api/task-sessions/*` by the agent scope, so the actor here is the user.
async fn update_task_board(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::UpdateTaskBoardRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskBoardItemResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let item = state
        .service
        .update_task_board(&user.id, &id, crate::board::BoardActor::User, request)
        .await?;
    Ok(Json(ApiResponse::ok(item)))
}

async fn review_task_board(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::TaskBoardReviewRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskBoardReviewResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .review_task_board(&user.id, &id, crate::board::BoardActor::User, request)
        .await?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn list_task_comments(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_api_types::TaskCommentResponse>>>, ApiError> {
    let comments = state.service.list_task_comments(&user.id, &id).await?;
    Ok(Json(ApiResponse::ok(comments)))
}

async fn create_task_comment(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::CreateTaskCommentRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::TaskCommentResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let comment = state.service.create_task_comment(&user.id, &id, &request.body).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(comment))))
}

async fn edit_task_comment(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, comment_id)): Path<(String, String)>,
    body: Result<Json<aionui_api_types::EditTaskCommentRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskCommentResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let comment = state
        .service
        .edit_task_comment(&user.id, &id, &comment_id, &request.body)
        .await?;
    Ok(Json(ApiResponse::ok(comment)))
}

async fn update_task_retry_policy(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateTaskRetryPolicyRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TaskSessionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let session = state
        .service
        .update_task_retry_policy(&user.id, &id, request)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(session)))
}

async fn submit_task_artifact(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SubmitTaskArtifactRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SubmitTaskArtifactResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.submit_task_artifact(&user.id, &id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn start_automatic_planning(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<StartAutomaticPlanningRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SubmitTaskArtifactResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let response = state.service.start_automatic_planning(&user.id, &id, request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn list_task_artifacts(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<TaskArtifactResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_task_artifacts(&user.id, &id).await?,
    )))
}

async fn list_task_approvals(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<TaskApprovalResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_task_approvals(&user.id, &id).await?,
    )))
}

async fn decide_task_approval(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    Path((id, approval_id)): Path<(String, String)>,
    body: Result<Json<DecideTaskApprovalRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TaskApprovalResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    // A paired phone answering is reported to the desktop as such (M8 E9-3).
    let decider = match device.as_ref() {
        Some(Extension(device)) => crate::TaskApprovalDecider::Device(&device.device_id),
        None => crate::TaskApprovalDecider::User,
    };
    Ok(Json(ApiResponse::ok(
        state
            .service
            .decide_task_approval_by(&user.id, &id, &approval_id, request, decider)
            .await?,
    )))
}

async fn execute_approved_plan(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ExecuteApprovedPlanRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<TaskRunResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.execute_approved_plan(&user.id, &id, request).await?,
    )))
}

/// 3.0 C2: start now or queue for a run slot; idempotent.
async fn start_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Option<Json<aionui_api_types::StartTaskRequest>>,
) -> Result<Json<ApiResponse<aionui_api_types::StartTaskResponse>>, ApiError> {
    let request = body.map(|Json(request)| request).unwrap_or_default();
    Ok(Json(ApiResponse::ok(
        state.service.start_task_with(&user.id, &id, request).await?,
    )))
}

/// 3.0 C8: a suggested comment for the task's Issue; posts nothing.
async fn issue_comment_draft(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<aionui_api_types::IssueCommentDraftResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.issue_comment_draft(&user.id, &id).await?,
    )))
}

/// 3.0 C8: post the user-edited comment under the Issue (`confirm: true`).
async fn post_issue_comment(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(request): Json<aionui_api_types::PostIssueCommentRequest>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskBoardItemResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.post_issue_comment(&user.id, &id, request).await?,
    )))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowUpTaskRequest {
    text: String,
}

/// 3.0 C3: "request changes" — continue in the same conversation / worktree.
async fn follow_up_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<FollowUpTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::StartTaskResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.follow_up_task(&user.id, &id, &request.text).await?,
    )))
}

/// 3.0 C3: push the task branch and open (or update) its pull request.
async fn deliver_task(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::DeliverTaskRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::TaskBoardItemResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.deliver_task(&user.id, &id, request).await?,
    )))
}

async fn list_task_runs(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<TaskRunResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_task_runs(&user.id, &id).await?,
    )))
}

async fn get_task_run_review(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<TaskReviewResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_task_run_review(&user.id, &id, &run_id).await?,
    )))
}

async fn get_task_run_trace(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<TaskTraceEventResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_task_run_trace(&user.id, &id, &run_id).await?,
    )))
}

async fn replay_task_run(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
    Query(query): Query<TaskRunReplayQuery>,
) -> Result<Json<ApiResponse<TaskRunReplayResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.replay_task_run(&user.id, &id, &run_id, query).await?,
    )))
}

async fn get_task_run_changes(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<TaskEvidenceResponse>>>, ApiError> {
    let evidence = state.service.get_task_run_evidence(&user.id, &id, &run_id).await?;
    Ok(Json(ApiResponse::ok(
        evidence
            .into_iter()
            .filter(|item| matches!(item.kind.as_str(), "file" | "diff"))
            .collect(),
    )))
}

async fn get_task_run_approvals(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<TaskApprovalResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_task_run_review(&user.id, &id, &run_id)
            .await?
            .approvals,
    )))
}

async fn get_task_run_verification(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, run_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<Vec<AcceptanceCriterionResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state
            .service
            .get_task_run_review(&user.id, &id, &run_id)
            .await?
            .acceptance_criteria,
    )))
}

async fn list_acceptance_criteria(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<AcceptanceCriterionResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_acceptance_criteria(&user.id, &id).await?,
    )))
}

async fn verify_acceptance_criterion(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, criterion_id)): Path<(String, String)>,
    body: Result<Json<VerifyAcceptanceCriterionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<AcceptanceCriterionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .verify_acceptance_criterion(&user.id, &id, &criterion_id, request)
            .await?,
    )))
}

async fn create(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), ApiError> {
    let Json(mut req) = body.map_err(ApiError::from)?;
    if crate::service::strip_client_team_binding(&mut req.extra) {
        tracing::warn!("Stripped client-supplied team binding fields from conversation create");
    }
    let conversation = state.service.create(&user.id, req).await.map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn list(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListConversationsQuery>,
) -> Result<Json<ApiResponse<ConversationListResponse>>, ApiError> {
    let result = state.service.list(&user.id, query).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn clone(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CloneConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), ApiError> {
    let Json(mut req) = body.map_err(ApiError::from)?;
    if crate::service::strip_client_team_binding(&mut req.conversation.extra) {
        tracing::warn!("Stripped client-supplied team binding fields from conversation clone");
    }
    let conversation = state
        .service
        .clone_create(&user.id, req)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn get_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationResponse>>, ApiError> {
    let conversation = state.service.get(&user.id, &id).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn update(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let conversation = state
        .service
        .update(&user.id, &id, req, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(conversation)))
}

async fn delete_one(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&user.id, &id).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn reset(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.reset(&user.id, &id).await.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

/// 3.0 C7: the user's explicit `@assistant` picks. User auth only — the
/// agent-token scope denies this route, so an agent cannot grant itself.
/// 3.0 C9: desktop issues (or re-issues) a remote approval request.
async fn issue_approval_request(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::IssueApprovalRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::ApprovalRequestResponse>>), ApiError> {
    if device.is_some() {
        return Err(ConversationError::Forbidden {
            reason: "Approval requests are issued from the desktop".into(),
        }
        .into());
    }
    let Json(req) = body.map_err(ApiError::from)?;
    let response = state.service.issue_approval_request(&user.id, &id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn list_task_approval_requests(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_api_types::ApprovalRequestResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_approval_requests(&user.id, Some(&id)).await?,
    )))
}

async fn list_pending_approval_requests(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<aionui_api_types::ApprovalRequestResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_approval_requests(&user.id, None).await?,
    )))
}

async fn get_approval_request(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<aionui_api_types::ApprovalRequestResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.get_approval_request(&user.id, &id).await?,
    )))
}

/// 3.0 C9: a paired device answers one request. The body is only
/// `{decision, digest, comment?}` (unknown fields refused).
async fn decide_approval_request(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::RemoteApprovalDecisionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::ApprovalRequestResponse>>, ApiError> {
    let Some(Extension(device)) = device else {
        return Err(ConversationError::Forbidden {
            reason: "approval_identity_untrusted: remote decisions need a paired device".into(),
        }
        .into());
    };
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state
            .service
            .decide_approval_request_remotely(
                &user.id,
                &id,
                req,
                crate::remote_approval::RemoteDecider::Device(device.device_id.clone()),
            )
            .await?,
    )))
}

async fn get_remote_approval_settings(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<aionui_api_types::RemoteApprovalSettings>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.remote_approval_settings(&user.id).await?,
    )))
}

async fn put_remote_approval_settings(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<aionui_api_types::RemoteApprovalSettings>, JsonRejection>,
) -> Result<Json<ApiResponse<aionui_api_types::RemoteApprovalSettings>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.set_remote_approval_settings(&user.id, req).await?,
    )))
}

async fn list_remote_approvers(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<aionui_api_types::RemoteApproverResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.list_remote_approvers(&user.id).await?,
    )))
}

async fn add_remote_approver(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<aionui_api_types::RemoteApproverRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<aionui_api_types::RemoteApproverResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let response = state.service.add_remote_approver(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

async fn remove_remote_approver(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((platform, identity)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .remove_remote_approver(&user.id, &platform, &identity)
        .await?;
    Ok(Json(ApiResponse::success()))
}

async fn grant_delegations(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<aionui_api_types::GrantDelegationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .grant_delegations(&user.id, &id, &req.assistant_ids)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn list_delegations(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<aionui_api_types::DelegationResponse>>>, ApiError> {
    let delegations = state
        .service
        .list_delegations(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(delegations)))
}

async fn fork(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<ForkConversationRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let conversation = state.service.fork(&user.id, &id, req).await.map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(conversation))))
}

async fn associated(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<ConversationResponse>>>, ApiError> {
    let items = state
        .service
        .list_associated(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(items)))
}

async fn list_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ListMessagesQuery>,
) -> Result<Json<ApiResponse<MessageListResponse>>, ApiError> {
    let result = state
        .service
        .list_messages(&user.id, &id, query)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct MessagePathParams {
    id: String,
    #[serde(rename = "messageId")]
    message_id: String,
}

#[derive(serde::Deserialize)]
struct LatestMessageQuery {
    r#type: String,
}

async fn latest_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<LatestMessageQuery>,
) -> Result<Json<ApiResponse<Option<MessageResponse>>>, ApiError> {
    let result = state
        .service
        .latest_message_of_type(&user.id, &id, &query.r#type)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn get_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<MessagePathParams>,
) -> Result<Json<ApiResponse<MessageResponse>>, ApiError> {
    let result = state
        .service
        .get_message(&user.id, &params.id, &params.message_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn send_msg(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SendMessageRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<SendMessageResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .send_message(&user.id, &id, req, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    // B5: a mid-turn delivery already handed the message to the RUNNING turn —
    // that is a completed delivery (200), not a scheduled one (202).
    let status = if response.delivered_midturn {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(ApiResponse::ok(response))))
}

async fn submit_input(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SubmitConversationInputRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<ConversationInputReceipt>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let receipt = state
        .service
        .submit_input(&user.id, &id, req)
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(receipt))))
}

async fn list_inputs(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ListConversationInputsQuery>,
) -> Result<Json<ApiResponse<ConversationInputListResponse>>, ApiError> {
    let inputs = state
        .service
        .list_inputs_with_terminal_limit(&user.id, &id, query.terminal_limit)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(inputs)))
}

#[derive(serde::Deserialize)]
struct InputPathParams {
    id: String,
    #[serde(rename = "inputId")]
    input_id: String,
}

async fn cancel_input(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<InputPathParams>,
) -> Result<Json<ApiResponse<aionui_api_types::ConversationInputResponse>>, ApiError> {
    let input = state
        .service
        .cancel_input(&user.id, &params.id, &params.input_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(input)))
}

async fn capabilities(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationCapabilities>>, ApiError> {
    let capabilities = state
        .service
        .conversation_capabilities(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(capabilities)))
}

async fn list_artifacts(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConversationArtifactListResponse>>, ApiError> {
    let result = state
        .service
        .list_artifacts(&user.id, &id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

#[derive(serde::Deserialize)]
struct ArtifactPathParams {
    id: String,
    #[serde(rename = "artifactId")]
    artifact_id: String,
}

async fn update_artifact(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ArtifactPathParams>,
    body: Result<Json<UpdateConversationArtifactRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ConversationArtifactResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let artifact = state
        .service
        .update_artifact(&user.id, &params.id, &params.artifact_id, req)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(artifact)))
}

async fn kill_terminal(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path((id, terminal_id)): Path<(String, String)>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .kill_terminal(&user.id, &id, &terminal_id, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn cancel(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<CancelConversationRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<CancelConversationResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let response = state
        .service
        .cancel(&user.id, &id, &req.turn_id, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn ensure_runtime(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<EnsureConversationRuntimeResponse>>, ApiError> {
    let response = state
        .service
        .ensure_runtime(&user.id, &id, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn restart_runtime(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<EnsureConversationRuntimeResponse>>, ApiError> {
    let response = state
        .service
        .restart_runtime(&user.id, &id, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(response)))
}

async fn active_lease(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state
        .service
        .renew_active_lease(&user.id, &id, &state.active_leases)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn search_messages(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<SearchMessagesQuery>,
) -> Result<Json<ApiResponse<MessageSearchResponse>>, ApiError> {
    let result = state
        .service
        .search_messages(&user.id, query)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

// ── Confirmation handlers ─────────────────────────────────────────

async fn list_confirmations(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<ConfirmationListResponse>>, ApiError> {
    let items = state
        .service
        .list_confirmations(&user.id, &id, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(items)))
}

#[derive(serde::Deserialize)]
struct ConfirmPathParams {
    id: String,
    #[serde(rename = "callId")]
    call_id: String,
}

async fn confirm(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<ConfirmPathParams>,
    body: Result<Json<ConfirmRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    state
        .service
        .confirm(&user.id, &params.id, &params.call_id, req, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

#[derive(serde::Deserialize)]
struct AskPathParams {
    id: String,
    #[serde(rename = "requestId")]
    request_id: String,
}

/// Dedicated answer channel for the structured question card (AskUserQuestion).
/// Body: `{answers:[{question, labels[]}]}` to answer, `{decline:true}` to
/// dismiss. Exactly one shape must be present — an empty answer set is
/// rejected rather than forwarded (claude silently drops unanswered questions
/// on an allow, so an "empty allow" is silent data loss).
async fn answer_ask(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(params): Path<AskPathParams>,
    body: Result<Json<aionui_api_types::AskAnswerRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let answers = if req.decline {
        if !req.answers.is_empty() {
            return Err(ApiError::BadRequest(
                "decline and answers are mutually exclusive".into(),
            ));
        }
        None
    } else {
        if req.answers.is_empty() {
            return Err(ApiError::BadRequest(
                "answers must not be empty unless decline is true".into(),
            ));
        }
        if req.answers.iter().any(|a| a.question.trim().is_empty()) {
            return Err(ApiError::BadRequest("every answer must name its question".into()));
        }
        Some(req.answers)
    };
    state
        .service
        .answer_ask(&user.id, &params.id, &params.request_id, answers, &state.task_manager)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::success()))
}

async fn check_approval(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Query(query): Query<ApprovalCheckQuery>,
) -> Result<Json<ApiResponse<ApprovalCheckResponse>>, ApiError> {
    if query.action.trim().is_empty() {
        return Err(ApiError::BadRequest("action must not be empty".into()));
    }

    let result = state
        .service
        .check_approval(
            &user.id,
            &id,
            &query.action,
            query.command_type.as_deref(),
            &state.task_manager,
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(result)))
}

async fn active_count(
    State(state): State<ConversationRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<ActiveCountResponse>>, ApiError> {
    let count = state.service.active_count_for_user(&user.id).await?;
    Ok(Json(ApiResponse::ok(ActiveCountResponse { count })))
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    #[test]
    fn conversation_not_found_maps_to_app_not_found() {
        let app = ApiError::from(ConversationError::NotFound { id: "conv_1".into() });
        assert!(matches!(app, ApiError::NotFound(message) if message == "Conversation conv_1 not found"));
    }

    #[test]
    fn cross_account_reference_maps_to_stable_conflict_code() {
        let app = ApiError::from(ConversationError::Busy {
            reason: "CROSS_ACCOUNT_REFERENCE: acp_session conversation 'conv-1' belongs to another user".into(),
        });

        assert_eq!(app.status_code(), StatusCode::CONFLICT);
        assert_eq!(app.error_code(), "CROSS_ACCOUNT_REFERENCE");
    }

    #[test]
    fn active_conversation_maps_to_stable_conflict_code() {
        let app = ApiError::from(ConversationError::Busy {
            reason: "CONVERSATION_ACTIVE: stop the active task before deleting this conversation".into(),
        });

        assert_eq!(app.status_code(), StatusCode::CONFLICT);
        assert_eq!(app.error_code(), "CONVERSATION_ACTIVE");
    }

    #[test]
    fn conversation_archived_maps_to_app_conversation_archived() {
        let app = ApiError::from(ConversationError::Archived {
            id: "conv_1".into(),
            reason: "legacy runtime".into(),
        });
        assert!(matches!(app, ApiError::ConversationArchived(message) if message == "legacy runtime"));
    }

    #[test]
    fn message_not_found_maps_to_app_not_found() {
        let app = ApiError::from(ConversationError::MessageNotFound { id: "msg_1".into() });
        assert!(matches!(app, ApiError::NotFound(message) if message == "Message msg_1 not found"));
    }

    #[test]
    fn artifact_not_found_maps_to_app_not_found() {
        let app = ApiError::from(ConversationError::ArtifactNotFound {
            id: "artifact_1".into(),
        });
        assert!(matches!(app, ApiError::NotFound(message) if message == "Artifact artifact_1 not found"));
    }

    #[test]
    fn active_agent_not_found_maps_to_app_not_found() {
        let app = ApiError::from(ConversationError::ActiveAgentNotFound {
            conversation_id: "conv_1".into(),
        });
        assert!(matches!(app, ApiError::NotFound(message) if message == "No active agent for this conversation"));
    }

    #[test]
    fn conversation_api_error_compat_preserves_special_codes() {
        let app = ApiError::from(ConversationError::WorkspacePathRuntimeUnavailable {
            path: "/tmp/my project".into(),
        });
        assert!(matches!(
            app,
            ApiError::WorkspacePathRuntimeUnavailable(message) if message == "/tmp/my project"
        ));
    }

    #[test]
    fn openclaw_gateway_unreachable_maps_to_coded_bad_gateway() {
        let app = ApiError::from(ConversationError::OpenClawGatewayUnreachable {
            detail: "OpenClaw Gateway is not running or cannot be reached at 127.0.0.1:18789.".into(),
        });

        assert_eq!(app.status_code(), StatusCode::BAD_GATEWAY);
        assert_eq!(app.error_code(), "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE");
        assert_eq!(app.public_message(), "OpenClaw Gateway is not reachable");
        let details = app.error_details().expect("details should be present");
        assert_eq!(details["backend"], "openclaw");
        assert_eq!(details["port"], 18789);
    }
}

#[cfg(test)]
mod route_shape_tests {
    /// axum builds its route trie eagerly, so an overlapping registration panics
    /// at construction, not at request time — `cargo check` would never catch it.
    /// `/messages/latest` deliberately sits before the `{messageId}` wildcard;
    /// this test is the guard that the pair stays registrable together.
    #[test]
    fn latest_message_route_coexists_with_the_message_id_wildcard() {
        let router: axum::Router<()> = axum::Router::new()
            .route(
                "/api/conversations/{id}/messages/latest",
                axum::routing::get(|| async { "latest" }),
            )
            .route(
                "/api/conversations/{id}/messages/{messageId}",
                axum::routing::get(|| async { "by-id" }),
            );
        let _ = router.into_make_service();
    }
}

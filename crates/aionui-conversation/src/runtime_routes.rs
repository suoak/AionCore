//! `POST /api/runtime/conversations/create` — the agent-facing create route,
//! and the 3.0 C7 delegation routes (`/api/runtime/delegations[/{id}]`).
//!
//! Authenticates on the three `x-aionui-*` headers itself, exactly like
//! `aionui-session-message/src/routes.rs`, and is therefore mounted OUTSIDE the
//! ordinary auth and CSRF middleware in `aionui-app`. The caller conversation is
//! the token-bound header — the body cannot name a different caller.

use std::sync::Arc;

use aionui_ai_agent::{RuntimeTokenScope, RuntimeTokenService, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use aionui_api_types::{
    ConversationCliEnvelope, ConversationCreateRequest, ConversationCreateResponse, ConversationToolErrorCode,
    ConversationToolErrorPayload, CreateDelegationRequest, DelegationResponse,
};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use tracing::warn;

use crate::error::ConversationError;
use crate::runtime_create::ConversationCreateError;
use crate::service::ConversationService;

const HEADER_USER_ID: &str = "x-aionui-user-id";
const HEADER_CONVERSATION_ID: &str = "x-aionui-conversation-id";
const HEADER_RUNTIME_TOKEN: &str = "x-aionui-runtime-token";
const COMMAND: &str = "conversation create";

/// Separate from `ConversationRouterState` so the ordinary conversation routes
/// keep their shape and this one carries only what it needs.
#[derive(Clone)]
pub struct ConversationRuntimeRouterState {
    pub service: ConversationService,
    pub runtime_token_service: Arc<RuntimeTokenService>,
}

pub fn conversation_runtime_routes(state: ConversationRuntimeRouterState) -> Router {
    Router::new()
        .route("/api/runtime/conversations/create", post(create))
        .route("/api/runtime/delegations", post(delegate).get(list_delegations))
        .route("/api/runtime/delegations/{id}", get(delegation_result))
        .with_state(state)
}

struct RuntimeCaller {
    user_id: String,
    conversation_id: String,
}

enum RuntimeCallerError {
    /// Missing or invalid token — 401.
    Unauthenticated,
    /// Valid helper token, but the session lacks the `conversation-create`
    /// skill (B-15) — 403 with the reason.
    SkillNotEnabled,
}

fn runtime_caller(
    state: &ConversationRuntimeRouterState,
    headers: &HeaderMap,
) -> Result<RuntimeCaller, RuntimeCallerError> {
    let caller = authenticated_caller(state, headers).ok_or(RuntimeCallerError::Unauthenticated)?;
    if let Some(token) = required_header(headers, HEADER_RUNTIME_TOKEN)
        && state
            .runtime_token_service
            .validate(
                Some(&token),
                &caller.user_id,
                &caller.conversation_id,
                RuntimeTokenScope::HelperConversationCreate,
                TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
            )
            .is_ok()
    {
        return Ok(caller);
    }
    Err(RuntimeCallerError::SkillNotEnabled)
}

fn authenticated_caller(state: &ConversationRuntimeRouterState, headers: &HeaderMap) -> Option<RuntimeCaller> {
    let user_id = required_header(headers, HEADER_USER_ID)?;
    let conversation_id = required_header(headers, HEADER_CONVERSATION_ID)?;
    let token = required_header(headers, HEADER_RUNTIME_TOKEN)?;
    state
        .runtime_token_service
        .validate(
            Some(&token),
            &user_id,
            &conversation_id,
            RuntimeTokenScope::ConversationHelper,
            // Every conversation's helper token is issued with this constant;
            // despite the prefix it is not team-specific.
            TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        )
        .ok()?;
    Some(RuntimeCaller {
        user_id,
        conversation_id,
    })
}

fn required_header(headers: &HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

type Reply = (StatusCode, Json<ConversationCliEnvelope<ConversationCreateResponse>>);

async fn create(
    State(state): State<ConversationRuntimeRouterState>,
    headers: HeaderMap,
    body: Result<Json<ConversationCreateRequest>, JsonRejection>,
) -> Reply {
    let caller = match runtime_caller(&state, &headers) {
        Ok(caller) => caller,
        Err(RuntimeCallerError::Unauthenticated) => {
            // No header values are logged — the token must never reach the logs.
            warn!(
                outcome = "rejected",
                error_code = "runtime_auth_failed",
                "agent conversation create refused"
            );
            return failure(
                StatusCode::UNAUTHORIZED,
                ConversationToolErrorPayload::new(ConversationToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
            );
        }
        Err(RuntimeCallerError::SkillNotEnabled) => {
            warn!(
                outcome = "rejected",
                error_code = "skill_not_enabled",
                "agent conversation create refused"
            );
            return failure(
                StatusCode::FORBIDDEN,
                ConversationToolErrorPayload::new(
                    ConversationToolErrorCode::SkillNotEnabled,
                    "skill `conversation-create` is not enabled in this conversation, so its helper token may not call this route",
                ),
            );
        }
    };
    // Body parsing is deliberately AFTER auth so an unauthenticated caller
    // learns nothing about the schema; `deny_unknown_fields` on the request
    // type turns stray fields into this same 400.
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => {
            return failure(
                StatusCode::BAD_REQUEST,
                ConversationToolErrorPayload::new(
                    ConversationToolErrorCode::SchemaValidationFailed,
                    format!("request body does not match the schema: {}", rejection.body_text()),
                ),
            );
        }
    };
    match state
        .service
        .create_for_conversation_helper(&caller.user_id, &caller.conversation_id, &request)
        .await
    {
        Ok(data) => (
            StatusCode::OK,
            Json(ConversationCliEnvelope::success(data, Some(COMMAND.to_owned()))),
        ),
        Err(error) => envelope_failure(error),
    }
}

fn envelope_failure(error: ConversationCreateError) -> Reply {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    failure(
        status,
        ConversationToolErrorPayload::new(error.code(), error.to_string()),
    )
}

fn failure(status: StatusCode, error: ConversationToolErrorPayload) -> Reply {
    (
        status,
        Json(ConversationCliEnvelope::failure(error, Some(COMMAND.to_owned()))),
    )
}

// ---------------------------------------------------------------------------
// 3.0 C7: delegation. Any conversation helper token may call these; the real
// gate is the user's per-conversation `@assistant` grant, checked by the
// service. The parent is always the token-bound conversation.
// ---------------------------------------------------------------------------

const DELEGATE_COMMAND: &str = "delegate";

type DelegationReply<T> = (StatusCode, Json<ConversationCliEnvelope<T>>);

fn delegation_failure<T>(status: StatusCode, error: ConversationToolErrorPayload) -> DelegationReply<T> {
    (
        status,
        Json(ConversationCliEnvelope::failure(
            error,
            Some(DELEGATE_COMMAND.to_owned()),
        )),
    )
}

fn delegation_unauthenticated<T>() -> DelegationReply<T> {
    // No header values are logged — the token must never reach the logs.
    warn!(
        outcome = "rejected",
        error_code = "runtime_auth_failed",
        "agent delegation refused"
    );
    delegation_failure(
        StatusCode::UNAUTHORIZED,
        ConversationToolErrorPayload::new(ConversationToolErrorCode::RuntimeAuthFailed, "runtime auth failed"),
    )
}

pub(crate) fn delegation_error_code(error: &ConversationError) -> (StatusCode, ConversationToolErrorCode) {
    use ConversationToolErrorCode as Code;
    match error {
        ConversationError::Forbidden { reason } if reason.starts_with("delegation_not_granted") => {
            (StatusCode::FORBIDDEN, Code::DelegationNotGranted)
        }
        ConversationError::Forbidden { .. } => (StatusCode::FORBIDDEN, Code::LaunchDenied),
        ConversationError::Unprocessable { reason } if reason.starts_with("delegation_depth") => {
            (StatusCode::UNPROCESSABLE_ENTITY, Code::DelegationDepthExceeded)
        }
        ConversationError::Busy { .. } => (StatusCode::TOO_MANY_REQUESTS, Code::DelegationLimitReached),
        ConversationError::NotFound { .. } | ConversationError::NotFoundReason { .. } => {
            (StatusCode::NOT_FOUND, Code::NotFound)
        }
        ConversationError::BadRequest { .. } | ConversationError::Unprocessable { .. } => {
            (StatusCode::BAD_REQUEST, Code::InvalidRequest)
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Code::Internal),
    }
}

fn delegation_reply<T>(result: Result<T, ConversationError>) -> DelegationReply<T> {
    match result {
        Ok(data) => (
            StatusCode::OK,
            Json(ConversationCliEnvelope::success(
                data,
                Some(DELEGATE_COMMAND.to_owned()),
            )),
        ),
        Err(error) => {
            let (status, code) = delegation_error_code(&error);
            warn!(
                outcome = "rejected",
                error_code = code.as_str(),
                "agent delegation refused"
            );
            delegation_failure(status, ConversationToolErrorPayload::new(code, error.to_string()))
        }
    }
}

async fn delegate(
    State(state): State<ConversationRuntimeRouterState>,
    headers: HeaderMap,
    body: Result<Json<CreateDelegationRequest>, JsonRejection>,
) -> DelegationReply<DelegationResponse> {
    let Some(caller) = authenticated_caller(&state, &headers) else {
        return delegation_unauthenticated();
    };
    let request = match body {
        Ok(Json(request)) => request,
        Err(rejection) => {
            return delegation_failure(
                StatusCode::BAD_REQUEST,
                ConversationToolErrorPayload::new(
                    ConversationToolErrorCode::SchemaValidationFailed,
                    format!("request body does not match the schema: {}", rejection.body_text()),
                ),
            );
        }
    };
    delegation_reply(
        state
            .service
            .delegate(
                &caller.user_id,
                &caller.conversation_id,
                &request.assistant_id,
                &request.task_text,
            )
            .await,
    )
}

async fn delegation_result(
    State(state): State<ConversationRuntimeRouterState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> DelegationReply<DelegationResponse> {
    let Some(caller) = authenticated_caller(&state, &headers) else {
        return delegation_unauthenticated();
    };
    delegation_reply(
        state
            .service
            .delegation_result(&caller.user_id, &caller.conversation_id, &id)
            .await,
    )
}

async fn list_delegations(
    State(state): State<ConversationRuntimeRouterState>,
    headers: HeaderMap,
) -> DelegationReply<Vec<DelegationResponse>> {
    let Some(caller) = authenticated_caller(&state, &headers) else {
        return delegation_unauthenticated();
    };
    delegation_reply(
        state
            .service
            .list_delegations(&caller.user_id, &caller.conversation_id)
            .await,
    )
}

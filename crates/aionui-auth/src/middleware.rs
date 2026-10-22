#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;
use aionui_db::{IUserRepository, UserStatus, UserType};

use crate::JwtService;
use crate::agent_approval::{
    AGENT_APPROVAL_ID_HEADER, AGENT_APPROVAL_REQUIRED_CODE, AgentActionApprovals, ApprovalCheck, request_fingerprint,
};
use crate::agent_scope::{
    AgentRouteDecision, classify_agent_request, filter_conversation_list, is_rename_only_patch, redact_secrets,
};
use crate::extract::extract_token_from_headers;
use crate::local_secret::LocalApiSecret;

/// Largest request body an agent token may send to a gated route (the body is
/// buffered to fingerprint it for approval).
const AGENT_REQUEST_BODY_LIMIT: usize = 8 * 1024 * 1024;
/// Largest JSON response redacted for an agent token; larger ones are refused
/// rather than passed through unredacted.
const AGENT_RESPONSE_BODY_LIMIT: usize = 32 * 1024 * 1024;

/// Marker inserted into request extensions when the caller authenticated with
/// an agent (conversation-runtime) token rather than as the user.
#[derive(Debug, Clone)]
pub struct AgentPrincipal {
    /// Conversation the agent token is bound to.
    pub conversation_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthIdentityMode {
    Local,
    UserSession,
    AionPro,
}

/// Header carrying the conversation-runtime helper token minted by the backend
/// and injected into agent subprocess environments as `AIONUI_RUNTIME_TOKEN`.
pub const RUNTIME_TOKEN_HEADER: &str = "x-aionui-runtime-token";
/// Header carrying the acting user id asserted by the helper CLI.
pub const RUNTIME_USER_ID_HEADER: &str = "x-aionui-user-id";
/// Header carrying the conversation id the helper CLI runs inside.
pub const RUNTIME_CONVERSATION_ID_HEADER: &str = "x-aionui-conversation-id";

/// Port for validating conversation-runtime helper tokens.
///
/// Implemented in the composition layer over the agent runtime's token
/// service; `aionui-auth` must not depend on `aionui-ai-agent` directly.
/// A verifier must confirm the token is a live, conversation-helper-scoped
/// token bound to exactly this (user_id, conversation_id) pair.
pub trait IRuntimeTokenVerifier: Send + Sync {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool;
}

/// Port answering "may the agent running in `own` read conversation `target`?"
/// beyond its own conversation — i.e. did the user grant `own -> target` with
/// `@@` (see `aionui_conversation::session_mentions::SessionMentionGrants`).
///
/// Implemented in the composition layer; `None` in [`AuthState`] means an agent
/// token may read only its own conversation.
pub trait IAgentConversationAccess: Send + Sync {
    fn is_granted(&self, user_id: &str, own_conversation_id: &str, target_conversation_id: &str) -> bool;
}

/// Authenticated user injected into request extensions by the auth middleware.
///
/// Route handlers extract this from `request.extensions()` to identify
/// the current user.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    /// User ID from the database.
    pub id: String,
    /// Username.
    pub username: String,
    /// Internal identity source for the current user.
    pub user_type: UserType,
    /// Current account status. Authenticated requests only receive active users.
    pub status: UserStatus,
}

impl CurrentUser {
    pub fn local_default() -> Self {
        Self {
            id: "system_default_user".to_string(),
            username: "system_default_user".to_string(),
            user_type: UserType::Local,
            status: UserStatus::Active,
        }
    }
}

/// Shared state for the authentication middleware.
#[derive(Clone)]
pub struct AuthState {
    pub jwt_service: Arc<JwtService>,
    pub user_repo: Arc<dyn IUserRepository>,
    pub identity_mode: AuthIdentityMode,
    /// Optional second credential channel for agent-subprocess helper CLIs
    /// (`aioncore config` / `diagnose`), which cannot carry a JWT or cookies.
    /// `None` disables the channel (requests without a JWT are rejected).
    pub runtime_token_verifier: Option<Arc<dyn IRuntimeTokenVerifier>>,
    /// Per-launch desktop secret (local mode only). `None` keeps the legacy
    /// behaviour where every local caller is the default user. When set, only
    /// requests presenting the secret are the desktop user; all other callers
    /// fall through to session-JWT and runtime-token authentication.
    pub local_secret: Option<LocalApiSecret>,
    /// Approval store for configuration writes requested by agent tokens.
    /// `None` denies such writes outright.
    pub agent_approvals: Option<Arc<AgentActionApprovals>>,
    /// `@@` read grants for agent tokens. `None`: own conversation only.
    pub agent_conversation_access: Option<Arc<dyn IAgentConversationAccess>>,
}

impl AuthState {
    /// Whether an agent token bound to `own` may read `target`.
    fn agent_may_read(&self, user_id: &str, own: &str, target: &str) -> bool {
        target == own
            || self
                .agent_conversation_access
                .as_ref()
                .is_some_and(|access| access.is_granted(user_id, own, target))
    }
}

/// Authentication middleware that verifies JWT tokens and injects `CurrentUser`.
///
/// Flow:
/// 1. Extract bearer token from `Authorization` header or `aionui-session` cookie
/// 2. Verify JWT signature, expiration, and blacklist
/// 3. Look up user in the database to ensure they still exist
/// 4. Insert [`CurrentUser`] into request extensions
///
/// Returns HTTP 401 for authentication failures.
///
/// Use with `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(state): State<AuthState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // In local mode the desktop app is the default user. With a per-launch
    // secret configured, only callers presenting it get that identity; others
    // (WebUI browsers via the desktop proxy, agent helper CLIs) must
    // authenticate below like any non-local client.
    if state.identity_mode == AuthIdentityMode::Local {
        let trusted = match state.local_secret.as_ref() {
            None => true,
            Some(secret) => secret.matches_headers(request.headers()),
        };
        if trusted {
            request.extensions_mut().insert(CurrentUser::local_default());
            return Ok(next.run(request).await);
        }
    }

    let Some(token) = extract_token_from_headers(request.headers()) else {
        // No JWT/cookie: fall back to the conversation-helper runtime-token
        // channel used by agent subprocess CLIs.
        return runtime_token_channel(&state, request, next).await;
    };

    // Require an *access* token here: a refresh token must never authenticate an
    // ordinary request — it is accepted only at the refresh endpoint.
    let payload = state.jwt_service.verify_access(&token).map_err(|e| {
        tracing::debug!("Token verification failed: {e}");
        ApiError::Unauthorized("Invalid or expired token".into())
    })?;

    let user = state
        .user_repo
        .find_active_by_id(&payload.user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "auth middleware user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    if payload.session_generation != user.session_generation {
        return Err(ApiError::Unauthorized("Invalid authentication session".into()));
    }

    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });

    Ok(next.run(request).await)
}

/// Authenticate a JWT-less request via the conversation-helper runtime token.
///
/// The helper CLI sends the token the backend minted for its conversation
/// runtime plus the (user, conversation) pair the token was bound to. The
/// verifier enforces that binding, so a forged user or conversation header
/// fails closed. On success the token's user is loaded and injected as
/// [`CurrentUser`], making ordinary user-scoped handlers work unchanged.
async fn runtime_token_channel(state: &AuthState, request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(verifier), Some(token), Some(user_id), Some(conversation_id)) = (
        state.runtime_token_verifier.as_ref(),
        header(RUNTIME_TOKEN_HEADER),
        header(RUNTIME_USER_ID_HEADER),
        header(RUNTIME_CONVERSATION_ID_HEADER),
    ) else {
        return Err(ApiError::Unauthorized("Authentication required".into()));
    };

    if !verifier.verify_conversation_helper(&token, &user_id, &conversation_id) {
        return Err(ApiError::Unauthorized("Invalid runtime token".into()));
    }

    let user = state
        .user_repo
        .find_active_by_id(&user_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "runtime token channel user lookup failed");
            ApiError::Internal("Authentication service unavailable".into())
        })?
        .ok_or_else(|| ApiError::Unauthorized("Invalid authentication subject".into()))?;

    if state.identity_mode == AuthIdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return Err(ApiError::coded(
            StatusCode::UNAUTHORIZED,
            "USER_CONTEXT_REQUIRED",
            "User context required.",
            None,
        ));
    }

    let is_conversation_list = matches!(
        classify_agent_request(request.method(), request.uri().path(), &conversation_id),
        AgentRouteDecision::AllowConversationList
    );
    let mut request = enforce_agent_scope(state, request, &user.id, &conversation_id).await?;
    let redact_response = request.method() == axum::http::Method::GET;
    request.extensions_mut().insert(AgentPrincipal {
        conversation_id: conversation_id.clone(),
    });
    request.extensions_mut().insert(CurrentUser {
        id: user.id,
        username: user.username.unwrap_or_else(|| "external_user".to_string()),
        user_type: user.user_type,
        status: user.status,
    });

    let user_id = request
        .extensions()
        .get::<CurrentUser>()
        .map(|user| user.id.clone())
        .unwrap_or_default();
    let response = next.run(request).await;
    if redact_response {
        let list_filter = is_conversation_list.then(|| (state, user_id.as_str(), conversation_id.as_str()));
        redact_json_response(response, list_filter).await
    } else {
        Ok(response)
    }
}

fn scope_denied(message: &str) -> ApiError {
    ApiError::coded(StatusCode::FORBIDDEN, "AGENT_SCOPE_DENIED", message, None)
}

async fn buffer_body(request: Request) -> Result<(Request, axum::body::Bytes), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, AGENT_REQUEST_BODY_LIMIT)
        .await
        .map_err(|_| ApiError::PayloadTooLarge("Request body too large".into()))?;
    Ok((Request::from_parts(parts, axum::body::Body::from(bytes.clone())), bytes))
}

/// Apply the agent-token route scope (see [`crate::agent_scope`]).
async fn enforce_agent_scope(
    state: &AuthState,
    request: Request,
    user_id: &str,
    conversation_id: &str,
) -> Result<Request, ApiError> {
    let method = request.method().clone();
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| request.uri().path().to_owned());

    match classify_agent_request(&method, request.uri().path(), conversation_id) {
        AgentRouteDecision::Allow | AgentRouteDecision::AllowConversationList => Ok(request),
        AgentRouteDecision::AllowConversationRead(target) => {
            if state.agent_may_read(user_id, conversation_id, &target) {
                Ok(request)
            } else {
                tracing::info!(
                    method = %method,
                    path = request.uri().path(),
                    "agent token read of a conversation that was not @@-granted"
                );
                Err(scope_denied(
                    "Agent tokens may only read their own conversation and conversations the user selected with @@.",
                ))
            }
        }
        AgentRouteDecision::Deny => {
            tracing::info!(method = %method, path = request.uri().path(), "agent token request outside scope");
            Err(scope_denied("This route is not available to agent tokens."))
        }
        AgentRouteDecision::AllowOwnRename => {
            let (request, body) = buffer_body(request).await?;
            if is_rename_only_patch(&body) {
                Ok(request)
            } else {
                Err(scope_denied("Agent tokens may only rename their own conversation."))
            }
        }
        AgentRouteDecision::RequireApproval(kind) => {
            let Some(store) = state.agent_approvals.as_ref() else {
                return Err(scope_denied(
                    "Configuration changes from agents are disabled; make the change in the app.",
                ));
            };
            let approval_id = request
                .headers()
                .get(AGENT_APPROVAL_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let (request, body) = buffer_body(request).await?;
            let fingerprint = request_fingerprint(method.as_str(), &path_and_query, &body);

            if let Some(approval_id) = approval_id {
                return match store.check_and_consume(&approval_id, user_id, conversation_id, &fingerprint) {
                    ApprovalCheck::Granted => {
                        tracing::info!(method = %method, path = request.uri().path(), "agent config write approved by user");
                        Ok(request)
                    }
                    ApprovalCheck::Pending => Err(ApiError::coded(
                        StatusCode::CONFLICT,
                        "AGENT_ACTION_APPROVAL_PENDING",
                        "Waiting for the user to approve this change in CSBU WorkMate.",
                        serde_json::json!({ "approval_id": approval_id }),
                    )),
                    ApprovalCheck::Denied => Err(ApiError::coded(
                        StatusCode::FORBIDDEN,
                        "AGENT_ACTION_APPROVAL_DENIED",
                        "The user denied this change.",
                        serde_json::json!({ "approval_id": approval_id }),
                    )),
                    ApprovalCheck::Invalid => Err(ApiError::coded(
                        StatusCode::FORBIDDEN,
                        "AGENT_ACTION_APPROVAL_INVALID",
                        "Approval is unknown, expired, already used, or does not match this request.",
                        None,
                    )),
                };
            }

            let Some(record) = store.request(user_id, conversation_id, kind, method.as_str(), &path_and_query, &body)
            else {
                return Err(ApiError::RateLimited);
            };
            Err(ApiError::coded(
                StatusCode::FORBIDDEN,
                AGENT_APPROVAL_REQUIRED_CODE,
                "This change needs the user's approval in CSBU WorkMate.",
                serde_json::json!({
                    "approval_id": record.id,
                    "kind": record.kind,
                    "status_path": format!("/api/agent-approvals/{}", record.id),
                    "expires_at": record.expires_at,
                }),
            ))
        }
    }
}

/// Redact secrets from a JSON response served to an agent token.
async fn redact_json_response(
    response: Response,
    conversation_list_filter: Option<(&AuthState, &str, &str)>,
) -> Result<Response, ApiError> {
    let is_json = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if !is_json {
        if conversation_list_filter.is_some() && response.status().is_success() {
            // Never pass an unfilterable conversation list through.
            return Err(scope_denied(
                "Unexpected conversation list response for an agent token.",
            ));
        }
        return Ok(response);
    }
    let (mut parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, AGENT_RESPONSE_BODY_LIMIT)
        .await
        .map_err(|_| ApiError::PayloadTooLarge("Response too large for an agent token".into()))?;
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        if conversation_list_filter.is_some() {
            return Err(scope_denied(
                "Unexpected conversation list response for an agent token.",
            ));
        }
        return Ok(Response::from_parts(parts, axum::body::Body::from(bytes)));
    };
    if let Some((state, user_id, own)) = conversation_list_filter {
        filter_conversation_list(&mut value, |id| state.agent_may_read(user_id, own, id));
    }
    redact_secrets(&mut value);
    let redacted = serde_json::to_vec(&value).map_err(|_| ApiError::Internal("Failed to encode response".into()))?;
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Ok(Response::from_parts(parts, axum::body::Body::from(redacted)))
}

/// Local-mode authentication middleware that skips JWT verification.
///
/// Injects a fixed `CurrentUser` with id and username `system_default_user`.
/// Used when the server runs as an embedded subprocess inside Electron.
pub async fn local_auth_middleware(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(CurrentUser::local_default());
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    async fn echo_user(request: Request<Body>) -> String {
        let user = request.extensions().get::<CurrentUser>().unwrap();
        format!("{}:{}", user.id, user.username)
    }

    #[tokio::test]
    async fn test_local_auth_middleware_injects_default_user() {
        let app = Router::new()
            .route("/test", get(echo_user))
            .route_layer(axum::middleware::from_fn(local_auth_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "system_default_user:system_default_user"
        );
    }
}

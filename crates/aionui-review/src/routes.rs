// `ApiError` is the intended error type at this HTTP boundary.
#![allow(clippy::disallowed_types)]

//! `/api/review-threads*` (design §6.1). User identity only: the prefix is
//! not in the agent-token allow-list, so agent tokens are denied (403) by
//! `aionui-auth`'s agent scope before reaching these handlers. The routes
//! deliberately live outside `/api/conversations/{id}/…` so they never fall
//! under `AllowConversationRead`.

use std::sync::Arc;

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use crate::error::ReviewError;
use crate::service::ReviewService;
use crate::types::{
    AddReviewCommentRequest, CreateReviewThreadRequest, ListReviewThreadsQuery, ReviewThread, UpdateReviewThreadRequest,
};

#[derive(Clone)]
pub struct ReviewRouterState {
    pub service: Arc<ReviewService>,
}

/// Build the review router. The caller applies `auth_middleware`.
pub fn review_routes(state: ReviewRouterState) -> Router {
    Router::new()
        .route("/api/review-threads", get(list_threads).post(create_thread))
        .route(
            "/api/review-threads/{id}",
            get(get_thread).patch(update_thread).delete(delete_thread),
        )
        .route("/api/review-threads/{id}/resolve", post(resolve_thread))
        .route("/api/review-threads/{id}/reopen", post(reopen_thread))
        .route("/api/review-threads/{id}/comments", post(add_comment))
        .with_state(state)
}

fn to_api_error(error: ReviewError) -> ApiError {
    match error {
        ReviewError::BadRequest(message) => {
            ApiError::coded(StatusCode::BAD_REQUEST, "REVIEW_BAD_REQUEST", message, None)
        }
        ReviewError::NotFound => ApiError::coded(
            StatusCode::NOT_FOUND,
            "REVIEW_THREAD_NOT_FOUND",
            "Review thread not found",
            None,
        ),
        ReviewError::Conflict { code, message } => ApiError::coded(StatusCode::CONFLICT, code, message, None),
        ReviewError::Internal(detail) => {
            tracing::error!(error = %detail, "review service failure");
            ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "REVIEW_INTERNAL",
                "Review service failure",
                None,
            )
        }
    }
}

type ThreadResponse = Result<Json<ApiResponse<ReviewThread>>, ApiError>;

async fn list_threads(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListReviewThreadsQuery>,
) -> Result<Json<ApiResponse<Vec<ReviewThread>>>, ApiError> {
    let threads = state.service.list(&user.id, query).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(threads)))
}

async fn create_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(req): Json<CreateReviewThreadRequest>,
) -> ThreadResponse {
    let thread = state.service.create(&user.id, req).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

async fn get_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> ThreadResponse {
    let thread = state.service.get(&user.id, &id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

async fn update_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(req): Json<UpdateReviewThreadRequest>,
) -> ThreadResponse {
    let thread = state.service.update(&user.id, &id, req).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

async fn delete_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<()>>, ApiError> {
    state.service.delete(&user.id, &id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(())))
}

async fn resolve_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> ThreadResponse {
    let thread = state.service.resolve(&user.id, &id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

async fn reopen_thread(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> ThreadResponse {
    let thread = state.service.reopen(&user.id, &id).await.map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

async fn add_comment(
    State(state): State<ReviewRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    Json(req): Json<AddReviewCommentRequest>,
) -> ThreadResponse {
    let thread = state
        .service
        .add_comment(&user.id, &id, req)
        .await
        .map_err(to_api_error)?;
    Ok(Json(ApiResponse::ok(thread)))
}

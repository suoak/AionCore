#![allow(clippy::disallowed_types)]

//! `/api/batch*`. User identity only: the prefix is deliberately absent from
//! the agent-token allowlists in `aionui_auth::agent_scope`, so agent tokens
//! get 403 `AGENT_SCOPE_DENIED` before reaching a handler (an agent must not
//! fan work out across many repositories on its own).

use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};

use aionui_api_types::{
    ApiResponse, BatchItemResponse, BatchJobResponse, BatchListQuery, BatchRetryRequest, CreateBatchJobRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::error::BatchError;
use crate::service::BatchService;

#[derive(Clone)]
pub struct BatchRouterState {
    pub service: Arc<BatchService>,
}

impl From<BatchError> for ApiError {
    fn from(err: BatchError) -> Self {
        let status = match &err {
            BatchError::BadRequest(_) => StatusCode::BAD_REQUEST,
            BatchError::NotFound(_) => StatusCode::NOT_FOUND,
            BatchError::InvalidState(_) => StatusCode::CONFLICT,
            BatchError::InsufficientDisk(_) => StatusCode::INSUFFICIENT_STORAGE,
            BatchError::Unavailable(_) => StatusCode::FAILED_DEPENDENCY,
            BatchError::Database(_) | BatchError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::coded(status, err.code(), err.to_string(), None)
    }
}

pub fn batch_routes(state: BatchRouterState) -> Router {
    Router::new()
        .route("/api/batch", get(list_jobs).post(create_job))
        .route("/api/batch/{id}", get(get_job))
        .route("/api/batch/{id}/items", get(list_items))
        .route("/api/batch/{id}/trial", post(start_trial))
        .route("/api/batch/{id}/scale", post(scale))
        .route("/api/batch/{id}/retry", post(retry))
        .route("/api/batch/{id}/cancel", post(cancel))
        .route("/api/batch/{id}/export.csv", get(export_csv))
        .with_state(state)
}

type JobResult = Result<Json<ApiResponse<BatchJobResponse>>, ApiError>;

async fn create_job(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateBatchJobRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<BatchJobResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let job = state.service.create(&user.id, req).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(job))))
}

async fn list_jobs(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<BatchListQuery>,
) -> Result<Json<ApiResponse<Vec<BatchJobResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.list(&user.id, &query).await?)))
}

async fn get_job(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> JobResult {
    Ok(Json(ApiResponse::ok(state.service.get(&user.id, &id).await?)))
}

async fn list_items(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<Vec<BatchItemResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.items(&user.id, &id).await?)))
}

async fn start_trial(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> JobResult {
    Ok(Json(ApiResponse::ok(state.service.start_trial(&user.id, &id).await?)))
}

async fn scale(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> JobResult {
    Ok(Json(ApiResponse::ok(state.service.scale(&user.id, &id).await?)))
}

async fn retry(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Option<Json<BatchRetryRequest>>,
) -> JobResult {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    Ok(Json(ApiResponse::ok(state.service.retry(&user.id, &id, &req).await?)))
}

async fn cancel(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> JobResult {
    Ok(Json(ApiResponse::ok(state.service.cancel(&user.id, &id).await?)))
}

async fn export_csv(
    State(state): State<BatchRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let csv = state.service.export_csv(&user.id, &id).await?;
    let disposition = format!("attachment; filename=\"batch-{}.csv\"", sanitize_filename(&id));
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        csv,
    ))
}

fn sanitize_filename(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

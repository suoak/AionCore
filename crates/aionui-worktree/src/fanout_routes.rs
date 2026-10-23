#![allow(clippy::disallowed_types)]

//! `/api/fanout-groups*` (E3-4). User identity only: the prefix is absent
//! from the agent-token allowlists, so agent tokens get 403
//! `AGENT_SCOPE_DENIED` before reaching a handler.

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;
use aionui_common::ApiError;

use crate::fanout::FanoutService;
use crate::fanout_types::{
    AdoptFanoutRequest, CreateFanoutGroupRequest, FanoutCompareResponse, FanoutGroupResponse, ListFanoutGroupsQuery,
};

#[derive(Clone)]
pub struct FanoutRouterState {
    pub service: FanoutService,
}

pub fn fanout_routes(state: FanoutRouterState) -> Router {
    Router::new()
        .route("/api/fanout-groups", get(list_groups).post(create_group))
        .route("/api/fanout-groups/{id}", get(get_group))
        .route("/api/fanout-groups/{id}/compare", get(compare_group))
        .route("/api/fanout-groups/{id}/adopt", post(adopt_group))
        .route("/api/fanout-groups/{id}/discard", post(discard_group))
        .route("/api/fanout-groups/{id}/cancel", post(cancel_group))
        .with_state(state)
}

async fn create_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<CreateFanoutGroupRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<FanoutGroupResponse>>), ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let group = state.service.create(&user.id, req).await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(group))))
}

async fn list_groups(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListFanoutGroupsQuery>,
) -> Result<Json<ApiResponse<Vec<FanoutGroupResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.list(&user.id, query).await?)))
}

async fn get_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FanoutGroupResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.get(&user.id, &id).await?)))
}

async fn compare_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FanoutCompareResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.compare(&user.id, &id).await?)))
}

async fn adopt_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<AdoptFanoutRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<FanoutGroupResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(state.service.adopt(&user.id, &id, req).await?)))
}

async fn discard_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FanoutGroupResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.discard(&user.id, &id).await?)))
}

async fn cancel_group(
    State(state): State<FanoutRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<FanoutGroupResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.cancel(&user.id, &id).await?)))
}

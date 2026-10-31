#![allow(clippy::disallowed_types)]
//! `/api/auth/devices/pair/*` (M8 E9-1).
//!
//! Desktop side (mount behind `auth_middleware`; a device token gets 403 from
//! the device scope, an agent token from the agent scope):
//! - `POST /api/auth/devices/pair/offers` — new QR token + 6-digit code
//! - `GET  /api/auth/devices/pair/offers/{offer_id}` — status (also pushed as
//!   the `device.pairing` WebSocket event)
//! - `POST /api/auth/devices/pair/offers/{offer_id}/reject` — reject / cancel
//!
//! Phone side (public, rate limited, CSRF exempt):
//! - `POST /api/auth/devices/pair/start {qr_token, device_name, platform}`
//! - `POST /api/auth/devices/pair/confirm {pairing_id, code}` → device token
use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use aionui_db::{CreateUserDeviceParams, IUserDeviceRepository};

use super::pairing::{DevicePairingStore, PairingConfirmError, PairingOffer, PairingOfferView, PairingStartError};
use super::token::{
    DevicePrincipal, MAX_DEVICES_PER_USER, MOBILE_DEFAULT_SCOPE, generate_device_token, hash_device_token,
};
use crate::middleware::{AgentPrincipal, CurrentUser};
use crate::rate_limit::{RateLimiter, auth_rate_limit_middleware};

const DEVICE_NAME_MAX_CHARS: usize = 64;
const PLATFORMS: &[&str] = &["ios", "android", "web"];
/// Failed phone-side calls allowed per client IP per window. The per-pairing
/// limit of five wrong codes is the real brute-force bound; this caps how
/// many pairings one client can grind through.
const PAIR_FAILURES_PER_WINDOW: u32 = 20;
const PAIR_WINDOW: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct DeviceRouterState {
    pub devices: Arc<dyn IUserDeviceRepository>,
    pub pairings: Arc<DevicePairingStore>,
}

#[derive(Debug, Deserialize)]
pub struct PairStartRequest {
    pub qr_token: String,
    pub device_name: String,
    pub platform: String,
}

#[derive(Debug, Serialize)]
pub struct PairStartResponse {
    pub pairing_id: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct PairConfirmRequest {
    pub pairing_id: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct PairConfirmResponse {
    pub device_id: String,
    /// Shown once; the server keeps only its SHA-256.
    pub device_token: String,
    pub name: String,
    pub platform: String,
    pub scopes: Vec<String>,
}

fn device_limit_reached() -> ApiError {
    ApiError::coded(
        StatusCode::CONFLICT,
        "DEVICE_LIMIT_REACHED",
        format!("At most {MAX_DEVICES_PER_USER} devices can be paired. Remove one first."),
        Some(json!({ "max_devices": MAX_DEVICES_PER_USER })),
    )
}

fn db_unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "device store unavailable");
    ApiError::Internal("Device store unavailable".into())
}

/// Desktop endpoints are for the signed-in user only — never a device or an
/// agent token, whatever middleware they end up mounted behind.
fn ensure_user_session(
    device: &Option<Extension<DevicePrincipal>>,
    agent: &Option<Extension<AgentPrincipal>>,
) -> Result<(), ApiError> {
    if device.is_some() || agent.is_some() {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "DEVICE_MANAGEMENT_REQUIRES_USER",
            "Device pairing is only available to the signed-in user.",
            None,
        ));
    }
    Ok(())
}

fn validate_device_name(raw: &str) -> Result<String, ApiError> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > DEVICE_NAME_MAX_CHARS || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(format!(
            "device_name must be 1-{DEVICE_NAME_MAX_CHARS} printable characters"
        )));
    }
    Ok(name.to_owned())
}

fn validate_platform(raw: &str) -> Result<&'static str, ApiError> {
    PLATFORMS
        .iter()
        .copied()
        .find(|platform| *platform == raw)
        .ok_or_else(|| ApiError::BadRequest("platform must be one of ios, android, web".into()))
}

async fn create_offer_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<PairingOffer>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let active = state.devices.count_active(&user.id).await.map_err(db_unavailable)?;
    if active >= MAX_DEVICES_PER_USER {
        return Err(device_limit_reached());
    }
    let offer = state.pairings.create_offer(&user.id);
    tracing::info!(
        target: "audit",
        action = "device.pair_offer",
        user_id = %user.id,
        offer_id = %offer.offer_id,
        "device pairing offer created"
    );
    Ok(Json(ApiResponse::ok(offer)))
}

async fn offer_status_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(offer_id): Path<String>,
) -> Result<Json<ApiResponse<PairingOfferView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    state
        .pairings
        .view(&user.id, &offer_id)
        .map(|view| Json(ApiResponse::ok(view)))
        .ok_or_else(|| ApiError::NotFound("Pairing offer not found".into()))
}

async fn reject_offer_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(offer_id): Path<String>,
) -> Result<Json<ApiResponse<PairingOfferView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let view = state.pairings.reject(&user.id, &offer_id).ok_or_else(|| {
        ApiError::coded(
            StatusCode::CONFLICT,
            "DEVICE_PAIRING_CLOSED",
            "Pairing offer is not open.",
            None,
        )
    })?;
    tracing::info!(
        target: "audit",
        action = "device.pair_reject",
        user_id = %user.id,
        offer_id = %offer_id,
        "device pairing rejected on the desktop"
    );
    Ok(Json(ApiResponse::ok(view)))
}

async fn pair_start_handler(
    State(state): State<DeviceRouterState>,
    body: Result<Json<PairStartRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PairStartResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let name = validate_device_name(&req.device_name)?;
    let platform = validate_platform(&req.platform)?;
    let started =
        state
            .pairings
            .start(req.qr_token.trim(), &name, platform)
            .map_err(|PairingStartError::InvalidQr| {
                ApiError::coded(
                    StatusCode::UNAUTHORIZED,
                    "DEVICE_PAIRING_QR_INVALID",
                    "Pairing QR code is invalid, expired, or already used.",
                    None,
                )
            })?;
    tracing::info!(
        target: "audit",
        action = "device.pair_start",
        pairing_id = %started.pairing_id,
        platform,
        "device pairing started"
    );
    Ok(Json(ApiResponse::ok(PairStartResponse {
        pairing_id: started.pairing_id,
        expires_at: started.expires_at,
    })))
}

async fn pair_confirm_handler(
    State(state): State<DeviceRouterState>,
    body: Result<Json<PairConfirmRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<PairConfirmResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let confirmed = state
        .pairings
        .confirm(req.pairing_id.trim(), &req.code)
        .map_err(|err| match err {
            PairingConfirmError::WrongCode { attempts_left } => ApiError::coded(
                StatusCode::UNAUTHORIZED,
                "DEVICE_PAIRING_CODE_INVALID",
                "Incorrect pairing code.",
                Some(json!({ "attempts_left": attempts_left })),
            ),
            PairingConfirmError::Closed { status } => ApiError::coded(
                StatusCode::GONE,
                "DEVICE_PAIRING_CLOSED",
                "Pairing is no longer valid. Scan a new code on the desktop.",
                Some(json!({ "status": status })),
            ),
        })?;

    let token = generate_device_token();
    let token_hash = hash_device_token(&token);
    let scopes = vec![MOBILE_DEFAULT_SCOPE.to_owned()];
    let scopes_json = serde_json::to_string(&scopes).map_err(|e| ApiError::Internal(e.to_string()))?;
    let created = state
        .devices
        .create_within_limit(
            &CreateUserDeviceParams {
                user_id: &confirmed.user_id,
                name: &confirmed.device_name,
                platform: &confirmed.platform,
                token_hash: &token_hash,
                scopes: &scopes_json,
                created_at: aionui_common::now_ms(),
            },
            MAX_DEVICES_PER_USER,
        )
        .await;
    let row = match created {
        Ok(Some(row)) => row,
        Ok(None) => {
            state.pairings.complete(&confirmed.offer_id, None);
            return Err(device_limit_reached());
        }
        Err(e) => {
            state.pairings.complete(&confirmed.offer_id, None);
            return Err(db_unavailable(e));
        }
    };
    state.pairings.complete(&confirmed.offer_id, Some(&row.id));
    tracing::info!(
        target: "audit",
        action = "device.paired",
        user_id = %row.user_id,
        device_id = %row.id,
        platform = %row.platform,
        "device paired"
    );
    Ok(Json(ApiResponse::ok(PairConfirmResponse {
        device_id: row.id,
        device_token: token,
        name: row.name,
        platform: row.platform,
        scopes,
    })))
}

/// Phone-side endpoints. Public (the QR token / pairing id + code are the
/// credentials) and rate limited per client IP on failures.
pub fn device_pairing_public_routes(state: DeviceRouterState) -> Router {
    let limiter = Arc::new(RateLimiter::new(PAIR_FAILURES_PER_WINDOW, PAIR_WINDOW));
    limiter.start_cleanup_task(Duration::from_secs(60));
    Router::new()
        .route("/api/auth/devices/pair/start", post(pair_start_handler))
        .route("/api/auth/devices/pair/confirm", post(pair_confirm_handler))
        .route_layer(from_fn_with_state(limiter, auth_rate_limit_middleware))
        .with_state(state)
}

/// Desktop-side endpoints. Mount behind `auth_middleware`.
pub fn device_pairing_user_routes(state: DeviceRouterState) -> Router {
    Router::new()
        .route("/api/auth/devices/pair/offers", post(create_offer_handler))
        .route("/api/auth/devices/pair/offers/{offer_id}", get(offer_status_handler))
        .route(
            "/api/auth/devices/pair/offers/{offer_id}/reject",
            post(reject_offer_handler),
        )
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_name_and_platform_are_validated() {
        assert_eq!(validate_device_name("  Pixel 9 ").unwrap(), "Pixel 9");
        assert!(validate_device_name("   ").is_err());
        assert!(validate_device_name("a\u{0007}b").is_err());
        assert!(validate_device_name(&"x".repeat(65)).is_err());
        assert!(validate_device_name(&"手".repeat(64)).is_ok());
        assert_eq!(validate_platform("ios").unwrap(), "ios");
        assert!(validate_platform("IOS").is_err());
        assert!(validate_platform("symbian").is_err());
    }
}

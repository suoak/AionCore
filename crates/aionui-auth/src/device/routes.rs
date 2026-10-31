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
//! Device management (M8 E9-2; signed-in user only):
//! - `GET    /api/auth/devices` — the user's active devices
//! - `PATCH  /api/auth/devices/{device_id} {name}` — rename
//! - `DELETE /api/auth/devices/{device_id}` — revoke; the next request with
//!   its token is 401 and its WebSocket connections are closed
//! - `PUT    /api/auth/devices/{device_id}/push {push_kind, push_target}` —
//!   how the device is notified (M8 E9-6): `none`, or `im` with an IM
//!   channel session the user owns. APNs/FCM are not implemented (D9-3).
//!
//! Phone side (public, rate limited, CSRF exempt):
//! - `POST /api/auth/devices/pair/start {qr_token, device_name, platform}`
//! - `POST /api/auth/devices/pair/confirm {pairing_id, code}` → device token
use std::sync::Arc;
use std::time::Duration;

use aionui_db::{AuditActorKind, AuditSink, AuditVia, NewAuditEvent};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, Path, State};
use axum::http::StatusCode;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, patch, post, put};
use axum::{Extension, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use aionui_db::{CreateUserDeviceParams, IUserDeviceRepository, UserDeviceRow};

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

/// Called after a device is revoked by its owner (closes its WebSockets).
pub type DeviceRevokedHook = Arc<dyn Fn(&UserDeviceRow) + Send + Sync>;

/// Checks that an IM push target (an IM channel session id) belongs to the
/// user and can be delivered to. Implemented by the app over the channel
/// store; without one, `push_kind = im` is refused.
#[async_trait::async_trait]
pub trait DevicePushTargetValidator: Send + Sync {
    async fn validate_im_target(&self, user_id: &str, target: &str) -> Result<(), String>;
}

const PUSH_TARGET_MAX_CHARS: usize = 128;

#[derive(Clone)]
pub struct DeviceRouterState {
    pub devices: Arc<dyn IUserDeviceRepository>,
    pub pairings: Arc<DevicePairingStore>,
    pub on_revoked: Option<DeviceRevokedHook>,
    pub push_targets: Option<Arc<dyn DevicePushTargetValidator>>,
    /// Audit ledger for pairing / rename / revoke / push changes (M8 E12-4).
    /// `None` keeps only the `target: "audit"` log lines.
    pub audit: Option<AuditSink>,
    /// `via` for desktop-side actions: `Desktop` in local mode, else `WebUi`.
    pub audit_via: AuditVia,
}

impl DeviceRouterState {
    async fn audit(&self, event: NewAuditEvent) {
        if let Some(sink) = &self.audit {
            sink.record(event).await;
        }
    }

    /// A desktop-side action by the signed-in user on `device` / `offer`.
    async fn audit_user(
        &self,
        action: &str,
        user_id: &str,
        target_kind: &str,
        target_id: &str,
        diff: serde_json::Value,
    ) {
        self.audit(
            NewAuditEvent::new(action, AuditActorKind::User, self.audit_via)
                .with_user(user_id)
                .with_actor(user_id)
                .with_target(target_kind, target_id)
                .with_diff(diff),
        )
        .await;
    }
}

/// A paired device as shown on the desktop. Never carries the token hash.
#[derive(Debug, Serialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub scopes: Vec<String>,
    pub push_kind: String,
    pub push_target: Option<String>,
    pub last_seen_at: Option<i64>,
    pub created_at: i64,
}

impl From<UserDeviceRow> for DeviceView {
    fn from(row: UserDeviceRow) -> Self {
        Self {
            scopes: serde_json::from_str(&row.scopes).unwrap_or_default(),
            id: row.id,
            name: row.name,
            platform: row.platform,
            push_kind: row.push_kind,
            push_target: row.push_target,
            last_seen_at: row.last_seen_at,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RenameDeviceRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct SetDevicePushRequest {
    pub push_kind: String,
    #[serde(default)]
    pub push_target: Option<String>,
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
    state
        .audit_user(
            "device.pair_offer",
            &user.id,
            "device_pairing",
            &offer.offer_id,
            serde_json::json!({}),
        )
        .await;
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
    state
        .audit_user(
            "device.pair_reject",
            &user.id,
            "device_pairing",
            &offer_id,
            serde_json::json!({}),
        )
        .await;
    Ok(Json(ApiResponse::ok(view)))
}

fn device_not_found() -> ApiError {
    ApiError::NotFound("Device not found".into())
}

async fn list_devices_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<Vec<DeviceView>>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let rows = state.devices.list_for_user(&user.id).await.map_err(db_unavailable)?;
    let views = rows
        .into_iter()
        .filter(|row| row.revoked_at.is_none())
        .map(DeviceView::from)
        .collect();
    Ok(Json(ApiResponse::ok(views)))
}

async fn rename_device_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(device_id): Path<String>,
    body: Result<Json<RenameDeviceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DeviceView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let name = validate_device_name(&req.name)?;
    if !state
        .devices
        .rename(&user.id, &device_id, &name)
        .await
        .map_err(db_unavailable)?
    {
        return Err(device_not_found());
    }
    let row = state
        .devices
        .find_for_user(&user.id, &device_id)
        .await
        .map_err(db_unavailable)?
        .ok_or_else(device_not_found)?;
    tracing::info!(
        target: "audit",
        action = "device.rename",
        user_id = %user.id,
        device_id = %device_id,
        "device renamed"
    );
    state
        .audit_user(
            "device.rename",
            &user.id,
            "device",
            &device_id,
            serde_json::json!({ "name": row.name }),
        )
        .await;
    Ok(Json(ApiResponse::ok(row.into())))
}

async fn revoke_device_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    ensure_user_session(&device, &agent)?;
    let now = aionui_common::now_ms();
    if !state
        .devices
        .revoke(&user.id, &device_id, now, "user")
        .await
        .map_err(db_unavailable)?
    {
        return Err(device_not_found());
    }
    tracing::info!(
        target: "audit",
        action = "device.revoke",
        user_id = %user.id,
        device_id = %device_id,
        revoked_by = "user",
        "device revoked"
    );
    state
        .audit_user(
            "device.revoke",
            &user.id,
            "device",
            &device_id,
            serde_json::json!({ "revoked_by": "user" }),
        )
        .await;
    if let Some(hook) = state.on_revoked.as_ref() {
        match state.devices.find_for_user(&user.id, &device_id).await {
            Ok(Some(row)) => hook(&row),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, device_id = %device_id, "revoked device reload failed"),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn set_device_push_handler(
    State(state): State<DeviceRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Path(device_id): Path<String>,
    body: Result<Json<SetDevicePushRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DeviceView>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let Json(req) = body.map_err(ApiError::from)?;
    let target = match req.push_kind.as_str() {
        "none" => None,
        "im" => {
            let target = req
                .push_target
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| ApiError::BadRequest("push_target is required for push_kind 'im'".into()))?;
            if target.chars().count() > PUSH_TARGET_MAX_CHARS {
                return Err(ApiError::BadRequest("push_target is too long".into()));
            }
            let validator = state
                .push_targets
                .as_ref()
                .ok_or_else(|| ApiError::BadRequest("IM push is not available".into()))?;
            validator
                .validate_im_target(&user.id, target)
                .await
                .map_err(|e| ApiError::BadRequest(format!("invalid push_target: {e}")))?;
            Some(target.to_owned())
        }
        "apns" | "fcm" => {
            return Err(ApiError::BadRequest(format!(
                "push_kind '{}' is not supported",
                req.push_kind
            )));
        }
        _ => return Err(ApiError::BadRequest("push_kind must be 'none' or 'im'".into())),
    };
    if !state
        .devices
        .set_push(&user.id, &device_id, &req.push_kind, target.as_deref())
        .await
        .map_err(db_unavailable)?
    {
        return Err(device_not_found());
    }
    let row = state
        .devices
        .find_for_user(&user.id, &device_id)
        .await
        .map_err(db_unavailable)?
        .ok_or_else(device_not_found)?;
    tracing::info!(
        target: "audit",
        action = "device.push",
        user_id = %user.id,
        device_id = %device_id,
        push_kind = %req.push_kind,
        "device push settings changed"
    );
    state
        .audit_user(
            "device.push",
            &user.id,
            "device",
            &device_id,
            serde_json::json!({ "push_kind": req.push_kind }),
        )
        .await;
    Ok(Json(ApiResponse::ok(row.into())))
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
    state
        .audit(
            NewAuditEvent::new("device.pair_start", AuditActorKind::Device, AuditVia::Mobile)
                .with_target("device_pairing", &started.pairing_id)
                .with_diff(serde_json::json!({ "device_name": name, "platform": platform })),
        )
        .await;
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
    state
        .audit(
            NewAuditEvent::new("device.paired", AuditActorKind::Device, AuditVia::Mobile)
                .with_user(&row.user_id)
                .with_actor(&row.id)
                .with_target("device", &row.id)
                .with_diff(serde_json::json!({ "name": row.name, "platform": row.platform })),
        )
        .await;
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

/// Desktop-side endpoints (pairing + device management). Mount behind
/// `auth_middleware`.
pub fn device_pairing_user_routes(state: DeviceRouterState) -> Router {
    Router::new()
        .route("/api/auth/devices", get(list_devices_handler))
        .route(
            "/api/auth/devices/{device_id}",
            patch(rename_device_handler).delete(revoke_device_handler),
        )
        .route("/api/auth/devices/{device_id}/push", put(set_device_push_handler))
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

    #[test]
    fn device_management_requires_a_user_session() {
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
}

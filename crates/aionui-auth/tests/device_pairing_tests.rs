//! M8 E9-1: desktop↔phone pairing and the device-token channel.
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Extension, Router, middleware};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_auth::device::token::hash_device_token;
use aionui_auth::{
    AuthIdentityMode, AuthState, CookieConfig, CurrentUser, DevicePairingStore, DevicePrincipal, DeviceRouterState,
    JwtService, auth_middleware, csrf_middleware, device_pairing_public_routes, device_pairing_user_routes,
};
use aionui_db::{
    CreateUserDeviceParams, IUserDeviceRepository, IUserRepository, SqliteUserDeviceRepository, SqliteUserRepository,
    init_database_memory,
};

struct Ctx {
    app: Router,
    devices: Arc<dyn IUserDeviceRepository>,
    user_id: String,
    jwt: String,
    _db: aionui_db::Database,
}

async fn setup() -> Ctx {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let devices = Arc::new(SqliteUserDeviceRepository::new(db.pool().clone())) as Arc<dyn IUserDeviceRepository>;
    let user = user_repo.create_user("alice", "x").await.unwrap();
    let jwt_service = Arc::new(JwtService::new("test_secret_for_devices".into()));
    let jwt = jwt_service.sign(&user.id, "alice").unwrap();

    let auth_state = AuthState {
        jwt_service,
        user_repo,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: None,
        local_secret: None,
        agent_approvals: None,
        agent_conversation_access: None,
        devices: Some(devices.clone()),
    };
    let router_state = DeviceRouterState {
        devices: devices.clone(),
        pairings: Arc::new(DevicePairingStore::new()),
    };
    let echo = |user: Extension<CurrentUser>, device: Option<Extension<DevicePrincipal>>| async move {
        axum::Json(json!({
            "user_id": user.id,
            "device_id": device.map(|d| d.device_id.clone()),
        }))
    };
    let protected = Router::new()
        .route("/api/conversations", get(echo))
        .route("/api/conversations/{id}/messages", post(echo))
        .route("/api/agent-approvals/{id}/approve", post(echo))
        .route("/api/providers", post(echo))
        .route("/api/fs/read", get(echo))
        .route("/api/auth/user", get(echo))
        .route_layer(middleware::from_fn_with_state(auth_state.clone(), auth_middleware));
    let user_routes = device_pairing_user_routes(router_state.clone())
        .route_layer(middleware::from_fn_with_state(auth_state, auth_middleware));
    let cookie = Arc::new(CookieConfig {
        secure: false,
        same_site: "Lax",
    });
    let app = Router::new()
        .merge(device_pairing_public_routes(router_state))
        .merge(user_routes)
        .merge(protected)
        .layer(middleware::from_fn_with_state(cookie, csrf_middleware));
    Ctx {
        app,
        devices,
        user_id: user.id,
        jwt,
        _db: db,
    }
}

async fn call(app: &Router, method: &str, uri: &str, bearer: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = match body {
        Some(body) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// JWT-authenticated desktop calls are POSTs; the CSRF layer wants the
/// double-submit pair, which a real WebUI sends.
async fn desktop_post(ctx: &Ctx, uri: &str) -> (StatusCode, Value) {
    let req = Request::post(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", ctx.jwt))
        .header(header::COOKIE, "aionui-csrf-token=t")
        .header("x-csrf-token", "t")
        .body(Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn pair(ctx: &Ctx) -> Value {
    let (status, offer) = desktop_post(ctx, "/api/auth/devices/pair/offers").await;
    assert_eq!(status, StatusCode::OK, "{offer}");
    let offer = &offer["data"];
    let (status, started) = call(
        &ctx.app,
        "POST",
        "/api/auth/devices/pair/start",
        None,
        Some(json!({"qr_token": offer["qr_token"], "device_name": "Pixel", "platform": "android"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let (status, confirmed) = call(
        &ctx.app,
        "POST",
        "/api/auth/devices/pair/confirm",
        None,
        Some(json!({"pairing_id": started["data"]["pairing_id"], "code": offer["code"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    confirmed["data"].clone()
}

#[tokio::test]
async fn pairing_issues_a_hashed_device_token_that_acts_as_the_user() {
    let ctx = setup().await;
    let paired = pair(&ctx).await;
    let token = paired["device_token"].as_str().unwrap();
    assert!(token.starts_with("wmd_"));
    assert_eq!(paired["scopes"], json!(["mobile_default"]));

    // Only the hash is stored.
    let rows = ctx.devices.list_for_user(&ctx.user_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].token_hash, token);
    assert_eq!(rows[0].token_hash, hash_device_token(token));

    let (status, body) = call(&ctx.app, "GET", "/api/conversations", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user_id"], ctx.user_id.as_str());
    assert_eq!(body["device_id"], paired["device_id"]);

    // Follow-ups and approvals work without a CSRF pair.
    let (status, _) = call(&ctx.app, "POST", "/api/conversations/c1/messages", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&ctx.app, "POST", "/api/agent-approvals/a1/approve", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(&ctx.app, "GET", "/api/auth/user", Some(token), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn device_token_is_limited_to_the_mobile_scope() {
    let ctx = setup().await;
    let token = pair(&ctx).await["device_token"].as_str().unwrap().to_owned();
    for (method, uri) in [
        ("POST", "/api/providers"),
        ("GET", "/api/fs/read"),
        ("POST", "/api/auth/devices/pair/offers"),
    ] {
        let (status, body) = call(&ctx.app, method, uri, Some(&token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        assert_eq!(body["code"], "DEVICE_SCOPE_DENIED");
    }
}

#[tokio::test]
async fn qr_is_single_use_and_five_wrong_codes_void_the_pairing() {
    let ctx = setup().await;
    let (_, offer) = desktop_post(&ctx, "/api/auth/devices/pair/offers").await;
    let offer = offer["data"].clone();
    let start = json!({"qr_token": offer["qr_token"], "device_name": "iPhone", "platform": "ios"});
    let (status, started) = call(
        &ctx.app,
        "POST",
        "/api/auth/devices/pair/start",
        None,
        Some(start.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(&ctx.app, "POST", "/api/auth/devices/pair/start", None, Some(start)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "DEVICE_PAIRING_QR_INVALID");

    let correct = offer["code"].as_str().unwrap();
    let wrong = if correct == "000000" { "000001" } else { "000000" };
    let pairing_id = started["data"]["pairing_id"].clone();
    for left in (0..5).rev() {
        let (status, body) = call(
            &ctx.app,
            "POST",
            "/api/auth/devices/pair/confirm",
            None,
            Some(json!({"pairing_id": pairing_id, "code": wrong})),
        )
        .await;
        if left > 0 {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(body["details"]["attempts_left"], left);
        } else {
            assert_eq!(status, StatusCode::GONE, "{body}");
        }
    }
    let (status, body) = call(
        &ctx.app,
        "POST",
        "/api/auth/devices/pair/confirm",
        None,
        Some(json!({"pairing_id": pairing_id, "code": correct})),
    )
    .await;
    assert_eq!(status, StatusCode::GONE, "{body}");
    assert!(ctx.devices.list_for_user(&ctx.user_id).await.unwrap().is_empty());
}

#[tokio::test]
async fn revoked_and_inactive_devices_are_refused_on_the_next_request() {
    let ctx = setup().await;
    let paired = pair(&ctx).await;
    let token = paired["device_token"].as_str().unwrap();
    let device_id = paired["device_id"].as_str().unwrap();
    assert!(
        ctx.devices
            .revoke(&ctx.user_id, device_id, aionui_common::now_ms(), "user")
            .await
            .unwrap()
    );
    let (status, _) = call(&ctx.app, "GET", "/api/conversations", Some(token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A device unused for 90+ days is revoked as `inactivity`.
    let stale = "wmd_".to_owned() + &"ab".repeat(32);
    let created_at = aionui_common::now_ms() - 91 * 24 * 60 * 60 * 1000;
    let row = ctx
        .devices
        .create_within_limit(
            &CreateUserDeviceParams {
                user_id: &ctx.user_id,
                name: "old",
                platform: "ios",
                token_hash: &hash_device_token(&stale),
                scopes: r#"["mobile_default"]"#,
                created_at,
            },
            5,
        )
        .await
        .unwrap()
        .unwrap();
    let (status, body) = call(&ctx.app, "GET", "/api/conversations", Some(&stale), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], "DEVICE_TOKEN_INACTIVE");
    let row = ctx.devices.find_for_user(&ctx.user_id, &row.id).await.unwrap().unwrap();
    assert_eq!(row.revoked_by.as_deref(), Some("inactivity"));
}

#[tokio::test]
async fn desktop_can_reject_and_offers_stop_at_five_devices() {
    let ctx = setup().await;
    let (_, offer) = desktop_post(&ctx, "/api/auth/devices/pair/offers").await;
    let offer_id = offer["data"]["offer_id"].as_str().unwrap().to_owned();
    let (status, body) = desktop_post(&ctx, &format!("/api/auth/devices/pair/offers/{offer_id}/reject")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["status"], "rejected");
    let (status, _) = call(
        &ctx.app,
        "POST",
        "/api/auth/devices/pair/start",
        None,
        Some(json!({"qr_token": offer["data"]["qr_token"], "device_name": "x", "platform": "ios"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    for _ in 0..5 {
        pair(&ctx).await;
    }
    let (status, body) = desktop_post(&ctx, "/api/auth/devices/pair/offers").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "DEVICE_LIMIT_REACHED");
}

#[tokio::test]
async fn device_csrf_exemption_does_not_cover_cookie_bearing_requests() {
    let ctx = setup().await;
    let device = pair(&ctx).await;
    let token = device["device_token"].as_str().unwrap();
    let uri = "/api/agent-approvals/a1/approve";

    // A real device: bearer only, no CSRF pair needed.
    let (status, body) = call(&ctx.app, "POST", uri, Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Same `wmd_` header plus an ambient session or refresh cookie: CSRF applies.
    for cookie in ["aionui-session=x", "aionui-refresh=x"] {
        let req = Request::post(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        let resp = ctx.app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{cookie}");
    }
}

//! M8 E9-1/E9-2: desktop↔phone pairing, the device-token channel, and
//! device management.
use std::sync::{Arc, Mutex};

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
    AuditEventFilter, AuditOrder, AuditSink, AuditVia, CreateUserDeviceParams, IAuditEventRepository,
    IUserDeviceRepository, IUserRepository, SqliteAuditEventRepository, SqliteUserDeviceRepository,
    SqliteUserRepository, init_database_memory,
};

/// Accepts `sess-<user_id>` only, like the app's channel-backed validator
/// accepts only the user's own IM sessions.
struct OnlyOwnSession;

#[async_trait::async_trait]
impl aionui_auth::DevicePushTargetValidator for OnlyOwnSession {
    async fn validate_im_target(&self, user_id: &str, target: &str) -> Result<(), String> {
        (target == format!("sess-{user_id}"))
            .then_some(())
            .ok_or_else(|| "not your channel".to_owned())
    }
}

struct Ctx {
    app: Router,
    devices: Arc<dyn IUserDeviceRepository>,
    user_id: String,
    jwt: String,
    revoked: Arc<Mutex<Vec<String>>>,
    audit: Arc<dyn IAuditEventRepository>,
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
    let revoked = Arc::new(Mutex::new(Vec::new()));
    let audit = Arc::new(SqliteAuditEventRepository::new(db.pool().clone())) as Arc<dyn IAuditEventRepository>;
    let revoked_log = revoked.clone();
    let router_state = DeviceRouterState {
        devices: devices.clone(),
        pairings: Arc::new(DevicePairingStore::new()),
        on_revoked: Some(Arc::new(move |row| revoked_log.lock().unwrap().push(row.id.clone()))),
        push_targets: Some(Arc::new(OnlyOwnSession)),
        audit: Some(AuditSink::new(audit.clone())),
        audit_via: AuditVia::WebUi,
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
        revoked,
        audit,
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
    desktop(ctx, "POST", uri, None).await
}

async fn desktop(ctx: &Ctx, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {}", ctx.jwt))
        .header(header::COOKIE, "aionui-csrf-token=t")
        .header("x-csrf-token", "t")
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
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
async fn desktop_lists_renames_and_revokes_devices() {
    let ctx = setup().await;
    let phone = pair(&ctx).await;
    let tablet = pair(&ctx).await;
    let phone_token = phone["device_token"].as_str().unwrap();
    let tablet_token = tablet["device_token"].as_str().unwrap();
    let phone_id = phone["device_id"].as_str().unwrap();

    let (status, list) = desktop(&ctx, "GET", "/api/auth/devices", None).await;
    assert_eq!(status, StatusCode::OK);
    let list = list["data"].as_array().unwrap().clone();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|d| d.get("token_hash").is_none()));
    assert_eq!(list[0]["scopes"], json!(["mobile_default"]));

    let uri = format!("/api/auth/devices/{phone_id}");
    let (status, renamed) = desktop(&ctx, "PATCH", &uri, Some(json!({"name": " Work phone "}))).await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["data"]["name"], "Work phone");
    let (status, _) = desktop(&ctx, "PATCH", &uri, Some(json!({"name": ""}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A device cannot manage devices, not even itself.
    for (method, path) in [("GET", "/api/auth/devices".to_owned()), ("DELETE", uri.clone())] {
        let (status, body) = call(&ctx.app, method, &path, Some(phone_token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }

    let (status, _) = desktop(&ctx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(*ctx.revoked.lock().unwrap(), vec![phone_id.to_owned()]);
    let (status, _) = desktop(&ctx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = desktop(&ctx, "PATCH", &uri, Some(json!({"name": "x"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The revoked phone is out on its next request; the tablet and the
    // desktop session are unaffected.
    let (status, _) = call(&ctx.app, "GET", "/api/conversations", Some(phone_token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(&ctx.app, "GET", "/api/conversations", Some(tablet_token), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, list) = desktop(&ctx, "GET", "/api/auth/devices", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn devices_of_another_user_cannot_be_touched() {
    let ctx = setup().await;
    let other = ctx
        .devices
        .create_within_limit(
            &CreateUserDeviceParams {
                user_id: "someone-else",
                name: "theirs",
                platform: "ios",
                token_hash: "h",
                scopes: r#"["mobile_default"]"#,
                created_at: aionui_common::now_ms(),
            },
            5,
        )
        .await
        .unwrap()
        .unwrap();
    let uri = format!("/api/auth/devices/{}", other.id);
    let (status, _) = desktop(&ctx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = desktop(&ctx, "PATCH", &uri, Some(json!({"name": "mine"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(ctx.revoked.lock().unwrap().is_empty());
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

#[tokio::test]
async fn desktop_sets_im_push_target_only_to_its_own_channel() {
    let ctx = setup().await;
    let phone = pair(&ctx).await;
    let phone_token = phone["device_token"].as_str().unwrap();
    let uri = format!("/api/auth/devices/{}/push", phone["device_id"].as_str().unwrap());
    let own = format!("sess-{}", ctx.user_id);

    let (status, body) = desktop(&ctx, "PUT", &uri, Some(json!({"push_kind": "im", "push_target": own}))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["push_kind"], "im");
    assert_eq!(body["data"]["push_target"], json!(own));

    for bad in [
        json!({"push_kind": "im", "push_target": "sess-someone-else"}),
        json!({"push_kind": "im"}),
        json!({"push_kind": "im", "push_target": "  "}),
        json!({"push_kind": "apns", "push_target": "tok"}),
        json!({"push_kind": "carrier-pigeon"}),
    ] {
        let (status, body) = desktop(&ctx, "PUT", &uri, Some(bad.clone())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }

    // The device cannot point its own notifications somewhere.
    let (status, _) = call(
        &ctx.app,
        "PUT",
        &uri,
        Some(phone_token),
        Some(json!({"push_kind": "none"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, body) = desktop(
        &ctx,
        "PUT",
        &uri,
        Some(json!({"push_kind": "none", "push_target": own})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["push_kind"], "none");
    assert!(body["data"]["push_target"].is_null());

    let (status, _) = desktop(
        &ctx,
        "PUT",
        "/api/auth/devices/nope/push",
        Some(json!({"push_kind": "none"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// M8 E12-4: pairing and device management land in the audit ledger.
#[tokio::test]
async fn device_lifecycle_is_recorded_in_the_audit_ledger() {
    let ctx = setup().await;
    let phone = pair(&ctx).await;
    let phone_id = phone["device_id"].as_str().unwrap();
    let uri = format!("/api/auth/devices/{phone_id}");
    let (status, _) = desktop(&ctx, "PATCH", &uri, Some(json!({"name": "Work phone"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = desktop(&ctx, "DELETE", &uri, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let rows = ctx
        .audit
        .list(&AuditEventFilter {
            order: AuditOrder::OldestFirst,
            ..Default::default()
        })
        .await
        .unwrap();
    let actions: Vec<&str> = rows.iter().map(|r| r.action.as_str()).collect();
    assert_eq!(
        actions,
        [
            "device.pair_offer",
            "device.pair_start",
            "device.paired",
            "device.rename",
            "device.revoke"
        ]
    );
    let paired = &rows[2];
    assert_eq!(paired.actor_kind, "device");
    assert_eq!(paired.via, "mobile");
    assert_eq!(paired.user_id.as_deref(), Some(ctx.user_id.as_str()));
    assert_eq!(paired.actor_id.as_deref(), Some(phone_id));
    let revoke = &rows[4];
    assert_eq!(revoke.actor_kind, "user");
    assert_eq!(revoke.via, "webui");
    assert_eq!(revoke.target_id.as_deref(), Some(phone_id));
    // The pairing token never reaches the ledger.
    let token = phone["device_token"].as_str().unwrap();
    assert!(rows.iter().all(|r| !r.diff.as_deref().unwrap_or("").contains(token)));
}

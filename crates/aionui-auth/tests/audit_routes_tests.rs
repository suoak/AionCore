//! M8 E12-2: `/api/audit/*` behind the real auth middleware.
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::middleware;
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_auth::device::token::{generate_device_token, hash_device_token};
use aionui_auth::{AuditRouterState, AuthIdentityMode, AuthState, JwtService, audit_routes, auth_middleware};
use aionui_common::now_ms;
use aionui_db::{
    AuditActorKind, AuditVia, CreateUserDeviceParams, IAuditEventRepository, IUserDeviceRepository, IUserRepository,
    NewAuditEvent, SqliteAuditEventRepository, SqliteUserDeviceRepository, SqliteUserRepository, init_database_memory,
};

struct Ctx {
    app: Router,
    alice_jwt: String,
    bob_jwt: String,
    device_token: String,
    _db: aionui_db::Database,
}

async fn setup() -> Ctx {
    let db = init_database_memory().await.unwrap();
    let user_repo = Arc::new(SqliteUserRepository::new(db.pool().clone())) as Arc<dyn IUserRepository>;
    let devices = Arc::new(SqliteUserDeviceRepository::new(db.pool().clone())) as Arc<dyn IUserDeviceRepository>;
    let audit = Arc::new(SqliteAuditEventRepository::new(db.pool().clone())) as Arc<dyn IAuditEventRepository>;
    let alice = user_repo.create_user("alice", "x").await.unwrap();
    let bob = user_repo.create_user("bob", "x").await.unwrap();
    let jwt_service = Arc::new(JwtService::new("test_secret_for_audit".into()));
    let alice_jwt = jwt_service.sign(&alice.id, "alice").unwrap();
    let bob_jwt = jwt_service.sign(&bob.id, "bob").unwrap();

    let device_token = generate_device_token();
    devices
        .create_within_limit(
            &CreateUserDeviceParams {
                user_id: &alice.id,
                name: "Phone",
                platform: "ios",
                token_hash: &hash_device_token(&device_token),
                scopes: r#"["mobile_default"]"#,
                created_at: now_ms(),
            },
            5,
        )
        .await
        .unwrap()
        .unwrap();

    audit
        .append(
            &NewAuditEvent::new("config.provider.update", AuditActorKind::User, AuditVia::Desktop)
                .with_user(&alice.id)
                .with_actor(&alice.id)
                .with_target("provider", "p1")
                .with_summary("=HYPERLINK(\"x\")")
                .with_diff(json!({ "api_key": "sk-secret-value-123456789" })),
        )
        .await
        .unwrap();
    audit
        .append(
            &NewAuditEvent::new("approval.decide", AuditActorKind::User, AuditVia::Desktop)
                .with_user(&alice.id)
                .with_actor(&alice.id),
        )
        .await
        .unwrap();
    audit
        .append(
            &NewAuditEvent::new("config.settings.update", AuditActorKind::User, AuditVia::WebUi)
                .with_user(&bob.id)
                .with_actor(&bob.id),
        )
        .await
        .unwrap();

    let auth_state = AuthState {
        jwt_service,
        user_repo,
        identity_mode: AuthIdentityMode::UserSession,
        runtime_token_verifier: None,
        local_secret: None,
        agent_approvals: None,
        agent_conversation_access: None,
        devices: Some(devices),
    };
    let app = audit_routes(AuditRouterState {
        repo: audit,
        governance: None,
    })
    .route_layer(middleware::from_fn_with_state(auth_state, auth_middleware));
    Ctx {
        app,
        alice_jwt,
        bob_jwt,
        device_token,
        _db: db,
    }
}

async fn get(app: &Router, uri: &str, bearer: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let req = Request::builder()
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers, bytes.to_vec())
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

#[tokio::test]
async fn user_sees_only_own_events_newest_first() {
    let ctx = setup().await;
    let (status, _, body) = get(&ctx.app, "/api/audit/events", &ctx.alice_jwt).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    let items = body["data"]["items"].as_array().unwrap();
    let actions: Vec<_> = items.iter().map(|i| i["action"].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["approval.decide", "config.provider.update"]);
    // diff is parsed JSON and the secret is a fingerprint.
    let key = items[1]["diff"]["api_key"].as_str().unwrap();
    assert!(key.starts_with("***sha256:"), "{key}");
    assert!(body["data"]["next_before_seq"].is_null());

    let (_, _, body) = get(&ctx.app, "/api/audit/events", &ctx.bob_jwt).await;
    let items = json_body(&body)["data"]["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["action"], "config.settings.update");
}

#[tokio::test]
async fn filters_and_paging() {
    let ctx = setup().await;
    let (_, _, body) = get(&ctx.app, "/api/audit/events?action_prefix=config.", &ctx.alice_jwt).await;
    assert_eq!(json_body(&body)["data"]["items"].as_array().unwrap().len(), 1);

    let (_, _, body) = get(&ctx.app, "/api/audit/events?limit=1", &ctx.alice_jwt).await;
    let body = json_body(&body);
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 1);
    let next = body["data"]["next_before_seq"].as_i64().unwrap();
    let (_, _, body) = get(
        &ctx.app,
        &format!("/api/audit/events?limit=1&before_seq={next}"),
        &ctx.alice_jwt,
    )
    .await;
    assert_eq!(json_body(&body)["data"]["items"][0]["action"], "config.provider.update");

    let (status, _, _) = get(&ctx.app, "/api/audit/events?actor_kind=robot", &ctx.alice_jwt).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn verify_and_csv_export_carry_chain_status() {
    let ctx = setup().await;
    let (status, _, body) = get(&ctx.app, "/api/audit/verify", &ctx.alice_jwt).await;
    assert_eq!(status, StatusCode::OK);
    let body = json_body(&body);
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["checked"], 3);

    let (status, headers, body) = get(&ctx.app, "/api/audit/export.csv", &ctx.alice_jwt).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/csv"));
    assert_eq!(headers["x-audit-chain-ok"], "true");
    assert_eq!(headers["x-audit-chain-checked"], "3");
    assert_eq!(headers["x-audit-exported-rows"], "2");
    let csv = String::from_utf8(body).unwrap();
    let lines: Vec<_> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 3, "{csv}");
    assert!(lines[0].starts_with("seq,id,ts,"));
    // Chain order, user-scoped, formula neutralised, secret not present.
    assert!(lines[1].contains("config.provider.update"));
    assert!(lines[1].contains("'=HYPERLINK"));
    assert!(!csv.contains("sk-secret-value"));
    assert!(lines[2].contains("approval.decide"));
}

#[tokio::test]
async fn device_tokens_are_refused() {
    let ctx = setup().await;
    for uri in ["/api/audit/events", "/api/audit/verify", "/api/audit/export.csv"] {
        let (status, _, _) = get(&ctx.app, uri, &ctx.device_token).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
    }
}

#[tokio::test]
async fn anonymous_requests_are_refused() {
    let ctx = setup().await;
    let req = Request::builder().uri("/api/audit/events").body(Body::empty()).unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

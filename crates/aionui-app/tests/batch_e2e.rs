//! M8 E8: `/api/batch*` over the real router.
//!
//! - agent (conversation-helper runtime) tokens get 403 on every batch route;
//! - user path: wiring, validation (unknown assistant, bad target), owner
//!   scope and unknown ids.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;
use common::{body_json, get_with_token, json_with_token, setup_and_login};

const CONVERSATION_ID: &str = "conv-batch-agent";

async fn aionpro_app_with_user() -> (axum::Router, aionui_app::AppServices, String) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/batch-pro-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Batch Pro User"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let user_id = body_json(resp).await["data"]["user_id"].as_str().unwrap().to_owned();
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at)
         VALUES (?, ?, 'Agent Conversation', 'acp', '{}', 'pending', 1, 1)",
    )
    .bind(CONVERSATION_ID)
    .bind(&user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    (router, services, user_id)
}

fn create_body(target: Value, assistant: &str) -> Value {
    json!({
        "title": "Upgrade deps",
        "template": {"prompt": "Upgrade {{target.name}}"},
        "targets": [target],
        "checks": [{"kind": "file_exists", "path": "result.json"}],
        "assistant_id": assistant
    })
}

#[tokio::test]
async fn agent_tokens_are_denied_on_every_batch_route() {
    let (app, services, user_id) = aionpro_app_with_user().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        aionui_ai_agent::helper_scopes::conversation_helper_scopes(None),
    );
    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        ("GET", "/api/batch", None),
        (
            "POST",
            "/api/batch",
            Some(create_body(json!({"workspace": "/tmp", "name": "a"}), "a")),
        ),
        ("GET", "/api/batch/bj_x", None),
        ("GET", "/api/batch/bj_x/items", None),
        ("POST", "/api/batch/bj_x/trial", Some(json!({}))),
        ("POST", "/api/batch/bj_x/scale", Some(json!({}))),
        ("POST", "/api/batch/bj_x/retry", Some(json!({}))),
        ("POST", "/api/batch/bj_x/cancel", Some(json!({}))),
        ("GET", "/api/batch/bj_x/export.csv", None),
    ];
    for (method, uri, body) in cases {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-aionui-user-id", &user_id)
            .header("x-aionui-conversation-id", CONVERSATION_ID)
            .header("x-aionui-runtime-token", &issue.token);
        let body = match body {
            Some(value) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&value).unwrap())
            }
            None => Body::empty(),
        };
        let resp = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(body_json(resp).await["code"], "AGENT_SCOPE_DENIED", "{method} {uri}");
    }
    for table in ["batch_jobs", "batch_items"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(services.database.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    services.database.close().await;
}

#[tokio::test]
async fn user_path_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let data = tmp.path().join("data");
    let config = aionui_app::AppConfig {
        data_dir: data.clone(),
        work_dir: data.clone(),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let mut app = aionui_app::create_router(&services).await.expect("router");
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "password123").await;
    let post = |uri: &str, body: Value| json_with_token("POST", uri, body, &token, &csrf);

    for body in [
        // Unknown assistant.
        create_body(json!({"workspace": ws, "name": "a"}), "missing-assistant"),
        // Target directory does not exist.
        create_body(
            json!({"workspace": tmp.path().join("nope"), "name": "a"}),
            "missing-assistant",
        ),
        // No targets.
        json!({"title": "x", "template": {"prompt": "p"}, "targets": [], "checks": [], "assistant_id": "a"}),
    ] {
        let resp = app.clone().oneshot(post("/api/batch", body.clone())).await.unwrap();
        let status = resp.status();
        let json = body_json(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json}");
    }

    let resp = app.clone().oneshot(get_with_token("/api/batch", &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"], json!([]));
    for uri in [
        "/api/batch/bj_missing",
        "/api/batch/bj_missing/items",
        "/api/batch/bj_missing/export.csv",
    ] {
        let resp = app.clone().oneshot(get_with_token(uri, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body_json(resp).await["code"], "BATCH_NOT_FOUND", "{uri}");
    }
    for action in ["trial", "scale", "retry", "cancel"] {
        let uri = format!("/api/batch/bj_missing/{action}");
        let resp = app.clone().oneshot(post(&uri, json!({}))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM batch_jobs")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    services.database.close().await;
}

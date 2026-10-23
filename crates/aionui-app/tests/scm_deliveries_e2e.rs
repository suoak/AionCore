//! M7 E4-5: `/api/scm-deliveries*` over the real router.
//!
//! - a user session reaches the routes (sandbox, validation and remote errors
//!   map to stable codes; no CLI is needed for these paths);
//! - agent (conversation-helper runtime) tokens get 403 on every delivery
//!   route, so an agent cannot open a PR through WorkMate.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use common::{body_json, get_with_token, json_with_token, setup_and_login};

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn user_session_reaches_delivery_routes() {
    let (mut app, services) = common::build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "password123").await;

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(status.success());
    let repo_str = std::fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();

    // No remote configured.
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/scm-deliveries/host?repo_path={}", url_encode(&repo_str)),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "PR_NO_REMOTE");

    // Outside the sandbox.
    let resp = app
        .clone()
        .oneshot(get_with_token("/api/scm-deliveries/host?repo_path=%2Fproc", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Validation happens before any CLI runs.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/scm-deliveries/pull-request",
            json!({"repo_path": repo_str, "branch": "-x", "base": "main", "title": "T"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["code"], "SCM_DELIVERY_BAD_REQUEST");

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/scm-deliveries", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"], json!([]));

    services.database.close().await;
}

const CONVERSATION_ID: &str = "conv-delivery-agent";

#[tokio::test]
async fn agent_tokens_are_denied_on_every_delivery_route() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let app = aionui_app::create_router(&services).await.expect("build router");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/delivery-pro-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Delivery Pro User"}"#))
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
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );
    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        ("GET", "/api/scm-deliveries", None),
        ("GET", "/api/scm-deliveries/host?repo_path=%2Ftmp", None),
        (
            "POST",
            "/api/scm-deliveries/pull-request",
            Some(json!({"repo_path": "/tmp", "branch": "feat/x", "base": "main", "title": "T"})),
        ),
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
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scm_deliveries")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    services.database.close().await;
}

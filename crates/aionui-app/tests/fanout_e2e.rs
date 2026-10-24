//! M7 E3-4: `/api/fanout-groups*` over the real router.
//!
//! - agent (conversation-helper runtime) tokens get 403 on every fan-out route;
//! - user-path validation: slot/parallel limits, unknown assistant, unknown group.

mod common;

use std::path::Path;
use std::process::Command;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;
use common::{body_json, get_with_token, json_with_token, setup_and_login};

const CONVERSATION_ID: &str = "conv-fanout-agent";

fn init_repo(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    let run = |args: &[&str]| {
        Command::new("git")
            .args([
                "-c",
                "user.email=dev@example.com",
                "-c",
                "user.name=Dev",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    run(&["init", "-q", "-b", "main"]) && run(&["add", "."]) && run(&["commit", "-q", "-m", "init"])
}

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
                .uri("/api/auth/internal/external-users/fanout-pro-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Fanout Pro User"}"#))
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

#[tokio::test]
async fn agent_tokens_are_denied_on_every_fanout_route() {
    let (app, services, user_id) = aionpro_app_with_user().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        aionui_ai_agent::helper_scopes::conversation_helper_scopes(None),
    );
    let create_body = json!({
        "repo_path": "/tmp", "prompt": "x",
        "slots": [{"assistant_id": "a"}, {"assistant_id": "b"}]
    });
    let cases: Vec<(&str, &str, Option<Value>)> = vec![
        ("GET", "/api/fanout-groups", None),
        ("GET", "/api/fanout-groups?task_session_id=ts_1", None),
        ("POST", "/api/fanout-groups", Some(create_body)),
        ("GET", "/api/fanout-groups/fg_x", None),
        ("GET", "/api/fanout-groups/fg_x/compare", None),
        (
            "POST",
            "/api/fanout-groups/fg_x/adopt",
            Some(json!({"slot": "A", "strategy": "squash"})),
        ),
        ("POST", "/api/fanout-groups/fg_x/discard", Some(json!({}))),
        ("POST", "/api/fanout-groups/fg_x/cancel", Some(json!({}))),
        ("DELETE", "/api/fanout-groups/fg_x", None),
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

    // Nothing was written through the denied create.
    for table in ["fanout_groups", "task_worktrees"] {
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
    let repo = tmp.path().join("repo");
    if !init_repo(&repo) {
        eprintln!("git not available; skipping");
        return;
    }
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

    let post = |body: Value| json_with_token("POST", "/api/fanout-groups", body, &token, &csrf);
    let slots = |n: usize| {
        (0..n)
            .map(|_| json!({"assistant_id": "missing-assistant"}))
            .collect::<Vec<_>>()
    };

    for (body, want_status, want_code) in [
        (
            json!({"repo_path": repo, "prompt": "x", "slots": slots(1)}),
            StatusCode::BAD_REQUEST,
            "FANOUT_LIMIT",
        ),
        (
            json!({"repo_path": repo, "prompt": "x", "slots": slots(6)}),
            StatusCode::BAD_REQUEST,
            "FANOUT_LIMIT",
        ),
        (
            json!({"repo_path": repo, "prompt": "x", "slots": slots(2), "max_parallel": 6}),
            StatusCode::BAD_REQUEST,
            "FANOUT_LIMIT",
        ),
        (
            json!({"repo_path": repo, "prompt": "x", "slots": slots(2)}),
            StatusCode::BAD_REQUEST,
            "",
        ),
    ] {
        let resp = app.clone().oneshot(post(body.clone())).await.unwrap();
        let status = resp.status();
        let json = body_json(resp).await;
        assert_eq!(status, want_status, "{body}: {json}");
        if !want_code.is_empty() {
            assert_eq!(json["code"], want_code, "{body}");
        }
    }
    // Inline backend / MCP overrides are rejected at deserialisation.
    let resp = app
        .clone()
        .oneshot(post(json!({
            "repo_path": repo, "prompt": "x",
            "slots": [{"assistant_id": "a", "backend": "claude"}, {"assistant_id": "b"}]
        })))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());

    let resp = app
        .clone()
        .oneshot(get_with_token("/api/fanout-groups", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"], json!([]));
    for uri in ["/api/fanout-groups/fg_missing", "/api/fanout-groups/fg_missing/compare"] {
        let resp = app.clone().oneshot(get_with_token(uri, &token)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/fanout-groups/fg_missing/cancel",
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_worktrees")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
    services.database.close().await;
}

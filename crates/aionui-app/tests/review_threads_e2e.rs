//! M7 E4-1: `/api/review-threads*` over the real router.
//!
//! - CRUD + resolve/reopen + comments + lazy re-anchoring for a user session;
//! - agent (conversation-helper runtime) tokens get 403 on every review route,
//!   including for the agent's own conversation.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_ai_agent::{RuntimeTokenScope, TEAM_RUNTIME_TOKEN_SESSION_GENERATION};
use common::{body_json, delete_with_token, get_with_token, json_with_token, setup_and_login};

fn write_lines(path: &std::path::Path, lines: &[String]) {
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn review_thread_crud_and_reanchor_over_http() {
    let (mut app, services) = common::build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "password123").await;

    // A repository under the default sandbox roots (temp dir).
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let file = repo.join("src/main.rs");
    let mut lines: Vec<String> = (1..=12).map(|i| format!("fn f{i}() {{}}")).collect();
    write_lines(&file, &lines);
    let repo_str = repo.to_string_lossy().into_owned();

    // Create.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/review-threads",
            json!({
                "scope_kind": "scm",
                "scope_id": repo_str,
                "file_path": "src/main.rs",
                "side": "new",
                "start_line": 5,
                "end_line": 6,
                "body": "rename these",
                "diff_context": {"hunk_header": "@@ -1,12 +1,12 @@", "lines": ["fn f5() {}", "fn f6() {}"]}
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let created = body_json(resp).await;
    let thread = &created["data"];
    let id = thread["id"].as_str().unwrap().to_owned();
    assert_eq!(thread["status"], "open");
    assert_eq!(thread["start_line"], 5);
    assert_eq!(thread["comments"][0]["body"], "rename these");
    assert_eq!(thread["comments"][0]["author"], "user");
    let scope_id = thread["scope_id"].as_str().unwrap().to_owned();

    // Agent revision: three lines inserted above → list re-anchors.
    for i in 0..3 {
        lines.insert(0, format!("// added {i}"));
    }
    write_lines(&file, &lines);
    let list_uri = format!("/api/review-threads?scope_kind=scm&scope_id={}", url_encode(&repo_str));
    let resp = app.clone().oneshot(get_with_token(&list_uri, &token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let listed = body_json(resp).await;
    let items = listed["data"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], id);
    assert_eq!(items[0]["scope_id"], scope_id);
    assert_eq!(
        (items[0]["start_line"].as_u64(), items[0]["end_line"].as_u64()),
        (Some(8), Some(9))
    );

    // Get one.
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/review-threads/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["start_line"], 8);

    // Patch body.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "PATCH",
            &format!("/api/review-threads/{id}"),
            json!({"body": "rename these two"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["comments"][0]["body"], "rename these two");

    // Reply.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/review-threads/{id}/comments"),
            json!({"body": "and add docs"}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["comments"].as_array().unwrap().len(), 2);

    // Resolve, filter, reopen.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/review-threads/{id}/resolve"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["data"]["status"], "resolved");
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("{list_uri}&status=open"), &token))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"].as_array().unwrap().len(), 0);
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            &format!("/api/review-threads/{id}/reopen"),
            json!({}),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["data"]["status"], "open");

    // Whole block deleted → outdated.
    lines.retain(|line| line != "fn f5() {}" && line != "fn f6() {}");
    write_lines(&file, &lines);
    let resp = app.clone().oneshot(get_with_token(&list_uri, &token)).await.unwrap();
    assert_eq!(body_json(resp).await["data"][0]["status"], "outdated");

    // Validation errors are coded 400s; stale diffs are 409s.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/review-threads",
            json!({
                "scope_kind": "scm", "scope_id": repo_str, "file_path": "../escape.rs", "side": "new",
                "start_line": 1, "end_line": 1, "body": "x"
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["code"], "REVIEW_BAD_REQUEST");
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/review-threads",
            json!({
                "scope_kind": "scm", "scope_id": repo_str, "file_path": "src/main.rs", "side": "new",
                "start_line": 1, "end_line": 1, "body": "x",
                "diff_context": {"lines": ["no such line"]}
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(body_json(resp).await["code"], "ANCHOR_STALE");

    // Delete, then 404.
    let resp = app
        .clone()
        .oneshot(delete_with_token(&format!("/api/review-threads/{id}"), &token, &csrf))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/review-threads/{id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_json(resp).await["code"], "REVIEW_THREAD_NOT_FOUND");

    services.database.close().await;
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

const CONVERSATION_ID: &str = "conv-review-agent";

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
                .uri("/api/auth/internal/external-users/review-pro-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Review Pro User"}"#))
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
async fn agent_tokens_are_denied_on_every_review_route() {
    let (app, services, user_id) = aionpro_app_with_user().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        [RuntimeTokenScope::ConversationHelper],
    );
    let create_body = json!({
        "scope_kind": "task_run", "scope_id": "run-1", "conversation_id": CONVERSATION_ID,
        "file_path": "a.rs", "side": "new", "start_line": 1, "end_line": 1, "body": "x",
        "diff_context": {"lines": ["y"]}
    });
    let cases: Vec<(&str, String, Option<Value>)> = vec![
        (
            "GET",
            "/api/review-threads?scope_kind=task_run&scope_id=run-1".into(),
            None,
        ),
        ("POST", "/api/review-threads".into(), Some(create_body)),
        ("GET", "/api/review-threads/rt_x".into(), None),
        ("PATCH", "/api/review-threads/rt_x".into(), Some(json!({"body": "y"}))),
        ("DELETE", "/api/review-threads/rt_x".into(), None),
        ("POST", "/api/review-threads/rt_x/resolve".into(), Some(json!({}))),
        ("POST", "/api/review-threads/rt_x/reopen".into(), Some(json!({}))),
        (
            "POST",
            "/api/review-threads/rt_x/comments".into(),
            Some(json!({"body": "agent says"})),
        ),
    ];
    for (method, uri, body) in cases {
        let mut builder = Request::builder()
            .method(method)
            .uri(&uri)
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
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM review_threads")
        .fetch_one(services.database.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);

    services.database.close().await;
}

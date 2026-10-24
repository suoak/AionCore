//! M7 E1 tail ②: agent (conversation-helper runtime) tokens only see teams
//! their own conversation belongs to; unrelated teams are filtered from the
//! list and direct reads answer 403.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use aionui_ai_agent::TEAM_RUNTIME_TOKEN_SESSION_GENERATION;
use common::body_json;

const CONVERSATION_ID: &str = "conv-agent-outsider";

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
                .uri("/api/auth/internal/external-users/team-scope-user")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Team Scope User"}"#))
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
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, created_at, updated_at)
         VALUES ('team_unrelated', ?, 'Unrelated Team', '', 1, 1)",
    )
    .bind(&user_id)
    .execute(services.database.pool())
    .await
    .unwrap();
    let member = format!(
        r#"[{{"slot_id":"slot-1","name":"Member","role":"teammate","conversation_id":"{CONVERSATION_ID}","backend":"claude"}}]"#
    );
    sqlx::query(
        "INSERT INTO teams (id, user_id, name, workspace, agents, created_at, updated_at)
         VALUES ('team_mine', ?, 'My Team', '', ?, 2, 2)",
    )
    .bind(&user_id)
    .bind(member)
    .execute(services.database.pool())
    .await
    .unwrap();
    (router, services, user_id)
}

#[tokio::test]
async fn agent_token_cannot_see_teams_it_is_not_a_member_of() {
    let (app, services, user_id) = aionpro_app_with_user().await;
    let issue = services.runtime_token_service.issue(
        user_id.as_str(),
        CONVERSATION_ID,
        TEAM_RUNTIME_TOKEN_SESSION_GENERATION,
        aionui_ai_agent::helper_scopes::conversation_helper_scopes(None),
    );
    let get = |uri: &str| {
        Request::builder()
            .method("GET")
            .uri(uri)
            .header("x-aionui-user-id", &user_id)
            .header("x-aionui-conversation-id", CONVERSATION_ID)
            .header("x-aionui-runtime-token", &issue.token)
            .body(Body::empty())
            .unwrap()
    };

    // The list is filtered to teams containing the token's conversation.
    let resp = app.clone().oneshot(get("/api/teams")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("team list")
        .iter()
        .filter_map(|team| team["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["team_mine"], "{body}");

    // The team the agent belongs to stays readable.
    let resp = app.clone().oneshot(get("/api/teams/team_mine")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Direct reads of an unrelated (or unknown) team are denied, not 404.
    for uri in [
        "/api/teams/team_unrelated",
        "/api/teams/team_unrelated/tasks",
        "/api/teams/team_unrelated/mailbox",
        "/api/teams/team_unrelated/activity",
        "/api/teams/team_unrelated/run-state",
        "/api/teams/team_missing",
    ] {
        let resp = app.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{uri}");
        assert_eq!(body_json(resp).await["code"], "AGENT_SCOPE_DENIED", "{uri}");
    }
}

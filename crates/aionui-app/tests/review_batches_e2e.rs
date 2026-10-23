//! M7 E4-3: `/api/review-batches` over the real router.
//!
//! - unresolved threads of a scope go out as one `[[AION_REVIEW]]` input to the
//!   chosen conversation (followup when idle) and become `sent`;
//! - sending to another conversation is a user-originated input: no `@@`
//!   session grant appears in either direction;
//! - the `review-notes` auto-inject skill ships with the documented contract.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app_with_mock_agents, get_with_token, json_with_token, setup_and_login};

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

async fn create_conversation(app: &axum::Router, token: &str, csrf: &str, name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "name": name, "extra": {} }),
            token,
            csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    body_json(resp).await["data"]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn unresolved_threads_are_sent_as_one_review_block_without_session_grants() {
    let (app, services) = build_app_with_mock_agents().await;
    let mut app = app;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let source = create_conversation(&app, &token, &csrf, "Source").await;
    let target = create_conversation(&app, &token, &csrf, "Target").await;
    let user_id: String = sqlx::query_scalar("SELECT user_id FROM conversations WHERE id = ?")
        .bind(&source)
        .fetch_one(services.database.pool())
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let lines: Vec<String> = (1..=10).map(|i| format!("let v{i} = {i};")).collect();
    std::fs::write(repo.join("src/lib.rs"), format!("{}\n", lines.join("\n"))).unwrap();
    let repo_str = repo.to_string_lossy().into_owned();

    for (line, body) in [(2, "first"), (7, "second")] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "POST",
                "/api/review-threads",
                json!({
                    "scope_kind": "scm",
                    "scope_id": repo_str,
                    "conversation_id": source,
                    "file_path": "src/lib.rs",
                    "side": "new",
                    "start_line": line,
                    "end_line": line,
                    "body": body,
                    "diff_context": {"lines": [format!("let v{line} = {line};")]}
                }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/review-batches",
            json!({ "target_conversation_id": target, "scope_kind": "scm", "scope_id": repo_str }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sent = body_json(resp).await;
    let batch = &sent["data"]["batch"];
    let batch_id = batch["id"].as_str().unwrap().to_owned();
    assert_eq!(batch["status"], "accepted");
    assert_eq!(batch["delivery"], "followup");
    assert_eq!(batch["target_conversation_id"], target);
    let content = batch["content"].as_str().unwrap();
    assert!(content.starts_with(&format!("[[AION_REVIEW batch={batch_id} count=2]]")));
    assert!(content.contains("#1 src/lib.rs  L2（新版本）\n```rust\nlet v2 = 2;\n```\n批注：first\n"));
    assert!(content.contains("#2 src/lib.rs  L7（新版本）"));
    let input = &sent["data"]["input_receipt"]["input"];
    assert_eq!(input["mode"], "followup");
    assert_eq!(input["content"], content);
    assert_eq!(input["client_key"], batch_id);
    assert_eq!(batch["input_id"], input["input_id"]);

    // No `@@` grant either way: this is the user's own send.
    let grants = services.conversation_service.session_mention_grants();
    assert!(!grants.is_granted(&user_id, &source, &target));
    assert!(!grants.is_granted(&user_id, &target, &source));

    let list_uri = format!("/api/review-threads?scope_kind=scm&scope_id={}", url_encode(&repo_str));
    let resp = app.clone().oneshot(get_with_token(&list_uri, &token)).await.unwrap();
    let threads = body_json(resp).await;
    for thread in threads["data"].as_array().unwrap() {
        assert_eq!(thread["status"], "sent");
        assert_eq!(thread["last_batch_id"], batch_id);
    }

    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/review-batches?target_conversation_id={target}"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let history = body_json(resp).await;
    assert_eq!(history["data"].as_array().unwrap().len(), 1);
    assert_eq!(history["data"][0]["id"], batch_id);

    // Another user's (or a missing) conversation is not a valid target.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/review-batches",
            json!({ "target_conversation_id": "conv-missing", "scope_kind": "scm", "scope_id": repo_str }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn review_notes_skill_documents_the_block_and_report_format() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/builtin-skills/auto-inject/review-notes/SKILL.md");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.starts_with("---\nname: review-notes\n"), "{body}");
    assert!(body.contains("description:"));
    for needle in [
        "[[AION_REVIEW batch=",
        "[[/AION_REVIEW]]",
        "#1 ✅",
        "#2 ⚠️",
        "CSBU WorkMate",
    ] {
        assert!(body.contains(needle), "review-notes skill must mention {needle}");
    }
}

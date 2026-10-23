//! M7 E3-2 end to end: real git + the real router.
//!
//! create worktree → conversation bound to it → workspace locked → write a
//! file → diff-stat / diff → conversation delete leaves it `unbound` with the
//! directory intact → remove.

mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{body_json, delete_with_token, get_with_token, json_with_token, setup_and_login};

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn wait_worktree(app: &axum::Router, token: &str, id: &str, want: &str) -> Value {
    for _ in 0..400 {
        let resp = app
            .clone()
            .oneshot(get_with_token(&format!("/api/worktrees/{id}"), token))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        if body["data"]["status"] == want {
            return body["data"].clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("worktree {id} never reached {want}");
}

#[tokio::test]
async fn conversation_in_a_managed_worktree_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    if git(&repo, &["init", "-q", "-b", "main"]).is_none() {
        eprintln!("git not available; skipping");
        return;
    }
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]).unwrap();
    git(&repo, &["commit", "-q", "-m", "init"]).unwrap();

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

    // 1. Managed worktree.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/worktrees",
            json!({ "repo_path": repo, "title": "e2e" }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let wt_id = body_json(resp).await["data"]["id"].as_str().unwrap().to_owned();
    let wt = wait_worktree(&app, &token, &wt_id, "ready").await;
    let wt_path = wt["path"].as_str().unwrap().to_owned();

    // 2. Conversation bound to it; a forged extra.worktree_id is ignored.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({
                "type": "acp",
                "worktree_id": wt_id,
                "extra": { "backend": "claude", "fanout_slot": "forged" }
            }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let conv = body_json(resp).await["data"].clone();
    let conv_id = conv["id"].as_str().unwrap().to_owned();
    assert_eq!(conv["extra"]["workspace"], wt_path.as_str());
    assert_eq!(conv["extra"]["worktree_id"], wt_id.as_str());
    assert!(conv["extra"].get("fanout_slot").is_none());

    // A second conversation cannot take the same worktree.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "worktree_id": wt_id, "extra": { "backend": "claude" } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // Forging the binding through extra alone binds nothing.
    let resp = app
        .clone()
        .oneshot(json_with_token(
            "POST",
            "/api/conversations",
            json!({ "type": "acp", "extra": { "backend": "claude", "worktree_id": wt_id } }),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let forged = body_json(resp).await["data"].clone();
    assert!(forged["extra"].get("worktree_id").is_none());
    assert_ne!(forged["extra"]["workspace"], wt_path.as_str());

    // 3. Workspace is locked: PATCH → 400.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    for extra in [json!({ "workspace": elsewhere }), json!({ "worktree_id": "wt_other" })] {
        let resp = app
            .clone()
            .oneshot(json_with_token(
                "PATCH",
                &format!("/api/conversations/{conv_id}"),
                json!({ "extra": extra }),
                &token,
                &csrf,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{extra}");
    }

    // 4. Changes show up in diff-stat / diff (including untracked).
    std::fs::write(Path::new(&wt_path).join("README.md"), "hello\nworld\n").unwrap();
    std::fs::write(Path::new(&wt_path).join("NEW.md"), "new\n").unwrap();
    let resp = app
        .clone()
        .oneshot(get_with_token(&format!("/api/worktrees/{wt_id}/diff-stat"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let stat = body_json(resp).await["data"].clone();
    let files = stat["files"].as_array().unwrap();
    let status_of = |p: &str| {
        files
            .iter()
            .find(|f| f["path"] == p)
            .map(|f| f["status"].as_str().unwrap().to_owned())
    };
    assert_eq!(status_of("README.md").as_deref(), Some("modified"));
    assert_eq!(status_of("NEW.md").as_deref(), Some("untracked"));
    let resp = app
        .clone()
        .oneshot(get_with_token(
            &format!("/api/worktrees/{wt_id}/diff?path=NEW.md"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        body_json(resp).await["data"]["patch"]
            .as_str()
            .unwrap()
            .contains("+new")
    );

    // A bound worktree cannot be removed.
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/worktrees/{wt_id}?force_dir=true"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // 5. Delete the conversation → unbound, directory and work kept.
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/conversations/{conv_id}"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let wt = wait_worktree(&app, &token, &wt_id, "unbound").await;
    assert_eq!(wt["path"], wt_path.as_str());
    assert_eq!(
        std::fs::read_to_string(Path::new(&wt_path).join("NEW.md")).unwrap(),
        "new\n",
        "uncommitted work survives the conversation delete"
    );

    // 6. Explicit removal still works afterwards.
    let resp = app
        .clone()
        .oneshot(delete_with_token(
            &format!("/api/worktrees/{wt_id}?force_dir=true&branch=force"),
            &token,
            &csrf,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    wait_worktree(&app, &token, &wt_id, "removed").await;
    assert!(!Path::new(&wt_path).exists());

    services.database.close().await;
}

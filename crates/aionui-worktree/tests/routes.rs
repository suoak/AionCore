//! `/api/worktrees*` behind the real auth middleware: the desktop user gets
//! through; agent (runtime) tokens get 403 on every route.

mod common;

use std::sync::Arc;

use aionui_auth::{AuthIdentityMode, AuthState, IRuntimeTokenVerifier, JwtService, LocalApiSecret, auth_middleware};
use aionui_db::{IUserRepository, SqliteUserRepository};
use aionui_worktree::{WorktreeRouterState, worktree_routes};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware;
use common::*;
use tower::ServiceExt;

const LOCAL_SECRET: &str = "desktop-launch-secret-0123456789-abcdefghijk";

struct MatchVerifier;

impl IRuntimeTokenVerifier for MatchVerifier {
    fn verify_conversation_helper(&self, token: &str, user_id: &str, conversation_id: &str) -> bool {
        token == "tok-1" && user_id == USER && conversation_id == "conv-1"
    }
}

fn app(h: &Harness) -> Router {
    let state = AuthState {
        jwt_service: Arc::new(JwtService::new("worktree_routes_test_secret".into())),
        user_repo: Arc::new(SqliteUserRepository::new(h.db.pool().clone())) as Arc<dyn IUserRepository>,
        identity_mode: AuthIdentityMode::Local,
        runtime_token_verifier: Some(Arc::new(MatchVerifier)),
        local_secret: Some(LocalApiSecret::parse(LOCAL_SECRET).unwrap()),
        agent_approvals: Some(Arc::new(aionui_auth::AgentActionApprovals::new())),
        agent_conversation_access: None,
        devices: None,
    };
    worktree_routes(WorktreeRouterState {
        service: h.service.clone(),
    })
    .route_layer(middleware::from_fn_with_state(state, auth_middleware))
}

fn user_request(method: &str, uri: &str, body: Option<serde_json::Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-aionui-local-secret", LOCAL_SECRET);
    builder
        .body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty))
        .unwrap()
}

fn agent_request(method: &str, uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-aionui-runtime-token", "tok-1")
        .header("x-aionui-user-id", USER)
        .header("x-aionui-conversation-id", "conv-1")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn desktop_user_drives_the_lifecycle_over_http() {
    require_git!();
    let h = harness().await;
    let app = app(&h);

    let resp = app
        .clone()
        .oneshot(user_request("GET", "/api/worktrees/capabilities", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["data"]["available"], true);

    // Disk estimate (E3-5): blob sizes of the base tree, nothing created.
    let enc: String = h
        .repo
        .to_string_lossy()
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_./".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect();
    let resp = app
        .clone()
        .oneshot(user_request(
            "GET",
            &format!("/api/worktrees/estimate?repo_path={enc}"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let est = json(resp).await["data"].clone();
    let want_bytes: u64 = git(&h.repo, &["ls-tree", "-r", "-l", "HEAD"])
        .lines()
        .filter_map(|l| l.split_whitespace().nth(3)?.parse::<u64>().ok())
        .sum();
    let want_files = git(&h.repo, &["ls-files"]).lines().count() as u64;
    assert_eq!(est["tracked_bytes"], want_bytes);
    assert_eq!(est["tracked_files"], want_files);
    assert_eq!(est["truncated"], false);
    assert_eq!(est["base_commit"], git(&h.repo, &["rev-parse", "HEAD"]));
    let resp = app
        .clone()
        .oneshot(user_request(
            "GET",
            "/api/worktrees/estimate?repo_path=/definitely/not/a/repo",
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());

    let resp = app
        .clone()
        .oneshot(user_request(
            "POST",
            "/api/worktrees",
            Some(serde_json::json!({ "repo_path": h.repo, "title": "http" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let body = json(resp).await;
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["status"], "creating");
    h.wait_status(&id, "ready").await;

    let resp = app
        .clone()
        .oneshot(user_request("GET", &format!("/api/worktrees/{id}"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(json(resp).await["data"]["status"], "ready");

    let resp = app
        .clone()
        .oneshot(user_request("GET", "/api/worktrees?status=ready", None))
        .await
        .unwrap();
    assert_eq!(json(resp).await["data"].as_array().unwrap().len(), 1);

    let resp = app
        .clone()
        .oneshot(user_request("POST", &format!("/api/worktrees/{id}/cancel"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "WORKTREE_INVALID_STATE");

    let resp = app
        .clone()
        .oneshot(user_request(
            "DELETE",
            &format!("/api/worktrees/{id}?branch=keep"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let done = h.wait_status(&id, "removed").await;
    assert!(done.branch_preserved);

    let resp = app
        .clone()
        .oneshot(user_request("POST", "/api/worktrees/prune", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(user_request("GET", "/api/worktrees/wt_missing", None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(resp).await["code"], "WORKTREE_NOT_FOUND");

    let resp = app
        .clone()
        .oneshot(user_request(
            "POST",
            "/api/worktrees",
            Some(serde_json::json!({ "repo_path": h.repo, "branch": "main" })),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(json(resp).await["code"], "BRANCH_EXISTS");
}

#[tokio::test]
async fn agent_tokens_are_denied_on_every_worktree_route() {
    require_git!();
    let h = harness().await;
    // A real worktree so a leak would be observable.
    let wt = h
        .service
        .create(
            USER,
            aionui_worktree::types::CreateWorktreeRequest {
                repo_path: h.repo.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    let app = app(&h);
    let id = wt.id.as_str();
    let create_body = serde_json::json!({ "repo_path": h.repo }).to_string();
    for (method, uri) in [
        ("GET", "/api/worktrees".to_owned()),
        ("GET", "/api/worktrees?status=ready".to_owned()),
        ("GET", "/api/worktrees/capabilities".to_owned()),
        ("GET", "/api/worktrees/estimate?repo_path=/r".to_owned()),
        ("POST", "/api/worktrees".to_owned()),
        ("POST", "/api/worktrees/prune".to_owned()),
        ("GET", format!("/api/worktrees/{id}")),
        ("DELETE", format!("/api/worktrees/{id}?branch=force&force_dir=true")),
        ("POST", format!("/api/worktrees/{id}/cancel")),
        ("POST", format!("/api/worktrees/{id}/retry")),
        ("GET", format!("/api/worktrees/{id}/diff-stat")),
        ("GET", format!("/api/worktrees/{id}/diff?path=README.md")),
    ] {
        let resp = app
            .clone()
            .oneshot(agent_request(method, &uri, &create_body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(json(resp).await["code"], "AGENT_SCOPE_DENIED", "{method} {uri}");
    }
    // Nothing changed behind the 403s.
    let after = h.service.get(USER, id).await.unwrap();
    assert_eq!(after.status, "ready");
    assert_eq!(
        h.service
            .list(USER, aionui_worktree::types::ListWorktreesQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn desktop_user_reads_diff_stat_and_file_diff_over_http() {
    require_git!();
    let h = harness().await;
    let wt = h
        .service
        .create(
            USER,
            aionui_worktree::types::CreateWorktreeRequest {
                repo_path: h.repo.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    std::fs::write(std::path::Path::new(&wt.path).join("README.md"), "hello\nworld\n").unwrap();
    let app = app(&h);

    let resp = app
        .clone()
        .oneshot(user_request(
            "GET",
            &format!("/api/worktrees/{}/diff-stat", wt.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json(resp).await;
    assert_eq!(body["data"]["files_changed"], 1);
    assert_eq!(body["data"]["files"][0]["path"], "README.md");
    assert_eq!(body["data"]["files"][0]["additions"], 1);

    let resp = app
        .clone()
        .oneshot(user_request(
            "GET",
            &format!("/api/worktrees/{}/diff?path=README.md", wt.id),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json(resp).await;
    assert_eq!(body["data"]["status"], "modified");
    assert!(body["data"]["patch"].as_str().unwrap().contains("+world"));

    for bad in ["..%2Fetc%2Fpasswd", ".git%2Fconfig", ""] {
        let resp = app
            .clone()
            .oneshot(user_request(
                "GET",
                &format!("/api/worktrees/{}/diff?path={bad}", wt.id),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "path {bad:?}");
    }

    // Another user's id is indistinguishable from a missing one.
    let other = h.service.diff_stat(OTHER_USER, &wt.id).await.unwrap_err();
    assert_eq!(other.code(), "WORKTREE_NOT_FOUND");
}

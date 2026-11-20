use super::*;
use crate::init_database_memory;

const USER: &str = "system_default_user";

async fn setup() -> (crate::Database, SqliteApprovalRequestRepository) {
    let db = init_database_memory().await.unwrap();
    for id in ["task-1", "task-2"] {
        sqlx::query(
            "INSERT INTO task_sessions (id, user_id, title, mode, objective, acceptance_criteria, status, agent_type, \
             created_at, updated_at) VALUES (?, ?, 't', 'plan', 'o', '[]', 'waiting_approval', 'codex', 1, 1)",
        )
        .bind(id)
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let repo = SqliteApprovalRequestRepository::new(db.pool().clone());
    (db, repo)
}

fn adopt<'a>(id: &'a str, task: &'a str, digest: &'a str) -> NewApprovalRequest<'a> {
    NewApprovalRequest {
        id,
        user_id: USER,
        task_session_id: task,
        kind: "adopt",
        fanout_group_id: Some("fg-1"),
        slot: Some("A"),
        diff_digest: Some(digest),
        target_branch: Some("main"),
        strategy: Some("squash"),
        digest,
        requested_at: 100,
        expires_at: 1_000,
        ..Default::default()
    }
}

#[tokio::test]
async fn a_new_request_supersedes_the_pending_one_of_the_same_kind() {
    let (_db, repo) = setup().await;
    assert!(repo.create(&adopt("r1", "task-1", "d1")).await.unwrap().is_empty());
    repo.create(&adopt("o1", "task-2", "d9")).await.unwrap();
    let superseded = repo.create(&adopt("r2", "task-1", "d2")).await.unwrap();
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0].id, "r1");
    let r1 = repo.get(USER, "r1").await.unwrap().unwrap();
    assert_eq!(
        (r1.status.as_str(), r1.reason.as_deref()),
        ("invalidated", Some("superseded"))
    );
    assert_eq!(repo.get(USER, "o1").await.unwrap().unwrap().status, "pending");
    assert_eq!(repo.list_pending(USER).await.unwrap().len(), 2);
    // Another user never sees or creates requests on this task.
    assert!(repo.get("someone-else", "r2").await.unwrap().is_none());
    assert!(
        repo.create(&NewApprovalRequest {
            user_id: "someone-else",
            ..adopt("x", "task-1", "d")
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_request_is_decided_exactly_once() {
    let (_db, repo) = setup().await;
    repo.create(&adopt("r1", "task-1", "d1")).await.unwrap();
    assert!(
        !repo
            .claim("someone-else", "r1", "approved", "device:x", 200)
            .await
            .unwrap()
    );
    assert!(repo.claim(USER, "r1", "approved", "device:d1", 200).await.unwrap());
    assert!(!repo.claim(USER, "r1", "approved", "device:d1", 201).await.unwrap());
    assert!(!repo.claim(USER, "r1", "rejected", "im:telegram:1", 202).await.unwrap());
    let row = repo.get(USER, "r1").await.unwrap().unwrap();
    assert_eq!(row.status, "approved");
    assert_eq!(row.decided_by.as_deref(), Some("device:d1"));
    assert_eq!(row.decided_at, Some(200));
    assert!(repo.claim(USER, "r1", "expired", "x", 1).await.is_err());
    repo.record_result("r1", Some("abc123"), None).await.unwrap();
    assert_eq!(
        repo.get(USER, "r1").await.unwrap().unwrap().result_ref.as_deref(),
        Some("abc123")
    );
}

#[tokio::test]
async fn concurrent_claims_have_one_winner() {
    let (_db, repo) = setup().await;
    repo.create(&adopt("r1", "task-1", "d1")).await.unwrap();
    let (a, b) = tokio::join!(
        repo.claim(USER, "r1", "approved", "device:a", 200),
        repo.claim(USER, "r1", "approved", "device:b", 200)
    );
    assert!(a.unwrap() ^ b.unwrap());
}

#[tokio::test]
async fn an_expired_request_cannot_be_claimed() {
    let (_db, repo) = setup().await;
    repo.create(&adopt("r1", "task-1", "d1")).await.unwrap();
    assert!(!repo.claim(USER, "r1", "approved", "device:a", 1_000).await.unwrap());
    assert!(repo.expire_due(999).await.unwrap().is_empty());
    let expired = repo.expire_due(1_000).await.unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].status, "expired");
    assert!(repo.expire_due(2_000).await.unwrap().is_empty());
}

#[tokio::test]
async fn invalidation_only_touches_pending_requests() {
    let (_db, repo) = setup().await;
    repo.create(&adopt("r1", "task-1", "d1")).await.unwrap();
    assert!(repo.invalidate(USER, "r1", "diff_changed", 150).await.unwrap());
    assert!(!repo.invalidate(USER, "r1", "diff_changed", 151).await.unwrap());
    assert!(!repo.claim(USER, "r1", "approved", "device:a", 160).await.unwrap());
    repo.create(&adopt("r2", "task-1", "d2")).await.unwrap();
    let changed = repo
        .invalidate_pending(USER, "task-1", Some("plan"), "cancelled", 170)
        .await
        .unwrap();
    assert!(changed.is_empty());
    let changed = repo
        .invalidate_pending(USER, "task-1", None, "cancelled", 170)
        .await
        .unwrap();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].reason.as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn approvers_and_ttl_are_per_user() {
    let (_db, repo) = setup().await;
    assert!(!repo.is_approver(USER, "telegram", "42").await.unwrap());
    repo.add_approver(USER, "telegram", "42", 1).await.unwrap();
    repo.add_approver(USER, "telegram", "42", 2).await.unwrap();
    assert!(repo.is_approver(USER, "telegram", "42").await.unwrap());
    assert!(!repo.is_approver(USER, "slack", "42").await.unwrap());
    assert!(!repo.is_approver("someone-else", "telegram", "42").await.unwrap());
    assert_eq!(repo.list_approvers(USER).await.unwrap().len(), 1);
    assert!(repo.remove_approver(USER, "telegram", "42").await.unwrap());
    assert!(!repo.is_approver(USER, "telegram", "42").await.unwrap());

    assert_eq!(repo.ttl_minutes(USER).await.unwrap(), None);
    repo.set_ttl_minutes(USER, 45, 1).await.unwrap();
    repo.set_ttl_minutes(USER, 60, 2).await.unwrap();
    assert_eq!(repo.ttl_minutes(USER).await.unwrap(), Some(60));
    assert!(repo.set_ttl_minutes(USER, 1_441, 3).await.is_err(), "at most 24 hours");
}

#[tokio::test]
async fn pending_approvals_from_before_c9_are_backfilled() {
    // The migration's backfill statement, run again over a fresh pending approval.
    let (db, repo) = setup().await;
    sqlx::query(
        "INSERT INTO task_artifacts (id, task_session_id, kind, version, content, content_hash, status, created_at, \
         updated_at) VALUES ('art-1', 'task-1', 'plan', 1, 'p', 'h1', 'submitted', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_approvals (id, task_session_id, approval_type, artifact_id, artifact_hash, status, \
         requested_at) VALUES ('ap-1', 'task-1', 'plan', 'art-1', 'h1', 'pending', 5)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let backfill = include_str!("../../migrations/084_approval_requests.sql");
    let insert = &backfill[backfill.find("INSERT INTO approval_requests").unwrap()..];
    sqlx::query(insert).execute(db.pool()).await.unwrap();
    sqlx::query(insert).execute(db.pool()).await.unwrap();
    let rows = repo.list_for_task(USER, "task-1").await.unwrap();
    assert_eq!(rows.len(), 1, "idempotent");
    assert_eq!(rows[0].approval_id.as_deref(), Some("ap-1"));
    assert_eq!(rows[0].digest, "h1");
    assert_eq!(rows[0].kind, "plan");
    assert_eq!(rows[0].status, "pending");
    assert_eq!(
        repo.find_for_approval(USER, "ap-1").await.unwrap().unwrap().id,
        rows[0].id
    );
}

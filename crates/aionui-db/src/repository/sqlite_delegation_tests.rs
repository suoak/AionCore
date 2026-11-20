use super::*;
use crate::init_database_memory;

const USER: &str = "system_default_user";

async fn setup() -> (crate::Database, SqliteDelegationRepository) {
    let db = init_database_memory().await.unwrap();
    for id in ["parent", "child-1", "child-2"] {
        sqlx::query("INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) VALUES (?, ?, 'c', 'acp', 1, 1)")
            .bind(id)
            .bind(USER)
            .execute(db.pool())
            .await
            .unwrap();
    }
    let repo = SqliteDelegationRepository::new(db.pool().clone());
    (db, repo)
}

fn new_delegation(id: &str) -> NewDelegation<'_> {
    NewDelegation {
        id,
        user_id: USER,
        parent_conversation_id: "parent",
        assistant_id: "writer",
        task_text: "draft the summary",
        created_at: 10,
    }
}

#[tokio::test]
async fn grants_are_per_conversation_and_owner() {
    let (_db, repo) = setup().await;
    assert!(!repo.is_granted(USER, "parent", "writer").await.unwrap());
    repo.grant(USER, "parent", "writer", 1).await.unwrap();
    repo.grant(USER, "parent", "writer", 2).await.unwrap();
    assert!(repo.is_granted(USER, "parent", "writer").await.unwrap());
    assert!(!repo.is_granted(USER, "child-1", "writer").await.unwrap());
    assert!(!repo.is_granted("someone-else", "parent", "writer").await.unwrap());
    // Granting on a conversation the caller does not own is a no-op.
    repo.grant("someone-else", "parent", "reviewer", 3).await.unwrap();
    assert!(!repo.is_granted("someone-else", "parent", "reviewer").await.unwrap());
}

#[tokio::test]
async fn the_unfinished_limit_is_enforced_atomically() {
    let (_db, repo) = setup().await;
    assert!(repo.create_within_limit(&new_delegation("d1"), 2).await.unwrap());
    assert!(repo.create_within_limit(&new_delegation("d2"), 2).await.unwrap());
    assert!(!repo.create_within_limit(&new_delegation("d3"), 2).await.unwrap());
    assert!(repo.finish("d1", "completed", None, Some("done"), 20).await.unwrap());
    assert!(repo.create_within_limit(&new_delegation("d3"), 2).await.unwrap());
}

#[tokio::test]
async fn a_delegation_runs_then_finishes_once() {
    let (_db, repo) = setup().await;
    repo.create_within_limit(&new_delegation("d1"), 3).await.unwrap();
    assert!(repo.mark_running("d1", "child-1", 11).await.unwrap());
    assert!(!repo.mark_running("d1", "child-2", 12).await.unwrap());
    let row = repo.find_by_child(USER, "child-1").await.unwrap().unwrap();
    assert_eq!(row.id, "d1");
    assert_eq!(row.status, "running");
    assert!(repo.finish("d1", "failed", Some("boom"), None, 13).await.unwrap());
    assert!(!repo.finish("d1", "completed", None, None, 14).await.unwrap());
    let row = repo.get(USER, "d1").await.unwrap().unwrap();
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("failed", Some("boom")));
    assert_eq!(row.result_text, None);
    assert!(repo.get("someone-else", "d1").await.unwrap().is_none());
    assert!(repo.finish("d1", "running", None, None, 15).await.is_err());
}

#[tokio::test]
async fn boot_fails_unfinished_delegations() {
    let (_db, repo) = setup().await;
    repo.create_within_limit(&new_delegation("d1"), 3).await.unwrap();
    repo.create_within_limit(&new_delegation("d2"), 3).await.unwrap();
    repo.finish("d2", "completed", None, Some("ok"), 11).await.unwrap();
    assert_eq!(repo.fail_unfinished(12).await.unwrap(), 1);
    let rows = repo.list_for_parent(USER, "parent").await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.status.as_str()).collect::<Vec<_>>(),
        vec!["failed", "completed"]
    );
}

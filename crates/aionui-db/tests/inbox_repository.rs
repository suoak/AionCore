use aionui_db::{IInboxRepository, InboxFilter, SqliteInboxRepository, UpsertInboxItemParams, init_database_memory};

fn item<'a>(user_id: &'a str, dedupe_key: &'a str, title: &'a str, created_at: i64) -> UpsertInboxItemParams<'a> {
    UpsertInboxItemParams {
        user_id,
        kind: "approval",
        source_kind: "conversation",
        source_id: "conv-1",
        conversation_id: Some("conv-1"),
        task_session_id: None,
        run_id: None,
        title,
        summary: Some("Bash: ls"),
        dedupe_key,
        created_at,
    }
}

fn all(limit: i64) -> InboxFilter {
    InboxFilter {
        unread_only: false,
        before: None,
        limit,
    }
}

#[tokio::test]
async fn upsert_dedupes_and_refreshes_read_state() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteInboxRepository::new(db.pool().clone());
    let first = repo
        .upsert(&item("u1", "approval:c1:t1", "Approve?", 10))
        .await
        .unwrap();
    assert!(repo.mark_read("u1", &first.id, 11).await.unwrap());
    assert_eq!(repo.unread_count("u1").await.unwrap(), 0);

    let again = repo
        .upsert(&item("u1", "approval:c1:t1", "Approve again?", 12))
        .await
        .unwrap();
    assert_eq!(again.id, first.id, "same dedupe key keeps identity");
    assert_eq!(again.title, "Approve again?");
    assert_eq!(again.read_at, None);
    assert_eq!(repo.unread_count("u1").await.unwrap(), 1);
    assert_eq!(repo.list("u1", &all(10)).await.unwrap().len(), 1);
}

#[tokio::test]
async fn resolve_marks_read_and_unread_filter_hides_it() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteInboxRepository::new(db.pool().clone());
    repo.upsert(&item("u1", "approval:c1:t1", "A", 10)).await.unwrap();
    repo.upsert(&item("u1", "approval:c1:t2", "B", 20)).await.unwrap();
    assert!(repo.resolve_by_dedupe_key("u1", "approval:c1:t1", 30).await.unwrap());
    assert!(!repo.resolve_by_dedupe_key("u1", "approval:c1:t1", 31).await.unwrap());
    let unread = repo
        .list(
            "u1",
            &InboxFilter {
                unread_only: true,
                ..all(10)
            },
        )
        .await
        .unwrap();
    assert_eq!(unread.iter().map(|row| row.title.as_str()).collect::<Vec<_>>(), ["B"]);
    let listed = repo.list("u1", &all(10)).await.unwrap();
    assert_eq!(listed[1].resolved_at, Some(30));
    assert_eq!(listed[1].read_at, Some(30));
}

#[tokio::test]
async fn items_are_user_scoped_and_paged_newest_first() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteInboxRepository::new(db.pool().clone());
    let other = repo.upsert(&item("u2", "k", "theirs", 5)).await.unwrap();
    for (key, at) in [("a", 10), ("b", 20), ("c", 30)] {
        repo.upsert(&item("u1", key, key, at)).await.unwrap();
    }
    assert!(
        !repo.mark_read("u1", &other.id, 40).await.unwrap(),
        "cannot touch another user's item"
    );
    let page = repo
        .list(
            "u1",
            &InboxFilter {
                before: Some(30),
                ..all(1)
            },
        )
        .await
        .unwrap();
    assert_eq!(page[0].title, "b");
    assert_eq!(repo.mark_all_read("u1", 50).await.unwrap(), 3);
    assert_eq!(repo.unread_count("u1").await.unwrap(), 0);
    assert_eq!(repo.unread_count("u2").await.unwrap(), 1);
}

#[tokio::test]
async fn prune_applies_age_and_count_caps() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteInboxRepository::new(db.pool().clone());
    for (key, at) in [("a", 10), ("b", 20), ("c", 30), ("d", 40)] {
        repo.upsert(&item("u1", key, key, at)).await.unwrap();
    }
    repo.upsert(&item("u2", "x", "x", 1)).await.unwrap();
    repo.prune_for_user("u1", 15, 2).await.unwrap();
    let left: Vec<_> = repo
        .list("u1", &all(10))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.title)
        .collect();
    assert_eq!(left, ["d", "c"]);
    assert_eq!(repo.list("u2", &all(10)).await.unwrap().len(), 1);
}

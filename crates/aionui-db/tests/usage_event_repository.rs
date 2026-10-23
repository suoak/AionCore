use aionui_common::now_ms;
use aionui_db::{
    IUsageEventRepository, InsertUsageEventParams, SqliteUsageEventRepository, UsageEventFilter, init_database_memory,
};

fn spend_params<'a>(
    user_id: &'a str,
    conversation_id: &'a str,
    fingerprint: &'a str,
    input_tokens: i64,
) -> InsertUsageEventParams<'a> {
    InsertUsageEventParams {
        user_id,
        conversation_id,
        recorded_at: now_ms(),
        fingerprint,
        backend: "claude",
        conversation_source: "lark",
        conversation_name: Some("Channel chat"),
        assistant_id: Some("asst-1"),
        assistant_name: Some("Claude"),
        model_id: Some("sonnet"),
        turn_id: Some("turn-1"),
        total_tokens: input_tokens + 4,
        input_tokens,
        output_tokens: 4,
        thought_tokens: 0,
        cached_read_tokens: 0,
        cached_write_tokens: 0,
        cost_delta: 0.1,
        session_cost_amount: Some(0.1),
        cost_currency: Some("USD"),
        event_source: "acp",
        task_session_id: None,
        run_id: None,
        usage_status: "reported",
    }
}

#[tokio::test]
async fn insert_is_idempotent_on_fingerprint() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    let first = repo
        .insert_if_new(&spend_params("user-1", "conv-1", "turn:turn-1", 10))
        .await
        .unwrap();
    let second = repo
        .insert_if_new(&spend_params("user-1", "conv-1", "turn:turn-1", 99))
        .await
        .unwrap();

    assert!(first.is_some());
    assert!(second.is_none());
    let listed = repo.list_for_user("user-1", None, 50).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].input_tokens, 10);
    assert_eq!(listed[0].total_tokens, 14);
    assert_eq!(listed[0].fingerprint, "turn:turn-1");
    assert_eq!(listed[0].turn_id.as_deref(), Some("turn-1"));
    assert_eq!(listed[0].conversation_source, "lark");
}

#[tokio::test]
async fn clear_and_list_are_user_scoped() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    repo.insert_if_new(&spend_params("user-1", "conv-1", "turn:a", 8))
        .await
        .unwrap();
    repo.insert_if_new(&spend_params("user-2", "conv-2", "turn:b", 12))
        .await
        .unwrap();

    assert_eq!(repo.clear_for_user("user-1").await.unwrap(), 1);
    assert!(repo.list_for_user("user-1", None, 50).await.unwrap().is_empty());
    assert_eq!(repo.list_for_user("user-2", None, 50).await.unwrap().len(), 1);
}

#[tokio::test]
async fn task_link_columns_default_and_filter() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    // A chat turn (no task linkage) gets the migration defaults.
    repo.insert_if_new(&spend_params("user-1", "conv-1", "turn:chat", 5))
        .await
        .unwrap();
    let mut linked = spend_params("user-1", "conv-2", "turn:task", 7);
    linked.task_session_id = Some("task-1");
    linked.run_id = Some("run-1");
    repo.insert_if_new(&linked).await.unwrap();
    // Another user's row with the same task id must never leak.
    let mut other = spend_params("user-2", "conv-3", "turn:other", 9);
    other.task_session_id = Some("task-1");
    other.run_id = Some("run-1");
    repo.insert_if_new(&other).await.unwrap();

    let all = repo.list_for_user("user-1", None, 50).await.unwrap();
    let chat = all.iter().find(|r| r.fingerprint == "turn:chat").unwrap();
    assert_eq!(chat.task_session_id, None);
    assert_eq!(chat.run_id, None);
    assert_eq!(chat.usage_status, "reported");

    let by_task = repo
        .list_filtered(
            "user-1",
            &UsageEventFilter {
                task_session_id: Some("task-1".into()),
                limit: 50,
                ..UsageEventFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_task.len(), 1);
    assert_eq!(by_task[0].run_id.as_deref(), Some("run-1"));

    let by_run = repo
        .list_filtered(
            "user-1",
            &UsageEventFilter {
                run_id: Some("run-1".into()),
                limit: 50,
                ..UsageEventFilter::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_run.len(), 1);

    let window = repo
        .list_filtered(
            "user-1",
            &UsageEventFilter {
                since: Some(0),
                until: Some(1),
                limit: 50,
                ..UsageEventFilter::default()
            },
        )
        .await
        .unwrap();
    assert!(window.is_empty(), "until is exclusive and rows are recorded now");
}

#[tokio::test]
async fn usage_status_rejects_unknown_values() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    let mut bad = spend_params("user-1", "conv-1", "turn:bad", 5);
    bad.usage_status = "estimated";
    // INSERT OR IGNORE turns the CHECK violation into a skipped row: nothing invented is stored.
    let inserted = repo.insert_if_new(&bad).await;
    assert!(matches!(inserted, Ok(None) | Err(_)), "{inserted:?}");
    assert!(repo.list_for_user("user-1", None, 50).await.unwrap().is_empty());
}

#[tokio::test]
async fn pre_067_rows_get_reported_status() {
    let db = init_database_memory().await.unwrap();
    // Simulate a row written by an older binary: omit the 067 columns entirely.
    sqlx::query(
        "INSERT INTO usage_events (id, user_id, conversation_id, recorded_at, fingerprint, backend, \
         conversation_source, total_tokens, input_tokens, output_tokens, thought_tokens, cached_read_tokens, \
         cached_write_tokens, cost_delta, event_source) \
         VALUES ('legacy', 'user-1', 'conv-1', 1, 'turn:legacy', 'claude', 'aionui', 3, 2, 1, 0, 0, 0, 0, 'acp')",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    let rows = repo.list_for_user("user-1", None, 50).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].usage_status, "reported");
    assert_eq!(rows[0].task_session_id, None);
}

#[tokio::test]
async fn reported_row_supersedes_unavailable_placeholder() {
    // M7 E5-2: a turn that ended without usage gets an `unavailable` row; a
    // late provider report for the same fingerprint replaces it, but an
    // `unavailable` write never overrides a reported row.
    let db = init_database_memory().await.unwrap();
    let repo = SqliteUsageEventRepository::new(db.pool().clone());
    let mut placeholder = spend_params("user-1", "conv-1", "turn:turn-1", 0);
    placeholder.total_tokens = 0;
    placeholder.output_tokens = 0;
    placeholder.cost_delta = 0.0;
    placeholder.session_cost_amount = None;
    placeholder.cost_currency = None;
    placeholder.usage_status = "unavailable";
    assert!(repo.insert_if_new(&placeholder).await.unwrap().is_some());
    // Repeated unavailable write is idempotent.
    assert!(repo.insert_if_new(&placeholder).await.unwrap().is_none());

    let reported = repo
        .insert_if_new(&spend_params("user-1", "conv-1", "turn:turn-1", 10))
        .await
        .unwrap();
    assert!(reported.is_some());
    let listed = repo.list_for_user("user-1", None, 50).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].usage_status, "reported");
    assert_eq!(listed[0].input_tokens, 10);

    assert!(repo.insert_if_new(&placeholder).await.unwrap().is_none());
    let listed = repo.list_for_user("user-1", None, 50).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].usage_status, "reported");
}

//! Migration 072 (M8 E6-2) rolls forward on a database with existing tasks
//! and maps them to board columns by run state (D6-3).

use std::borrow::Cow;
use std::path::Path;

use sqlx::Row;
use sqlx::migrate::Migrator;
use sqlx::sqlite::SqlitePoolOptions;

async fn run_migrations(pool: &sqlx::SqlitePool, keep: impl Fn(i64) -> bool, ignore_missing: bool) {
    let full = Migrator::new(Path::new("migrations")).await.unwrap();
    let migrations = full
        .migrations
        .iter()
        .filter(|migration| keep(migration.version))
        .cloned()
        .collect::<Vec<_>>();
    Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing,
        locking: true,
        no_tx: false,
    }
    .run(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn migration_072_maps_existing_tasks_to_board_columns() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    run_migrations(&pool, |version| version <= 71, false).await;

    let statuses = [
        ("draft", "backlog"),
        ("ready", "backlog"),
        ("running", "in_progress"),
        ("waiting_approval", "in_progress"),
        ("paused", "in_progress"),
        ("completed", "done"),
        ("failed", "cancelled"),
        ("cancelled", "cancelled"),
    ];
    for (status, _) in statuses {
        sqlx::query(
            "INSERT INTO task_sessions (id, user_id, title, mode, objective, acceptance_criteria, status, \
             agent_type, created_at, updated_at) VALUES (?, 'system_default_user', 't', 'agent', 'o', '[]', ?, \
             'codex', 1, 1)",
        )
        .bind(format!("task-{status}"))
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }

    run_migrations(&pool, |version| version == 72, true).await;

    for (status, column) in statuses {
        let row = sqlx::query(
            "SELECT board_status, priority, labels, assignee_kind, updated_at FROM task_sessions WHERE id = ?",
        )
        .bind(format!("task-{status}"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>("board_status"), column, "{status}");
        assert_eq!(row.get::<i64, _>("priority"), 0);
        assert_eq!(row.get::<String, _>("labels"), "[]");
        assert_eq!(row.get::<Option<String>, _>("assignee_kind"), None);
        // The mapping does not touch updated_at.
        assert_eq!(row.get::<i64, _>("updated_at"), 1);
    }
    let comments: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_comments")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(comments, 0);
}

#[tokio::test]
async fn migration_082_adds_task_flow_columns_without_touching_rows() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&pool).await.unwrap();
    run_migrations(&pool, |version| version <= 81, false).await;
    sqlx::query(
        "INSERT INTO task_sessions (id, user_id, title, mode, objective, acceptance_criteria, status, agent_type, \
         created_at, updated_at) VALUES ('old', 'system_default_user', 't', 'agent', 'o', '[]', 'running', 'codex', \
         1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    run_migrations(&pool, |version| version == 82, true).await;

    let row = sqlx::query(
        "SELECT status, board_status, updated_at, assistant_id, prompt_json, repo_path, base_ref, plan_first, \
         source, source_ref, fanout_group_id, delivery_json, queued_at FROM task_sessions WHERE id = 'old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.get::<String, _>("status"), "running");
    assert_eq!(row.get::<String, _>("board_status"), "backlog");
    assert_eq!(row.get::<i64, _>("updated_at"), 1);
    assert_eq!(row.get::<i64, _>("plan_first"), 0);
    for column in [
        "assistant_id",
        "prompt_json",
        "repo_path",
        "base_ref",
        "source",
        "source_ref",
        "fanout_group_id",
        "delivery_json",
    ] {
        assert_eq!(row.get::<Option<String>, _>(column), None, "{column}");
    }
    assert_eq!(row.get::<Option<i64>, _>("queued_at"), None);

    for (column, value) in [
        ("source", "'email'"),
        ("prompt_json", "'[]'"),
        ("delivery_json", "'not json'"),
        ("plan_first", "2"),
    ] {
        let result = sqlx::query(&format!("UPDATE task_sessions SET {column} = {value} WHERE id = 'old'"))
            .execute(&pool)
            .await;
        assert!(result.is_err(), "{column} = {value} should be rejected");
    }
}

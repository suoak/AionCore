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

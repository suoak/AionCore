//! M7 E5-4: migration 069 (`inbox_items`) merges after 070 (`task_run_retry`).
//!
//! A database that was already migrated to 070 without 069 (a dev build of
//! main between the two merges) must pick up 069 on the next startup instead
//! of failing: the sqlx migrator applies every embedded version that is not in
//! `_sqlx_migrations`, regardless of order.

use aionui_db::init_database_staged;

async fn has_table(pool: &sqlx::SqlitePool, name: &str) -> bool {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn applied(pool: &sqlx::SqlitePool, version: i64) -> bool {
    sqlx::query_scalar("SELECT COUNT(*) > 0 FROM _sqlx_migrations WHERE version = ? AND success = 1")
        .bind(version)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn database_at_070_without_069_backfills_inbox_on_startup() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aionui-backend.db");

    // Arrange: a database migrated through 070 that never saw 069.
    let db = init_database_staged(&path).await.unwrap();
    assert!(applied(db.pool(), 70).await, "070 must be embedded and applied");
    sqlx::query("DROP TABLE inbox_items").execute(db.pool()).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 69")
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!has_table(db.pool(), "inbox_items").await);
    assert!(!applied(db.pool(), 69).await);
    db.close().await;

    // Act: restart with the migrator that embeds both.
    let db = init_database_staged(&path)
        .await
        .expect("an out-of-order 069 must be applied, not rejected");

    // Assert: 069 ran, 070 is untouched, and the table is usable.
    assert!(applied(db.pool(), 69).await);
    assert!(applied(db.pool(), 70).await);
    assert!(has_table(db.pool(), "inbox_items").await);
    let index: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'index' AND name = 'idx_inbox_user_unread'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(index);
    db.close().await;

    // A further restart is a no-op.
    let db = init_database_staged(&path).await.expect("second restart");
    assert!(applied(db.pool(), 69).await);
    db.close().await;
}

use aionui_common::{TimestampMs, generate_prefixed_id};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::InboxItemRow;
use crate::repository::inbox::{IInboxRepository, InboxFilter, UpsertInboxItemParams};

const SELECT_COLUMNS: &str = "SELECT id, user_id, kind, source_kind, source_id, conversation_id, task_session_id, \
     run_id, title, summary, dedupe_key, created_at, read_at, resolved_at FROM inbox_items";

#[derive(Clone, Debug)]
pub struct SqliteInboxRepository {
    pool: SqlitePool,
}

impl SqliteInboxRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IInboxRepository for SqliteInboxRepository {
    async fn upsert(&self, params: &UpsertInboxItemParams<'_>) -> Result<InboxItemRow, DbError> {
        let id = generate_prefixed_id("inbox");
        let sql = format!(
            "INSERT INTO inbox_items (\
                id, user_id, kind, source_kind, source_id, conversation_id, task_session_id, run_id, \
                title, summary, dedupe_key, created_at, read_at, resolved_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL) \
            ON CONFLICT (user_id, dedupe_key) DO UPDATE SET \
                kind = excluded.kind, source_kind = excluded.source_kind, source_id = excluded.source_id, \
                conversation_id = excluded.conversation_id, task_session_id = excluded.task_session_id, \
                run_id = excluded.run_id, title = excluded.title, summary = excluded.summary, \
                created_at = excluded.created_at, read_at = NULL, resolved_at = NULL \
            RETURNING {}",
            SELECT_COLUMNS
                .trim_start_matches("SELECT ")
                .trim_end_matches(" FROM inbox_items")
        );
        Ok(sqlx::query_as::<_, InboxItemRow>(&sql)
            .bind(&id)
            .bind(params.user_id)
            .bind(params.kind)
            .bind(params.source_kind)
            .bind(params.source_id)
            .bind(params.conversation_id)
            .bind(params.task_session_id)
            .bind(params.run_id)
            .bind(params.title)
            .bind(params.summary)
            .bind(params.dedupe_key)
            .bind(params.created_at)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn list(&self, user_id: &str, filter: &InboxFilter) -> Result<Vec<InboxItemRow>, DbError> {
        let mut sql = format!("{SELECT_COLUMNS} WHERE user_id = ?");
        if filter.unread_only {
            sql.push_str(" AND read_at IS NULL AND resolved_at IS NULL");
        }
        if filter.before.is_some() {
            sql.push_str(" AND created_at < ?");
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
        let mut query = sqlx::query_as::<_, InboxItemRow>(&sql).bind(user_id);
        if let Some(before) = filter.before {
            query = query.bind(before);
        }
        Ok(query.bind(filter.limit.clamp(1, 500)).fetch_all(&self.pool).await?)
    }

    async fn unread_count(&self, user_id: &str) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM inbox_items WHERE user_id = ? AND read_at IS NULL AND resolved_at IS NULL",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn mark_read(&self, user_id: &str, id: &str, at: TimestampMs) -> Result<bool, DbError> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM inbox_items WHERE user_id = ? AND id = ?")
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            return Ok(false);
        }
        sqlx::query("UPDATE inbox_items SET read_at = ? WHERE user_id = ? AND id = ? AND read_at IS NULL")
            .bind(at)
            .bind(user_id)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(true)
    }

    async fn mark_all_read(&self, user_id: &str, at: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE inbox_items SET read_at = ? WHERE user_id = ? AND read_at IS NULL")
            .bind(at)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    async fn resolve_by_dedupe_key(&self, user_id: &str, dedupe_key: &str, at: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE inbox_items SET resolved_at = ?, read_at = COALESCE(read_at, ?) \
             WHERE user_id = ? AND dedupe_key = ? AND resolved_at IS NULL",
        )
        .bind(at)
        .bind(at)
        .bind(user_id)
        .bind(dedupe_key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn prune_for_user(&self, user_id: &str, cutoff: TimestampMs, max_items: i64) -> Result<(), DbError> {
        sqlx::query("DELETE FROM inbox_items WHERE user_id = ? AND created_at < ?")
            .bind(user_id)
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "DELETE FROM inbox_items WHERE user_id = ? AND id NOT IN (\
                SELECT id FROM inbox_items WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?\
            )",
        )
        .bind(user_id)
        .bind(user_id)
        .bind(max_items.max(0))
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

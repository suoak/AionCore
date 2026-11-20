use aionui_common::TimestampMs;
use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::repository::delegation::{DELEGATION_STATUSES, DelegationRow, IDelegationRepository, NewDelegation};

const COLUMNS: &str = "id, user_id, parent_conversation_id, child_conversation_id, assistant_id, task_text, status, \
     error, result_text, created_at, updated_at";

#[derive(Clone)]
pub struct SqliteDelegationRepository {
    pool: SqlitePool,
}

impl SqliteDelegationRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl IDelegationRepository for SqliteDelegationRepository {
    async fn grant(
        &self,
        user_id: &str,
        conversation_id: &str,
        assistant_id: &str,
        at: TimestampMs,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO delegation_grants (conversation_id, assistant_id, user_id, created_at) \
             SELECT id, ?, user_id, ? FROM conversations WHERE id = ? AND user_id = ? \
             ON CONFLICT (conversation_id, assistant_id) DO NOTHING",
        )
        .bind(assistant_id)
        .bind(at)
        .bind(conversation_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn is_granted(&self, user_id: &str, conversation_id: &str, assistant_id: &str) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM delegation_grants WHERE user_id = ? AND conversation_id = ? AND assistant_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .bind(assistant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn create_within_limit(&self, new: &NewDelegation<'_>, limit: i64) -> Result<bool, DbError> {
        let result = sqlx::query(
            "INSERT INTO delegations (id, user_id, parent_conversation_id, assistant_id, task_text, status, \
             created_at, updated_at) \
             SELECT ?, ?, ?, ?, ?, 'starting', ?, ? WHERE \
             (SELECT COUNT(*) FROM delegations WHERE parent_conversation_id = ? AND status IN ('starting', 'running')) < ?",
        )
        .bind(new.id)
        .bind(new.user_id)
        .bind(new.parent_conversation_id)
        .bind(new.assistant_id)
        .bind(new.task_text)
        .bind(new.created_at)
        .bind(new.created_at)
        .bind(new.parent_conversation_id)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn mark_running(&self, id: &str, child_conversation_id: &str, at: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE delegations SET status = 'running', child_conversation_id = ?, updated_at = ? \
             WHERE id = ? AND status = 'starting'",
        )
        .bind(child_conversation_id)
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn finish(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
        result_text: Option<&str>,
        at: TimestampMs,
    ) -> Result<bool, DbError> {
        if !DELEGATION_STATUSES[2..].contains(&status) {
            return Err(DbError::Conflict(format!("Invalid final delegation status '{status}'")));
        }
        let result = sqlx::query(
            "UPDATE delegations SET status = ?, error = ?, result_text = ?, updated_at = ? \
             WHERE id = ? AND status IN ('starting', 'running')",
        )
        .bind(status)
        .bind(error)
        .bind(result_text)
        .bind(at)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<DelegationRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM delegations WHERE user_id = ? AND id = ?"
        ))
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn find_by_child(
        &self,
        user_id: &str,
        child_conversation_id: &str,
    ) -> Result<Option<DelegationRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM delegations WHERE user_id = ? AND child_conversation_id = ?"
        ))
        .bind(user_id)
        .bind(child_conversation_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_for_parent(
        &self,
        user_id: &str,
        parent_conversation_id: &str,
    ) -> Result<Vec<DelegationRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM delegations WHERE user_id = ? AND parent_conversation_id = ? \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(user_id)
        .bind(parent_conversation_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn fail_unfinished(&self, at: TimestampMs) -> Result<u64, DbError> {
        Ok(sqlx::query(
            "UPDATE delegations SET status = 'failed', error = 'interrupted by a restart', updated_at = ? \
             WHERE status IN ('starting', 'running')",
        )
        .bind(at)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[cfg(test)]
#[path = "sqlite_delegation_tests.rs"]
mod tests;

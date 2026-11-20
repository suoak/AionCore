use aionui_common::TimestampMs;
use async_trait::async_trait;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::repository::approval_request::{
    ApprovalRequestRow, IApprovalRequestRepository, NewApprovalRequest, RemoteApproverRow,
};

const COLUMNS: &str = "id, user_id, task_session_id, kind, approval_id, artifact_id, artifact_hash, fanout_group_id, \
     slot, diff_digest, target_branch, strategy, digest, status, requested_at, expires_at, decided_at, decided_by, \
     reason, result_ref, error";

#[derive(Clone)]
pub struct SqliteApprovalRequestRepository {
    pool: SqlitePool,
}

impl SqliteApprovalRequestRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl IApprovalRequestRepository for SqliteApprovalRequestRepository {
    async fn create(&self, new: &NewApprovalRequest<'_>) -> Result<Vec<ApprovalRequestRow>, DbError> {
        let mut tx = self.pool.begin().await?;
        let superseded: Vec<ApprovalRequestRow> = sqlx::query_as(&format!(
            "UPDATE approval_requests SET status = 'invalidated', reason = 'superseded', decided_at = ? \
             WHERE user_id = ? AND task_session_id = ? AND kind = ? AND status = 'pending' RETURNING {COLUMNS}"
        ))
        .bind(new.requested_at)
        .bind(new.user_id)
        .bind(new.task_session_id)
        .bind(new.kind)
        .fetch_all(&mut *tx)
        .await?;
        let inserted = sqlx::query(
            "INSERT INTO approval_requests (id, user_id, task_session_id, kind, approval_id, artifact_id, \
             artifact_hash, fanout_group_id, slot, diff_digest, target_branch, strategy, digest, status, \
             requested_at, expires_at) \
             SELECT ?, ?, id, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ? FROM task_sessions \
             WHERE id = ? AND user_id = ?",
        )
        .bind(new.id)
        .bind(new.user_id)
        .bind(new.kind)
        .bind(new.approval_id)
        .bind(new.artifact_id)
        .bind(new.artifact_hash)
        .bind(new.fanout_group_id)
        .bind(new.slot)
        .bind(new.diff_digest)
        .bind(new.target_branch)
        .bind(new.strategy)
        .bind(new.digest)
        .bind(new.requested_at)
        .bind(new.expires_at)
        .bind(new.task_session_id)
        .bind(new.user_id)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(DbError::NotFound(format!("task session {}", new.task_session_id)));
        }
        tx.commit().await?;
        Ok(superseded)
    }

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM approval_requests WHERE user_id = ? AND id = ?"
        ))
        .bind(user_id)
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_for_task(&self, user_id: &str, task_session_id: &str) -> Result<Vec<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM approval_requests WHERE user_id = ? AND task_session_id = ? \
             ORDER BY requested_at DESC, id"
        ))
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_pending(&self, user_id: &str) -> Result<Vec<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM approval_requests WHERE user_id = ? AND status = 'pending' \
             ORDER BY requested_at DESC, id"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn find_for_approval(&self, user_id: &str, approval_id: &str) -> Result<Option<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM approval_requests WHERE user_id = ? AND approval_id = ? \
             ORDER BY requested_at DESC LIMIT 1"
        ))
        .bind(user_id)
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn claim(
        &self,
        user_id: &str,
        id: &str,
        status: &str,
        decided_by: &str,
        at: TimestampMs,
    ) -> Result<bool, DbError> {
        if status != "approved" && status != "rejected" {
            return Err(DbError::Conflict(format!(
                "cannot decide an approval request as {status}"
            )));
        }
        let result = sqlx::query(
            "UPDATE approval_requests SET status = ?, decided_by = ?, decided_at = ? \
             WHERE user_id = ? AND id = ? AND status = 'pending' AND expires_at > ?",
        )
        .bind(status)
        .bind(decided_by)
        .bind(at)
        .bind(user_id)
        .bind(id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn record_result(&self, id: &str, result_ref: Option<&str>, error: Option<&str>) -> Result<(), DbError> {
        sqlx::query("UPDATE approval_requests SET result_ref = ?, error = ? WHERE id = ?")
            .bind(result_ref)
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn invalidate_pending(
        &self,
        user_id: &str,
        task_session_id: &str,
        kind: Option<&str>,
        reason: &str,
        at: TimestampMs,
    ) -> Result<Vec<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "UPDATE approval_requests SET status = 'invalidated', reason = ?, decided_at = ? \
             WHERE user_id = ? AND task_session_id = ? AND (? IS NULL OR kind = ?) AND status = 'pending' \
             RETURNING {COLUMNS}"
        ))
        .bind(reason)
        .bind(at)
        .bind(user_id)
        .bind(task_session_id)
        .bind(kind)
        .bind(kind)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn invalidate(&self, user_id: &str, id: &str, reason: &str, at: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE approval_requests SET status = 'invalidated', reason = ?, decided_at = ? \
             WHERE user_id = ? AND id = ? AND status = 'pending'",
        )
        .bind(reason)
        .bind(at)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn expire_due(&self, now: TimestampMs) -> Result<Vec<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as(&format!(
            "UPDATE approval_requests SET status = 'expired', reason = 'expired', decided_at = ? \
             WHERE status = 'pending' AND expires_at <= ? RETURNING {COLUMNS}"
        ))
        .bind(now)
        .bind(now)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn add_approver(
        &self,
        user_id: &str,
        platform_type: &str,
        platform_user_id: &str,
        at: TimestampMs,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO remote_approvers (user_id, platform_type, platform_user_id, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (user_id, platform_type, platform_user_id) DO NOTHING",
        )
        .bind(user_id)
        .bind(platform_type)
        .bind(platform_user_id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_approver(
        &self,
        user_id: &str,
        platform_type: &str,
        platform_user_id: &str,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "DELETE FROM remote_approvers WHERE user_id = ? AND platform_type = ? AND platform_user_id = ?",
        )
        .bind(user_id)
        .bind(platform_type)
        .bind(platform_user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn list_approvers(&self, user_id: &str) -> Result<Vec<RemoteApproverRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT user_id, platform_type, platform_user_id, created_at FROM remote_approvers \
             WHERE user_id = ? ORDER BY created_at, platform_type, platform_user_id",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn is_approver(&self, user_id: &str, platform_type: &str, platform_user_id: &str) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM remote_approvers WHERE user_id = ? AND platform_type = ? AND platform_user_id = ?",
        )
        .bind(user_id)
        .bind(platform_type)
        .bind(platform_user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }

    async fn ttl_minutes(&self, user_id: &str) -> Result<Option<i64>, DbError> {
        Ok(
            sqlx::query_scalar("SELECT ttl_minutes FROM remote_approval_settings WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn set_ttl_minutes(&self, user_id: &str, minutes: i64, at: TimestampMs) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO remote_approval_settings (user_id, ttl_minutes, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT (user_id) DO UPDATE SET ttl_minutes = excluded.ttl_minutes, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(minutes)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "sqlite_approval_request_tests.rs"]
mod tests;

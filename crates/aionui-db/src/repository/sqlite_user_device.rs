use aionui_common::{TimestampMs, generate_prefixed_id};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::UserDeviceRow;
use crate::repository::user_device::{CreateUserDeviceParams, IUserDeviceRepository};

const COLUMNS: &str = "id, user_id, name, platform, token_hash, scopes, push_kind, push_target, \
     last_seen_at, created_at, revoked_at, revoked_by";

#[derive(Clone, Debug)]
pub struct SqliteUserDeviceRepository {
    pool: SqlitePool,
}

impl SqliteUserDeviceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IUserDeviceRepository for SqliteUserDeviceRepository {
    async fn create_within_limit(
        &self,
        params: &CreateUserDeviceParams<'_>,
        max_active: i64,
    ) -> Result<Option<UserDeviceRow>, DbError> {
        let id = generate_prefixed_id("dev");
        let sql = format!(
            "INSERT INTO user_devices (id, user_id, name, platform, token_hash, scopes, push_kind, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, 'none', ? \
             WHERE (SELECT COUNT(*) FROM user_devices WHERE user_id = ? AND revoked_at IS NULL) < ? \
             RETURNING {COLUMNS}"
        );
        Ok(sqlx::query_as::<_, UserDeviceRow>(&sql)
            .bind(&id)
            .bind(params.user_id)
            .bind(params.name)
            .bind(params.platform)
            .bind(params.token_hash)
            .bind(params.scopes)
            .bind(params.created_at)
            .bind(params.user_id)
            .bind(max_active)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn find_active_by_token_hash(&self, token_hash: &str) -> Result<Option<UserDeviceRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM user_devices WHERE token_hash = ? AND revoked_at IS NULL");
        Ok(sqlx::query_as::<_, UserDeviceRow>(&sql)
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn find_for_user(&self, user_id: &str, id: &str) -> Result<Option<UserDeviceRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM user_devices WHERE user_id = ? AND id = ?");
        Ok(sqlx::query_as::<_, UserDeviceRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_for_user(&self, user_id: &str) -> Result<Vec<UserDeviceRow>, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM user_devices WHERE user_id = ? ORDER BY created_at DESC, id DESC");
        Ok(sqlx::query_as::<_, UserDeviceRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn count_active(&self, user_id: &str) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM user_devices WHERE user_id = ? AND revoked_at IS NULL")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn touch_last_seen(&self, id: &str, at: TimestampMs) -> Result<(), DbError> {
        sqlx::query("UPDATE user_devices SET last_seen_at = ? WHERE id = ? AND revoked_at IS NULL")
            .bind(at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn revoke(&self, user_id: &str, id: &str, at: TimestampMs, revoked_by: &str) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE user_devices SET revoked_at = ?, revoked_by = ? \
             WHERE user_id = ? AND id = ? AND revoked_at IS NULL",
        )
        .bind(at)
        .bind(revoked_by)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

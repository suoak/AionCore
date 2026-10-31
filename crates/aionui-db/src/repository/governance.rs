//! Storage for M8 E12-3 governance: `user_roles`, `agent_run_permissions`
//! and `policies` (migration 080). Rows are inserted or updated in place;
//! nothing here deletes.
use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct UserRoleRow {
    pub user_id: String,
    pub role: String,
    pub granted_by: Option<String>,
    pub granted_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct AgentRunPermissionRow {
    pub subject_kind: String,
    pub subject_id: String,
    /// JSON array of role names; `None` = unrestricted.
    pub allowed_roles: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow, serde::Serialize)]
pub struct PolicyRow {
    pub key: String,
    /// JSON value.
    pub value: String,
    pub updated_by: Option<String>,
    pub updated_at: i64,
}

#[async_trait::async_trait]
pub trait IGovernanceRepository: Send + Sync {
    async fn get_role(&self, user_id: &str) -> Result<Option<UserRoleRow>, DbError>;
    async fn list_roles(&self) -> Result<Vec<UserRoleRow>, DbError>;
    async fn upsert_role(&self, user_id: &str, role: &str, granted_by: &str) -> Result<UserRoleRow, DbError>;
    /// Users whose stored role is `owner`.
    async fn count_owners(&self) -> Result<i64, DbError>;

    async fn get_agent_run_permission(
        &self,
        subject_kind: &str,
        subject_id: &str,
    ) -> Result<Option<AgentRunPermissionRow>, DbError>;
    async fn list_agent_run_permissions(&self) -> Result<Vec<AgentRunPermissionRow>, DbError>;
    async fn upsert_agent_run_permission(
        &self,
        subject_kind: &str,
        subject_id: &str,
        allowed_roles: Option<&str>,
        updated_by: &str,
    ) -> Result<AgentRunPermissionRow, DbError>;

    async fn list_policies(&self) -> Result<Vec<PolicyRow>, DbError>;
    async fn upsert_policy(&self, key: &str, value: &str, updated_by: &str) -> Result<PolicyRow, DbError>;
}

#[derive(Clone, Debug)]
pub struct SqliteGovernanceRepository {
    pool: SqlitePool,
}

impl SqliteGovernanceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IGovernanceRepository for SqliteGovernanceRepository {
    async fn get_role(&self, user_id: &str) -> Result<Option<UserRoleRow>, DbError> {
        Ok(sqlx::query_as::<_, UserRoleRow>(
            "SELECT user_id, role, granted_by, granted_at FROM user_roles WHERE user_id = ?",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_roles(&self) -> Result<Vec<UserRoleRow>, DbError> {
        Ok(sqlx::query_as::<_, UserRoleRow>(
            "SELECT user_id, role, granted_by, granted_at FROM user_roles ORDER BY user_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn upsert_role(&self, user_id: &str, role: &str, granted_by: &str) -> Result<UserRoleRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO user_roles (user_id, role, granted_by, granted_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(user_id) DO UPDATE SET role = excluded.role, granted_by = excluded.granted_by, \
             granted_at = excluded.granted_at",
        )
        .bind(user_id)
        .bind(role)
        .bind(granted_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_role(user_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("user_roles row for {user_id}")))
    }

    async fn count_owners(&self) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_roles WHERE role = 'owner'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn get_agent_run_permission(
        &self,
        subject_kind: &str,
        subject_id: &str,
    ) -> Result<Option<AgentRunPermissionRow>, DbError> {
        Ok(sqlx::query_as::<_, AgentRunPermissionRow>(
            "SELECT subject_kind, subject_id, allowed_roles, updated_by, updated_at FROM agent_run_permissions \
             WHERE subject_kind = ? AND subject_id = ?",
        )
        .bind(subject_kind)
        .bind(subject_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_agent_run_permissions(&self) -> Result<Vec<AgentRunPermissionRow>, DbError> {
        Ok(sqlx::query_as::<_, AgentRunPermissionRow>(
            "SELECT subject_kind, subject_id, allowed_roles, updated_by, updated_at FROM agent_run_permissions \
             ORDER BY subject_kind, subject_id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn upsert_agent_run_permission(
        &self,
        subject_kind: &str,
        subject_id: &str,
        allowed_roles: Option<&str>,
        updated_by: &str,
    ) -> Result<AgentRunPermissionRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO agent_run_permissions (subject_kind, subject_id, allowed_roles, updated_by, updated_at) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT(subject_kind, subject_id) DO UPDATE SET \
             allowed_roles = excluded.allowed_roles, updated_by = excluded.updated_by, updated_at = excluded.updated_at",
        )
        .bind(subject_kind)
        .bind(subject_id)
        .bind(allowed_roles)
        .bind(updated_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_agent_run_permission(subject_kind, subject_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("agent_run_permissions row {subject_kind}/{subject_id}")))
    }

    async fn list_policies(&self) -> Result<Vec<PolicyRow>, DbError> {
        Ok(
            sqlx::query_as::<_, PolicyRow>("SELECT key, value, updated_by, updated_at FROM policies ORDER BY key")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    async fn upsert_policy(&self, key: &str, value: &str, updated_by: &str) -> Result<PolicyRow, DbError> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO policies (key, value, updated_by, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_by = excluded.updated_by, \
             updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(updated_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        let row =
            sqlx::query_as::<_, PolicyRow>("SELECT key, value, updated_by, updated_at FROM policies WHERE key = ?")
                .bind(key)
                .fetch_one(&self.pool)
                .await?;
        Ok(row)
    }
}

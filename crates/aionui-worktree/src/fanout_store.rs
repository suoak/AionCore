//! `fanout_groups` persistence (migration 066). User-facing reads are scoped
//! by `user_id`.

use aionui_db::SqlitePool;
use serde::Serialize;

use crate::error::WorktreeError;

const COLUMNS: &str = "id, user_id, task_session_id, title, source_repo_path, repo_common_dir, base_ref, \
     base_commit, prompt, max_parallel, status, winner_slot, adopt_result, created_at, updated_at";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct FanoutGroupRow {
    pub id: String,
    pub user_id: String,
    pub task_session_id: Option<String>,
    pub title: String,
    pub source_repo_path: String,
    pub repo_common_dir: String,
    pub base_ref: String,
    pub base_commit: String,
    pub prompt: String,
    pub max_parallel: i64,
    pub status: String,
    pub winner_slot: Option<String>,
    pub adopt_result: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct NewFanoutGroup {
    pub id: String,
    pub user_id: String,
    pub task_session_id: Option<String>,
    pub title: String,
    pub source_repo_path: String,
    pub repo_common_dir: String,
    pub base_ref: String,
    pub base_commit: String,
    pub prompt: String,
    pub max_parallel: i64,
    pub now: i64,
}

#[derive(Clone)]
pub struct FanoutStore {
    pool: SqlitePool,
}

fn map_err(err: sqlx::Error) -> WorktreeError {
    let text = err.to_string();
    if text.contains("FOREIGN KEY constraint failed") {
        WorktreeError::BadRequest("unknown user or task_session_id".into())
    } else {
        WorktreeError::Database(text)
    }
}

impl FanoutStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, new: &NewFanoutGroup) -> Result<FanoutGroupRow, WorktreeError> {
        sqlx::query(
            "INSERT INTO fanout_groups (id, user_id, task_session_id, title, source_repo_path, repo_common_dir, \
             base_ref, base_commit, prompt, max_parallel, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'preparing', ?, ?)",
        )
        .bind(&new.id)
        .bind(&new.user_id)
        .bind(&new.task_session_id)
        .bind(&new.title)
        .bind(&new.source_repo_path)
        .bind(&new.repo_common_dir)
        .bind(&new.base_ref)
        .bind(&new.base_commit)
        .bind(&new.prompt)
        .bind(new.max_parallel)
        .bind(new.now)
        .bind(new.now)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        self.get_any(&new.id)
            .await?
            .ok_or_else(|| WorktreeError::Database("inserted fan-out group vanished".into()))
    }

    pub async fn get_any(&self, id: &str) -> Result<Option<FanoutGroupRow>, WorktreeError> {
        sqlx::query_as::<_, FanoutGroupRow>(&format!("SELECT {COLUMNS} FROM fanout_groups WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<FanoutGroupRow, WorktreeError> {
        sqlx::query_as::<_, FanoutGroupRow>(&format!(
            "SELECT {COLUMNS} FROM fanout_groups WHERE id = ? AND user_id = ?"
        ))
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?
        .ok_or_else(|| WorktreeError::FanoutNotFound(id.to_owned()))
    }

    pub async fn list(
        &self,
        user_id: &str,
        task_session_id: Option<&str>,
    ) -> Result<Vec<FanoutGroupRow>, WorktreeError> {
        let mut sql = format!("SELECT {COLUMNS} FROM fanout_groups WHERE user_id = ?");
        if task_session_id.is_some() {
            sql.push_str(" AND task_session_id = ?");
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT 200");
        let mut query = sqlx::query_as::<_, FanoutGroupRow>(&sql).bind(user_id);
        if let Some(ts) = task_session_id {
            query = query.bind(ts);
        }
        query.fetch_all(&self.pool).await.map_err(map_err)
    }

    /// Groups in the given statuses across all users (boot recovery).
    pub async fn list_any_in_status(&self, statuses: &[&str]) -> Result<Vec<FanoutGroupRow>, WorktreeError> {
        let placeholders = vec!["?"; statuses.len()].join(", ");
        let sql = format!("SELECT {COLUMNS} FROM fanout_groups WHERE status IN ({placeholders})");
        let mut query = sqlx::query_as::<_, FanoutGroupRow>(&sql);
        for status in statuses {
            query = query.bind(*status);
        }
        query.fetch_all(&self.pool).await.map_err(map_err)
    }

    /// Compare-and-set the group status. Returns whether a row changed.
    pub async fn set_status(&self, id: &str, from: &[&str], status: &str, now: i64) -> Result<bool, WorktreeError> {
        let sql = format!(
            "UPDATE fanout_groups SET status = ?, updated_at = ? WHERE id = ? AND status IN ({})",
            vec!["?"; from.len()].join(", ")
        );
        let mut query = sqlx::query(&sql).bind(status).bind(now).bind(id);
        for f in from {
            query = query.bind(*f);
        }
        Ok(query.execute(&self.pool).await.map_err(map_err)?.rows_affected() > 0)
    }

    /// Record the adoption (from `from`, compare-and-set).
    pub async fn set_adopted(
        &self,
        id: &str,
        from: &[&str],
        winner_slot: &str,
        adopt_result: &str,
        now: i64,
    ) -> Result<bool, WorktreeError> {
        let sql = format!(
            "UPDATE fanout_groups SET status = 'adopted', winner_slot = ?, adopt_result = ?, updated_at = ? \
             WHERE id = ? AND status IN ({})",
            vec!["?"; from.len()].join(", ")
        );
        let mut query = sqlx::query(&sql)
            .bind(winner_slot)
            .bind(adopt_result)
            .bind(now)
            .bind(id);
        for f in from {
            query = query.bind(*f);
        }
        Ok(query.execute(&self.pool).await.map_err(map_err)?.rows_affected() > 0)
    }
}

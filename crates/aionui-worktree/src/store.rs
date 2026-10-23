//! `task_worktrees` persistence (migration 066). Every user-facing query is
//! scoped by `user_id`; only boot recovery reads across users.

use aionui_db::SqlitePool;
use serde::Serialize;

use crate::error::WorktreeError;

pub const COLUMNS: &str = "id, user_id, fanout_group_id, slot, slot_spec, task_session_id, conversation_id, \
     source_repo_path, repo_common_dir, path, branch, base_ref, base_commit, status, phase, error_code, \
     error_message, size_bytes, branch_preserved, created_at, updated_at, removed_at";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct WorktreeRow {
    pub id: String,
    pub user_id: String,
    pub fanout_group_id: Option<String>,
    pub slot: Option<String>,
    pub slot_spec: String,
    pub task_session_id: Option<String>,
    pub conversation_id: Option<String>,
    pub source_repo_path: String,
    pub repo_common_dir: String,
    pub path: String,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
    pub status: String,
    pub phase: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub size_bytes: Option<i64>,
    pub branch_preserved: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub removed_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct NewWorktree {
    pub id: String,
    pub user_id: String,
    pub source_repo_path: String,
    pub repo_common_dir: String,
    pub path: String,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
    pub task_session_id: Option<String>,
    /// E3-4: set for a fan-out slot worktree.
    pub fanout_group_id: Option<String>,
    pub slot: Option<String>,
    /// JSON slot spec (`{}` for a standalone worktree).
    pub slot_spec: String,
    pub now: i64,
}

#[derive(Clone)]
pub struct WorktreeStore {
    pool: SqlitePool,
}

fn map_err(err: sqlx::Error) -> WorktreeError {
    let text = err.to_string();
    if aionui_db::message_indicates_unique_violation(&text) {
        WorktreeError::PathConflict(text)
    } else if text.contains("FOREIGN KEY constraint failed") {
        WorktreeError::BadRequest("unknown user or task_session_id".into())
    } else {
        WorktreeError::Database(text)
    }
}

impl WorktreeStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, new: &NewWorktree) -> Result<WorktreeRow, WorktreeError> {
        sqlx::query(
            "INSERT INTO task_worktrees (id, user_id, task_session_id, fanout_group_id, slot, slot_spec, \
             source_repo_path, repo_common_dir, path, branch, base_ref, base_commit, status, phase, created_at, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'creating', 'add', ?, ?)",
        )
        .bind(&new.id)
        .bind(&new.user_id)
        .bind(&new.task_session_id)
        .bind(&new.fanout_group_id)
        .bind(&new.slot)
        .bind(&new.slot_spec)
        .bind(&new.source_repo_path)
        .bind(&new.repo_common_dir)
        .bind(&new.path)
        .bind(&new.branch)
        .bind(&new.base_ref)
        .bind(&new.base_commit)
        .bind(new.now)
        .bind(new.now)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        self.get_any(&new.id)
            .await?
            .ok_or_else(|| WorktreeError::Database("inserted worktree row vanished".into()))
    }

    /// Fetch by id regardless of owner (internal use only).
    pub async fn get_any(&self, id: &str) -> Result<Option<WorktreeRow>, WorktreeError> {
        sqlx::query_as::<_, WorktreeRow>(&format!("SELECT {COLUMNS} FROM task_worktrees WHERE id = ?"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<WorktreeRow, WorktreeError> {
        sqlx::query_as::<_, WorktreeRow>(&format!(
            "SELECT {COLUMNS} FROM task_worktrees WHERE id = ? AND user_id = ?"
        ))
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)?
        .ok_or_else(|| WorktreeError::NotFound(id.to_owned()))
    }

    pub async fn list(
        &self,
        user_id: &str,
        repo_path: Option<&str>,
        status: Option<&str>,
        include_removed: bool,
    ) -> Result<Vec<WorktreeRow>, WorktreeError> {
        let mut sql = format!("SELECT {COLUMNS} FROM task_worktrees WHERE user_id = ?");
        if repo_path.is_some() {
            sql.push_str(" AND source_repo_path = ?");
        }
        if status.is_some() {
            sql.push_str(" AND status = ?");
        } else if !include_removed {
            sql.push_str(" AND status <> 'removed'");
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC");
        let mut query = sqlx::query_as::<_, WorktreeRow>(&sql).bind(user_id);
        if let Some(repo) = repo_path {
            query = query.bind(repo);
        }
        if let Some(status) = status {
            query = query.bind(status);
        }
        query.fetch_all(&self.pool).await.map_err(map_err)
    }

    /// Rows in the given statuses across all users (boot recovery).
    pub async fn list_any_in_status(&self, statuses: &[&str]) -> Result<Vec<WorktreeRow>, WorktreeError> {
        if statuses.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; statuses.len()].join(", ");
        let sql = format!("SELECT {COLUMNS} FROM task_worktrees WHERE status IN ({placeholders}) ORDER BY created_at");
        let mut query = sqlx::query_as::<_, WorktreeRow>(&sql);
        for status in statuses {
            query = query.bind(*status);
        }
        query.fetch_all(&self.pool).await.map_err(map_err)
    }

    /// Transition `id` to `status`, optionally only if it is currently in
    /// one of `from` (compare-and-set). Returns whether a row changed.
    pub async fn set_status(
        &self,
        id: &str,
        from: &[&str],
        status: &str,
        phase: Option<&str>,
        error: Option<(&str, &str)>,
        now: i64,
    ) -> Result<bool, WorktreeError> {
        let mut sql = String::from(
            "UPDATE task_worktrees SET status = ?, phase = ?, error_code = ?, error_message = ?, updated_at = ? \
             WHERE id = ?",
        );
        if !from.is_empty() {
            sql.push_str(&format!(" AND status IN ({})", vec!["?"; from.len()].join(", ")));
        }
        let mut query = sqlx::query(&sql)
            .bind(status)
            .bind(phase)
            .bind(error.map(|e| e.0))
            .bind(error.map(|e| e.1))
            .bind(now)
            .bind(id);
        for f in from {
            query = query.bind(*f);
        }
        let result = query.execute(&self.pool).await.map_err(map_err)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn set_phase(&self, id: &str, phase: &str, now: i64) -> Result<(), WorktreeError> {
        sqlx::query("UPDATE task_worktrees SET phase = ?, updated_at = ? WHERE id = ? AND status = 'creating'")
            .bind(phase)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    pub async fn mark_removed(&self, id: &str, branch_preserved: bool, now: i64) -> Result<(), WorktreeError> {
        sqlx::query(
            "UPDATE task_worktrees SET status = 'removed', phase = NULL, error_code = NULL, error_message = NULL, \
             branch_preserved = ?, removed_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(i64::from(branch_preserved))
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(())
    }

    pub async fn set_size(&self, id: &str, size_bytes: Option<i64>) -> Result<(), WorktreeError> {
        sqlx::query("UPDATE task_worktrees SET size_bytes = ? WHERE id = ?")
            .bind(size_bytes)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Conversation binding (E3-2)
    // ------------------------------------------------------------------

    /// Attach `conversation_id` to a bindable worktree (compare-and-set): the
    /// row must belong to `user_id`, be `ready` or `unbound`, and hold no
    /// conversation. Binding moves an `unbound` worktree back to `ready`.
    pub async fn bind_conversation(
        &self,
        user_id: &str,
        id: &str,
        conversation_id: &str,
        now: i64,
    ) -> Result<bool, WorktreeError> {
        let result = sqlx::query(
            "UPDATE task_worktrees SET conversation_id = ?, status = 'ready', error_code = NULL, \
             error_message = NULL, updated_at = ? \
             WHERE id = ? AND user_id = ? AND conversation_id IS NULL AND status IN ('ready', 'unbound')",
        )
        .bind(conversation_id)
        .bind(now)
        .bind(id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(result.rows_affected() > 0)
    }

    /// Live worktrees bound to `conversation_id`.
    pub async fn list_by_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Vec<WorktreeRow>, WorktreeError> {
        sqlx::query_as::<_, WorktreeRow>(&format!(
            "SELECT {COLUMNS} FROM task_worktrees WHERE user_id = ? AND conversation_id = ? AND status <> 'removed'"
        ))
        .bind(user_id)
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)
    }

    /// Whether the conversation row still exists. Used to tell an `unbound`
    /// worktree whose conversation delete was rolled back from one whose
    /// delete is merely still in flight.
    pub async fn conversation_exists(&self, conversation_id: &str) -> Result<bool, WorktreeError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM conversations WHERE id = ?")
            .bind(conversation_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(found.is_some())
    }

    /// The live managed worktree at exactly `path`, if any (any owner).
    pub async fn find_live_by_path(&self, path: &str) -> Result<Option<WorktreeRow>, WorktreeError> {
        sqlx::query_as::<_, WorktreeRow>(&format!(
            "SELECT {COLUMNS} FROM task_worktrees WHERE path = ? AND status NOT IN ('removed', 'failed') \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_err)
    }

    // ------------------------------------------------------------------
    // Fan-out (E3-4)
    // ------------------------------------------------------------------

    /// Every worktree of a fan-out group (any status), in slot order.
    pub async fn list_by_group(&self, group_id: &str) -> Result<Vec<WorktreeRow>, WorktreeError> {
        sqlx::query_as::<_, WorktreeRow>(&format!(
            "SELECT {COLUMNS} FROM task_worktrees WHERE fanout_group_id = ? ORDER BY slot, created_at"
        ))
        .bind(group_id)
        .fetch_all(&self.pool)
        .await
        .map_err(map_err)
    }

    /// Release a slot worktree from its conversation so it can be removed:
    /// `conversation_id` is cleared and a `ready` row becomes `unbound`.
    pub async fn detach_conversation(&self, id: &str, now: i64) -> Result<bool, WorktreeError> {
        let result = sqlx::query(
            "UPDATE task_worktrees SET conversation_id = NULL, status = 'unbound', updated_at = ? \
             WHERE id = ? AND status IN ('ready', 'unbound')",
        )
        .bind(now)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(map_err)?;
        Ok(result.rows_affected() > 0)
    }

    /// Record a non-fatal slot error on a live worktree (status unchanged).
    pub async fn set_error(&self, id: &str, code: &str, message: &str, now: i64) -> Result<(), WorktreeError> {
        sqlx::query("UPDATE task_worktrees SET error_code = ?, error_message = ?, updated_at = ? WHERE id = ?")
            .bind(code)
            .bind(message)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(map_err)?;
        Ok(())
    }
}

use aionui_common::TimestampMs;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{TaskAcceptanceCriterionRow, TaskApprovalRow, TaskArtifactRow, TaskRunRow, TaskSessionRow};
use crate::repository::task_session::{
    CreateTaskArtifactParams, CreateTaskRunParams, CreateTaskSessionParams, FinishTaskRunParams,
    ITaskSessionRepository, ResolveTaskApprovalParams, UpdateAcceptanceCriterionParams, UpdateTaskSessionParams,
};

#[derive(Clone, Debug)]
pub struct SqliteTaskSessionRepository {
    pool: SqlitePool,
}

impl SqliteTaskSessionRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ITaskSessionRepository for SqliteTaskSessionRepository {
    async fn list(&self, user_id: &str, conversation_id: Option<&str>) -> Result<Vec<TaskSessionRow>, DbError> {
        let rows = if let Some(conversation_id) = conversation_id {
            sqlx::query_as::<_, TaskSessionRow>(
                "SELECT * FROM task_sessions WHERE user_id = ? AND conversation_id = ? ORDER BY updated_at DESC",
            )
            .bind(user_id)
            .bind(conversation_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, TaskSessionRow>(
                "SELECT * FROM task_sessions WHERE user_id = ? ORDER BY updated_at DESC",
            )
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn get(&self, user_id: &str, id: &str) -> Result<Option<TaskSessionRow>, DbError> {
        Ok(
            sqlx::query_as::<_, TaskSessionRow>("SELECT * FROM task_sessions WHERE user_id = ? AND id = ?")
                .bind(user_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn create(&self, params: &CreateTaskSessionParams<'_>) -> Result<TaskSessionRow, DbError> {
        let id = aionui_common::generate_prefixed_id("task");
        let now = aionui_common::now_ms();
        sqlx::query(
            "INSERT INTO task_sessions (id, user_id, title, project_id, conversation_id, mode, objective, \
             acceptance_criteria, status, agent_type, agent_session_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.user_id)
        .bind(params.title)
        .bind(params.project_id)
        .bind(params.conversation_id)
        .bind(params.mode)
        .bind(params.objective)
        .bind(params.acceptance_criteria)
        .bind(params.status)
        .bind(params.agent_type)
        .bind(params.agent_session_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;

        self.get(params.user_id, &id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Task session '{id}' was not persisted")))
    }

    async fn update(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateTaskSessionParams<'_>,
    ) -> Result<TaskSessionRow, DbError> {
        let existing = self
            .get(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Task session '{id}' not found")))?;
        let updated_at = aionui_common::now_ms();
        sqlx::query(
            "UPDATE task_sessions SET title = ?, project_id = ?, conversation_id = ?, mode = ?, objective = ?, \
             acceptance_criteria = ?, status = ?, agent_type = ?, agent_session_id = ?, updated_at = ? \
             WHERE user_id = ? AND id = ?",
        )
        .bind(params.title.unwrap_or(&existing.title))
        .bind(params.project_id.or(existing.project_id.as_deref()))
        .bind(params.conversation_id.or(existing.conversation_id.as_deref()))
        .bind(params.mode.unwrap_or(&existing.mode))
        .bind(params.objective.unwrap_or(&existing.objective))
        .bind(params.acceptance_criteria.unwrap_or(&existing.acceptance_criteria))
        .bind(params.status.unwrap_or(&existing.status))
        .bind(params.agent_type.unwrap_or(&existing.agent_type))
        .bind(params.agent_session_id.or(existing.agent_session_id.as_deref()))
        .bind(updated_at)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;

        self.get(user_id, id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Task session '{id}' not found after update")))
    }

    async fn pause_incomplete(&self, updated_at: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE task_sessions SET status = 'paused', updated_at = ? \
             WHERE status IN ('running', 'waiting_approval')",
        )
        .bind(updated_at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn create_artifact_with_approval(
        &self,
        params: &CreateTaskArtifactParams<'_>,
    ) -> Result<(TaskArtifactRow, TaskApprovalRow, Vec<TaskAcceptanceCriterionRow>), DbError> {
        let mut tx = self.pool.begin().await?;
        let owned: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM task_sessions WHERE id = ? AND user_id = ?")
            .bind(params.task_session_id)
            .bind(params.user_id)
            .fetch_one(&mut *tx)
            .await?;
        if !owned {
            return Err(DbError::NotFound(format!(
                "Task session '{}' not found",
                params.task_session_id
            )));
        }

        let now = aionui_common::now_ms();
        sqlx::query(
            "UPDATE task_approvals SET \
                 status = CASE WHEN status = 'pending' THEN 'cancelled' ELSE 'expired' END, resolved_at = ? \
             WHERE task_session_id = ? AND (approval_type = ? OR (? = 'goal' AND approval_type = 'plan')) \
             AND (status = 'pending' OR (status = 'approved' AND run_id IS NULL))",
        )
        .bind(now)
        .bind(params.task_session_id)
        .bind(params.kind)
        .bind(params.kind)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE task_artifacts SET status = 'superseded', updated_at = ? \
             WHERE task_session_id = ? AND (kind = ? OR (? = 'goal' AND kind = 'plan')) \
             AND (status = 'submitted' OR (status = 'approved' AND id IN (\
                 SELECT artifact_id FROM task_approvals WHERE task_session_id = ? AND status = 'expired'\
             )))",
        )
        .bind(now)
        .bind(params.task_session_id)
        .bind(params.kind)
        .bind(params.kind)
        .bind(params.task_session_id)
        .execute(&mut *tx)
        .await?;

        let version: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM task_artifacts WHERE task_session_id = ? AND kind = ?",
        )
        .bind(params.task_session_id)
        .bind(params.kind)
        .fetch_one(&mut *tx)
        .await?;
        let artifact_id = aionui_common::generate_prefixed_id("artifact");
        sqlx::query(
            "INSERT INTO task_artifacts \
             (id, task_session_id, kind, version, content, content_hash, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'submitted', ?, ?)",
        )
        .bind(&artifact_id)
        .bind(params.task_session_id)
        .bind(params.kind)
        .bind(version)
        .bind(params.content)
        .bind(params.content_hash)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let approval_id = aionui_common::generate_prefixed_id("approval");
        sqlx::query(
            "INSERT INTO task_approvals \
             (id, task_session_id, approval_type, artifact_id, artifact_hash, status, requested_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', ?)",
        )
        .bind(&approval_id)
        .bind(params.task_session_id)
        .bind(params.kind)
        .bind(&artifact_id)
        .bind(params.content_hash)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        for (position, description) in params.acceptance_criteria.iter().enumerate() {
            sqlx::query(
                "INSERT INTO task_acceptance_criteria \
                 (id, task_session_id, goal_artifact_id, position, description, status, evidence) \
                 VALUES (?, ?, ?, ?, ?, 'pending', '[]')",
            )
            .bind(aionui_common::generate_prefixed_id("criterion"))
            .bind(params.task_session_id)
            .bind(&artifact_id)
            .bind(position as i64)
            .bind(description)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "UPDATE task_sessions SET status = 'waiting_approval', updated_at = ? WHERE id = ? AND user_id = ?",
        )
        .bind(now)
        .bind(params.task_session_id)
        .bind(params.user_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let artifact = self
            .get_artifact(params.user_id, params.task_session_id, &artifact_id)
            .await?
            .ok_or_else(|| DbError::NotFound("Created task artifact is missing".into()))?;
        let approval = self
            .get_approval(params.user_id, params.task_session_id, &approval_id)
            .await?
            .ok_or_else(|| DbError::NotFound("Created task approval is missing".into()))?;
        let criteria = self
            .list_acceptance_criteria(params.user_id, params.task_session_id)
            .await?
            .into_iter()
            .filter(|criterion| criterion.goal_artifact_id == artifact_id)
            .collect();
        Ok((artifact, approval, criteria))
    }

    async fn list_artifacts(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskArtifactRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskArtifactRow>(
            "SELECT artifact.* FROM task_artifacts artifact \
             JOIN task_sessions task ON task.id = artifact.task_session_id \
             WHERE task.user_id = ? AND task.id = ? ORDER BY artifact.created_at DESC",
        )
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get_artifact(
        &self,
        user_id: &str,
        task_session_id: &str,
        artifact_id: &str,
    ) -> Result<Option<TaskArtifactRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskArtifactRow>(
            "SELECT artifact.* FROM task_artifacts artifact \
             JOIN task_sessions task ON task.id = artifact.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND artifact.id = ?",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn list_approvals(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskApprovalRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskApprovalRow>(
            "SELECT approval.* FROM task_approvals approval \
             JOIN task_sessions task ON task.id = approval.task_session_id \
             WHERE task.user_id = ? AND task.id = ? ORDER BY approval.requested_at DESC",
        )
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn get_approval(
        &self,
        user_id: &str,
        task_session_id: &str,
        approval_id: &str,
    ) -> Result<Option<TaskApprovalRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskApprovalRow>(
            "SELECT approval.* FROM task_approvals approval \
             JOIN task_sessions task ON task.id = approval.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND approval.id = ?",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(approval_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn resolve_approval(&self, params: &ResolveTaskApprovalParams<'_>) -> Result<TaskApprovalRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE task_approvals SET status = ?, resolved_at = ?, resolved_by = ?, comment = ? \
             WHERE id = ? AND task_session_id = ? AND artifact_id = ? AND artifact_hash = ? \
             AND status = 'pending' AND task_session_id IN \
             (SELECT id FROM task_sessions WHERE user_id = ?)",
        )
        .bind(params.status)
        .bind(params.resolved_at)
        .bind(params.resolved_by)
        .bind(params.comment)
        .bind(params.approval_id)
        .bind(params.task_session_id)
        .bind(params.artifact_id)
        .bind(params.artifact_hash)
        .bind(params.user_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::Conflict(
                "Approval is not pending or does not match the task artifact".into(),
            ));
        }
        let artifact_status = if params.status == "approved" {
            "approved"
        } else {
            "rejected"
        };
        let task_status = if params.status == "approved" { "ready" } else { "paused" };
        sqlx::query(
            "UPDATE task_artifacts SET status = ?, updated_at = ? \
             WHERE id = ? AND task_session_id = ? AND content_hash = ?",
        )
        .bind(artifact_status)
        .bind(params.resolved_at)
        .bind(params.artifact_id)
        .bind(params.task_session_id)
        .bind(params.artifact_hash)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE task_sessions SET status = ?, updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(task_status)
            .bind(params.resolved_at)
            .bind(params.task_session_id)
            .bind(params.user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.get_approval(params.user_id, params.task_session_id, params.approval_id)
            .await?
            .ok_or_else(|| DbError::NotFound("Resolved task approval is missing".into()))
    }

    async fn create_run(&self, params: &CreateTaskRunParams<'_>) -> Result<TaskRunRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let run_id = aionui_common::generate_prefixed_id("run");
        // The authorization check and claim are one conditional write. This is
        // the first statement in the transaction, so concurrent executors
        // serialize on SQLite's writer lock and only one can change run_id from
        // NULL. No process-local mutex is involved.
        let claim = sqlx::query(
            "UPDATE task_approvals SET run_id = ? \
             WHERE id = ? AND task_session_id = ? AND status = 'approved' AND run_id IS NULL \
             AND artifact_id = ? AND artifact_hash = ? \
             AND task_session_id IN (\
                 SELECT task.id FROM task_sessions task \
                 JOIN task_artifacts artifact ON artifact.task_session_id = task.id \
                 WHERE task.user_id = ? AND task.id = ? AND task.conversation_id = ? \
                 AND artifact.id = ? AND artifact.content_hash = ? AND artifact.status = 'approved'\
             )",
        )
        .bind(&run_id)
        .bind(params.approval_id)
        .bind(params.task_session_id)
        .bind(params.plan_artifact_id)
        .bind(params.artifact_hash)
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.conversation_id)
        .bind(params.plan_artifact_id)
        .bind(params.artifact_hash)
        .execute(&mut *tx)
        .await?;
        if claim.rows_affected() != 1 {
            return Err(DbError::Conflict(
                "Execution requires an approved artifact matching this task and hash".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO task_runs \
             (id, task_session_id, conversation_id, plan_artifact_id, goal_artifact_id, approval_id, status, started_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'running', ?)",
        )
        .bind(&run_id)
        .bind(params.task_session_id)
        .bind(params.conversation_id)
        .bind(params.plan_artifact_id)
        .bind(params.goal_artifact_id)
        .bind(params.approval_id)
        .bind(params.started_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE task_sessions SET status = 'running', updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(params.started_at)
            .bind(params.task_session_id)
            .bind(params.user_id)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as::<_, TaskRunRow>("SELECT * FROM task_runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn finish_run(&self, params: &FinishTaskRunParams<'_>) -> Result<TaskRunRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "UPDATE task_runs SET status = ?, finished_at = ?, error_message = ? \
             WHERE id = ? AND task_session_id = ? AND status = 'running' \
             AND task_session_id IN (SELECT id FROM task_sessions WHERE user_id = ?)",
        )
        .bind(params.run_status)
        .bind(params.finished_at)
        .bind(params.error_message)
        .bind(params.run_id)
        .bind(params.task_session_id)
        .bind(params.user_id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::Conflict("Task run is no longer running".into()));
        }
        sqlx::query("UPDATE task_sessions SET status = ?, updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(params.task_status)
            .bind(params.finished_at)
            .bind(params.task_session_id)
            .bind(params.user_id)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query_as::<_, TaskRunRow>("SELECT * FROM task_runs WHERE id = ?")
            .bind(params.run_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn list_runs(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskRunRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskRunRow>(
            "SELECT task_run.* FROM task_runs task_run JOIN task_sessions task ON task.id = task_run.task_session_id \
             WHERE task.user_id = ? AND task.id = ? ORDER BY task_run.started_at DESC",
        )
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_acceptance_criteria(
        &self,
        user_id: &str,
        task_session_id: &str,
    ) -> Result<Vec<TaskAcceptanceCriterionRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskAcceptanceCriterionRow>(
            "SELECT criterion.* FROM task_acceptance_criteria criterion \
             JOIN task_sessions task ON task.id = criterion.task_session_id \
             WHERE task.user_id = ? AND task.id = ? ORDER BY criterion.position",
        )
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update_acceptance_criterion(
        &self,
        params: &UpdateAcceptanceCriterionParams<'_>,
    ) -> Result<TaskAcceptanceCriterionRow, DbError> {
        let result = sqlx::query(
            "UPDATE task_acceptance_criteria SET status = ?, evidence = ?, verified_at = ? \
             WHERE id = ? AND task_session_id = ? AND task_session_id IN \
             (SELECT id FROM task_sessions WHERE user_id = ? AND mode = 'goal' AND status = 'paused') \
             AND goal_artifact_id IN (SELECT id FROM task_artifacts WHERE status = 'approved') \
             AND goal_artifact_id IN (SELECT goal_artifact_id FROM task_runs WHERE status = 'completed')",
        )
        .bind(params.status)
        .bind(params.evidence)
        .bind(params.verified_at)
        .bind(params.criterion_id)
        .bind(params.task_session_id)
        .bind(params.user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!(
                "Acceptance criterion '{}' not found",
                params.criterion_id
            )));
        }
        Ok(
            sqlx::query_as::<_, TaskAcceptanceCriterionRow>("SELECT * FROM task_acceptance_criteria WHERE id = ?")
                .bind(params.criterion_id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn pause_incomplete_runs(&self, updated_at: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE task_runs SET status = 'paused', finished_at = ? WHERE status = 'running'")
            .bind(updated_at)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::task_session::{
        CreateTaskArtifactParams, CreateTaskRunParams, CreateTaskSessionParams, ResolveTaskApprovalParams,
        UpdateTaskSessionParams,
    };
    use crate::{ITaskSessionRepository, init_database_memory, init_database_staged};

    const USER: &str = "system_default_user";

    fn create_params(status: &str) -> CreateTaskSessionParams<'_> {
        CreateTaskSessionParams {
            user_id: USER,
            title: "Review release",
            project_id: None,
            conversation_id: None,
            mode: "plan",
            objective: "Prepare a safe release",
            acceptance_criteria: "[\"tests pass\"]",
            status,
            agent_type: "codex",
            agent_session_id: None,
        }
    }

    async fn init_concurrent_database() -> (tempfile::TempDir, crate::Database) {
        let directory = tempfile::tempdir().unwrap();
        let database = init_database_staged(&directory.path().join("task-session-concurrency.db"))
            .await
            .unwrap();
        (directory, database)
    }

    #[tokio::test]
    async fn persists_updates_and_reloads_task_session() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let created = repo.create(&create_params("draft")).await.unwrap();
        repo.update(
            USER,
            &created.id,
            &UpdateTaskSessionParams {
                status: Some("ready"),
                agent_session_id: Some("agent-session-1"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let reloaded = repo.get(USER, &created.id).await.unwrap().unwrap();
        assert_eq!(reloaded.status, "ready");
        assert_eq!(reloaded.agent_session_id.as_deref(), Some("agent-session-1"));
    }

    #[tokio::test]
    async fn startup_recovery_pauses_only_incomplete_execution() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let running = repo.create(&create_params("running")).await.unwrap();
        let ready = repo.create(&create_params("ready")).await.unwrap();

        assert_eq!(repo.pause_incomplete(42).await.unwrap(), 1);
        assert_eq!(repo.get(USER, &running.id).await.unwrap().unwrap().status, "paused");
        assert_eq!(repo.get(USER, &ready.id).await.unwrap().unwrap().status, "ready");
    }

    #[tokio::test]
    async fn task_sessions_are_scoped_to_their_owner() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let created = repo.create(&create_params("draft")).await.unwrap();

        assert!(repo.get("other-user", &created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_filters_by_conversation_and_relations_clear_on_delete() {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO projects (project_id, name, kind, created_at, updated_at) \
             VALUES ('project-1', 'Project', 'standard', 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conversation-1', ?, 'Task conversation', 'acp', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let mut params = create_params("ready");
        params.project_id = Some("project-1");
        params.conversation_id = Some("conversation-1");
        let task = repo.create(&params).await.unwrap();

        let listed = repo.list(USER, Some("conversation-1")).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, task.id);
        assert_eq!(listed[0].project_id.as_deref(), Some("project-1"));

        sqlx::query("DELETE FROM conversations WHERE id = 'conversation-1'")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("DELETE FROM projects WHERE project_id = 'project-1'")
            .execute(db.pool())
            .await
            .unwrap();
        let reloaded = repo.get(USER, &task.id).await.unwrap().unwrap();
        assert!(reloaded.conversation_id.is_none());
        assert!(reloaded.project_id.is_none());
    }

    #[tokio::test]
    async fn artifact_approval_is_versioned_hash_bound_and_single_resolution() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let task = repo.create(&create_params("ready")).await.unwrap();
        let (artifact, approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "Inspect, test, then release.",
                content_hash: "hash-v1",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();

        let mismatch = repo
            .resolve_approval(&ResolveTaskApprovalParams {
                user_id: USER,
                task_session_id: &task.id,
                approval_id: &approval.id,
                artifact_id: &artifact.id,
                artifact_hash: "different-hash",
                status: "approved",
                resolved_by: USER,
                comment: None,
                resolved_at: 10,
            })
            .await;
        assert!(matches!(mismatch, Err(DbError::Conflict(_))));

        let approved = repo
            .resolve_approval(&ResolveTaskApprovalParams {
                user_id: USER,
                task_session_id: &task.id,
                approval_id: &approval.id,
                artifact_id: &artifact.id,
                artifact_hash: &artifact.content_hash,
                status: "approved",
                resolved_by: USER,
                comment: Some("reviewed"),
                resolved_at: 11,
            })
            .await
            .unwrap();
        assert_eq!(approved.status, "approved");
        assert_eq!(
            repo.get_artifact(USER, &task.id, &artifact.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "approved",
        );

        let duplicate = repo
            .resolve_approval(&ResolveTaskApprovalParams {
                user_id: USER,
                task_session_id: &task.id,
                approval_id: &approval.id,
                artifact_id: &artifact.id,
                artifact_hash: &artifact.content_hash,
                status: "approved",
                resolved_by: USER,
                comment: None,
                resolved_at: 12,
            })
            .await;
        assert!(matches!(duplicate, Err(DbError::Conflict(_))));
    }

    #[tokio::test]
    async fn new_artifact_version_cancels_pending_approval_and_supersedes_submission() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let task = repo.create(&create_params("ready")).await.unwrap();
        let (first, first_approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "v1",
                content_hash: "hash-v1",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();
        let (second, _, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "v2",
                content_hash: "hash-v2",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();

        assert_eq!(second.version, 2);
        assert_eq!(
            repo.get_artifact(USER, &task.id, &first.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "superseded",
        );
        assert_eq!(
            repo.get_approval(USER, &task.id, &first_approval.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "cancelled",
        );
    }

    #[tokio::test]
    async fn goal_artifact_creates_ordered_acceptance_criteria() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let mut params = create_params("ready");
        params.mode = "goal";
        let task = repo.create(&params).await.unwrap();
        let criteria = vec!["tests pass".to_owned(), "release notes exist".to_owned()];
        let (artifact, _, created) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "goal",
                content: "Ship safely",
                content_hash: "goal-hash",
                acceptance_criteria: &criteria,
            })
            .await
            .unwrap();

        assert_eq!(created.len(), 2);
        assert!(
            created
                .iter()
                .all(|criterion| criterion.goal_artifact_id == artifact.id)
        );
        assert_eq!(created[0].description, "tests pass");
        assert_eq!(created[1].position, 1);
    }

    #[tokio::test]
    async fn execution_requires_the_matching_unconsumed_approval_and_recovery_does_not_replay() {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conversation-1', ?, 'Plan test', 'acp', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let mut params = create_params("ready");
        params.conversation_id = Some("conversation-1");
        let task = repo.create(&params).await.unwrap();
        let (artifact, approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "approved content",
                content_hash: "approved-hash",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();
        repo.resolve_approval(&ResolveTaskApprovalParams {
            user_id: USER,
            task_session_id: &task.id,
            approval_id: &approval.id,
            artifact_id: &artifact.id,
            artifact_hash: &artifact.content_hash,
            status: "approved",
            resolved_by: USER,
            comment: None,
            resolved_at: 2,
        })
        .await
        .unwrap();

        let wrong_hash = repo
            .create_run(&CreateTaskRunParams {
                user_id: USER,
                task_session_id: &task.id,
                conversation_id: "conversation-1",
                plan_artifact_id: &artifact.id,
                goal_artifact_id: None,
                approval_id: &approval.id,
                artifact_hash: "wrong-hash",
                started_at: 3,
            })
            .await;
        assert!(matches!(wrong_hash, Err(DbError::Conflict(_))));

        let run = repo
            .create_run(&CreateTaskRunParams {
                user_id: USER,
                task_session_id: &task.id,
                conversation_id: "conversation-1",
                plan_artifact_id: &artifact.id,
                goal_artifact_id: None,
                approval_id: &approval.id,
                artifact_hash: &artifact.content_hash,
                started_at: 4,
            })
            .await
            .unwrap();
        let duplicate = repo
            .create_run(&CreateTaskRunParams {
                user_id: USER,
                task_session_id: &task.id,
                conversation_id: "conversation-1",
                plan_artifact_id: &artifact.id,
                goal_artifact_id: None,
                approval_id: &approval.id,
                artifact_hash: &artifact.content_hash,
                started_at: 5,
            })
            .await;
        assert!(matches!(duplicate, Err(DbError::Conflict(_))));

        assert_eq!(repo.pause_incomplete(6).await.unwrap(), 1);
        assert_eq!(repo.pause_incomplete_runs(6).await.unwrap(), 1);
        let recovered = repo.list_runs(USER, &task.id).await.unwrap();
        assert_eq!(recovered[0].id, run.id);
        assert_eq!(recovered[0].status, "paused");
        assert_eq!(recovered.len(), 1, "startup recovery must not create or replay a run");
    }

    #[tokio::test]
    async fn concurrent_approval_claim_allows_exactly_one_resolution() {
        let (_directory, db) = init_concurrent_database().await;
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let task = repo.create(&create_params("ready")).await.unwrap();
        let (artifact, approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "review once",
                content_hash: "approval-race-hash",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();
        let params = || ResolveTaskApprovalParams {
            user_id: USER,
            task_session_id: &task.id,
            approval_id: &approval.id,
            artifact_id: &artifact.id,
            artifact_hash: &artifact.content_hash,
            status: "approved",
            resolved_by: USER,
            comment: None,
            resolved_at: 10,
        };

        let left_params = params();
        let right_params = params();
        let (left, right) = tokio::join!(
            repo.resolve_approval(&left_params),
            repo.resolve_approval(&right_params)
        );
        assert_eq!(
            [left.is_ok(), right.is_ok()].into_iter().filter(|value| *value).count(),
            1
        );
        let rejected = if left.is_err() {
            left.unwrap_err()
        } else {
            right.unwrap_err()
        };
        assert!(matches!(rejected, DbError::Conflict(_)), "unexpected error: {rejected}");
        let stored = repo.get_approval(USER, &task.id, &approval.id).await.unwrap().unwrap();
        assert_eq!(stored.status, "approved");
    }

    #[tokio::test]
    async fn concurrent_execution_claim_allows_exactly_one_run() {
        let (_directory, db) = init_concurrent_database().await;
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conversation-race', ?, 'Execution race', 'acp', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let mut task_params = create_params("ready");
        task_params.conversation_id = Some("conversation-race");
        let task = repo.create(&task_params).await.unwrap();
        let (artifact, approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "execute once",
                content_hash: "execution-race-hash",
                acceptance_criteria: &[],
            })
            .await
            .unwrap();
        repo.resolve_approval(&ResolveTaskApprovalParams {
            user_id: USER,
            task_session_id: &task.id,
            approval_id: &approval.id,
            artifact_id: &artifact.id,
            artifact_hash: &artifact.content_hash,
            status: "approved",
            resolved_by: USER,
            comment: None,
            resolved_at: 20,
        })
        .await
        .unwrap();
        let params = || CreateTaskRunParams {
            user_id: USER,
            task_session_id: &task.id,
            conversation_id: "conversation-race",
            plan_artifact_id: &artifact.id,
            goal_artifact_id: None,
            approval_id: &approval.id,
            artifact_hash: &artifact.content_hash,
            started_at: 21,
        };

        let left_params = params();
        let right_params = params();
        let (left, right) = tokio::join!(repo.create_run(&left_params), repo.create_run(&right_params));
        assert_eq!(
            [left.is_ok(), right.is_ok()].into_iter().filter(|value| *value).count(),
            1
        );
        let rejected = if left.is_err() {
            left.unwrap_err()
        } else {
            right.unwrap_err()
        };
        assert!(matches!(rejected, DbError::Conflict(_)), "unexpected error: {rejected}");
        assert_eq!(repo.list_runs(USER, &task.id).await.unwrap().len(), 1);
    }
}

use aionui_common::TimestampMs;
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::error::DbError;
use crate::models::{
    ContextSnapshotArtifactRow, ContextSnapshotRow, TaskAcceptanceCriterionRow, TaskApprovalRow, TaskArtifactRow,
    TaskCheckpointRow, TaskEvidenceRow, TaskRunRow, TaskSessionRow, TaskTraceEventRow,
};
use crate::repository::task_session::{
    AppendTaskTraceEventParams, CreateContextSnapshotParams, CreatePlanningTaskRunParams, CreateTaskArtifactParams,
    CreateTaskCheckpointParams, CreateTaskEvidenceParams, CreateTaskRunParams, CreateTaskSessionParams,
    FinishTaskRunParams, ITaskSessionRepository, LinkContextSnapshotArtifactParams, ResolveTaskApprovalParams,
    UpdateAcceptanceCriterionParams, UpdateTaskSessionParams,
};

async fn append_trace_event_tx(
    tx: &mut Transaction<'_, Sqlite>,
    task_session_id: &str,
    run_id: &str,
    event_type: &str,
    timestamp: TimestampMs,
    payload: &str,
) -> Result<TaskTraceEventRow, DbError> {
    let sequence: i64 = sqlx::query_scalar(
        "UPDATE task_runs SET next_trace_sequence = next_trace_sequence + 1 \
         WHERE id = ? AND task_session_id = ? RETURNING next_trace_sequence",
    )
    .bind(run_id)
    .bind(task_session_id)
    .fetch_one(&mut **tx)
    .await?;
    let id = aionui_common::generate_prefixed_id("trace");
    sqlx::query(
        "INSERT INTO task_trace_events \
         (id, task_session_id, run_id, sequence, event_type, timestamp, payload) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(task_session_id)
    .bind(run_id)
    .bind(sequence)
    .bind(event_type)
    .bind(timestamp)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(TaskTraceEventRow {
        id,
        task_session_id: task_session_id.to_owned(),
        run_id: run_id.to_owned(),
        sequence,
        event_type: event_type.to_owned(),
        timestamp,
        payload: payload.to_owned(),
    })
}

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

    async fn claim_automatic_planning(
        &self,
        user_id: &str,
        id: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE task_sessions SET status = 'running', updated_at = ? \
             WHERE user_id = ? AND id = ? AND mode = 'plan' AND status IN ('ready', 'paused')",
        )
        .bind(updated_at)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn create_planning_run(&self, params: &CreatePlanningTaskRunParams<'_>) -> Result<TaskRunRow, DbError> {
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE task_sessions SET status = 'running', updated_at = ? \
             WHERE user_id = ? AND id = ? AND mode = 'plan' AND conversation_id = ? \
             AND status IN ('ready', 'paused')",
        )
        .bind(params.started_at)
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.conversation_id)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            return Err(DbError::Conflict(
                "Automatic planning is already active or the task is not ready".into(),
            ));
        }
        let run_id = aionui_common::generate_prefixed_id("run");
        sqlx::query(
            "INSERT INTO task_runs \
             (id, task_session_id, conversation_id, run_kind, status, started_at, agent_id, agent_runtime, model, \
              mode, planning_isolation) \
             VALUES (?, ?, ?, 'planning', 'running', ?, ?, ?, ?, 'plan', ?)",
        )
        .bind(&run_id)
        .bind(params.task_session_id)
        .bind(params.conversation_id)
        .bind(params.started_at)
        .bind(params.agent_id)
        .bind(params.agent_runtime)
        .bind(params.model)
        .bind(params.planning_isolation)
        .execute(&mut *tx)
        .await?;
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "task.created",
            params.started_at,
            r#"{"mode":"plan"}"#,
        )
        .await?;
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "run.started",
            params.started_at,
            "{}",
        )
        .await?;
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "planning.started",
            params.started_at,
            r#"{"policy":"strict_planning","isolation":"guaranteed"}"#,
        )
        .await?;
        let row = sqlx::query_as::<_, TaskRunRow>("SELECT * FROM task_runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
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
        if let Some(run_id) = params.planning_run_id {
            let completed = sqlx::query(
                "UPDATE task_runs SET plan_artifact_id = ?, approval_id = ?, status = 'completed', finished_at = ?, \
                 result_summary = ?, usage = ? \
                 WHERE id = ? AND task_session_id = ? AND run_kind = 'planning' AND status = 'running' \
                 AND task_session_id IN (SELECT id FROM task_sessions WHERE user_id = ?)",
            )
            .bind(&artifact_id)
            .bind(&approval_id)
            .bind(now)
            .bind(params.planning_result_summary)
            .bind(params.planning_usage)
            .bind(run_id)
            .bind(params.task_session_id)
            .bind(params.user_id)
            .execute(&mut *tx)
            .await?;
            if completed.rows_affected() != 1 {
                return Err(DbError::Conflict("Planning run is no longer running".into()));
            }
            let artifact_payload = serde_json::json!({
                "artifact_id": artifact_id,
                "artifact_hash": params.content_hash,
                "kind": params.kind,
                "version": version
            })
            .to_string();
            append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                run_id,
                "artifact.created",
                now,
                &artifact_payload,
            )
            .await?;
            let approval_payload = serde_json::json!({
                "approval_id": approval_id,
                "artifact_id": artifact_id,
                "artifact_hash": params.content_hash,
                "status": "pending"
            })
            .to_string();
            append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                run_id,
                "approval.requested",
                now,
                &approval_payload,
            )
            .await?;
            append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                run_id,
                "planning.completed",
                now,
                &artifact_payload,
            )
            .await?;
            let terminal = append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                run_id,
                "run.completed",
                now,
                r#"{"result_summary":"Automatic plan submitted for approval"}"#,
            )
            .await?;
            let checkpoint_id = aionui_common::generate_prefixed_id("checkpoint");
            sqlx::query(
                "INSERT INTO task_checkpoints \
                 (id, task_session_id, run_id, checkpoint_type, sequence, artifact_id, state, created_at) \
                 VALUES (?, ?, ?, 'plan_submitted', ?, ?, ?, ?)",
            )
            .bind(checkpoint_id)
            .bind(params.task_session_id)
            .bind(run_id)
            .bind(terminal.sequence)
            .bind(&artifact_id)
            .bind(&artifact_payload)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
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
        let planning_run_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM task_runs WHERE task_session_id = ? AND run_kind = 'planning' \
             AND plan_artifact_id = ? ORDER BY started_at DESC LIMIT 1",
        )
        .bind(params.task_session_id)
        .bind(params.artifact_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(run_id) = planning_run_id {
            let event_type = if params.status == "approved" {
                "approval.approved"
            } else {
                "approval.rejected"
            };
            append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                &run_id,
                event_type,
                params.resolved_at,
                &serde_json::json!({
                    "approval_id": params.approval_id,
                    "artifact_id": params.artifact_id,
                    "artifact_hash": params.artifact_hash,
                    "status": params.status,
                    "resolved_by": params.resolved_by
                })
                .to_string(),
            )
            .await?;
        }
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
             (id, task_session_id, conversation_id, plan_artifact_id, goal_artifact_id, approval_id, status, started_at, \
              agent_id, agent_runtime, model, mode, planning_isolation) \
             VALUES (?, ?, ?, ?, ?, ?, 'running', ?, ?, ?, ?, ?, ?)",
        )
        .bind(&run_id)
        .bind(params.task_session_id)
        .bind(params.conversation_id)
        .bind(params.plan_artifact_id)
        .bind(params.goal_artifact_id)
        .bind(params.approval_id)
        .bind(params.started_at)
        .bind(params.agent_id)
        .bind(params.agent_runtime)
        .bind(params.model)
        .bind(params.mode)
        .bind(params.planning_isolation)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE task_sessions SET status = 'running', updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(params.started_at)
            .bind(params.task_session_id)
            .bind(params.user_id)
            .execute(&mut *tx)
            .await?;
        let task_payload = serde_json::json!({ "mode": params.mode, "agent_id": params.agent_id }).to_string();
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "task.created",
            params.started_at,
            &task_payload,
        )
        .await?;
        let artifact_payload = serde_json::json!({
            "artifact_id": params.plan_artifact_id,
            "artifact_hash": params.artifact_hash,
            "kind": "plan"
        })
        .to_string();
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "artifact.created",
            params.started_at,
            &artifact_payload,
        )
        .await?;
        let approval_payload = serde_json::json!({
            "approval_id": params.approval_id,
            "artifact_id": params.plan_artifact_id,
            "artifact_hash": params.artifact_hash,
            "status": "approved"
        })
        .to_string();
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "approval.approved",
            params.started_at,
            &approval_payload,
        )
        .await?;
        append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            &run_id,
            "run.started",
            params.started_at,
            "{}",
        )
        .await?;
        let checkpoint_id = aionui_common::generate_prefixed_id("checkpoint");
        let checkpoint_sequence: i64 = sqlx::query_scalar("SELECT next_trace_sequence FROM task_runs WHERE id = ?")
            .bind(&run_id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO task_checkpoints \
             (id, task_session_id, run_id, checkpoint_type, sequence, artifact_id, state, created_at) \
             VALUES (?, ?, ?, 'before_execution', ?, ?, ?, ?)",
        )
        .bind(checkpoint_id)
        .bind(params.task_session_id)
        .bind(&run_id)
        .bind(checkpoint_sequence)
        .bind(params.plan_artifact_id)
        .bind(&artifact_payload)
        .bind(params.started_at)
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
            "UPDATE task_runs SET status = ?, finished_at = ?, error_message = ?, result_summary = ?, usage = ? \
             WHERE id = ? AND task_session_id = ? AND status = 'running' \
             AND task_session_id IN (SELECT id FROM task_sessions WHERE user_id = ?)",
        )
        .bind(params.run_status)
        .bind(params.finished_at)
        .bind(params.error_message)
        .bind(params.result_summary)
        .bind(params.usage)
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
        let event_type = match params.run_status {
            "completed" => "run.completed",
            "cancelled" => "run.cancelled",
            _ => "run.failed",
        };
        let payload = serde_json::json!({
            "status": params.run_status,
            "result_summary": params.result_summary,
            "error": params.error_message
        })
        .to_string();
        let terminal = append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            params.run_id,
            event_type,
            params.finished_at,
            &payload,
        )
        .await?;
        let checkpoint_id = aionui_common::generate_prefixed_id("checkpoint");
        sqlx::query(
            "INSERT INTO task_checkpoints \
             (id, task_session_id, run_id, checkpoint_type, sequence, state, created_at) \
             VALUES (?, ?, ?, 'before_completion', ?, ?, ?)",
        )
        .bind(checkpoint_id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .bind(terminal.sequence)
        .bind(&payload)
        .bind(params.finished_at)
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

    async fn get_run(&self, user_id: &str, task_session_id: &str, run_id: &str) -> Result<Option<TaskRunRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskRunRow>(
            "SELECT task_run.* FROM task_runs task_run JOIN task_sessions task ON task.id = task_run.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND task_run.id = ?",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(run_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn append_trace_event(&self, params: &AppendTaskTraceEventParams<'_>) -> Result<TaskTraceEventRow, DbError> {
        let owned: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM task_runs run JOIN task_sessions task ON task.id = run.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND run.id = ?",
        )
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .fetch_one(&self.pool)
        .await?;
        if !owned {
            return Err(DbError::NotFound(format!("Task run '{}' not found", params.run_id)));
        }
        // Begin the transaction immediately before the first write. A read in
        // the same deferred transaction would require a lock upgrade and can
        // fail with SQLITE_BUSY when multiple connections append together.
        let mut tx = self.pool.begin().await?;
        let event = append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            params.run_id,
            params.event_type,
            params.timestamp,
            params.payload,
        )
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    async fn list_trace_events(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskTraceEventRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskTraceEventRow>(
            "SELECT event.* FROM task_trace_events event \
             JOIN task_sessions task ON task.id = event.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND event.run_id = ? ORDER BY event.sequence",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_checkpoint(&self, params: &CreateTaskCheckpointParams<'_>) -> Result<TaskCheckpointRow, DbError> {
        let owned: bool = sqlx::query_scalar(
            "SELECT COUNT(*) > 0 FROM task_sessions task JOIN task_runs run ON run.task_session_id = task.id \
             WHERE task.user_id = ? AND task.id = ? AND run.id = ?",
        )
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .fetch_one(&self.pool)
        .await?;
        if !owned {
            return Err(DbError::NotFound(format!("Task run '{}' not found", params.run_id)));
        }
        let mut tx = self.pool.begin().await?;
        let payload = serde_json::json!({ "checkpoint_type": params.checkpoint_type }).to_string();
        let event = append_trace_event_tx(
            &mut tx,
            params.task_session_id,
            params.run_id,
            "checkpoint.created",
            params.created_at,
            &payload,
        )
        .await?;
        let id = aionui_common::generate_prefixed_id("checkpoint");
        sqlx::query(
            "INSERT INTO task_checkpoints \
             (id, task_session_id, run_id, checkpoint_type, sequence, artifact_id, state, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .bind(params.checkpoint_type)
        .bind(event.sequence)
        .bind(params.artifact_id)
        .bind(params.state)
        .bind(params.created_at)
        .execute(&mut *tx)
        .await?;
        let row = sqlx::query_as::<_, TaskCheckpointRow>("SELECT * FROM task_checkpoints WHERE id = ?")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(row)
    }

    async fn list_checkpoints(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskCheckpointRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskCheckpointRow>(
            "SELECT checkpoint.* FROM task_checkpoints checkpoint \
             JOIN task_sessions task ON task.id = checkpoint.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND checkpoint.run_id = ? ORDER BY checkpoint.sequence",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_evidence(&self, params: &CreateTaskEvidenceParams<'_>) -> Result<TaskEvidenceRow, DbError> {
        let id = aionui_common::generate_prefixed_id("evidence");
        let result = sqlx::query(
            "INSERT INTO task_evidence \
             (id, task_session_id, run_id, trace_event_id, criterion_id, kind, summary, reference, metadata, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (\
                 SELECT 1 FROM task_sessions task JOIN task_runs run ON run.task_session_id = task.id \
                 WHERE task.user_id = ? AND task.id = ? AND run.id = ?\
             )",
        )
        .bind(&id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .bind(params.trace_event_id)
        .bind(params.criterion_id)
        .bind(params.kind)
        .bind(params.summary)
        .bind(params.reference)
        .bind(params.metadata)
        .bind(params.created_at)
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!("Task run '{}' not found", params.run_id)));
        }
        Ok(
            sqlx::query_as::<_, TaskEvidenceRow>("SELECT * FROM task_evidence WHERE id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn list_evidence(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskEvidenceRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskEvidenceRow>(
            "SELECT evidence.* FROM task_evidence evidence \
             JOIN task_sessions task ON task.id = evidence.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND evidence.run_id = ? ORDER BY evidence.created_at, evidence.id",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn create_context_snapshot(
        &self,
        params: &CreateContextSnapshotParams<'_>,
    ) -> Result<ContextSnapshotRow, DbError> {
        let id = aionui_common::generate_prefixed_id("context_snapshot");
        let result = sqlx::query(
            "INSERT INTO context_snapshots \
             (id, task_session_id, run_id, provider, query, scope, purpose, result_refs, snapshot_hash, created_at) \
             SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (\
                 SELECT 1 FROM task_sessions task JOIN task_runs run ON run.task_session_id = task.id \
                 WHERE task.user_id = ? AND task.id = ? AND run.id = ?\
             )",
        )
        .bind(&id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .bind(params.provider)
        .bind(params.query)
        .bind(params.scope)
        .bind(params.purpose)
        .bind(params.result_refs)
        .bind(params.snapshot_hash)
        .bind(params.created_at)
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.run_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!("Task run '{}' not found", params.run_id)));
        }
        Ok(
            sqlx::query_as::<_, ContextSnapshotRow>("SELECT * FROM context_snapshots WHERE id = ?")
                .bind(id)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn list_context_snapshots(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<ContextSnapshotRow>, DbError> {
        Ok(sqlx::query_as::<_, ContextSnapshotRow>(
            "SELECT snapshot.* FROM context_snapshots snapshot \
             JOIN task_sessions task ON task.id = snapshot.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND snapshot.run_id = ? \
             ORDER BY snapshot.created_at, snapshot.id",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn link_context_snapshot_artifact(
        &self,
        params: &LinkContextSnapshotArtifactParams<'_>,
    ) -> Result<ContextSnapshotArtifactRow, DbError> {
        let result = sqlx::query(
            "INSERT INTO context_snapshot_artifacts (task_session_id, snapshot_id, artifact_id, created_at) \
             SELECT ?, ?, ?, ? WHERE EXISTS (\
                 SELECT 1 FROM task_sessions task \
                 JOIN context_snapshots snapshot ON snapshot.task_session_id = task.id \
                 JOIN task_artifacts artifact ON artifact.task_session_id = task.id \
                 WHERE task.user_id = ? AND task.id = ? AND snapshot.id = ? AND artifact.id = ?\
             )",
        )
        .bind(params.task_session_id)
        .bind(params.snapshot_id)
        .bind(params.artifact_id)
        .bind(params.created_at)
        .bind(params.user_id)
        .bind(params.task_session_id)
        .bind(params.snapshot_id)
        .bind(params.artifact_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!(
                "Context snapshot '{}' or task artifact '{}' not found",
                params.snapshot_id, params.artifact_id
            )));
        }
        Ok(sqlx::query_as::<_, ContextSnapshotArtifactRow>(
            "SELECT * FROM context_snapshot_artifacts WHERE snapshot_id = ? AND artifact_id = ?",
        )
        .bind(params.snapshot_id)
        .bind(params.artifact_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn list_context_snapshots_for_artifact(
        &self,
        user_id: &str,
        task_session_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<ContextSnapshotRow>, DbError> {
        Ok(sqlx::query_as::<_, ContextSnapshotRow>(
            "SELECT snapshot.* FROM context_snapshots snapshot \
             JOIN context_snapshot_artifacts link ON link.snapshot_id = snapshot.id \
             JOIN task_sessions task ON task.id = snapshot.task_session_id \
             WHERE task.user_id = ? AND task.id = ? AND link.task_session_id = task.id AND link.artifact_id = ? \
             ORDER BY snapshot.created_at, snapshot.id",
        )
        .bind(user_id)
        .bind(task_session_id)
        .bind(artifact_id)
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
        let mut tx = self.pool.begin().await?;
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
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() != 1 {
            return Err(DbError::NotFound(format!(
                "Acceptance criterion '{}' not found",
                params.criterion_id
            )));
        }
        let row =
            sqlx::query_as::<_, TaskAcceptanceCriterionRow>("SELECT * FROM task_acceptance_criteria WHERE id = ?")
                .bind(params.criterion_id)
                .fetch_one(&mut *tx)
                .await?;
        let run_id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM task_runs WHERE task_session_id = ? AND goal_artifact_id = ? \
             AND run_kind = 'execution' AND status = 'completed' ORDER BY started_at DESC LIMIT 1",
        )
        .bind(params.task_session_id)
        .bind(&row.goal_artifact_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(run_id) = run_id {
            let started = append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                &run_id,
                "verification.started",
                params.verified_at,
                &serde_json::json!({ "criterion_id": params.criterion_id }).to_string(),
            )
            .await?;
            let checkpoint_id = aionui_common::generate_prefixed_id("checkpoint");
            sqlx::query(
                "INSERT INTO task_checkpoints \
                 (id, task_session_id, run_id, checkpoint_type, sequence, artifact_id, state, created_at) \
                 VALUES (?, ?, ?, 'before_verification', ?, ?, ?, ?)",
            )
            .bind(checkpoint_id)
            .bind(params.task_session_id)
            .bind(&run_id)
            .bind(started.sequence)
            .bind(&row.goal_artifact_id)
            .bind(serde_json::json!({ "criterion_id": params.criterion_id }).to_string())
            .bind(params.verified_at)
            .execute(&mut *tx)
            .await?;
            let criterion_event_type = match params.status {
                "passed" => "criterion.passed",
                "failed" => "criterion.failed",
                _ => "criterion.needs_verification",
            };
            let criterion_payload = serde_json::json!({
                "criterion_id": params.criterion_id,
                "status": params.status,
                "evidence": serde_json::from_str::<serde_json::Value>(params.evidence).unwrap_or_default()
            })
            .to_string();
            let criterion_event = append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                &run_id,
                criterion_event_type,
                params.verified_at,
                &criterion_payload,
            )
            .await?;
            if let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(params.evidence) {
                for item in items {
                    let source_kind = item
                        .get("kind")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("user_confirmation");
                    let kind = match source_kind {
                        "test_result" => "test",
                        "command_result" => "command",
                        "file_diff" => "diff",
                        "artifact" => "artifact",
                        _ => "user",
                    };
                    let summary = item
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("Verification evidence");
                    let reference = item.get("reference").and_then(serde_json::Value::as_str);
                    sqlx::query(
                        "INSERT INTO task_evidence \
                         (id, task_session_id, run_id, trace_event_id, criterion_id, kind, summary, reference, metadata, created_at) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(aionui_common::generate_prefixed_id("evidence"))
                    .bind(params.task_session_id)
                    .bind(&run_id)
                    .bind(&criterion_event.id)
                    .bind(params.criterion_id)
                    .bind(kind)
                    .bind(summary)
                    .bind(reference)
                    .bind(item.to_string())
                    .bind(params.verified_at)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            append_trace_event_tx(
                &mut tx,
                params.task_session_id,
                &run_id,
                "verification.completed",
                params.verified_at,
                &serde_json::json!({ "criterion_id": params.criterion_id, "status": params.status }).to_string(),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(row)
    }

    async fn pause_incomplete_runs(&self, updated_at: TimestampMs) -> Result<u64, DbError> {
        let mut tx = self.pool.begin().await?;
        let runs: Vec<(String, String)> =
            sqlx::query_as("SELECT id, task_session_id FROM task_runs WHERE status = 'running'")
                .fetch_all(&mut *tx)
                .await?;
        for (run_id, task_session_id) in &runs {
            sqlx::query("UPDATE task_runs SET status = 'paused', finished_at = ? WHERE id = ? AND status = 'running'")
                .bind(updated_at)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
            append_trace_event_tx(
                &mut tx,
                task_session_id,
                run_id,
                "run.interrupted",
                updated_at,
                r#"{"reason":"application_restart"}"#,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(runs.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::task_session::{
        CreateContextSnapshotParams, CreateTaskArtifactParams, CreateTaskRunParams, CreateTaskSessionParams,
        FinishTaskRunParams, LinkContextSnapshotArtifactParams, ResolveTaskApprovalParams, UpdateTaskSessionParams,
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
    async fn failed_and_cancelled_runs_keep_terminal_review_records() {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('terminal-review', ?, 'Terminal review', 'aionrs', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());

        for (index, status) in ["failed", "cancelled"].into_iter().enumerate() {
            let task = repo.create(&create_params("running")).await.unwrap();
            let run_id = format!("terminal-run-{index}");
            sqlx::query(
                "INSERT INTO task_runs \
                 (id, task_session_id, conversation_id, run_kind, status, started_at, mode) \
                 VALUES (?, ?, 'terminal-review', 'planning', 'running', 2, 'plan')",
            )
            .bind(&run_id)
            .bind(&task.id)
            .execute(db.pool())
            .await
            .unwrap();

            let finished = repo
                .finish_run(&FinishTaskRunParams {
                    user_id: USER,
                    task_session_id: &task.id,
                    run_id: &run_id,
                    run_status: status,
                    task_status: status,
                    finished_at: 3,
                    error_message: (status == "failed").then_some("tool failed"),
                    result_summary: None,
                    usage: None,
                })
                .await
                .unwrap();
            assert_eq!(finished.status, status);
            let trace = repo.list_trace_events(USER, &task.id, &run_id).await.unwrap();
            assert_eq!(trace.last().unwrap().event_type, format!("run.{status}"));
            let checkpoints = repo.list_checkpoints(USER, &task.id, &run_id).await.unwrap();
            assert_eq!(checkpoints.last().unwrap().checkpoint_type, "before_completion");
        }
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
    async fn automatic_planning_claim_is_atomic_and_plan_only() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let ready = repo.create(&create_params("ready")).await.unwrap();
        let paused = repo.create(&create_params("paused")).await.unwrap();
        let mut agent_params = create_params("ready");
        agent_params.mode = "agent";
        let agent = repo.create(&agent_params).await.unwrap();

        assert!(repo.claim_automatic_planning(USER, &ready.id, 10).await.unwrap());
        assert!(!repo.claim_automatic_planning(USER, &ready.id, 11).await.unwrap());
        assert!(repo.claim_automatic_planning(USER, &paused.id, 12).await.unwrap());
        assert!(!repo.claim_automatic_planning(USER, &agent.id, 13).await.unwrap());
        assert_eq!(repo.get(USER, &ready.id).await.unwrap().unwrap().status, "running");
        assert_eq!(repo.get(USER, &paused.id).await.unwrap().unwrap().status, "running");
        assert_eq!(repo.get(USER, &agent.id).await.unwrap().unwrap().status, "ready");
    }

    #[tokio::test]
    async fn task_sessions_are_scoped_to_their_owner() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let created = repo.create(&create_params("draft")).await.unwrap();

        assert!(repo.get("other-user", &created.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn context_snapshots_persist_and_link_to_artifacts_with_owner_scope() {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('context-conversation', ?, 'Context', 'aionrs', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let task = repo.create(&create_params("running")).await.unwrap();
        sqlx::query(
            "INSERT INTO task_runs \
             (id, task_session_id, conversation_id, run_kind, status, started_at) \
             VALUES ('context-run', ?, 'context-conversation', 'planning', 'running', 2)",
        )
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_artifacts \
             (id, task_session_id, kind, version, content, content_hash, status, created_at, updated_at) \
             VALUES ('context-plan', ?, 'plan', 1, 'plan', 'plan-hash', 'submitted', 3, 3)",
        )
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

        let snapshot = repo
            .create_context_snapshot(&CreateContextSnapshotParams {
                user_id: USER,
                task_session_id: &task.id,
                run_id: "context-run",
                provider: "test",
                query: "release requirements",
                scope: r#"{"kind":"all_accessible"}"#,
                purpose: "planning",
                result_refs: r#"[{"source_id":"doc-1","snippet":"bounded"}]"#,
                snapshot_hash: "snapshot-hash",
                created_at: 4,
            })
            .await
            .unwrap();
        assert_eq!(snapshot.run_id, "context-run");
        assert_eq!(snapshot.snapshot_hash, "snapshot-hash");
        assert_eq!(
            repo.list_context_snapshots(USER, &task.id, "context-run")
                .await
                .unwrap(),
            [snapshot.clone()]
        );
        assert!(
            repo.list_context_snapshots("other-user", &task.id, "context-run")
                .await
                .unwrap()
                .is_empty()
        );

        repo.link_context_snapshot_artifact(&LinkContextSnapshotArtifactParams {
            user_id: USER,
            task_session_id: &task.id,
            snapshot_id: &snapshot.id,
            artifact_id: "context-plan",
            created_at: 5,
        })
        .await
        .unwrap();
        assert_eq!(
            repo.list_context_snapshots_for_artifact(USER, &task.id, "context-plan")
                .await
                .unwrap(),
            [snapshot]
        );
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                agent_id: "aionrs",
                agent_runtime: Some("aionrs"),
                model: None,
                mode: "plan",
                planning_isolation: Some("guaranteed"),
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
                agent_id: "aionrs",
                agent_runtime: Some("aionrs"),
                model: None,
                mode: "plan",
                planning_isolation: Some("guaranteed"),
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
                agent_id: "aionrs",
                agent_runtime: Some("aionrs"),
                model: None,
                mode: "plan",
                planning_isolation: Some("guaranteed"),
            })
            .await;
        assert!(matches!(duplicate, Err(DbError::Conflict(_))));

        assert_eq!(repo.pause_incomplete(6).await.unwrap(), 1);
        assert_eq!(repo.pause_incomplete_runs(6).await.unwrap(), 1);
        let recovered = repo.list_runs(USER, &task.id).await.unwrap();
        assert_eq!(recovered[0].id, run.id);
        assert_eq!(recovered[0].status, "paused");
        assert_eq!(recovered.len(), 1, "startup recovery must not create or replay a run");
        let trace = repo.list_trace_events(USER, &task.id, &run.id).await.unwrap();
        assert_eq!(trace.last().unwrap().event_type, "run.interrupted");
        assert_eq!(trace.last().unwrap().sequence, 5);
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
            agent_id: "aionrs",
            agent_runtime: Some("aionrs"),
            model: None,
            mode: "plan",
            planning_isolation: Some("guaranteed"),
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

    #[tokio::test]
    async fn concurrent_trace_appends_allocate_unique_sequences_and_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("task-trace-concurrency.db");
        let db = init_database_staged(&path).await.unwrap();
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at) \
             VALUES ('conversation-trace', ?, 'Trace', 'aionrs', 1, 1)",
        )
        .bind(USER)
        .execute(db.pool())
        .await
        .unwrap();
        let repo = SqliteTaskSessionRepository::new(db.pool().clone());
        let mut task_params = create_params("ready");
        task_params.conversation_id = Some("conversation-trace");
        let task = repo.create(&task_params).await.unwrap();
        let (artifact, approval, _) = repo
            .create_artifact_with_approval(&CreateTaskArtifactParams {
                user_id: USER,
                task_session_id: &task.id,
                kind: "plan",
                content: "trace this run",
                content_hash: "trace-hash",
                acceptance_criteria: &[],
                planning_run_id: None,
                planning_result_summary: None,
                planning_usage: None,
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
        let run = repo
            .create_run(&CreateTaskRunParams {
                user_id: USER,
                task_session_id: &task.id,
                conversation_id: "conversation-trace",
                plan_artifact_id: &artifact.id,
                goal_artifact_id: None,
                approval_id: &approval.id,
                artifact_hash: &artifact.content_hash,
                started_at: 3,
                agent_id: "aionrs",
                agent_runtime: Some("aionrs"),
                model: Some("test-model"),
                mode: "plan",
                planning_isolation: Some("guaranteed"),
            })
            .await
            .unwrap();

        let mut joins = tokio::task::JoinSet::new();
        for index in 0..24 {
            let repo = repo.clone();
            let task_id = task.id.clone();
            let run_id = run.id.clone();
            joins.spawn(async move {
                let payload = format!(r#"{{"index":{index}}}"#);
                repo.append_trace_event(&AppendTaskTraceEventParams {
                    user_id: USER,
                    task_session_id: &task_id,
                    run_id: &run_id,
                    event_type: "tool.completed",
                    timestamp: 10 + index,
                    payload: &payload,
                })
                .await
                .unwrap()
            });
        }
        while let Some(result) = joins.join_next().await {
            result.unwrap();
        }

        let trace = repo.list_trace_events(USER, &task.id, &run.id).await.unwrap();
        assert_eq!(trace.len(), 28);
        assert_eq!(
            trace.iter().map(|event| event.sequence).collect::<Vec<_>>(),
            (1..=28).collect::<Vec<_>>()
        );

        db.pool().close().await;
        drop(repo);
        drop(db);
        let reopened = init_database_staged(&path).await.unwrap();
        let reopened_repo = SqliteTaskSessionRepository::new(reopened.pool().clone());
        let persisted = reopened_repo.list_trace_events(USER, &task.id, &run.id).await.unwrap();
        assert_eq!(persisted.len(), 28);
        assert_eq!(persisted.last().unwrap().sequence, 28);
    }
}

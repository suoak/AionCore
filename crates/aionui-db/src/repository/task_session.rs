use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{
    ContextSnapshotArtifactRow, ContextSnapshotRow, TaskAcceptanceCriterionRow, TaskApprovalRow, TaskArtifactRow,
    TaskCheckpointRow, TaskEvidenceRow, TaskRunRow, TaskSessionRow, TaskTraceEventRow,
};

#[async_trait::async_trait]
pub trait ITaskSessionRepository: Send + Sync {
    async fn list(&self, user_id: &str, conversation_id: Option<&str>) -> Result<Vec<TaskSessionRow>, DbError>;
    async fn get(&self, user_id: &str, id: &str) -> Result<Option<TaskSessionRow>, DbError>;
    async fn create(&self, params: &CreateTaskSessionParams<'_>) -> Result<TaskSessionRow, DbError>;
    async fn update(
        &self,
        user_id: &str,
        id: &str,
        params: &UpdateTaskSessionParams<'_>,
    ) -> Result<TaskSessionRow, DbError>;
    async fn claim_automatic_planning(&self, user_id: &str, id: &str, updated_at: TimestampMs)
    -> Result<bool, DbError>;
    async fn create_planning_run(&self, params: &CreatePlanningTaskRunParams<'_>) -> Result<TaskRunRow, DbError>;
    async fn pause_incomplete(&self, updated_at: TimestampMs) -> Result<u64, DbError>;
    async fn create_artifact_with_approval(
        &self,
        params: &CreateTaskArtifactParams<'_>,
    ) -> Result<(TaskArtifactRow, TaskApprovalRow, Vec<TaskAcceptanceCriterionRow>), DbError>;
    async fn list_artifacts(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskArtifactRow>, DbError>;
    async fn get_artifact(
        &self,
        user_id: &str,
        task_session_id: &str,
        artifact_id: &str,
    ) -> Result<Option<TaskArtifactRow>, DbError>;
    async fn list_approvals(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskApprovalRow>, DbError>;
    async fn get_approval(
        &self,
        user_id: &str,
        task_session_id: &str,
        approval_id: &str,
    ) -> Result<Option<TaskApprovalRow>, DbError>;
    async fn resolve_approval(&self, params: &ResolveTaskApprovalParams<'_>) -> Result<TaskApprovalRow, DbError>;
    async fn create_run(&self, params: &CreateTaskRunParams<'_>) -> Result<TaskRunRow, DbError>;
    async fn finish_run(&self, params: &FinishTaskRunParams<'_>) -> Result<TaskRunRow, DbError>;
    async fn list_runs(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskRunRow>, DbError>;
    async fn get_run(&self, user_id: &str, task_session_id: &str, run_id: &str) -> Result<Option<TaskRunRow>, DbError>;
    async fn append_trace_event(&self, params: &AppendTaskTraceEventParams<'_>) -> Result<TaskTraceEventRow, DbError>;
    async fn list_trace_events(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskTraceEventRow>, DbError>;
    async fn create_checkpoint(&self, params: &CreateTaskCheckpointParams<'_>) -> Result<TaskCheckpointRow, DbError>;
    async fn list_checkpoints(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskCheckpointRow>, DbError>;
    async fn create_evidence(&self, params: &CreateTaskEvidenceParams<'_>) -> Result<TaskEvidenceRow, DbError>;
    async fn list_evidence(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<TaskEvidenceRow>, DbError>;
    async fn create_context_snapshot(
        &self,
        params: &CreateContextSnapshotParams<'_>,
    ) -> Result<ContextSnapshotRow, DbError>;
    async fn list_context_snapshots(
        &self,
        user_id: &str,
        task_session_id: &str,
        run_id: &str,
    ) -> Result<Vec<ContextSnapshotRow>, DbError>;
    async fn link_context_snapshot_artifact(
        &self,
        params: &LinkContextSnapshotArtifactParams<'_>,
    ) -> Result<ContextSnapshotArtifactRow, DbError>;
    async fn list_context_snapshots_for_artifact(
        &self,
        user_id: &str,
        task_session_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<ContextSnapshotRow>, DbError>;
    async fn list_acceptance_criteria(
        &self,
        user_id: &str,
        task_session_id: &str,
    ) -> Result<Vec<TaskAcceptanceCriterionRow>, DbError>;
    async fn update_acceptance_criterion(
        &self,
        params: &UpdateAcceptanceCriterionParams<'_>,
    ) -> Result<TaskAcceptanceCriterionRow, DbError>;
    async fn pause_incomplete_runs(&self, updated_at: TimestampMs) -> Result<u64, DbError>;
}

pub struct CreateTaskSessionParams<'a> {
    pub user_id: &'a str,
    pub title: &'a str,
    pub project_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub mode: &'a str,
    pub objective: &'a str,
    pub acceptance_criteria: &'a str,
    pub status: &'a str,
    pub agent_type: &'a str,
    pub agent_session_id: Option<&'a str>,
}

#[derive(Default)]
pub struct UpdateTaskSessionParams<'a> {
    pub title: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub objective: Option<&'a str>,
    pub acceptance_criteria: Option<&'a str>,
    pub status: Option<&'a str>,
    pub agent_type: Option<&'a str>,
    pub agent_session_id: Option<&'a str>,
}

pub struct CreateTaskArtifactParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub kind: &'a str,
    pub content: &'a str,
    pub content_hash: &'a str,
    pub acceptance_criteria: &'a [String],
    pub planning_run_id: Option<&'a str>,
    pub planning_result_summary: Option<&'a str>,
    pub planning_usage: Option<&'a str>,
}

pub struct CreatePlanningTaskRunParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub conversation_id: &'a str,
    pub agent_id: &'a str,
    pub agent_runtime: Option<&'a str>,
    pub model: Option<&'a str>,
    pub planning_isolation: &'a str,
    pub started_at: TimestampMs,
}

pub struct ResolveTaskApprovalParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub approval_id: &'a str,
    pub artifact_id: &'a str,
    pub artifact_hash: &'a str,
    pub status: &'a str,
    pub resolved_by: &'a str,
    pub comment: Option<&'a str>,
    pub resolved_at: TimestampMs,
}

pub struct CreateTaskRunParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub conversation_id: &'a str,
    pub plan_artifact_id: &'a str,
    pub goal_artifact_id: Option<&'a str>,
    pub approval_id: &'a str,
    pub artifact_hash: &'a str,
    pub started_at: TimestampMs,
    pub agent_id: &'a str,
    pub agent_runtime: Option<&'a str>,
    pub model: Option<&'a str>,
    pub mode: &'a str,
    pub planning_isolation: Option<&'a str>,
}

pub struct FinishTaskRunParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub run_status: &'a str,
    pub task_status: &'a str,
    pub finished_at: TimestampMs,
    pub error_message: Option<&'a str>,
    pub result_summary: Option<&'a str>,
    pub usage: Option<&'a str>,
}

pub struct AppendTaskTraceEventParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub event_type: &'a str,
    pub timestamp: TimestampMs,
    pub payload: &'a str,
}

pub struct CreateTaskCheckpointParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub checkpoint_type: &'a str,
    pub artifact_id: Option<&'a str>,
    pub state: &'a str,
    pub created_at: TimestampMs,
}

pub struct CreateTaskEvidenceParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub trace_event_id: Option<&'a str>,
    pub criterion_id: Option<&'a str>,
    pub kind: &'a str,
    pub summary: &'a str,
    pub reference: Option<&'a str>,
    pub metadata: &'a str,
    pub created_at: TimestampMs,
}

pub struct CreateContextSnapshotParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub provider: &'a str,
    pub query: &'a str,
    pub scope: &'a str,
    pub purpose: &'a str,
    pub result_refs: &'a str,
    pub snapshot_hash: &'a str,
    pub created_at: TimestampMs,
}

pub struct LinkContextSnapshotArtifactParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub snapshot_id: &'a str,
    pub artifact_id: &'a str,
    pub created_at: TimestampMs,
}

pub struct UpdateAcceptanceCriterionParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub criterion_id: &'a str,
    pub status: &'a str,
    pub evidence: &'a str,
    pub verified_at: TimestampMs,
}

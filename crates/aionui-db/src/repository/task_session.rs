use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{TaskAcceptanceCriterionRow, TaskApprovalRow, TaskArtifactRow, TaskRunRow, TaskSessionRow};

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
}

pub struct FinishTaskRunParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub run_id: &'a str,
    pub run_status: &'a str,
    pub task_status: &'a str,
    pub finished_at: TimestampMs,
    pub error_message: Option<&'a str>,
}

pub struct UpdateAcceptanceCriterionParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub criterion_id: &'a str,
    pub status: &'a str,
    pub evidence: &'a str,
    pub verified_at: TimestampMs,
}

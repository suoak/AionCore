use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskSessionRow {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub conversation_id: Option<String>,
    pub mode: String,
    pub objective: String,
    pub acceptance_criteria: String,
    pub status: String,
    pub agent_type: String,
    pub agent_session_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskArtifactRow {
    pub id: String,
    pub task_session_id: String,
    pub kind: String,
    pub version: i64,
    pub content: String,
    pub content_hash: String,
    pub status: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskApprovalRow {
    pub id: String,
    pub task_session_id: String,
    pub run_id: Option<String>,
    pub approval_type: String,
    pub artifact_id: String,
    pub artifact_hash: String,
    pub status: String,
    pub requested_at: TimestampMs,
    pub resolved_at: Option<TimestampMs>,
    pub resolved_by: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskRunRow {
    pub id: String,
    pub task_session_id: String,
    pub conversation_id: String,
    pub run_kind: String,
    pub plan_artifact_id: Option<String>,
    pub goal_artifact_id: Option<String>,
    pub approval_id: Option<String>,
    pub status: String,
    pub started_at: TimestampMs,
    pub finished_at: Option<TimestampMs>,
    pub error_message: Option<String>,
    pub agent_id: Option<String>,
    pub agent_runtime: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub planning_isolation: Option<String>,
    pub result_summary: Option<String>,
    pub usage: Option<String>,
    pub next_trace_sequence: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskTraceEventRow {
    pub id: String,
    pub task_session_id: String,
    pub run_id: String,
    pub sequence: i64,
    pub event_type: String,
    pub timestamp: TimestampMs,
    pub payload: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskCheckpointRow {
    pub id: String,
    pub task_session_id: String,
    pub run_id: String,
    pub checkpoint_type: String,
    pub sequence: i64,
    pub artifact_id: Option<String>,
    pub state: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskEvidenceRow {
    pub id: String,
    pub task_session_id: String,
    pub run_id: String,
    pub trace_event_id: Option<String>,
    pub criterion_id: Option<String>,
    pub kind: String,
    pub summary: String,
    pub reference: Option<String>,
    pub metadata: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskAcceptanceCriterionRow {
    pub id: String,
    pub task_session_id: String,
    pub goal_artifact_id: String,
    pub position: i64,
    pub description: String,
    pub status: String,
    pub evidence: String,
    pub verified_at: Option<TimestampMs>,
}

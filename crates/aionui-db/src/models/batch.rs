use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One batch job row (M8 E8-1 data layer).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct BatchJobRow {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub template: String,
    pub agent_type: Option<String>,
    pub assistant_id: Option<String>,
    pub mode: String,
    pub concurrency: i64,
    pub item_timeout_sec: i64,
    pub checks: String,
    pub status: String,
    pub trial_size: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// One batch item row (M8 E8-1 data layer).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct BatchItemRow {
    pub id: String,
    pub batch_job_id: String,
    pub seq: i64,
    pub target: String,
    pub task_session_id: Option<String>,
    pub attempt: i64,
    pub status: String,
    pub check_results: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<TimestampMs>,
    pub finished_at: Option<TimestampMs>,
}

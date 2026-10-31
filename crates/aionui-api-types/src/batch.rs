//! M8 E8: batch jobs (`/api/batch*`) — one prompt template run once per
//! target, judged only by produced artifacts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// D8-3: batch items only run in agent mode.
pub const BATCH_MODE_AGENT: &str = "agent";
/// D8-4 defaults and caps.
pub const BATCH_DEFAULT_CONCURRENCY: u32 = 4;
pub const BATCH_MAX_CONCURRENCY: u32 = 16;
pub const BATCH_DEFAULT_ITEM_TIMEOUT_SEC: u32 = 30 * 60;
pub const BATCH_MIN_ITEM_TIMEOUT_SEC: u32 = 60;
pub const BATCH_MAX_ITEM_TIMEOUT_SEC: u32 = 4 * 60 * 60;
/// D8-2: trial is mandatory; 1..=5 items, default 1.
pub const BATCH_DEFAULT_TRIAL_SIZE: u32 = 1;
pub const BATCH_MAX_TRIAL_SIZE: u32 = 5;
pub const BATCH_MAX_TARGETS: usize = 500;
pub const BATCH_MAX_CHECKS: usize = 20;

/// WebSocket: a job's status or counts changed (refetch the job).
pub const EVENT_BATCH_JOB_UPDATED: &str = "batch.jobUpdated";
/// WebSocket: one item's status / check results changed.
pub const EVENT_BATCH_ITEM_UPDATED: &str = "batch.itemUpdated";

/// Job status vocabulary (`batch_jobs.status`).
pub mod batch_job_status {
    pub const DRAFT: &str = "draft";
    pub const TRIAL: &str = "trial";
    pub const RUNNING: &str = "running";
    pub const PAUSED: &str = "paused";
    pub const COMPLETED: &str = "completed";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// Item status vocabulary (`batch_items.status`).
pub mod batch_item_status {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const PASSED: &str = "passed";
    pub const FAILED: &str = "failed";
    pub const TIMEOUT: &str = "timeout";
    pub const SKIPPED: &str = "skipped";
    pub const CANCELLED: &str = "cancelled";
}

/// One artifact check. Paths are relative to the item's workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchCheckSpec {
    /// A regular file exists at `path`.
    FileExists { path: String },
    /// Between `min` (default 1) and `max` (default unbounded) files match.
    Glob {
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
    },
    /// The JSON file at `path` validates against the inline `schema`.
    JsonSchema { path: String, schema: Value },
}

impl BatchCheckSpec {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::FileExists { .. } => "file_exists",
            Self::Glob { .. } => "glob",
            Self::JsonSchema { .. } => "json_schema",
        }
    }
}

/// Prompt template. `{{target}}` renders the whole target as JSON,
/// `{{target.<key>}}` one top-level field, `{{seq}}` the 1-based item number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchTemplate {
    pub prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBatchJobRequest {
    pub title: String,
    pub template: BatchTemplate,
    /// One JSON object per item. `repo` (a git repository: the item runs in
    /// its own managed worktree) or `workspace` (an existing directory used
    /// in place) is required; other fields are template variables.
    pub targets: Vec<Value>,
    pub checks: Vec<BatchCheckSpec>,
    pub assistant_id: String,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
    /// Must be `agent` when present (D8-3).
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub concurrency: Option<u32>,
    #[serde(default)]
    pub item_timeout_sec: Option<u32>,
    #[serde(default)]
    pub trial_size: Option<u32>,
}

/// `POST /api/batch/{id}/retry`: explicit `item_ids`, or `filter` =
/// `failed` (every failed / timeout item).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchRetryRequest {
    #[serde(default)]
    pub item_ids: Option<Vec<String>>,
    #[serde(default)]
    pub filter: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BatchListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    /// Exclusive `updated_at` cursor.
    #[serde(default)]
    pub before: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCheckResult {
    pub kind: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCounts {
    pub total: u32,
    pub pending: u32,
    pub running: u32,
    pub passed: u32,
    pub failed: u32,
    pub timeout: u32,
    pub skipped: u32,
    pub cancelled: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchItemResponse {
    pub id: String,
    pub batch_job_id: String,
    pub seq: i64,
    pub target: Value,
    pub task_session_id: Option<String>,
    pub attempt: i64,
    pub status: String,
    pub check_results: Vec<BatchCheckResult>,
    pub error: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Item is part of the mandatory trial (`seq <= trial_size`).
    pub trial: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchJobResponse {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub template: Value,
    pub agent_type: Option<String>,
    pub assistant_id: Option<String>,
    pub mode: String,
    pub concurrency: i64,
    pub item_timeout_sec: i64,
    pub checks: Value,
    pub status: String,
    pub trial_size: i64,
    pub counts: BatchCounts,
    /// Every trial item has finished (scale is allowed from `trial`).
    pub trial_settled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn check_specs_round_trip_with_kind_tag() {
        let raw = json!([
            {"kind": "file_exists", "path": "release-check.json"},
            {"kind": "glob", "pattern": "reports/*.md", "min": 2},
            {"kind": "json_schema", "path": "release-check.json", "schema": {"type": "object"}}
        ]);
        let specs: Vec<BatchCheckSpec> = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(specs[1].kind(), "glob");
        assert_eq!(serde_json::to_value(&specs).unwrap(), raw);
    }

    #[test]
    fn unknown_check_kind_or_field_is_rejected() {
        assert!(serde_json::from_value::<BatchCheckSpec>(json!({"kind": "agent_says_done"})).is_err());
        assert!(serde_json::from_value::<BatchCheckSpec>(json!({"kind": "file_exists", "path": "a", "x": 1})).is_err());
    }
}

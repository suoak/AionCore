use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One completed-turn spend row. Occupancy-only snapshots are never stored.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct UsageEventRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub recorded_at: TimestampMs,
    pub fingerprint: String,
    pub backend: String,
    pub conversation_source: String,
    pub conversation_name: Option<String>,
    pub assistant_id: Option<String>,
    pub assistant_name: Option<String>,
    pub model_id: Option<String>,
    pub turn_id: Option<String>,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub cost_delta: f64,
    pub session_cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
    pub event_source: String,
    /// M7 E5-1 (migration 067): task linkage; NULL for chat turns and older rows.
    #[sqlx(default)]
    pub task_session_id: Option<String>,
    #[sqlx(default)]
    pub run_id: Option<String>,
    /// `reported` or `unavailable` (provider gave no usage for the turn).
    #[sqlx(default)]
    pub usage_status: String,
}

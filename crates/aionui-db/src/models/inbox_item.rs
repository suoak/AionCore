use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One inbox entry (M7 E5-4).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, PartialEq)]
pub struct InboxItemRow {
    pub id: String,
    pub user_id: String,
    pub kind: String,
    pub source_kind: String,
    pub source_id: String,
    pub conversation_id: Option<String>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub dedupe_key: String,
    pub created_at: TimestampMs,
    pub read_at: Option<TimestampMs>,
    pub resolved_at: Option<TimestampMs>,
}

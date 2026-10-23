use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One inbox entry (M7 E5-4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxItemDto {
    pub id: String,
    /// `approval` | `blocked` | `completed` | `failed` | `question` | `mention`.
    pub kind: String,
    pub source_kind: String,
    pub source_id: String,
    pub conversation_id: Option<String>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub created_at: TimestampMs,
    pub read_at: Option<TimestampMs>,
    pub resolved_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InboxListQuery {
    #[serde(default)]
    pub unread_only: Option<bool>,
    /// Cursor: only items created strictly before this timestamp.
    #[serde(default)]
    pub before: Option<TimestampMs>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxListResponse {
    pub items: Vec<InboxItemDto>,
    pub unread_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxMarkReadResponse {
    pub updated: u64,
    pub unread_count: i64,
}

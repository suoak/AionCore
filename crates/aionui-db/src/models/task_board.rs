//! Work-item board rows (M8 E6-2, migration 072).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Board columns, in display order. `cancelled` is hidden by default in the UI.
pub const BOARD_STATUSES: [&str; 6] = ["backlog", "todo", "in_progress", "in_review", "done", "cancelled"];
/// Assignee kinds; `None` means unassigned.
pub const BOARD_ASSIGNEE_KINDS: [&str; 3] = ["user", "agent", "squad"];
/// Highest priority value (0 none … 4 urgent).
pub const BOARD_MAX_PRIORITY: i64 = 4;
pub const TASK_COMMENT_AUTHOR_KINDS: [&str; 3] = ["user", "agent", "system"];
pub const TASK_COMMENT_KINDS: [&str; 4] = ["comment", "status_change", "assignment", "review_decision"];
/// Authors may correct a comment body for this long after posting.
pub const TASK_COMMENT_EDIT_WINDOW_MS: i64 = 15 * 60 * 1000;

/// One work item as the board shows it: the task_session plus its board fields.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskBoardItemRow {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub conversation_id: Option<String>,
    pub mode: String,
    /// Run state (task_sessions.status), independent of the board column.
    pub status: String,
    pub board_status: String,
    pub assignee_kind: Option<String>,
    pub assignee_id: Option<String>,
    pub priority: i64,
    /// JSON array of label strings.
    pub labels: String,
    pub board_rank: Option<String>,
    pub external_ref: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskCommentRow {
    pub id: String,
    pub task_session_id: String,
    pub user_id: String,
    pub author_kind: String,
    pub author_id: String,
    pub kind: String,
    pub body: String,
    pub created_at: TimestampMs,
    pub edited_at: Option<TimestampMs>,
}

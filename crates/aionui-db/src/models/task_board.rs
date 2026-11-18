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
    /// 3.0 task flow (migration 082). `None` on rows created before it.
    pub source: Option<String>,
    pub assistant_id: Option<String>,
    /// 0 / 1.
    pub plan_first: i64,
    pub queued_at: Option<TimestampMs>,
}

/// `task_sessions.source` values (migration 082).
pub const TASK_SOURCES: [&str; 5] = ["manual", "conversation", "github", "cron", "agent"];

/// The task-flow columns of one task (migration 082), as stored.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct TaskFlowRow {
    pub assistant_id: Option<String>,
    pub prompt_json: Option<String>,
    pub repo_path: Option<String>,
    pub base_ref: Option<String>,
    pub plan_first: i64,
    pub source: Option<String>,
    pub source_ref: Option<String>,
    pub fanout_group_id: Option<String>,
    pub delivery_json: Option<String>,
    pub queued_at: Option<TimestampMs>,
}

/// Latest persisted ACP plan message of a conversation (`messages.type = 'plan'`).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct LatestPlanMessageRow {
    pub conversation_id: String,
    pub content: String,
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

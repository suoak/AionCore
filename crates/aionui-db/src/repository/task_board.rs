use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{LatestPlanMessageRow, TaskBoardItemRow, TaskCommentRow, TaskFlowRow};

/// Board filter; every `Some` narrows the result.
#[derive(Debug, Clone, Default)]
pub struct TaskBoardFilter<'a> {
    pub project_id: Option<&'a str>,
    pub board_status: Option<&'a str>,
    pub assignee_kind: Option<&'a str>,
    pub assignee_id: Option<&'a str>,
    /// Item must carry this label.
    pub label: Option<&'a str>,
}

/// Assignee value for [`UpdateTaskBoardParams::assignee`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardAssignee<'a> {
    pub kind: &'a str,
    pub id: &'a str,
}

/// Partial update of the board fields. `None` leaves a field unchanged;
/// `assignee: Some(None)` / `board_rank: Some(None)` clear it.
///
/// This layer only validates values. Workflow rules (which column moves are
/// legal, the review gate, plan-approval precondition) belong to the board
/// service (E6-3).
#[derive(Debug, Clone, Default)]
pub struct UpdateTaskBoardParams<'a> {
    pub board_status: Option<&'a str>,
    pub board_rank: Option<Option<&'a str>>,
    pub assignee: Option<Option<BoardAssignee<'a>>>,
    pub priority: Option<i64>,
    pub labels: Option<&'a [String]>,
    pub updated_at: TimestampMs,
    /// Optimistic concurrency guard: when set, the update applies only if the
    /// row's `updated_at` still equals this value. A mismatch on an existing
    /// row fails with `DbError::Conflict` starting with
    /// [`BOARD_STALE_CONFLICT_PREFIX`].
    pub expected_updated_at: Option<TimestampMs>,
}

/// Message prefix of the `DbError::Conflict` returned when
/// `UpdateTaskBoardParams::expected_updated_at` no longer matches.
pub const BOARD_STALE_CONFLICT_PREFIX: &str = "stale board item";

/// 3.0 task-flow fields written at creation (migration 082). `None` keeps
/// the column NULL; the service validates values before calling.
#[derive(Debug, Clone, Default)]
pub struct SetTaskFlowParams<'a> {
    pub assistant_id: Option<&'a str>,
    /// JSON object text.
    pub prompt_json: Option<&'a str>,
    pub repo_path: Option<&'a str>,
    pub base_ref: Option<&'a str>,
    pub plan_first: bool,
    pub source: Option<&'a str>,
    pub source_ref: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct AppendTaskCommentParams<'a> {
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub author_kind: &'a str,
    pub author_id: &'a str,
    pub kind: &'a str,
    pub body: &'a str,
    pub created_at: TimestampMs,
}

#[async_trait::async_trait]
pub trait ITaskBoardRepository: Send + Sync {
    /// Board items for a user, ordered by column, then rank (unranked last),
    /// then most recently updated.
    async fn list_board(&self, user_id: &str, filter: &TaskBoardFilter<'_>) -> Result<Vec<TaskBoardItemRow>, DbError>;
    async fn get_board_item(&self, user_id: &str, task_session_id: &str) -> Result<Option<TaskBoardItemRow>, DbError>;
    /// Apply a partial board update. `DbError::NotFound` for another user's or
    /// a missing task; `DbError::Conflict` for out-of-range values (same
    /// convention as `set_retry_policy`).
    async fn update_board(
        &self,
        user_id: &str,
        task_session_id: &str,
        params: &UpdateTaskBoardParams<'_>,
    ) -> Result<TaskBoardItemRow, DbError>;
    /// Set the read-only import source. `DbError::Conflict` if this user
    /// already has another item with the same `external_ref`.
    async fn set_external_ref(
        &self,
        user_id: &str,
        task_session_id: &str,
        external_ref: &str,
        updated_at: TimestampMs,
    ) -> Result<TaskBoardItemRow, DbError>;
    async fn find_by_external_ref(
        &self,
        user_id: &str,
        external_ref: &str,
    ) -> Result<Option<TaskBoardItemRow>, DbError>;
    /// 3.0 C6: newest task of `source` / `source_ref` that is not done or
    /// cancelled (a cron job's previous occurrence still on the board).
    async fn find_open_task_by_source(
        &self,
        user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<String>, DbError>;
    /// Write the task-flow fields of a task (does not touch `updated_at`).
    /// `DbError::NotFound` for another user's or a missing task;
    /// `DbError::Conflict` for an unknown `source`.
    async fn set_task_flow(
        &self,
        user_id: &str,
        task_session_id: &str,
        params: &SetTaskFlowParams<'_>,
    ) -> Result<TaskFlowRow, DbError>;
    async fn get_task_flow(&self, user_id: &str, task_session_id: &str) -> Result<Option<TaskFlowRow>, DbError>;
    /// Newest `plan` message per conversation, only for this user's
    /// conversations. Conversations without a plan are absent.
    async fn latest_plan_messages(
        &self,
        user_id: &str,
        conversation_ids: &[String],
    ) -> Result<Vec<LatestPlanMessageRow>, DbError>;
    /// 3.0 C2: tasks whose run is live, across all users (the run-slot
    /// limit is per Host).
    async fn count_running_tasks(&self) -> Result<i64, DbError>;
    /// Put a task in (`Some(at)`) or take it out of (`None`) the run queue.
    async fn set_task_queued(
        &self,
        user_id: &str,
        task_session_id: &str,
        queued_at: Option<TimestampMs>,
    ) -> Result<bool, DbError>;
    /// Oldest queued tasks first, as `(user_id, task_session_id)`.
    async fn list_queued_tasks(&self, limit: i64) -> Result<Vec<(String, String)>, DbError>;
    /// Startable (`draft|ready|paused|failed`) → `running`, dequeued, bound to
    /// its conversation / fan-out group. `false` when the task was not in a
    /// startable state (someone else started it).
    async fn mark_task_started(
        &self,
        user_id: &str,
        task_session_id: &str,
        conversation_id: Option<&str>,
        fanout_group_id: Option<&str>,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;
    /// 3.0 C4: a plan-first task got its conversation; it stays startable
    /// (`ready`) so the strict planning run can claim it. `false` when it is
    /// not a startable Plan task.
    /// A worktree task binds its fan-out group first and its slot
    /// conversation once the slot starts (`None` keeps the stored value).
    async fn bind_task_for_planning(
        &self,
        user_id: &str,
        task_session_id: &str,
        conversation_id: Option<&str>,
        fanout_group_id: Option<&str>,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;
    /// 3.0 C4: planning could not start → `paused` (needs you). `ready` only.
    async fn pause_ready_task(
        &self,
        user_id: &str,
        task_session_id: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;
    /// The started (agent-mode, `running`) task whose run lives in this
    /// conversation, directly or as the slot of its fan-out group.
    async fn find_running_task_for_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<String>, DbError>;
    /// `running` → `status` (`completed` | `failed`), binding the run's
    /// conversation. `false` when the task was no longer running.
    async fn settle_started_task(
        &self,
        user_id: &str,
        task_session_id: &str,
        conversation_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;
    /// Replace the task's delivery record (JSON object text, or clear it).
    async fn set_task_delivery(
        &self,
        user_id: &str,
        task_session_id: &str,
        delivery_json: Option<&str>,
    ) -> Result<bool, DbError>;
    /// A finished run (`completed|failed|paused`) → `running` again for a
    /// follow-up in the same conversation. `false` when not resumable.
    async fn mark_task_resumed(
        &self,
        user_id: &str,
        task_session_id: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;
    async fn append_comment(&self, params: &AppendTaskCommentParams<'_>) -> Result<TaskCommentRow, DbError>;
    /// Comments oldest first.
    async fn list_comments(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskCommentRow>, DbError>;
    /// Correct a comment body. Only the author of a plain `comment`, within
    /// [`crate::models::TASK_COMMENT_EDIT_WINDOW_MS`] of posting; otherwise
    /// `DbError::Conflict`.
    async fn edit_comment(
        &self,
        user_id: &str,
        task_session_id: &str,
        comment_id: &str,
        author_id: &str,
        body: &str,
        edited_at: TimestampMs,
    ) -> Result<TaskCommentRow, DbError>;
}

use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{TaskBoardItemRow, TaskCommentRow};

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

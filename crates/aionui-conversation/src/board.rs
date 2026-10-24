//! Work-item board service (M8 E6-3, D6-1: lives in aionui-conversation).
//!
//! The board column (`board_status`) is the user-facing workflow state. This
//! module owns the column state machine and the review gate; the DB layer only
//! validates values.
//!
//! Rules (E6-tasks-board.md §2.2):
//! - `done` is reachable only through the review endpoint (`in_review → done`)
//!   and only for a user credential. Agent runtime tokens are denied on
//!   `/api/task-sessions/*` by the agent scope; [`BoardActor::Agent`] is the
//!   service-side second check and may only move `in_progress → in_review`.
//! - `backlog` never starts work: `backlog → in_progress` is rejected; items go
//!   through `todo`.
//! - `plan` / `goal` items need an approved plan/goal approval before
//!   `in_progress`.
//! - Review reject sends the item back to `todo` (D6-5) with a mandatory
//!   comment.
//!
//! Realtime: every successful board change pushes `task.board.updated`, and
//! every timeline entry (comments and the status/assignment/review events)
//! pushes `task.comment.created`; comment edits push `task.comment.updated`.
//! Events are published only after the write succeeded.
//!
//! Concurrency: `UpdateTaskBoardRequest::expected_updated_at` is an optional
//! optimistic lock. When given, the row must still carry that `updated_at`
//! (checked before the state machine and again atomically in the UPDATE);
//! otherwise the call fails with 409 `board_item_stale`. Omitted, the update
//! stays last-write-wins.

use aionui_api_types::{
    EVENT_TASK_BOARD_UPDATED, EVENT_TASK_COMMENT_CREATED, EVENT_TASK_COMMENT_UPDATED, TaskBoardChange,
    TaskBoardUpdatedEvent, TaskCommentEvent, WebSocketMessage,
};
use aionui_api_types::{
    TASK_BOARD_COLUMNS, TaskAssignee, TaskAssigneeKind, TaskBoardColumnResponse, TaskBoardItemResponse, TaskBoardQuery,
    TaskBoardResponse, TaskBoardReviewDecision, TaskBoardReviewRequest, TaskBoardReviewResponse, TaskBoardStatus,
    TaskCommentKind, TaskCommentResponse, UpdateTaskBoardRequest,
};
use aionui_common::now_ms;
use aionui_db::models::{TaskBoardItemRow, TaskCommentRow};
use aionui_db::{
    AppendTaskCommentParams, BOARD_STALE_CONFLICT_PREFIX, BoardAssignee, DbError, TaskBoardFilter,
    UpdateTaskBoardParams,
};

use crate::error::ConversationError;
use crate::service::ConversationService;

pub const BOARD_MAX_LABELS: usize = 20;
pub const BOARD_MAX_LABEL_CHARS: usize = 50;
pub const BOARD_MAX_RANK_CHARS: usize = 64;
pub const BOARD_MAX_COMMENT_CHARS: usize = 10_000;

/// Who is asking for a column move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardActor {
    User,
    Agent,
}

/// Why a column move was refused. `code` is stable for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRejection {
    pub code: &'static str,
    pub reason: String,
}

fn reject(code: &'static str, reason: impl Into<String>) -> BoardRejection {
    BoardRejection {
        code,
        reason: reason.into(),
    }
}

/// Facts the state machine needs about the item.
#[derive(Debug, Clone, Copy)]
pub struct TransitionContext<'a> {
    pub mode: &'a str,
    pub plan_approved: bool,
}

/// Column moves through `PATCH /board` (not the review endpoint).
pub fn check_transition(
    from: TaskBoardStatus,
    to: TaskBoardStatus,
    actor: BoardActor,
    ctx: TransitionContext<'_>,
) -> Result<(), BoardRejection> {
    use TaskBoardStatus::*;
    if from == to {
        return Ok(());
    }
    if actor == BoardActor::Agent {
        return if from == InProgress && to == InReview {
            Ok(())
        } else {
            Err(reject(
                "agent_review_gate",
                "An agent may only submit an in-progress item for review",
            ))
        };
    }
    if to == Done {
        return Err(reject(
            "review_required",
            "Items move to done only by approving the review",
        ));
    }
    if from == InReview && to == Todo {
        return Err(reject(
            "review_required",
            "Send an item back from review by rejecting the review with a comment",
        ));
    }
    let allowed = match from {
        Backlog => matches!(to, Todo | Cancelled),
        Todo => matches!(to, Backlog | InProgress | Cancelled),
        InProgress => matches!(to, Todo | InReview | Cancelled),
        InReview => matches!(to, Cancelled),
        Done => false,
        Cancelled => matches!(to, Backlog),
    };
    if !allowed {
        let code = if from == Backlog && to == InProgress {
            "backlog_not_started"
        } else {
            "not_allowed"
        };
        return Err(reject(
            code,
            format!("Cannot move from {} to {}", from.as_str(), to.as_str()),
        ));
    }
    if to == InProgress && matches!(ctx.mode, "plan" | "goal") && !ctx.plan_approved {
        return Err(reject(
            "plan_not_approved",
            format!("A {} item needs an approved {} before it can start", ctx.mode, ctx.mode),
        ));
    }
    Ok(())
}

/// Review decision: `in_review → done` (approve) or `→ todo` (reject).
pub fn check_review(from: TaskBoardStatus, actor: BoardActor) -> Result<(), BoardRejection> {
    if actor != BoardActor::User {
        return Err(reject("agent_review_gate", "Only a user can decide a review"));
    }
    if from != TaskBoardStatus::InReview {
        return Err(reject("not_in_review", "Only an item in review can be decided"));
    }
    Ok(())
}

fn rejection_error(from: TaskBoardStatus, to: TaskBoardStatus, rejection: BoardRejection) -> ConversationError {
    ConversationError::BoardTransitionRejected {
        from: from.as_str().into(),
        to: to.as_str().into(),
        code: rejection.code,
        reason: rejection.reason,
    }
}

fn item_response(row: TaskBoardItemRow) -> Result<TaskBoardItemResponse, ConversationError> {
    let board_status = TaskBoardStatus::parse(&row.board_status)
        .ok_or_else(|| ConversationError::internal(format!("Unknown board_status '{}'", row.board_status)))?;
    let assignee = match (row.assignee_kind.as_deref(), row.assignee_id) {
        (Some(kind), Some(id)) => TaskAssigneeKind::parse(kind).map(|kind| TaskAssignee { kind, id }),
        _ => None,
    };
    let labels = serde_json::from_str::<Vec<String>>(&row.labels).unwrap_or_default();
    Ok(TaskBoardItemResponse {
        id: row.id,
        title: row.title,
        project_id: row.project_id,
        conversation_id: row.conversation_id,
        mode: row.mode,
        status: row.status,
        board_status,
        assignee,
        priority: row.priority,
        labels,
        board_rank: row.board_rank,
        external_ref: row.external_ref,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn comment_kind(kind: &str) -> TaskCommentKind {
    match kind {
        "status_change" => TaskCommentKind::StatusChange,
        "assignment" => TaskCommentKind::Assignment,
        "review_decision" => TaskCommentKind::ReviewDecision,
        _ => TaskCommentKind::Comment,
    }
}

fn comment_response(row: TaskCommentRow) -> TaskCommentResponse {
    TaskCommentResponse {
        kind: comment_kind(&row.kind),
        id: row.id,
        task_session_id: row.task_session_id,
        author_kind: row.author_kind,
        author_id: row.author_id,
        body: row.body,
        created_at: row.created_at,
        edited_at: row.edited_at,
    }
}

fn task_not_found(id: &str) -> ConversationError {
    ConversationError::not_found_reason(format!("Task session '{id}' not found"))
}

fn stale_error(id: &str, expected: i64, current: Option<i64>) -> ConversationError {
    ConversationError::BoardItemStale {
        id: id.to_owned(),
        expected,
        current,
    }
}

fn map_db(id: &str, error: DbError) -> ConversationError {
    match error {
        DbError::NotFound(_) => task_not_found(id),
        DbError::Conflict(reason) => ConversationError::bad_request(reason),
        other => other.into(),
    }
}

fn validate_comment_body(body: &str) -> Result<&str, ConversationError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ConversationError::bad_request("Comment body must not be empty"));
    }
    if body.chars().count() > BOARD_MAX_COMMENT_CHARS {
        return Err(ConversationError::bad_request(format!(
            "Comment body must be at most {BOARD_MAX_COMMENT_CHARS} characters"
        )));
    }
    Ok(body)
}

fn validate_labels(labels: &[String]) -> Result<Vec<String>, ConversationError> {
    if labels.len() > BOARD_MAX_LABELS {
        return Err(ConversationError::bad_request(format!(
            "At most {BOARD_MAX_LABELS} labels"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(labels.len());
    for label in labels {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > BOARD_MAX_LABEL_CHARS {
            return Err(ConversationError::bad_request(format!(
                "Labels must be 1 to {BOARD_MAX_LABEL_CHARS} characters"
            )));
        }
        if !out.iter().any(|existing| existing == label) {
            out.push(label.to_string());
        }
    }
    Ok(out)
}

impl ConversationService {
    async fn board_item(&self, user_id: &str, id: &str) -> Result<TaskBoardItemRow, ConversationError> {
        self.task_board_repo()?
            .get_board_item(user_id, id)
            .await?
            .ok_or_else(|| task_not_found(id))
    }

    async fn plan_approved(&self, user_id: &str, id: &str, mode: &str) -> Result<bool, ConversationError> {
        let approvals = self.task_session_repo()?.list_approvals(user_id, id).await?;
        Ok(approvals
            .iter()
            .any(|approval| approval.status == "approved" && approval.approval_type == mode))
    }

    async fn append_event(
        &self,
        user_id: &str,
        id: &str,
        kind: &str,
        body: serde_json::Value,
        now: i64,
    ) -> Result<TaskCommentRow, ConversationError> {
        let body = body.to_string();
        self.task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "user",
                author_id: user_id,
                kind,
                body: &body,
                created_at: now,
            })
            .await
            .map_err(|error| map_db(id, error))
            .inspect(|row| self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(row.clone())))
    }

    fn publish_board_updated(
        &self,
        user_id: &str,
        change: TaskBoardChange,
        from: TaskBoardStatus,
        item: &TaskBoardItemResponse,
    ) {
        let event = TaskBoardUpdatedEvent {
            user_id: user_id.to_owned(),
            task_session_id: item.id.clone(),
            change,
            from_board_status: from,
            item: item.clone(),
        };
        match serde_json::to_value(event) {
            Ok(data) => self
                .broadcaster()
                .broadcast(WebSocketMessage::new(EVENT_TASK_BOARD_UPDATED, data)),
            Err(error) => tracing::warn!(%error, "failed to serialize task.board.updated"),
        }
    }

    fn publish_comment(&self, name: &str, user_id: &str, comment: TaskCommentResponse) {
        let event = TaskCommentEvent {
            user_id: user_id.to_owned(),
            task_session_id: comment.task_session_id.clone(),
            comment,
        };
        match serde_json::to_value(event) {
            Ok(data) => self.broadcaster().broadcast(WebSocketMessage::new(name, data)),
            Err(error) => tracing::warn!(%error, name, "failed to serialize task comment event"),
        }
    }

    /// `GET /api/task-sessions/board`.
    pub async fn task_board(
        &self,
        user_id: &str,
        query: TaskBoardQuery,
    ) -> Result<TaskBoardResponse, ConversationError> {
        let filter = TaskBoardFilter {
            project_id: query.project_id.as_deref(),
            board_status: None,
            assignee_kind: query.assignee_kind.map(TaskAssigneeKind::as_str),
            assignee_id: query.assignee_id.as_deref(),
            label: query.label.as_deref(),
        };
        let rows = self.task_board_repo()?.list_board(user_id, &filter).await?;
        let mut columns: Vec<TaskBoardColumnResponse> = TASK_BOARD_COLUMNS
            .into_iter()
            .map(|board_status| TaskBoardColumnResponse {
                board_status,
                items: Vec::new(),
            })
            .collect();
        for row in rows {
            let item = item_response(row)?;
            if let Some(column) = columns
                .iter_mut()
                .find(|column| column.board_status == item.board_status)
            {
                column.items.push(item);
            }
        }
        Ok(TaskBoardResponse { columns })
    }

    /// `PATCH /api/task-sessions/{id}/board`: move, rank, assign, prioritize,
    /// label. Column and assignee changes are recorded on the timeline.
    pub async fn update_task_board(
        &self,
        user_id: &str,
        id: &str,
        actor: BoardActor,
        request: UpdateTaskBoardRequest,
    ) -> Result<TaskBoardItemResponse, ConversationError> {
        let current = self.board_item(user_id, id).await?;
        if let Some(expected) = request.expected_updated_at
            && expected != current.updated_at
        {
            return Err(stale_error(id, expected, Some(current.updated_at)));
        }
        let from = TaskBoardStatus::parse(&current.board_status)
            .ok_or_else(|| ConversationError::internal("Unknown board_status"))?;
        if actor == BoardActor::Agent
            && (request.board_rank.is_some()
                || request.assignee.is_some()
                || request.priority.is_some()
                || request.labels.is_some())
        {
            return Err(rejection_error(
                from,
                request.board_status.unwrap_or(from),
                reject("agent_review_gate", "An agent may only submit an item for review"),
            ));
        }
        if let Some(to) = request.board_status {
            let plan_approved = if to == TaskBoardStatus::InProgress && from != to {
                self.plan_approved(user_id, id, &current.mode).await?
            } else {
                false
            };
            check_transition(
                from,
                to,
                actor,
                TransitionContext {
                    mode: &current.mode,
                    plan_approved,
                },
            )
            .map_err(|rejection| rejection_error(from, to, rejection))?;
        }
        if let Some(priority) = request.priority
            && !(0..=aionui_db::models::BOARD_MAX_PRIORITY).contains(&priority)
        {
            return Err(ConversationError::bad_request(format!(
                "priority must be between 0 and {}",
                aionui_db::models::BOARD_MAX_PRIORITY
            )));
        }
        if let Some(Some(rank)) = &request.board_rank
            && (rank.is_empty() || rank.chars().count() > BOARD_MAX_RANK_CHARS)
        {
            return Err(ConversationError::bad_request(format!(
                "board_rank must be 1 to {BOARD_MAX_RANK_CHARS} characters"
            )));
        }
        if let Some(Some(assignee)) = &request.assignee
            && assignee.id.trim().is_empty()
        {
            return Err(ConversationError::bad_request("assignee.id must not be empty"));
        }
        let labels = request.labels.as_deref().map(validate_labels).transpose()?;
        // With a lock the new value must differ from the guarded one, or two
        // writes in the same millisecond would both pass the next guard.
        let now = match request.expected_updated_at {
            Some(expected) => now_ms().max(expected + 1),
            None => now_ms(),
        };
        let params = UpdateTaskBoardParams {
            board_status: request.board_status.map(TaskBoardStatus::as_str),
            board_rank: request.board_rank.as_ref().map(|rank| rank.as_deref()),
            assignee: request.assignee.as_ref().map(|assignee| {
                assignee.as_ref().map(|assignee| BoardAssignee {
                    kind: assignee.kind.as_str(),
                    id: assignee.id.trim(),
                })
            }),
            priority: request.priority,
            labels: labels.as_deref(),
            updated_at: now,
            expected_updated_at: request.expected_updated_at,
        };
        let row = self
            .task_board_repo()?
            .update_board(user_id, id, &params)
            .await
            .map_err(|error| match (error, request.expected_updated_at) {
                (DbError::Conflict(message), Some(expected)) if message.starts_with(BOARD_STALE_CONFLICT_PREFIX) => {
                    stale_error(id, expected, None)
                }
                (error, _) => map_db(id, error),
            })?;
        if let Some(to) = request.board_status
            && to != from
        {
            self.append_event(
                user_id,
                id,
                "status_change",
                serde_json::json!({ "from": from.as_str(), "to": to.as_str() }),
                now,
            )
            .await?;
        }
        if let Some(assignee) = &request.assignee {
            let previous = current.assignee_kind.as_deref().zip(current.assignee_id.as_deref());
            let next = assignee.as_ref().map(|value| (value.kind.as_str(), value.id.trim()));
            if previous != next {
                self.append_event(
                    user_id,
                    id,
                    "assignment",
                    serde_json::json!({ "assignee": assignee }),
                    now,
                )
                .await?;
            }
        }
        let item = item_response(row)?;
        self.publish_board_updated(user_id, TaskBoardChange::Board, from, &item);
        Ok(item)
    }

    /// `POST /api/task-sessions/{id}/review` (user credential only).
    pub async fn review_task_board(
        &self,
        user_id: &str,
        id: &str,
        actor: BoardActor,
        request: TaskBoardReviewRequest,
    ) -> Result<TaskBoardReviewResponse, ConversationError> {
        let current = self.board_item(user_id, id).await?;
        let from = TaskBoardStatus::parse(&current.board_status)
            .ok_or_else(|| ConversationError::internal("Unknown board_status"))?;
        let to = match request.decision {
            TaskBoardReviewDecision::Approve => TaskBoardStatus::Done,
            TaskBoardReviewDecision::Reject => TaskBoardStatus::Todo,
        };
        check_review(from, actor).map_err(|rejection| rejection_error(from, to, rejection))?;
        let comment = request
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|comment| !comment.is_empty());
        if request.decision == TaskBoardReviewDecision::Reject && comment.is_none() {
            return Err(ConversationError::bad_request("Rejecting a review needs a comment"));
        }
        if let Some(comment) = comment {
            validate_comment_body(comment)?;
        }
        let now = now_ms();
        let row = self
            .task_board_repo()?
            .update_board(
                user_id,
                id,
                &UpdateTaskBoardParams {
                    board_status: Some(to.as_str()),
                    updated_at: now,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| map_db(id, error))?;
        let decision = self
            .append_event(
                user_id,
                id,
                "review_decision",
                serde_json::json!({ "decision": request.decision, "comment": comment }),
                now,
            )
            .await?;
        let item = item_response(row)?;
        self.publish_board_updated(user_id, TaskBoardChange::Review, from, &item);
        Ok(TaskBoardReviewResponse {
            item,
            decision: comment_response(decision),
        })
    }

    pub async fn list_task_comments(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Vec<TaskCommentResponse>, ConversationError> {
        self.board_item(user_id, id).await?;
        let rows = self.task_board_repo()?.list_comments(user_id, id).await?;
        Ok(rows.into_iter().map(comment_response).collect())
    }

    pub async fn create_task_comment(
        &self,
        user_id: &str,
        id: &str,
        body: &str,
    ) -> Result<TaskCommentResponse, ConversationError> {
        let body = validate_comment_body(body)?;
        let row = self
            .task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "user",
                author_id: user_id,
                kind: "comment",
                body,
                created_at: now_ms(),
            })
            .await
            .map_err(|error| map_db(id, error))?;
        let comment = comment_response(row);
        self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment.clone());
        Ok(comment)
    }

    /// Author-only body correction within the edit window; otherwise 409.
    pub async fn edit_task_comment(
        &self,
        user_id: &str,
        id: &str,
        comment_id: &str,
        body: &str,
    ) -> Result<TaskCommentResponse, ConversationError> {
        let body = validate_comment_body(body)?;
        let row = self
            .task_board_repo()?
            .edit_comment(user_id, id, comment_id, user_id, body, now_ms())
            .await
            .map_err(|error| match error {
                DbError::NotFound(_) => {
                    ConversationError::not_found_reason(format!("Comment '{comment_id}' not found"))
                }
                DbError::Conflict(reason) => ConversationError::Busy { reason },
                other => other.into(),
            })?;
        let comment = comment_response(row);
        self.publish_comment(EVENT_TASK_COMMENT_UPDATED, user_id, comment.clone());
        Ok(comment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskBoardStatus::*;

    const AGENT_MODE: TransitionContext<'static> = TransitionContext {
        mode: "agent",
        plan_approved: false,
    };

    fn user(from: TaskBoardStatus, to: TaskBoardStatus) -> Result<(), &'static str> {
        check_transition(from, to, BoardActor::User, AGENT_MODE).map_err(|rejection| rejection.code)
    }

    #[test]
    fn user_moves_follow_the_column_graph() {
        assert_eq!(user(Backlog, Todo), Ok(()));
        assert_eq!(user(Todo, InProgress), Ok(()));
        assert_eq!(user(InProgress, InReview), Ok(()));
        assert_eq!(user(InProgress, Todo), Ok(()));
        assert_eq!(user(Cancelled, Backlog), Ok(()));
        for from in TASK_BOARD_COLUMNS {
            if from != Done && from != Cancelled {
                assert_eq!(user(from, Cancelled), Ok(()), "{from:?}");
            }
        }
        assert_eq!(user(Done, Todo), Err("not_allowed"));
        assert_eq!(user(Cancelled, InProgress), Err("not_allowed"));
    }

    #[test]
    fn done_needs_review_and_backlog_never_starts() {
        for from in TASK_BOARD_COLUMNS {
            if from != Done {
                assert_eq!(user(from, Done), Err("review_required"), "{from:?}");
            }
        }
        assert_eq!(user(InReview, Todo), Err("review_required"));
        assert_eq!(user(Backlog, InProgress), Err("backlog_not_started"));
    }

    #[test]
    fn plan_items_need_approval_before_in_progress() {
        let unapproved = TransitionContext {
            mode: "plan",
            plan_approved: false,
        };
        let approved = TransitionContext {
            mode: "plan",
            plan_approved: true,
        };
        assert_eq!(
            check_transition(Todo, InProgress, BoardActor::User, unapproved)
                .unwrap_err()
                .code,
            "plan_not_approved"
        );
        let goal = TransitionContext {
            mode: "goal",
            plan_approved: false,
        };
        assert!(check_transition(Todo, InProgress, BoardActor::User, goal).is_err());
        assert!(check_transition(Todo, InProgress, BoardActor::User, approved).is_ok());
    }

    #[test]
    fn agent_may_only_submit_for_review() {
        assert!(check_transition(InProgress, InReview, BoardActor::Agent, AGENT_MODE).is_ok());
        for (from, to) in [
            (InReview, Done),
            (InProgress, Done),
            (Todo, InProgress),
            (Backlog, Todo),
        ] {
            assert_eq!(
                check_transition(from, to, BoardActor::Agent, AGENT_MODE)
                    .unwrap_err()
                    .code,
                "agent_review_gate"
            );
        }
        assert_eq!(
            check_review(InReview, BoardActor::Agent).unwrap_err().code,
            "agent_review_gate"
        );
        assert!(check_review(InReview, BoardActor::User).is_ok());
        assert_eq!(
            check_review(InProgress, BoardActor::User).unwrap_err().code,
            "not_in_review"
        );
    }

    #[test]
    fn labels_are_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            validate_labels(&[" a ".into(), "a".into(), "b".into()]).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(validate_labels(&["".into()]).is_err());
        assert!(validate_labels(&vec!["x".to_string(); BOARD_MAX_LABELS + 1]).is_err());
    }
}

//! 3.0 C3: after a task run — follow-up ("request changes"), adopt → done,
//! adopt conflict → needs you.
//!
//! - `POST /api/task-sessions/{id}/follow-up` sends the user's note to the
//!   same conversation (and so the same worktree) and moves the card back to
//!   `in_progress`. Same governance check and run-slot limit as start.
//! - A successful adopt of the task's fan-out group (always a user action)
//!   records `delivery.adopt` and approves the review: `in_review → done`
//!   with the usual review record, so `review_required` still holds.
//! - An adopt that stops on a merge conflict records `delivery.adopt.status =
//!   "conflict"`; the lane becomes needs_you / merge_conflict. Nothing is
//!   merged automatically.

use std::sync::atomic::Ordering;

use aionui_api_types::{
    StartTaskResponse, TaskBoardChange, TaskBoardReviewDecision, TaskBoardReviewRequest, TaskBoardStatus,
    TaskStartState,
};
use aionui_common::now_ms;
use aionui_db::UpdateTaskBoardParams;
use serde_json::{Map, Value, json};

use super::{BoardActor, item_response, map_db};
use crate::error::ConversationError;
use crate::service::ConversationService;

pub const FOLLOW_UP_MAX_CHARS: usize = 32 * 1024;
const CONFLICT_MESSAGE_MAX_CHARS: usize = 2000;

/// What the fan-out reported for a task's adopt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskAdopted {
    pub group_id: String,
    pub strategy: String,
    pub branch: String,
    pub target_branch: Option<String>,
    pub result_commit: Option<String>,
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

impl ConversationService {
    async fn delivery_object(&self, user_id: &str, id: &str) -> Result<Map<String, Value>, ConversationError> {
        let flow = self
            .task_board_repo()?
            .get_task_flow(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        Ok(flow
            .delivery_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default())
    }

    async fn write_delivery(&self, user_id: &str, id: &str, map: Map<String, Value>) -> Result<(), ConversationError> {
        let raw = if map.is_empty() {
            None
        } else {
            Some(Value::Object(map).to_string())
        };
        self.task_board_repo()?
            .set_task_delivery(user_id, id, raw.as_deref())
            .await?;
        Ok(())
    }

    async fn move_board(
        &self,
        user_id: &str,
        id: &str,
        from: TaskBoardStatus,
        to: TaskBoardStatus,
    ) -> Result<(), ConversationError> {
        let row = self.board_item(user_id, id).await?;
        let now = now_ms().max(row.updated_at + 1);
        self.task_board_repo()?
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
        self.append_event(
            user_id,
            id,
            "status_change",
            json!({ "from": from.as_str(), "to": to.as_str() }),
            now,
        )
        .await?;
        Ok(())
    }

    fn publish_item(&self, user_id: &str, from: TaskBoardStatus, row: aionui_db::models::TaskBoardItemRow) {
        if let Ok(item) = item_response(row) {
            self.publish_board_updated(user_id, TaskBoardChange::Board, from, &item);
        }
    }

    /// Fan-out hook: the task's group was adopted by the user.
    pub async fn on_task_adopted(
        &self,
        user_id: &str,
        id: &str,
        adopted: &TaskAdopted,
    ) -> Result<(), ConversationError> {
        let mut delivery = self.delivery_object(user_id, id).await?;
        delivery.insert(
            "adopt".into(),
            json!({
                "status": "adopted",
                "group_id": adopted.group_id,
                "strategy": adopted.strategy,
                "branch": adopted.branch,
                "target_branch": adopted.target_branch,
                "result_commit": adopted.result_commit,
                "at": now_ms(),
            }),
        );
        self.write_delivery(user_id, id, delivery).await?;

        let row = self.board_item(user_id, id).await?;
        let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
        match from {
            TaskBoardStatus::InProgress => {
                self.move_board(user_id, id, from, TaskBoardStatus::InReview).await?;
            }
            TaskBoardStatus::InReview => {}
            // Done / cancelled / not started: record only.
            _ => {
                self.publish_item(user_id, from, row);
                return Ok(());
            }
        }
        let comment = match (&adopted.target_branch, &adopted.result_commit) {
            (Some(target), Some(commit)) => format!(
                "Adopted ({}) into {target} as {}",
                adopted.strategy,
                commit.get(..12).unwrap_or(commit)
            ),
            _ => format!("Adopted ({}), branch {} kept", adopted.strategy, adopted.branch),
        };
        self.review_task_board(
            user_id,
            id,
            BoardActor::User,
            TaskBoardReviewRequest {
                decision: TaskBoardReviewDecision::Approve,
                comment: Some(comment),
            },
        )
        .await?;
        tracing::info!(task_session_id = %id, group_id = %adopted.group_id, "task adopted → done");
        Ok(())
    }

    /// Fan-out hook: adopt stopped on a merge conflict (target left clean).
    pub async fn on_task_adopt_conflicted(
        &self,
        user_id: &str,
        id: &str,
        group_id: &str,
        message: &str,
    ) -> Result<(), ConversationError> {
        let mut delivery = self.delivery_object(user_id, id).await?;
        delivery.insert(
            "adopt".into(),
            json!({
                "status": "conflict",
                "group_id": group_id,
                "message": truncate(message, CONFLICT_MESSAGE_MAX_CHARS),
                "at": now_ms(),
            }),
        );
        self.write_delivery(user_id, id, delivery).await?;
        let row = self.board_item(user_id, id).await?;
        let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
        tracing::info!(task_session_id = %id, group_id, "task adopt conflicted → needs you");
        self.publish_item(user_id, from, row);
        Ok(())
    }

    /// `POST /api/task-sessions/{id}/follow-up`.
    pub async fn follow_up_task(
        &self,
        user_id: &str,
        id: &str,
        text: &str,
    ) -> Result<StartTaskResponse, ConversationError> {
        let text = text.trim();
        if text.is_empty() {
            return Err(ConversationError::bad_request("Write what should change"));
        }
        if text.chars().count() > FOLLOW_UP_MAX_CHARS {
            return Err(ConversationError::bad_request(format!(
                "A follow-up is limited to {FOLLOW_UP_MAX_CHARS} characters"
            )));
        }
        let _guard = self.task_start_lock.lock().await;
        let board = self.task_board_repo()?;
        let task = self
            .task_session_repo()?
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        let row = self.board_item(user_id, id).await?;
        if task.status == "running" {
            return item_response(row).map(|item| StartTaskResponse {
                state: TaskStartState::AlreadyRunning,
                item,
            });
        }
        let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
        if !matches!(from, TaskBoardStatus::InProgress | TaskBoardStatus::InReview)
            || !matches!(task.status.as_str(), "completed" | "failed" | "paused")
        {
            return Err(ConversationError::bad_request(
                "Only a task waiting for review or input can get a follow-up",
            ));
        }
        let conversation_id = task
            .conversation_id
            .clone()
            .ok_or_else(|| ConversationError::bad_request("The task has no conversation to continue"))?;
        let flow = board
            .get_task_flow(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;

        let mut facts = aionui_db::LaunchFacts::new(aionui_db::LaunchSurface::Task);
        facts.target = Some(("task_session", id));
        facts.assistant_id = flow.assistant_id.as_deref();
        self.enforce_launch_policy(user_id, &facts).await?;
        let running = usize::try_from(board.count_running_tasks().await?).unwrap_or(usize::MAX);
        if running >= self.max_running_tasks.load(Ordering::Relaxed) {
            return Err(ConversationError::Busy {
                reason: "Every task run slot is busy; try again when one finishes".into(),
            });
        }

        self.task_launch_port()?
            .follow_up(user_id, &conversation_id, text)
            .await?;
        if !board.mark_task_resumed(user_id, id, now_ms()).await? {
            return Err(ConversationError::Busy {
                reason: format!("Task session '{id}' changed while continuing"),
            });
        }
        // A new attempt supersedes the conflicted adopt.
        let mut delivery = self.delivery_object(user_id, id).await?;
        if crate::board::adopt_conflicted(Some(&Value::Object(delivery.clone()))) {
            delivery.remove("adopt");
            self.write_delivery(user_id, id, delivery).await?;
        }
        if from == TaskBoardStatus::InReview {
            self.move_board(user_id, id, from, TaskBoardStatus::InProgress).await?;
        }
        let row = self.board_item(user_id, id).await?;
        self.publish_item(user_id, from, row.clone());
        tracing::info!(task_session_id = %id, "task follow-up sent");
        item_response(row).map(|item| StartTaskResponse {
            state: TaskStartState::Started,
            item,
        })
    }
}

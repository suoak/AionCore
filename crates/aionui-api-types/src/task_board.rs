//! Work-item board (M8 E6-3) request/response types.
//!
//! Board columns (`board_status`) are the user-facing workflow state and are
//! separate from the run state in `TaskSessionResponse::status`.

use serde::{Deserialize, Deserializer, Serialize};

/// Board columns in display order.
pub const TASK_BOARD_COLUMNS: [TaskBoardStatus; 6] = [
    TaskBoardStatus::Backlog,
    TaskBoardStatus::Todo,
    TaskBoardStatus::InProgress,
    TaskBoardStatus::InReview,
    TaskBoardStatus::Done,
    TaskBoardStatus::Cancelled,
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardStatus {
    Backlog,
    Todo,
    InProgress,
    InReview,
    Done,
    Cancelled,
}

impl TaskBoardStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        TASK_BOARD_COLUMNS.into_iter().find(|status| status.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssigneeKind {
    User,
    Agent,
    Squad,
}

impl TaskAssigneeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Squad => "squad",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::User, Self::Agent, Self::Squad]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAssignee {
    pub kind: TaskAssigneeKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardItemResponse {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub conversation_id: Option<String>,
    /// `agent` | `plan` | `goal`.
    pub mode: String,
    /// Run state (`TaskSessionStatus` wire value), shown as a badge.
    pub status: String,
    pub board_status: TaskBoardStatus,
    pub assignee: Option<TaskAssignee>,
    /// 0 none, 1 low, 2 medium, 3 high, 4 urgent.
    pub priority: i64,
    pub labels: Vec<String>,
    /// Lexicographic order inside a column; `null` sorts last.
    pub board_rank: Option<String>,
    pub external_ref: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardColumnResponse {
    pub board_status: TaskBoardStatus,
    pub items: Vec<TaskBoardItemResponse>,
}

/// `GET /api/task-sessions/board`: always all six columns, in display order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardResponse {
    pub columns: Vec<TaskBoardColumnResponse>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardQuery {
    pub project_id: Option<String>,
    pub assignee_kind: Option<TaskAssigneeKind>,
    pub assignee_id: Option<String>,
    pub label: Option<String>,
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `PATCH /api/task-sessions/{id}/board`. Absent fields are unchanged;
/// `board_rank: null` / `assignee: null` clear the value. `done` cannot be set
/// here; use the review endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateTaskBoardRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_status: Option<TaskBoardStatus>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub board_rank: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Option<TaskAssignee>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskCommentKind {
    Comment,
    StatusChange,
    Assignment,
    ReviewDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCommentResponse {
    pub id: String,
    pub task_session_id: String,
    /// `user` | `agent` | `system`.
    pub author_kind: String,
    pub author_id: String,
    pub kind: TaskCommentKind,
    /// Plain text for `comment`; a JSON object for the event kinds:
    /// `status_change` `{"from","to"}`, `assignment` `{"assignee"}`,
    /// `review_decision` `{"decision","comment"}`.
    pub body: String,
    pub created_at: i64,
    pub edited_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateTaskCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditTaskCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardReviewDecision {
    Approve,
    Reject,
}

/// `POST /api/task-sessions/{id}/review`; user credential only. `reject`
/// requires a non-empty `comment` and sends the item back to `todo` (D6-5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardReviewRequest {
    pub decision: TaskBoardReviewDecision,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardReviewResponse {
    pub item: TaskBoardItemResponse,
    pub decision: TaskCommentResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_request_distinguishes_absent_and_null() {
        let absent: UpdateTaskBoardRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent, UpdateTaskBoardRequest::default());
        let cleared: UpdateTaskBoardRequest = serde_json::from_str(r#"{"assignee":null,"board_rank":null}"#).unwrap();
        assert_eq!(cleared.assignee, Some(None));
        assert_eq!(cleared.board_rank, Some(None));
        let set: UpdateTaskBoardRequest = serde_json::from_str(
            r#"{"board_status":"in_progress","assignee":{"kind":"agent","id":"codex"},"labels":["a"]}"#,
        )
        .unwrap();
        assert_eq!(set.board_status, Some(TaskBoardStatus::InProgress));
        assert_eq!(
            set.assignee,
            Some(Some(TaskAssignee {
                kind: TaskAssigneeKind::Agent,
                id: "codex".into()
            }))
        );
    }

    #[test]
    fn status_round_trips_wire_values() {
        for status in TASK_BOARD_COLUMNS {
            assert_eq!(TaskBoardStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().into())
            );
        }
    }
}

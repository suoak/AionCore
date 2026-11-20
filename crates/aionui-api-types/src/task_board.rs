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

/// 3.0 board lane (plan B C1), derived at read time from `board_status` and
/// the run state; the stored enums are unchanged.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskLane {
    /// backlog / todo, not running.
    #[default]
    Todo,
    InProgress,
    /// Waiting on the user; see `needs_you_reason`.
    NeedsYou,
    Done,
    /// Hidden by default in the UI.
    Cancelled,
}

/// Why a task sits in the `needs_you` lane.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskNeedsYouReason {
    PlanApproval,
    ToolPermission,
    Review,
    Failed,
    MergeConflict,
    Input,
}

/// Where a task came from. Rows created before migration 082 have no stored
/// source; it is inferred from `external_ref` / `conversation_id`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    #[default]
    Manual,
    Conversation,
    Github,
    Cron,
    Agent,
}

impl TaskSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Conversation => "conversation",
            Self::Github => "github",
            Self::Cron => "cron",
            Self::Agent => "agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Manual, Self::Conversation, Self::Github, Self::Cron, Self::Agent]
            .into_iter()
            .find(|source| source.as_str() == value)
    }
}

/// Progress of the agent's own checklist (latest ACP plan of the task's conversation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskPlanSummary {
    pub done: u32,
    pub total: u32,
    /// The step in progress, else the next pending one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
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
    /// M8 E7-6: run state of the newest Squad instance started for this item
    /// (`POST /api/squads/{id}/instantiate` with `work_item_id`). Omitted when
    /// the item is not assigned to a Squad or no instance has anything to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squad_run: Option<crate::SquadRunPayload>,
    /// 3.0 lane (plan B C1). Older clients ignore it.
    #[serde(default)]
    pub lane: TaskLane,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_you_reason: Option<TaskNeedsYouReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_plan: Option<TaskPlanSummary>,
    #[serde(default)]
    pub source: TaskSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_first: bool,
    /// Set while the task waits for a run slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<i64>,
    /// 3.0 C3: adopt / PR outcome, e.g. `{"adopt": {"status": "adopted",
    /// "strategy": "squash", "result_commit": "…"}, "pull_request": {"url": "…"}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<serde_json::Value>,
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
    /// Optimistic concurrency: the `updated_at` the client last saw. When it
    /// no longer matches, the update is refused with 409 `board_item_stale`.
    /// Omit it to keep last-write-wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_updated_at: Option<i64>,
}

/// WebSocket event names for the board (M8 E6). Every payload carries
/// `user_id` (routing) and `task_session_id`.
pub const EVENT_TASK_BOARD_UPDATED: &str = "task.board.updated";
pub const EVENT_TASK_COMMENT_CREATED: &str = "task.comment.created";
pub const EVENT_TASK_COMMENT_UPDATED: &str = "task.comment.updated";

/// What caused a `task.board.updated` event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardChange {
    /// `PATCH /api/task-sessions/{id}/board`.
    Board,
    /// `POST /api/task-sessions/{id}/review`.
    Review,
    /// M8 E7-6: the linked Squad instance's `squad_run` changed state; the
    /// card did not move (`from_board_status == item.board_status`).
    SquadRun,
    /// M8 E6-7: created or refreshed by a read-only GitHub Issue import.
    Import,
}

/// `POST /api/task-sessions/import/github` (M8 E6-7, D6-6): one manual,
/// read-only import of GitHub Issues into the backlog.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportGithubIssuesRequest {
    /// `owner/name`.
    pub repo: String,
    /// GitHub issue search qualifiers (`label:bug`, `milestone:"v2"`, …).
    /// Only open issues are imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Maximum issues to fetch (1–100, default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What an import did to one issue.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportedIssueOutcome {
    /// New backlog work item.
    Created,
    /// Existing item refreshed (title / body / labels only).
    Updated,
    /// Existing item already matched the issue.
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedIssueItem {
    /// `github:owner/name#123`.
    pub external_ref: String,
    pub task_session_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub outcome: ImportedIssueOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportGithubIssuesResponse {
    /// Normalized `owner/name`.
    pub repo: String,
    pub created: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub items: Vec<ImportedIssueItem>,
}

/// `task.board.updated` payload: the item after the change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardUpdatedEvent {
    pub user_id: String,
    pub task_session_id: String,
    pub change: TaskBoardChange,
    /// Column before the change (equal to `item.board_status` when it did not move).
    pub from_board_status: TaskBoardStatus,
    pub item: TaskBoardItemResponse,
}

/// `task.comment.created` / `task.comment.updated` payload. Timeline events
/// (`status_change`, `assignment`, `review_decision`) are comments too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCommentEvent {
    pub user_id: String,
    pub task_session_id: String,
    pub comment: TaskCommentResponse,
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
    fn board_item_new_fields_default_for_old_payloads() {
        let item: TaskBoardItemResponse = serde_json::from_value(serde_json::json!({
            "id": "t", "title": "T", "project_id": null, "conversation_id": null, "mode": "agent",
            "status": "draft", "board_status": "backlog", "assignee": null, "priority": 0, "labels": [],
            "board_rank": null, "external_ref": null, "created_at": 1, "updated_at": 1
        }))
        .unwrap();
        assert_eq!(item.lane, TaskLane::Todo);
        assert_eq!(item.source, TaskSource::Manual);
        assert!(!item.plan_first);
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["lane"], "todo");
        assert!(value.get("needs_you_reason").is_none());
        assert!(value.get("latest_plan").is_none());
        assert!(value.get("plan_first").is_none());
    }

    #[test]
    fn task_source_round_trips() {
        for source in ["manual", "conversation", "github", "cron", "agent"] {
            assert_eq!(TaskSource::parse(source).unwrap().as_str(), source);
        }
        assert_eq!(TaskSource::parse("other"), None);
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

/// What `POST /api/task-sessions/{id}/start` did (3.0 C2). The call is
/// idempotent: a task already running or queued is reported, not restarted.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStartState {
    Started,
    Queued,
    AlreadyRunning,
    AlreadyQueued,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StartTaskResponse {
    pub state: TaskStartState,
    pub item: TaskBoardItemResponse,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskDeliveryKind {
    /// Push the task branch and open a pull request.
    Pr,
    /// Push new commits to the branch of the task's open pull request.
    UpdatePr,
}

/// `POST /api/task-sessions/{id}/deliver` (3.0 C3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliverTaskRequest {
    pub kind: TaskDeliveryKind,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

/// 3.0 C8: how an Issue task is handled, chosen when it starts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueApproach {
    /// Work on a fix in the task's worktree; review, then adopt or open a PR.
    Fix,
    /// Plan first, wait for plan approval, then implement (C4).
    PlanFirst,
    /// Analyse and report only. The task can never be adopted or delivered.
    ReviewOnly,
}

impl IssueApproach {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fix => "fix",
            Self::PlanFirst => "plan_first",
            Self::ReviewOnly => "review_only",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::Fix, Self::PlanFirst, Self::ReviewOnly]
            .into_iter()
            .find(|approach| approach.as_str() == value)
    }
}

/// Optional body of `POST /api/task-sessions/{id}/start` (3.0 C8). Only a
/// task that has not run yet can take options; `approach` is for Issue
/// tasks (`source = github`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartTaskRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approach: Option<IssueApproach>,
    /// Pick the assistant now (imported Issues have none yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
}

/// `GET /api/task-sessions/{id}/issue-comment/draft` (3.0 C8): a suggested
/// comment for the task's Issue. Nothing is posted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IssueCommentDraftResponse {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
    pub body: String,
    /// Set once a comment was posted for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posted_url: Option<String>,
}

/// `POST /api/task-sessions/{id}/issue-comment` (3.0 C8). Posts exactly
/// `body` (the user-edited draft) under the Issue; `confirm` must be `true`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PostIssueCommentRequest {
    pub body: String,
    #[serde(default)]
    pub confirm: bool,
}

#[cfg(test)]
mod c8_tests {
    use super::*;

    #[test]
    fn start_options_and_comment_bodies_are_strict() {
        assert_eq!(
            serde_json::from_value::<StartTaskRequest>(serde_json::json!({})).unwrap(),
            StartTaskRequest::default()
        );
        let start: StartTaskRequest =
            serde_json::from_value(serde_json::json!({ "approach": "review_only", "assistant_id": "a1" })).unwrap();
        assert_eq!(start.approach, Some(IssueApproach::ReviewOnly));
        assert!(serde_json::from_value::<StartTaskRequest>(serde_json::json!({ "approach": "merge" })).is_err());
        assert!(serde_json::from_value::<StartTaskRequest>(serde_json::json!({ "mode": "yolo" })).is_err());
        for approach in [IssueApproach::Fix, IssueApproach::PlanFirst, IssueApproach::ReviewOnly] {
            assert_eq!(IssueApproach::parse(approach.as_str()), Some(approach));
        }

        let post: PostIssueCommentRequest = serde_json::from_value(serde_json::json!({ "body": "Fixed" })).unwrap();
        assert!(!post.confirm, "confirmation is never implied");
        assert!(
            serde_json::from_value::<PostIssueCommentRequest>(
                serde_json::json!({ "body": "x", "confirm": true, "repo": "other/repo" })
            )
            .is_err()
        );
    }
}

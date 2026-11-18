//! 3.0 C2: `POST /api/task-sessions/{id}/start`.
//!
//! Start = governance check (`LaunchSurface::Task`) → run slot or queue →
//! launch through [`TaskLaunchPort`] (one-slot worktree fan-out when the task
//! has a `repo_path`, otherwise a plain conversation) → `status = running`,
//! board `in_progress`. The call is idempotent. A Host restart pauses running
//! tasks (`recover_task_sessions`, lane needs_you) and re-dispatches the queue.
//!
//! The launched conversation uses the assistant's own permission mode; the
//! request carries no permission override (no-YOLO invariant), and creating
//! that conversation goes through the usual `ConversationCreate` checks too.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aionui_api_types::{StartTaskResponse, TaskBoardChange, TaskBoardStatus, TaskStartState};
use aionui_common::now_ms;
use aionui_db::UpdateTaskBoardParams;
use async_trait::async_trait;

use super::{item_response, map_db};
use crate::error::ConversationError;
use crate::service::ConversationService;

/// Tasks running at once on this Host unless the setting says otherwise.
pub const DEFAULT_MAX_RUNNING_TASKS: usize = 2;
pub const MAX_RUNNING_TASKS_LIMIT: usize = 8;
/// How many queued tasks one drain looks at.
const QUEUE_SCAN: i64 = 64;

/// What to launch. `prompt` is the composer text (or the objective).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLaunchSpec {
    pub task_session_id: String,
    pub title: String,
    pub assistant_id: String,
    pub prompt: String,
    pub repo_path: Option<String>,
    pub base_ref: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskLaunched {
    pub conversation_id: Option<String>,
    pub fanout_group_id: Option<String>,
}

/// Implemented by the app over the fan-out service and conversation create.
#[async_trait]
pub trait TaskLaunchPort: Send + Sync {
    async fn launch(&self, user_id: &str, spec: &TaskLaunchSpec) -> Result<TaskLaunched, ConversationError>;
}

fn prompt_text(prompt_json: Option<&str>, objective: &str, title: &str) -> String {
    let from_prompt = prompt_json
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|value| value.get("text").and_then(|text| text.as_str()).map(str::to_owned))
        .filter(|text| !text.trim().is_empty());
    from_prompt
        .or_else(|| Some(objective.to_owned()).filter(|text| !text.trim().is_empty()))
        .unwrap_or_else(|| title.to_owned())
}

impl ConversationService {
    pub fn with_task_launch_port(&self, port: Arc<dyn TaskLaunchPort>) {
        if let Ok(mut guard) = self.task_launch.write() {
            *guard = Some(port);
        }
    }

    /// Clamped to `1..=MAX_RUNNING_TASKS_LIMIT`.
    pub fn set_max_running_tasks(&self, limit: usize) {
        self.max_running_tasks
            .store(limit.clamp(1, MAX_RUNNING_TASKS_LIMIT), Ordering::Relaxed);
    }

    fn task_launch_port(&self) -> Result<Arc<dyn TaskLaunchPort>, ConversationError> {
        self.task_launch
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| ConversationError::internal("Task launching is not configured"))
    }

    pub async fn start_task(&self, user_id: &str, id: &str) -> Result<StartTaskResponse, ConversationError> {
        let _guard = self.task_start_lock.lock().await;
        let board = self.task_board_repo()?;
        let task = self
            .task_session_repo()?
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        let flow = board
            .get_task_flow(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        let row = self.board_item(user_id, id).await?;
        let respond = |state, row| item_response(row).map(|item| StartTaskResponse { state, item });

        if task.status == "running" || task.status == "waiting_approval" {
            return respond(TaskStartState::AlreadyRunning, row);
        }
        if matches!(row.board_status.as_str(), "done" | "cancelled" | "in_review") {
            return Err(ConversationError::bad_request(format!(
                "A task in '{}' cannot be started",
                row.board_status
            )));
        }
        if !matches!(task.status.as_str(), "draft" | "ready" | "paused" | "failed") {
            return Err(ConversationError::bad_request(format!(
                "A task in run state '{}' cannot be started",
                task.status
            )));
        }
        let assistant_id = flow
            .assistant_id
            .clone()
            .ok_or_else(|| ConversationError::bad_request("Pick an assistant before starting the task"))?;
        self.task_launch_port()?;

        let mut facts = aionui_db::LaunchFacts::new(aionui_db::LaunchSurface::Task);
        facts.target = Some(("task_session", id));
        facts.assistant_id = Some(assistant_id.as_str());
        self.enforce_launch_policy(user_id, &facts).await?;

        let running = usize::try_from(board.count_running_tasks().await?).unwrap_or(usize::MAX);
        if running >= self.max_running_tasks.load(Ordering::Relaxed) {
            let state = if flow.queued_at.is_some() {
                TaskStartState::AlreadyQueued
            } else {
                board.set_task_queued(user_id, id, Some(now_ms())).await?;
                tracing::info!(task_session_id = %id, running, "task queued for a run slot");
                TaskStartState::Queued
            };
            let row = self.board_item(user_id, id).await?;
            return respond(state, row);
        }

        let spec = TaskLaunchSpec {
            task_session_id: id.to_owned(),
            title: task.title.clone(),
            assistant_id,
            prompt: prompt_text(flow.prompt_json.as_deref(), &task.objective, &task.title),
            repo_path: flow.repo_path.clone(),
            base_ref: flow.base_ref.clone(),
        };
        let row = self.launch_task(user_id, &spec, row).await?;
        respond(TaskStartState::Started, row)
    }

    async fn launch_task(
        &self,
        user_id: &str,
        spec: &TaskLaunchSpec,
        row: aionui_db::models::TaskBoardItemRow,
    ) -> Result<aionui_db::models::TaskBoardItemRow, ConversationError> {
        let id = spec.task_session_id.as_str();
        let board = self.task_board_repo()?;
        let launched = match self.task_launch_port()?.launch(user_id, spec).await {
            Ok(launched) => launched,
            Err(error) => {
                // Leave it startable; the error explains why (stays queued: no).
                board.set_task_queued(user_id, id, None).await?;
                return Err(error);
            }
        };
        let now = now_ms();
        if !board
            .mark_task_started(
                user_id,
                id,
                launched.conversation_id.as_deref(),
                launched.fanout_group_id.as_deref(),
                now,
            )
            .await?
        {
            return Err(ConversationError::Busy {
                reason: format!("Task session '{id}' changed while starting"),
            });
        }
        let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
        if matches!(from, TaskBoardStatus::Backlog | TaskBoardStatus::Todo) {
            let moved_at = now_ms().max(row.updated_at + 1);
            board
                .update_board(
                    user_id,
                    id,
                    &UpdateTaskBoardParams {
                        board_status: Some(TaskBoardStatus::InProgress.as_str()),
                        updated_at: moved_at,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|error| map_db(id, error))?;
            self.append_event(
                user_id,
                id,
                "status_change",
                serde_json::json!({ "from": from.as_str(), "to": TaskBoardStatus::InProgress.as_str() }),
                moved_at,
            )
            .await?;
        }
        tracing::info!(
            task_session_id = %id,
            worktree = launched.fanout_group_id.is_some(),
            "task started"
        );
        let row = self.board_item(user_id, id).await?;
        if let Ok(item) = item_response(row.clone()) {
            self.publish_board_updated(user_id, TaskBoardChange::Board, from, &item);
        }
        Ok(row)
    }

    /// Start queued tasks while run slots are free (oldest first). Called
    /// after a task run settles and on boot. Failures are logged and leave
    /// the task out of the queue so one bad task cannot block the rest.
    pub async fn drain_task_queue(&self) -> usize {
        let mut started = 0;
        let Ok(board) = self.task_board_repo() else {
            return 0;
        };
        let queued = match board.list_queued_tasks(QUEUE_SCAN).await {
            Ok(queued) => queued,
            Err(error) => {
                tracing::warn!(%error, "task queue: list failed");
                return 0;
            }
        };
        for (user_id, id) in queued {
            match board.count_running_tasks().await {
                Ok(running)
                    if usize::try_from(running).unwrap_or(usize::MAX)
                        < self.max_running_tasks.load(Ordering::Relaxed) => {}
                Ok(_) => break,
                Err(error) => {
                    tracing::warn!(%error, "task queue: count failed");
                    break;
                }
            }
            if let Err(error) = board.set_task_queued(&user_id, &id, None).await {
                tracing::warn!(%error, task_session_id = %id, "task queue: dequeue failed");
                continue;
            }
            match self.start_task(&user_id, &id).await {
                Ok(response) if response.state == TaskStartState::Started => started += 1,
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, task_session_id = %id, "queued task could not start"),
            }
        }
        started
    }
}

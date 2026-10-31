//! M8 E8 glue: the batch runner's executor port on top of the conversation,
//! assistant, worktree and task-board services (the batch crate depends on
//! none of them).
//!
//! Each attempt is one TaskSession (`mode = agent`, D8-3) labelled `batch`
//! on the board, run in its own managed worktree (repo targets, keyed by the
//! TaskSession id) or in place (workspace targets).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aionui_api_types::{EVENT_BATCH_ITEM_UPDATED, EVENT_BATCH_JOB_UPDATED, WebSocketMessage};
use aionui_batch::{AttemptOutcome, BatchError, BatchEvent, IBatchExecutor, ItemSpec, PreparedItem, TargetLocation};
use aionui_db::{
    CreateTaskSessionParams, ITaskBoardRepository, ITaskSessionRepository, UpdateTaskBoardParams,
    UpdateTaskSessionParams,
};
use aionui_realtime::EventBroadcaster;
use aionui_worktree::{WorkItemWorktreeSource, WorktreeService};
use async_trait::async_trait;

/// Board label every batch TaskSession carries.
pub(crate) const BATCH_LABEL: &str = "batch";
/// Headroom kept on top of `concurrency × checkout size` (512 MiB).
const DISK_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) struct BatchExecutorAdapter {
    pub(crate) conversations: aionui_conversation::ConversationService,
    pub(crate) task_manager: Arc<dyn aionui_ai_agent::IWorkerTaskManager>,
    pub(crate) assistants: Arc<aionui_assistant::AssistantService>,
    pub(crate) worktrees: WorktreeService,
    pub(crate) task_sessions: Arc<dyn ITaskSessionRepository>,
    pub(crate) board: Arc<dyn ITaskBoardRepository>,
    pub(crate) broadcaster: Option<Arc<dyn EventBroadcaster>>,
}

fn unavailable(e: impl std::fmt::Display) -> BatchError {
    BatchError::Unavailable(e.to_string())
}

/// `(task_sessions.status, board_status)` for an attempt outcome. Failures go
/// to `in_review` so a person looks at them; cancellations close the card.
pub(crate) fn outcome_states(outcome: AttemptOutcome) -> (&'static str, &'static str) {
    match outcome {
        AttemptOutcome::Passed => ("completed", "done"),
        AttemptOutcome::Failed | AttemptOutcome::Timeout => ("failed", "in_review"),
        AttemptOutcome::Cancelled => ("cancelled", "cancelled"),
    }
}

/// Free bytes needed for `concurrency` checkouts of `tracked` bytes each.
pub(crate) fn required_bytes(tracked: &[u64], concurrency: u32) -> u64 {
    let mut sizes = tracked.to_vec();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    // Worst case: the `concurrency` largest repositories at once.
    sizes
        .iter()
        .take(concurrency.max(1) as usize)
        .fold(0u64, |acc, s| acc.saturating_add(*s))
        .saturating_add(DISK_HEADROOM_BYTES)
}

fn free_bytes(dir: &Path) -> Option<u64> {
    // The worktree root may not exist yet: walk up to the nearest ancestor.
    let mut probe: Option<&Path> = Some(dir);
    while let Some(p) = probe {
        if p.exists() {
            return fs2::available_space(p).ok();
        }
        probe = p.parent();
    }
    None
}

impl BatchExecutorAdapter {
    async fn set_board(&self, user_id: &str, task_session_id: &str, board_status: &str, labels: Option<&[String]>) {
        let params = UpdateTaskBoardParams {
            board_status: Some(board_status),
            board_rank: None,
            assignee: None,
            priority: None,
            labels,
            updated_at: aionui_common::now_ms(),
            expected_updated_at: None,
        };
        if let Err(error) = self.board.update_board(user_id, task_session_id, &params).await {
            tracing::warn!(task_session_id, error = %error, "batch: board update failed");
        }
    }

    async fn agent_type(&self, user_id: &str, spec: &ItemSpec) -> Result<String, BatchError> {
        if let Some(t) = spec.agent_type.as_deref().filter(|t| !t.trim().is_empty()) {
            return Ok(t.to_owned());
        }
        let assistant = self
            .assistants
            .get_for_user(user_id, &spec.assistant_id)
            .await
            .map_err(unavailable)?;
        Ok(assistant.agent_id)
    }

    async fn create_conversation(
        &self,
        user_id: &str,
        spec: &ItemSpec,
        worktree_id: Option<String>,
        workspace: Option<&Path>,
    ) -> Result<String, BatchError> {
        let extra = match workspace {
            Some(dir) => serde_json::json!({ "workspace": dir.to_string_lossy() }),
            None => serde_json::json!({}),
        };
        let req = aionui_api_types::CreateConversationRequest {
            r#type: None,
            name: Some(format!("{} #{}", spec.job_title, spec.seq)),
            model: None,
            assistant: Some(aionui_api_types::AssistantConversationRequest {
                id: spec.assistant_id.clone(),
                locale: None,
                conversation_overrides: None,
            }),
            source: None,
            channel_chat_id: None,
            extra,
            worktree_id,
        };
        self.conversations
            .create(user_id, req)
            .await
            .map(|c| c.id)
            .map_err(unavailable)
    }
}

#[async_trait]
impl IBatchExecutor for BatchExecutorAdapter {
    async fn check_assistant(&self, user_id: &str, assistant_id: &str) -> Result<(), BatchError> {
        let assistant = self
            .assistants
            .get_for_user(user_id, assistant_id)
            .await
            .map_err(|e| BatchError::BadRequest(format!("assistant {assistant_id}: {e}")))?;
        if !assistant.enabled {
            return Err(BatchError::BadRequest(format!("assistant {assistant_id} is disabled")));
        }
        Ok(())
    }

    async fn check_target(&self, location: &TargetLocation) -> Result<(), BatchError> {
        let path = location.path();
        if !path.is_dir() {
            return Err(BatchError::BadRequest(format!("{} is not a directory", path.display())));
        }
        if let TargetLocation::Repo(repo) = location {
            self.worktrees
                .estimate(&repo.to_string_lossy(), None)
                .await
                .map_err(|e| BatchError::BadRequest(format!("{}: {e}", repo.display())))?;
        }
        Ok(())
    }

    async fn disk_precheck(&self, repos: &[PathBuf], concurrency: u32) -> Result<(), BatchError> {
        if repos.is_empty() {
            return Ok(());
        }
        let mut tracked = Vec::with_capacity(repos.len());
        for repo in repos {
            let estimate = self
                .worktrees
                .estimate(&repo.to_string_lossy(), None)
                .await
                .map_err(|e| BatchError::BadRequest(format!("{}: {e}", repo.display())))?;
            tracked.push(estimate.tracked_bytes);
        }
        let needed = required_bytes(&tracked, concurrency);
        let root = self.worktrees.root_dir();
        let Some(free) = free_bytes(root) else {
            tracing::warn!(root = %root.display(), "batch: free space unknown; precheck skipped");
            return Ok(());
        };
        if free < needed {
            return Err(BatchError::InsufficientDisk(format!(
                "not enough free disk for {concurrency} concurrent worktrees: need ~{} MiB, {} MiB free",
                needed / (1024 * 1024),
                free / (1024 * 1024)
            )));
        }
        Ok(())
    }

    async fn create_session(&self, user_id: &str, spec: &ItemSpec) -> Result<String, BatchError> {
        let agent_type = self.agent_type(user_id, spec).await?;
        let title = format!("{} #{}", spec.job_title, spec.seq);
        let objective = format!(
            "Batch {} item {}: {}",
            spec.job_id,
            spec.seq,
            spec.location.path().display()
        );
        let row = self
            .task_sessions
            .create(&CreateTaskSessionParams {
                user_id,
                title: &title,
                project_id: spec.project_id.as_deref(),
                conversation_id: None,
                mode: "agent",
                objective: &objective,
                acceptance_criteria: "[]",
                status: "running",
                agent_type: &agent_type,
                agent_session_id: None,
            })
            .await
            .map_err(|e| BatchError::Database(e.to_string()))?;
        let labels = [BATCH_LABEL.to_owned()];
        self.set_board(user_id, &row.id, "in_progress", Some(&labels)).await;
        Ok(row.id)
    }

    async fn prepare(&self, user_id: &str, task_session_id: &str, spec: &ItemSpec) -> Result<PreparedItem, BatchError> {
        let (conversation_id, workspace) = match &spec.location {
            TargetLocation::Repo(repo) => {
                let title = format!("{} #{}", spec.job_title, spec.seq);
                let wt = self
                    .worktrees
                    .ensure_for_work_item(
                        user_id,
                        task_session_id,
                        Some(&title),
                        &[WorkItemWorktreeSource {
                            repo_path: repo.to_string_lossy().into_owned(),
                            explicit: true,
                        }],
                    )
                    .await
                    .map_err(unavailable)?
                    .ok_or_else(|| BatchError::Unavailable(format!("{} is not a git repository", repo.display())))?;
                if wt.row.status != "ready" {
                    return Err(BatchError::Unavailable(format!(
                        "worktree {} is {}{}",
                        wt.row.id,
                        wt.row.status,
                        wt.row
                            .error_message
                            .as_deref()
                            .map(|e| format!(": {e}"))
                            .unwrap_or_default()
                    )));
                }
                let conv = self
                    .create_conversation(user_id, spec, Some(wt.row.id.clone()), None)
                    .await?;
                (conv, PathBuf::from(wt.row.path))
            }
            TargetLocation::Workspace(dir) => {
                let conv = self.create_conversation(user_id, spec, None, Some(dir)).await?;
                (conv, dir.clone())
            }
        };
        self.task_sessions
            .update(
                user_id,
                task_session_id,
                &UpdateTaskSessionParams {
                    conversation_id: Some(&conversation_id),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| BatchError::Database(e.to_string()))?;
        Ok(PreparedItem {
            conversation_id,
            workspace,
        })
    }

    async fn send_prompt(&self, user_id: &str, conversation_id: &str, prompt: &str) -> Result<(), BatchError> {
        let req = aionui_api_types::SendMessageRequest {
            content: prompt.to_owned(),
            files: Vec::new(),
            sessions: Vec::new(),
            inject_skills: Vec::new(),
            hidden: false,
        };
        self.conversations
            .send_message(user_id, conversation_id, req, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(unavailable)
    }

    async fn is_busy(&self, conversation_id: &str) -> bool {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        summary.is_processing || summary.state != aionui_api_types::ConversationRuntimeStateKind::Idle
    }

    async fn cancel_turn(&self, user_id: &str, conversation_id: &str) {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        let Some(turn_id) = summary.turn_id else {
            return;
        };
        if let Err(error) = self
            .conversations
            .cancel(user_id, conversation_id, &turn_id, &self.task_manager)
            .await
        {
            tracing::warn!(conversation_id, error = %error, "batch: cancel turn failed");
        }
    }

    async fn finish_session(&self, user_id: &str, task_session_id: &str, outcome: AttemptOutcome, summary: &str) {
        let (status, board_status) = outcome_states(outcome);
        if let Err(error) = self
            .task_sessions
            .update(
                user_id,
                task_session_id,
                &UpdateTaskSessionParams {
                    status: Some(status),
                    ..Default::default()
                },
            )
            .await
        {
            tracing::warn!(task_session_id, error = %error, "batch: task session update failed");
        }
        self.set_board(user_id, task_session_id, board_status, None).await;
        tracing::info!(task_session_id, status, summary, "batch: attempt finished");
    }

    fn notify(&self, user_id: &str, event: BatchEvent) {
        let Some(b) = &self.broadcaster else {
            return;
        };
        let (name, payload) = match event {
            BatchEvent::JobUpdated { job_id, status } => (
                EVENT_BATCH_JOB_UPDATED,
                serde_json::json!({ "user_id": user_id, "job_id": job_id, "status": status }),
            ),
            BatchEvent::ItemUpdated {
                job_id,
                item_id,
                status,
            } => (
                EVENT_BATCH_ITEM_UPDATED,
                serde_json::json!({ "user_id": user_id, "job_id": job_id, "item_id": item_id, "status": status }),
            ),
        };
        b.broadcast(WebSocketMessage::new(name, payload));
    }
}

/// Build the batch state and its executor. Called once from
/// `build_module_states`; the caller runs `recover_on_boot`.
pub(crate) fn build_batch_state(
    services: &crate::services::AppServices,
    worktrees: &WorktreeService,
    assistants: Arc<aionui_assistant::AssistantService>,
) -> aionui_batch::BatchRouterState {
    let pool = services.database.pool().clone();
    let exec = Arc::new(BatchExecutorAdapter {
        conversations: services.conversation_service.clone(),
        task_manager: services.worker_task_manager.clone(),
        assistants,
        worktrees: worktrees.clone(),
        task_sessions: Arc::new(aionui_db::SqliteTaskSessionRepository::new(pool.clone())),
        board: Arc::new(aionui_db::SqliteTaskBoardRepository::new(pool.clone())),
        broadcaster: Some(services.event_bus.clone() as Arc<dyn EventBroadcaster>),
    });
    let repo = Arc::new(aionui_db::SqliteBatchRepository::new(pool));
    let service = aionui_batch::BatchService::new(repo, exec, aionui_batch::BatchRunnerConfig::default());
    aionui_batch::BatchRouterState {
        service: Arc::new(service),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_maps_to_run_and_board_state() {
        assert_eq!(outcome_states(AttemptOutcome::Passed), ("completed", "done"));
        assert_eq!(outcome_states(AttemptOutcome::Failed), ("failed", "in_review"));
        assert_eq!(outcome_states(AttemptOutcome::Timeout), ("failed", "in_review"));
        assert_eq!(outcome_states(AttemptOutcome::Cancelled), ("cancelled", "cancelled"));
    }

    #[test]
    fn disk_need_is_the_largest_concurrent_checkouts_plus_headroom() {
        let mib = 1024 * 1024;
        assert_eq!(
            required_bytes(&[10 * mib, 30 * mib, 20 * mib], 2),
            50 * mib + DISK_HEADROOM_BYTES
        );
        assert_eq!(required_bytes(&[10 * mib], 4), 10 * mib + DISK_HEADROOM_BYTES);
        assert_eq!(required_bytes(&[5 * mib, 5 * mib], 0), 5 * mib + DISK_HEADROOM_BYTES);
        assert_eq!(required_bytes(&[u64::MAX, u64::MAX], 2), u64::MAX);
    }

    #[test]
    fn free_bytes_walks_up_to_an_existing_ancestor() {
        let dir = std::env::temp_dir().join("aionui-batch-free-probe/does/not/exist");
        assert!(free_bytes(&dir).is_some());
    }
}

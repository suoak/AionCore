//! 3.0 C6 (plan B §3): a cron job with `run_as = task` creates a board task
//! per occurrence instead of sending a conversation turn.
//!
//! - The task has `source = cron`, `source_ref = <job id>`, the job's name as
//!   title, the job's message as objective and the job's assistant.
//! - `start = false` leaves it in the backlog. `start = true` runs
//!   [`ConversationService::start_task`]: the E12 run permission
//!   (`LaunchSurface::Task`, audited) and the run-slot queue apply exactly as
//!   for a manual start. A refused start leaves the task in the backlog and is
//!   reported as the occurrence's error.
//! - A finished run goes to review like every task (`review_required`,
//!   `agent_review_gate`); adopt is always a human decision (desktop, or a
//!   C9 remote approval bound to the diff). Nothing merges on its own.
//! - `skip_if_open`: while an earlier task of the same job is still on the
//!   board (not done / cancelled) the occurrence creates nothing.

use aionui_api_types::{
    CreateTaskFlowFields, CreateTaskSessionRequest, TaskSessionMode, TaskSessionStatus, TaskSource, TaskStartState,
};

use crate::error::ConversationError;
use crate::service::ConversationService;

/// Task titles are 1..=200 characters.
const TASK_TITLE_MAX_CHARS: usize = 200;

/// What a `run_as = task` occurrence asks for.
#[derive(Debug, Clone, Default)]
pub struct CronTaskRequest {
    pub job_id: String,
    pub job_name: String,
    pub message: String,
    pub agent_type: String,
    pub assistant_id: Option<String>,
    pub repo_path: Option<String>,
    pub base_ref: Option<String>,
    pub start: bool,
    pub skip_if_open: bool,
}

/// What the occurrence did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronTaskOutcome {
    /// A task of this job is still open; nothing was created.
    SkippedOpen { task_session_id: String },
    /// Created in the backlog (`start = false`).
    Created { task_session_id: String },
    /// Created and started, or queued for a run slot.
    Started {
        task_session_id: String,
        state: TaskStartState,
    },
    /// Created, but the start was refused (run permission, missing
    /// assistant, …). The task stays in the backlog.
    StartRefused { task_session_id: String, reason: String },
}

impl CronTaskOutcome {
    pub fn task_session_id(&self) -> &str {
        match self {
            Self::SkippedOpen { task_session_id }
            | Self::Created { task_session_id }
            | Self::Started { task_session_id, .. }
            | Self::StartRefused { task_session_id, .. } => task_session_id,
        }
    }
}

fn task_title(name: &str) -> String {
    let name = name.trim();
    let name = if name.is_empty() { "Scheduled task" } else { name };
    name.chars().take(TASK_TITLE_MAX_CHARS).collect()
}

impl ConversationService {
    pub async fn enqueue_cron_task(
        &self,
        user_id: &str,
        request: CronTaskRequest,
    ) -> Result<CronTaskOutcome, ConversationError> {
        let job_id = request.job_id.trim();
        if job_id.is_empty() {
            return Err(ConversationError::bad_request("A cron task needs its job id"));
        }
        if request.skip_if_open
            && let Some(task_session_id) = self
                .task_board_repo()?
                .find_open_task_by_source(user_id, TaskSource::Cron.as_str(), job_id)
                .await?
        {
            tracing::info!(job_id, task_session_id = %task_session_id, "cron task: previous task still open, skipped");
            return Ok(CronTaskOutcome::SkippedOpen { task_session_id });
        }
        let task = self
            .create_task_session_from(
                user_id,
                CreateTaskSessionRequest {
                    title: task_title(&request.job_name),
                    project_id: None,
                    conversation_id: None,
                    mode: TaskSessionMode::Agent,
                    objective: request.message.clone(),
                    acceptance_criteria: Vec::new(),
                    status: TaskSessionStatus::Ready,
                    agent_type: request.agent_type.clone(),
                    agent_session_id: None,
                    flow: CreateTaskFlowFields {
                        assistant_id: request.assistant_id.clone(),
                        repo_path: request.repo_path.clone(),
                        base_ref: request.base_ref.clone(),
                        source_ref: Some(job_id.to_owned()),
                        ..Default::default()
                    },
                },
                Some(TaskSource::Cron),
            )
            .await?;
        tracing::info!(job_id, task_session_id = %task.id, start = request.start, "cron task created");
        if !request.start {
            return Ok(CronTaskOutcome::Created {
                task_session_id: task.id,
            });
        }
        match self.start_task(user_id, &task.id).await {
            Ok(started) => Ok(CronTaskOutcome::Started {
                task_session_id: task.id,
                state: started.state,
            }),
            Err(error) => {
                tracing::warn!(job_id, task_session_id = %task.id, %error, "cron task: start refused");
                Ok(CronTaskOutcome::StartRefused {
                    task_session_id: task.id,
                    reason: error.to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::task_title;

    #[test]
    fn titles_are_never_empty_or_too_long() {
        assert_eq!(task_title("  "), "Scheduled task");
        assert_eq!(task_title(" Nightly deps "), "Nightly deps");
        assert_eq!(task_title(&"é".repeat(300)).chars().count(), 200);
    }
}
